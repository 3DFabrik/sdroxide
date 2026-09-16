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
    /// Decoded mono audio waiting to be played, at [`AUDIO_RATE`].
    audio: VecDeque<i16>,
    /// Raw audio scratch for one drain.
    raw: Vec<i16>,

    /// Mean square of the resampled input, for the level normalisation.
    level: f32,
    /// Post-filter signal power for the S-meter.
    power: f32,
    /// Fractional part of the input-to-audio sample accounting.
    frame_debt: f64,

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
            raw: Vec::new(),
            level: 0.0,
            power: 0.0,
            frame_debt: 0.0,
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
        self.raw.clear();
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
        // acquiring.
        while self.audio.len() > MAX_BACKLOG_FRAMES {
            let drop = self.audio.len() - TARGET_BACKLOG_FRAMES;
            self.audio.drain(..drop);
            debug!(drop, "dropped an HD Radio audio backlog");
        }
    }
}

impl Demodulator for HdDemod {
    fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
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

        let take = want.min(self.audio.len());
        out.reserve(want);
        for _ in 0..take {
            if let Some(s) = self.audio.pop_front() {
                out.push(s as f32 / 32_768.0);
            }
        }
        // Whatever the queue could not supply is silence, not a gap: the block
        // still has to be as long as real time says.
        out.resize(out.len() + (want - take), 0.0);
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
