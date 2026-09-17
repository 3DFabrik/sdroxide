//! The HD Radio decoder on its own thread.
//!
//! nrsc5 opened on a pipe starts no thread of its own: `nrsc5_pipe_samples_cf32`
//! runs acquisition, the OFDM demodulator, Viterbi, Reed-Solomon and the HDC
//! audio decoder inside the call, and fires every callback before returning.
//! Fed from [`crate::HdDemod::process`] that is all of it on the receive chain's
//! thread, at three quarters of a million samples a second — along with the
//! resampling to get there — and every lane that thread serves would wait on it.
//! So the chain only queues its channel I/Q, and this thread does the rest: the
//! same arrangement `sdroxide-drm` has, and for the same reason.
//!
//! The two sides meet in [`Shared`]: channel I/Q goes in over a lock-free ring,
//! and decoded audio and the status snapshot come back under short locks.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use num_complex::Complex32;
use sdroxide_dsp::ComplexResampler;
use sdroxide_types::{HdAudioService, HdRadioStatus};
use tracing::{debug, warn};

use crate::demod::FM_RATE_HZ;
use crate::{Event, HdReceiver, Mode, NrsError};

/// Target RMS of the samples handed to the decoder.
///
/// The C library works on 32-bit floats and its OFDM demodulator is level
/// agnostic, but the HDC decoder downstream expects a sane amplitude, and a
/// level that swings with fading costs decodes. Slow to follow, like DRM's.
const TARGET_RMS: f32 = 0.06;

/// One-pole coefficient for the level estimate, ~0.5 s at the channel rate.
const LEVEL_ALPHA: f32 = 1.0 / (0.5 * 96_000.0);

/// How much channel I/Q the queue into this thread holds, in seconds. A
/// decoder that falls further behind than this loses the oldest samples rather
/// than stalling the receive chain — and re-acquires, which is what it would
/// have to do after a gap that long anyway.
const INPUT_SECONDS: f64 = 0.5;

/// The most channel samples taken off the queue per pass, so a restart or a
/// stop is noticed within a block rather than after a backlog.
const CHUNK: usize = 32_768;

/// How long the thread sleeps when the queue is empty. Channel I/Q arrives in
/// blocks a few milliseconds apart; this only has to keep an idle decoder from
/// spinning.
const IDLE: Duration = Duration::from_millis(2);

/// What the receive chain and the decoder thread share.
pub(crate) struct Shared {
    /// Decoded audio for the selected programme, and which programme that is.
    /// One lock for both, so a change of programme and the audio it clears
    /// cannot interleave with a block of the old programme arriving.
    pub audio: Mutex<AudioQueue>,
    pub status: Mutex<HdRadioStatus>,
    /// Set whenever `status` has moved and not yet been taken.
    pub status_dirty: AtomicBool,
    restart: AtomicBool,
    stop: AtomicBool,
}

/// Decoded audio waiting to be played.
pub(crate) struct AudioQueue {
    /// Which programme is being listened to, 0-based.
    pub selected: u8,
    /// At 44.1 kHz, as the interleaved stereo pairs nrsc5 produces — two `i16`
    /// per frame.
    pub frames: VecDeque<i16>,
}

impl Shared {
    fn new() -> Self {
        Shared {
            audio: Mutex::new(AudioQueue { selected: 0, frames: VecDeque::new() }),
            status: Mutex::new(HdRadioStatus::default()),
            status_dirty: AtomicBool::new(true),
            restart: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        }
    }

    pub fn audio(&self) -> MutexGuard<'_, AudioQueue> {
        self.audio.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn status(&self) -> MutexGuard<'_, HdRadioStatus> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The decoder thread and the queue into it.
pub(crate) struct HdWorker {
    input: rtrb::Producer<Complex32>,
    pub shared: Arc<Shared>,
    /// Channel samples dropped because the queue was full.
    input_drops: AtomicU64,
    thread: Option<JoinHandle<()>>,
}

impl HdWorker {
    /// Start a decoder for a chain whose channel rate is `channel_rate`. The
    /// receiver is opened on the thread that drives it, and a failed open comes
    /// back here rather than leaving a thread with nothing to do.
    pub fn new(channel_rate: f64) -> Result<Self, NrsError> {
        let capacity = ((channel_rate * INPUT_SECONDS) as usize).max(CHUNK);
        let (input, queue) = rtrb::RingBuffer::new(capacity);
        let shared = Arc::new(Shared::new());
        let (tx, rx) = sync_channel::<Result<(), NrsError>>(1);
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("hd-radio".into())
                .spawn(move || {
                    let receiver = match HdReceiver::open(Mode::Fm) {
                        Ok(r) => {
                            let _ = tx.send(Ok(()));
                            r
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    };
                    run(receiver, queue, &shared, channel_rate);
                })
                .expect("spawn the HD Radio decoder thread")
        };
        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(NrsError::Open);
            }
        }
        Ok(HdWorker { input, shared, input_drops: AtomicU64::new(0), thread: Some(thread) })
    }

    /// Queue channel I/Q for the decoder. Never blocks: what does not fit is
    /// dropped and counted.
    pub fn push(&mut self, iq: &[Complex32]) {
        let n = iq.len().min(self.input.slots());
        if n > 0
            && let Ok(chunk) = self.input.write_chunk_uninit(n)
        {
            chunk.fill_from_iter(iq.iter().copied());
        }
        let dropped = iq.len() - n;
        if dropped > 0 {
            let before = self.input_drops.fetch_add(dropped as u64, Ordering::Relaxed);
            if before == 0 {
                warn!(dropped, "the HD Radio decoder is not keeping up with the channel");
            }
        }
    }

    /// Ask for re-acquisition, after a retune.
    pub fn restart(&self) {
        self.shared.restart.store(true, Ordering::Relaxed);
    }
}

impl Drop for HdWorker {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The thread's loop: take channel I/Q off the queue, resample it to the
/// decoder's rate, normalise it, pipe it in and publish what came out.
fn run(
    receiver: HdReceiver,
    mut queue: rtrb::Consumer<Complex32>,
    shared: &Shared,
    channel_rate: f64,
) {
    let mut receiver = Some(receiver);
    let mut feed = Feed::new(channel_rate);
    let mut block: Vec<Complex32> = Vec::with_capacity(CHUNK);
    while !shared.stop.load(Ordering::Relaxed) {
        if shared.restart.swap(false, Ordering::Relaxed) {
            // The station the queue holds is the one being tuned away from.
            let pending = queue.slots();
            if let Ok(chunk) = queue.read_chunk(pending) {
                chunk.commit_all();
            }
            receiver = None;
            feed = Feed::new(channel_rate);
            // The programme was put back to HD-1 by whoever asked for the
            // restart, and may have been changed again since; keep what it is.
            let program = {
                let mut audio = shared.audio();
                audio.frames.clear();
                audio.selected
            };
            *shared.status() = HdRadioStatus { program, ..HdRadioStatus::default() };
            shared.status_dirty.store(true, Ordering::Relaxed);
            match HdReceiver::open(Mode::Fm) {
                Ok(r) => receiver = Some(r),
                Err(e) => {
                    warn!(?e, "could not restart the HD Radio decoder; the mode will be silent")
                }
            }
        }

        let n = queue.slots().min(CHUNK);
        if n == 0 {
            std::thread::sleep(IDLE);
            continue;
        }
        block.clear();
        if let Ok(chunk) = queue.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            block.extend_from_slice(a);
            block.extend_from_slice(b);
            chunk.commit_all();
        }
        let Some(rx) = receiver.as_ref() else { continue };
        feed.pipe(&block, rx);
        drain(rx, shared);
    }
}

/// Resampling and level normalisation ahead of the decoder.
struct Feed {
    resampler: Option<ComplexResampler>,
    /// Channel IQ resampled to the decoder's rate.
    rs_buf: Vec<Complex32>,
    /// ...and the same, as the interleaved floats the decoder reads.
    iq_buf: Vec<f32>,
    /// Mean square of the resampled input, for the level normalisation.
    level: f32,
}

impl Feed {
    fn new(channel_rate: f64) -> Self {
        Feed {
            resampler: ComplexResampler::new(channel_rate, FM_RATE_HZ),
            rs_buf: Vec::new(),
            iq_buf: Vec::new(),
            level: 0.0,
        }
    }

    /// Resample the block to the decoder's rate, normalise it and pipe it in.
    fn pipe(&mut self, iq: &[Complex32], receiver: &HdReceiver) {
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
}

/// Publish whatever the decoder queued while the last block was piped in.
fn drain(receiver: &HdReceiver, shared: &Shared) {
    let mut moved = false;
    // `poll` is non-blocking, so this ends when the queue is empty.
    while let Some(ev) = receiver.poll() {
        match ev {
            Event::Audio { program, data } => {
                let mut audio = shared.audio();
                if program == audio.selected {
                    audio.frames.extend(data);
                    drop(audio);
                    let mut status = shared.status();
                    if !status.audio {
                        status.audio = true;
                        moved = true;
                    }
                }
            }
            other => {
                apply(&mut shared.status(), other);
                moved = true;
            }
        }
    }
    if moved {
        shared.status_dirty.store(true, Ordering::Relaxed);
    }
}

/// Fold one non-audio event into the status snapshot.
fn apply(status: &mut HdRadioStatus, ev: Event) {
    match ev {
        Event::Sync { freq_offset, psmi } => {
            status.locked = true;
            status.audio = false;
            status.freq_offset_hz = freq_offset;
            status.psmi = psmi;
        }
        Event::LostSync => {
            status.locked = false;
            status.audio = false;
        }
        Event::Mer { lower, upper } => {
            status.mer_lower_db = lower;
            status.mer_upper_db = upper;
        }
        Event::Ber { cber } => status.cber = cber,
        Event::AudioService { program, access, codec_mode } => {
            let svc = HdAudioService { program, access, codec_mode };
            match status.audio_services.iter_mut().find(|s| s.program == program) {
                Some(s) => *s = svc,
                None => status.audio_services.push(svc),
            }
            status.audio_services.sort_by_key(|s| s.program);
        }
        Event::StationName(name) => status.station_name = name,
        Event::StationSlogan(slogan) => status.station_slogan = slogan,
        Event::StationMessage(message) => status.station_message = message,
        Event::Audio { .. } => debug!("audio reached the status fold"),
    }
}
