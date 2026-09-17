//! [`HdDemod`] — HD Radio as a demodulator: channel-rate complex baseband in,
//! decoded programme audio out.
//!
//! It sits where `DrmDemod` or `WfmDemod` would, because that is what it is —
//! RF in, sound out, no keyboard, no transmit, no QSO. The differences from an
//! analog demod are the same ones DRM has, and for the same reasons:
//!
//! * the audio for a block is not made from that block's samples. HD Radio's
//!   audio is a block-interleaved HDC stream behind an OFDM frame, so what
//!   comes back now was transmitted a fraction of a second ago;
//! * the decoder produces audio at its own rate (44.1 kHz), not ours, so the
//!   two are rate-matched here rather than assumed equal;
//! * the decoder is a vendored C library that runs its own worker thread, so
//!   samples are piped in and events — sync, MER, audio, station text — are
//!   drained back out.
//!
//! FM only for now. The AM-band variant (HD on AM) needs the decoder opened in
//! its AM mode and fed at 46,511.71875 S/s instead, which the engine would have
//! to choose from the dial frequency; until then this demod serves the FM band,
//! where nearly all HD Radio listening is.

use std::collections::VecDeque;

use num_complex::Complex32;
use sdroxide_dsp::{ComplexResampler, Demodulator};
use sdroxide_types::{HdAudioService, HdRadioStatus};
use tracing::{debug, warn};

use crate::{Event, HdReceiver, Mode};

/// FM hybrid HD Radio's native pipe rate, in samples per second.
pub const FM_RATE_HZ: f64 = 744_187.5;

/// Rate of the decoded audio nrsc5 emits, in samples per second.
pub const AUDIO_RATE: f64 = 44_100.0;

/// Decoded audio held back before playback, and the point at which a backlog is
/// dropped rather than allowed to become latency. In steady state the decoder
/// produces almost exactly real time and neither applies; the cap is what keeps
/// a burst after re-acquisition from playing out seconds late.
const MAX_BACKLOG_FRAMES: usize = (0.6 * AUDIO_RATE) as usize;
const TARGET_BACKLOG_FRAMES: usize = (0.3 * AUDIO_RATE) as usize;

/// Target RMS of the samples handed to the decoder.
///
/// The C library works on 32-bit floats and its OFDM demodulator is level
/// agnostic, but the HDC decoder downstream expects a sane amplitude, and a
/// level that swings with fading costs decodes. Slow to follow, like DRM's.
const TARGET_RMS: f32 = 0.06;

/// One-pole coefficient for the level estimate, ~0.5 s at the channel rate.
const LEVEL_ALPHA: f32 = 1.0 / (0.5 * 96_000.0);

/// HD Radio (NRSC-5) as a receive-chain demodulator, FM only.
pub struct HdDemod {
    /// `None` when the decoder could not start — the mode then behaves as a
    /// silent receiver rather than taking the radio down.
    receiver: Option<HdReceiver>,
    channel_rate: f64,
    resampler: Option<ComplexResampler>,

    /// Channel IQ resampled to the decoder's rate.
    rs_buf: Vec<Complex32>,
    /// ...and the same, as the interleaved floats the decoder reads.
    iq_buf: Vec<f32>,
    /// Decoded audio waiting to be played, at [`AUDIO_RATE`], as the
    /// interleaved stereo pairs nrsc5 produces — two `i16` per frame.
    audio: VecDeque<i16>,
    /// The side, `(L - R) / 2`, of the block being played, for the stereo
    /// blend. Built by `process`, read once by `take_side`.
    side: Vec<f32>,

    /// Mean square of the resampled input, for the level normalisation.
    level: f32,
    /// Post-filter signal power for the S-meter.
    power: f32,
    /// Fractional part of the input-to-audio sample accounting.
    frame_debt: f64,

    /// Audio frames dropped by the backlog trim, cumulatively. A steady
    /// stream of these is a decoder that is not being drained at the rate it
    /// produces, which is how the old one-value-per-frame pacing bug showed
    /// itself; exposing the count lets a test or the capture harness assert
    /// it stays zero.
    drops: u64,

    /// Which programme is being listened to, 0-based.
    selected: u8,

    status: HdRadioStatus,
    /// Cleared by `take_hd_radio`, so the engine only republishes what moved.
    status_dirty: bool,
}

impl HdDemod {
    /// Build an FM HD Radio demodulator for a chain whose channel rate is
    /// `channel_rate`.
    pub fn new(channel_rate: f64) -> Self {
        let receiver = match HdReceiver::open(Mode::Fm) {
            Ok(r) => {
                debug!(channel_rate, "HD Radio decoder attached to the receive chain");
                Some(r)
            }
            Err(e) => {
                warn!(?e, "could not start the HD Radio decoder; the mode will be silent");
                None
            }
        };
        HdDemod {
            receiver,
            channel_rate,
            resampler: ComplexResampler::new(channel_rate, FM_RATE_HZ),
            rs_buf: Vec::new(),
            iq_buf: Vec::new(),
            audio: VecDeque::new(),
            side: Vec::new(),
            level: 0.0,
            power: 0.0,
            frame_debt: 0.0,
            drops: 0,
            selected: 0,
            status: HdRadioStatus::default(),
            status_dirty: true,
        }
    }

    /// Re-acquire from scratch. The demod cannot see a retune — the DDC ahead of
    /// it absorbs that — so the engine says.
    pub fn restart(&mut self) {
        let was_open = self.receiver.is_some();
        self.receiver = None;
        self.audio.clear();
        self.side.clear();
        self.frame_debt = 0.0;
        self.status = HdRadioStatus::default();
        self.status_dirty = true;
        if was_open {
            match HdReceiver::open(Mode::Fm) {
                Ok(r) => self.receiver = Some(r),
                Err(e) => {
                    warn!(?e, "could not restart the HD Radio decoder; the mode will be silent")
                }
            }
        }
    }

    /// Listen to a different programme of the multiplex, 0-based. Audio for the
    /// previous one is dropped rather than mixed in.
    pub fn select_program(&mut self, program: u8) {
        if program == self.selected {
            return;
        }
        self.selected = program;
        self.audio.clear();
        self.status.program = program;
        self.status_dirty = true;
    }

    /// Audio frames dropped by the backlog trim since construction.
    ///
    /// Zero on a healthy decode at the right rate. A climbing count means the
    /// queue is not being drained as fast as the decoder fills it — the shape
    /// the old one-value-per-frame pacing bug took — so a capture test or the
    /// harness can assert this stays at zero.
    pub fn backlog_drops(&self) -> u64 {
        self.drops
    }

    /// Resample the block to the decoder's rate, normalise it and pipe it in.
    fn feed(&mut self, iq: &[Complex32]) {
        let Some(receiver) = self.receiver.as_ref() else {
            return;
        };
        self.rs_buf.clear();
        match self.resampler.as_mut() {
            Some(rs) => rs.push(iq, &mut self.rs_buf),
            None => self.rs_buf.extend_from_slice(iq),
        }
        if self.rs_buf.is_empty() {
            return;
        }

        for z in &self.rs_buf {
            let p = z.re * z.re + z.im * z.im;
            self.level += LEVEL_ALPHA * (p - self.level);
        }
        // A silent input would divide by zero and then clip on the first real
        // sample; hold the gain where it was until there is something to
        // measure.
        let rms = self.level.sqrt();
        let gain = if rms > 1e-9 { (TARGET_RMS / rms).clamp(1.0e-3, 1.0e4) } else { 0.0 };

        self.iq_buf.clear();
        self.iq_buf.reserve(self.rs_buf.len() * 2);
        for z in &self.rs_buf {
            self.iq_buf.push(z.re * gain);
            self.iq_buf.push(z.im * gain);
        }
        if let Err(e) = receiver.pipe_cf32(&self.iq_buf) {
            warn!(?e, "the HD Radio decoder rejected a block of samples");
        }
    }

    /// Drain whatever the decoder has queued: status events and audio.
    fn drain(&mut self) {
        let Some(receiver) = self.receiver.as_ref() else {
            return;
        };
        // `poll` is non-blocking, so this ends when the queue is empty.
        while let Some(ev) = receiver.poll() {
            match ev {
                Event::Sync { freq_offset, psmi } => {
                    self.status.locked = true;
                    self.status.audio = false;
                    self.status.freq_offset_hz = freq_offset;
                    self.status.psmi = psmi;
                    self.status_dirty = true;
                }
                Event::LostSync => {
                    self.status.locked = false;
                    self.status.audio = false;
                    self.status_dirty = true;
                }
                Event::Mer { lower, upper } => {
                    self.status.mer_lower_db = lower;
                    self.status.mer_upper_db = upper;
                    self.status_dirty = true;
                }
                Event::Ber { cber } => {
                    self.status.cber = cber;
                    self.status_dirty = true;
                }
                Event::Audio { program, data } => {
                    if program == self.selected {
                        self.status.audio = true;
                        self.audio.extend(data);
                        self.status_dirty = true;
                    }
                }
                Event::AudioService { program, access, codec_mode } => {
                    let svc = HdAudioService { program, access, codec_mode };
                    match self.status.audio_services.iter_mut().find(|s| s.program == program) {
                        Some(s) => *s = svc,
                        None => self.status.audio_services.push(svc),
                    }
                    self.status.audio_services.sort_by_key(|s| s.program);
                    self.status_dirty = true;
                }
                Event::StationName(name) => {
                    self.status.station_name = name;
                    self.status_dirty = true;
                }
                Event::StationSlogan(slogan) => {
                    self.status.station_slogan = slogan;
                    self.status_dirty = true;
                }
                Event::StationMessage(message) => {
                    self.status.station_message = message;
                    self.status_dirty = true;
                }
            }
        }

        // A backlog is latency; drop the oldest audio rather than play it out
        // late. It only builds if the decoder catches up in a burst after
        // acquiring. The queue holds interleaved stereo pairs, so it is two
        // values per frame and the caps are in frames.
        while self.audio.len() > MAX_BACKLOG_FRAMES * 2 {
            let drop = (self.audio.len() / 2 - TARGET_BACKLOG_FRAMES) * 2;
            self.audio.drain(..drop);
            self.drops += (drop / 2) as u64;
            debug!(frames = drop / 2, "dropped an HD Radio audio backlog");
        }
    }
}

/// Move `want` audio frames from the interleaved stereo queue into `out` (as
/// mono, `(L + R) / 2`) and `side` (`(L - R) / 2`), padding with silence for
/// frames the queue cannot supply.
///
/// Free rather than a method so the pairing can be tested without a decoder.
fn emit_frames(audio: &mut VecDeque<i16>, side: &mut Vec<f32>, want: usize, out: &mut Vec<f32>) {
    let available = audio.len() / 2;
    let take = want.min(available);
    out.reserve(want);
    side.reserve(want);
    for _ in 0..take {
        let l = audio.pop_front().unwrap_or(0) as f32 / 32_768.0;
        let r = audio.pop_front().unwrap_or(0) as f32 / 32_768.0;
        out.push((l + r) * 0.5);
        side.push((l - r) * 0.5);
    }
    // Whatever the queue could not supply is silence, not a gap: the block
    // still has to be as long as real time says.
    for _ in take..want {
        out.push(0.0);
        side.push(0.0);
    }
}

impl Demodulator for HdDemod {
    fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        self.side.clear();
        if iq.is_empty() {
            return;
        }

        let mut p = 0.0f32;
        for z in iq {
            p += z.re * z.re + z.im * z.im;
        }
        self.power = p / iq.len() as f32;

        self.feed(iq);
        self.drain();

        // How much audio this block is worth in real time. The decoder's output
        // is paced by the broadcast, so this keeps the two clocks together
        // instead of playing back whatever happens to have arrived.
        self.frame_debt += iq.len() as f64 * AUDIO_RATE / self.channel_rate;
        let want = self.frame_debt as usize;
        self.frame_debt -= want as f64;
        if want == 0 {
            return;
        }

        // nrsc5 hands over interleaved stereo pairs, two `i16` per frame, so a
        // frame is taken as a pair: the sum for the mono output the receive
        // chain plays, the difference for the side `take_side` offers the
        // stereo blend. Popping one value per frame (the old behaviour) halved
        // the rate, played L and R alternately, and grew the queue until the
        // backlog trim fired several times a second.
        emit_frames(&mut self.audio, &mut self.side, want, out);
    }

    /// Nothing to do: the HD Radio channel's width is fixed by the FM hybrid,
    /// which the decoder reads for itself. The operator's filter edges still set
    /// what the panadapter draws and what the S-meter measures.
    fn set_filter(&mut self, _lo_hz: f32, _hi_hz: f32) {}

    fn audio_rate(&self) -> f64 {
        AUDIO_RATE
    }

    fn power_dbfs(&self) -> f32 {
        if self.power <= 1e-20 { -200.0 } else { 10.0 * self.power.log10() }
    }

    /// The side channel of the block just decoded, for the stereo blend. HDC
    /// audio is stereo, so this is offered whenever there is anything to offer;
    /// a mono transmission has `L == R` and a zero side, which sums back to the
    /// same mono signal.
    fn take_side(&mut self, out: &mut Vec<f32>) -> bool {
        if self.side.is_empty() {
            return false;
        }
        out.extend_from_slice(&self.side);
        true
    }

    fn stereo_locked(&self) -> bool {
        self.status.audio
    }

    fn reset_hd_radio(&mut self) {
        self.restart();
    }

    fn set_hd_program(&mut self, program: u8) {
        self.select_program(program);
    }

    fn take_hd_radio(&mut self) -> Option<HdRadioStatus> {
        if !std::mem::take(&mut self.status_dirty) {
            return None;
        }
        Some(self.status.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One output sample per stereo *frame*, not per queued value: nrsc5 hands
    /// over two `i16` per frame, and consuming one per frame halved the rate
    /// and played L and R alternately (the on-air bug).
    #[test]
    fn a_frame_is_a_stereo_pair() {
        let mut audio: VecDeque<i16> = VecDeque::new();
        audio.extend([-32768i16, -32768, 16384, -16384]);
        let mut side = Vec::new();
        let mut out = Vec::new();
        emit_frames(&mut audio, &mut side, 2, &mut out);
        assert_eq!(out.len(), 2, "one sample per frame, not per value");
        assert_eq!(side.len(), 2);
        assert!((out[0] + 1.0).abs() < 1e-6, "mono is (L+R)/2: {}", out[0]);
        assert!(side[0].abs() < 1e-6, "equal channels carry no side: {}", side[0]);
        assert!(out[1].abs() < 1e-6);
        assert!((side[1] - 0.5).abs() < 1e-6, "side is (L-R)/2: {}", side[1]);
        assert!(audio.is_empty(), "both frames consumed, four values");
    }

    /// A block is always as long as real time says, whether or not the decoder
    /// has caught up.
    #[test]
    fn a_short_queue_is_padded_with_silence() {
        let mut audio: VecDeque<i16> = VecDeque::new();
        audio.extend([16384i16, 16384]);
        let mut side = Vec::new();
        let mut out = Vec::new();
        emit_frames(&mut audio, &mut side, 3, &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(side.len(), 3);
        assert_eq!(out[1], 0.0);
        assert_eq!(out[2], 0.0);
        assert!(audio.is_empty());
    }
}
