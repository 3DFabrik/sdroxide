//! HD Radio (NRSC-5) decoding, wrapping the vendored `nrsc5` library.
//!
//! The crate is the decoder for the HD Radio band: it takes raw I/Q samples
//! (via `HdReceiver::pipe_*`, at nrsc5's native sample rate — see the
//! `NRSC5_SAMPLE_RATE_*` definitions upstream) and turns them into decoded
//! audio and the SIS/ID3 metadata roadcasters send.
//!
//! It is a thin binding on purpose. The receive path is a worker thread owned
//! by the C library; the caller pipes samples in from its own thread and
//! collects `Event`s on this side through a plain channel. `HdReceiver` is
//! therefore `Send` (move it to the sampler thread) but not `Sync`: the pipe
//! functions and the close must not run concurrently.
//!
//! Audio reaches the caller as `Event::Audio` in signed 16-bit mono PCM at
//! 44.1 kHz, exactly as nrsc5 emits it. Metadata arrives as the station / SIS
//! events; the finer data services (LOT files, HERE images, ID3 tags, the SIG
//! table) are decoded inside the library but not yet surfaced here.
//!
//! # Linking faad2
//!
//! nrsc5 decodes its HDC audio with faad2's `NeAACDec*` symbols. They come
//! from `sdroxide-faad2`, the one faad2 archive in the binary — upstream faad2
//! with nrsc5's HDC patch applied at build time, shared with the DRM receiver.
//! Two faad2 copies in the same binary must never happen: they collide on
//! every `NeAACDec*` symbol.

#![deny(missing_docs)]

pub mod demod;
mod worker;
pub use demod::HdDemod;

// Named so its faad2 archive is linked: nrsc5 calls `NeAACDec*` from C, which
// Rust cannot see.
use sdroxide_faad2 as _;

use std::ffi::{c_char, c_float, c_int, c_uint, c_void};
use std::sync::mpsc::{self, Receiver};

/// The analogue source the decoder expects to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Frequency-modulated hybrid HD Radio, native pipes at 744,187.5 S/s.
    Fm = 0,
    /// Amplitude-modulated HBO (HD on AM), native pipes at 46,511.71875 S/s.
    Am = 1,
}

/// A decoded event from the HD Radio receive path.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Lock has been acquired; `freq_offset` is the residual offset in Hz and
    /// `psmi` the Primary Service Mode Indicator.
    Sync {
        /// Residual carrier frequency offset, in Hz.
        freq_offset: f32,
        /// Primary Service Mode Indicator (1, 2, 3, 5, 6 or 11 in FM).
        psmi: i32,
    },
    /// The previously acquired lock was lost.
    LostSync,
    /// Modulation error ratio of the lower and upper sidebands, in dB.
    Mer {
        /// Lower sideband, in dB.
        lower: f32,
        /// Upper sideband, in dB.
        upper: f32,
    },
    /// Channel bit-error ratio.
    Ber {
        /// Channel bit-error ratio, 0.0 to 1.0.
        cber: f32,
    },
    /// Decoded PCM audio: signed 16-bit interleaved stereo, 44.1 kHz.
    Audio {
        /// The program the audio belongs to.
        program: u8,
        /// Signed 16-bit PCM, left and right interleaved, 44,100 frames per
        /// second.
        data: Vec<i16>,
        /// `NRSC5_AUDIO_FLAGS_*`. With [`AUDIO_FLAG_UNAVAILABLE`] set the frame
        /// is silence the decoder filled in for a packet that was missing or
        /// failed its check, not sound from the station.
        flags: u32,
    },
    /// An audio service is available on the wave (from SIS descriptors).
    AudioService {
        /// Program number, 0 to 7.
        program: u8,
        /// `NRSC5_ACCESS_PUBLIC` (0) or `NRSC5_ACCESS_RESTRICTED` (1).
        access: u8,
        /// Audio codec mode, per SY_IDD_1017s Table 5-2.
        codec_mode: u8,
    },
    /// The roadcaster's station name, e.g. "Q107".
    StationName(String),
    /// The station slogan, e.g. "You're Listening to Q".
    StationSlogan(String),
    /// A short text the roadcaster is currently airing.
    StationMessage(String),
}

/// `NRSC5_AUDIO_FLAGS_UNAVAILABLE`: an [`Event::Audio`] frame that is silence
/// standing in for audio the decoder did not have.
pub const AUDIO_FLAG_UNAVAILABLE: u32 = 1 << 0;

/// Errors opening or driving an `HdReceiver`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NrsError {
    /// The library rejected the open or the mode switch.
    Open,
    /// A pipe call rejected its arguments (length not matching the format).
    Pipe,
}

/// A handle to an HD Radio receive session.
///
/// Opening starts the library's worker thread; audio and metadata callbacks
/// are marshalled into a channel and drained with `poll`/`wait`. Dropping the
/// receiver detaches the callback before the worker is joined, so no callback
/// can outlive it.
pub struct HdReceiver {
    st: *mut NrsCtx,
    rx: Receiver<Event>,
    _sink: Box<CbSink>,
}

// The C handle is only touched from the (single) calling thread after the
// worker has been joined, and the channel end is `Send`.
unsafe impl Send for HdReceiver {}

impl HdReceiver {
    /// Opens an HD Radio session in the given mode and starts its worker.
    pub fn open(mode: Mode) -> Result<Self, NrsError> {
        let mut st: *mut NrsCtx = std::ptr::null_mut();
        if unsafe { nrsc5_open_pipe(&mut st) } != 0 || st.is_null() {
            return Err(NrsError::Open);
        }
        if unsafe { nrsc5_set_mode(st, mode as c_int) } != 0 {
            unsafe { nrsc5_close(st) };
            return Err(NrsError::Open);
        }
        let (tx, rx) = mpsc::channel();
        let sink = Box::new(CbSink { tx });
        let opaque = &*sink as *const CbSink as *mut c_void;
        unsafe { nrsc5_set_callback(st, Some(trampoline), opaque) };
        unsafe { nrsc5_start(st) };
        Ok(HdReceiver { st, rx, _sink: sink })
    }

    /// Pipes raw 8-bit unsigned I/Q samples (2 bytes per complex sample).
    ///
    /// `samples.len()` is in bytes; a trailing odd pair is buffered by the
    /// library across calls, so the total count matters more than the chunk
    /// boundaries. Feed at `NRSC5_SAMPLE_RATE_CU8` (1,488,375 S/s).
    pub fn pipe_cu8(&self, samples: &[u8]) -> Result<(), NrsError> {
        let len = c_uint::try_from(samples.len()).map_err(|_| NrsError::Pipe)?;
        if unsafe { nrsc5_pipe_samples_cu8(self.st, samples.as_ptr(), len) } != 0 {
            return Err(NrsError::Pipe);
        }
        Ok(())
    }

    /// Pipes raw signed 16-bit I/Q samples (4 bytes per complex sample).
    ///
    /// `samples.len()` is in bytes. Feed at `NRSC5_SAMPLE_RATE_CU8`.
    pub fn pipe_cs16(&self, samples: &[i16]) -> Result<(), NrsError> {
        let count = samples.len() * 2;
        let len = c_uint::try_from(count).map_err(|_| NrsError::Pipe)?;
        if unsafe { nrsc5_pipe_samples_cs16(self.st, samples.as_ptr(), len) } != 0 {
            return Err(NrsError::Pipe);
        }
        Ok(())
    }

    /// Pipes single-precision complex I/Q samples (2 floats per sample).
    ///
    /// `samples.len()` is in floats and must be even. Feed at the native FM
    /// (744,187.5 S/s) or AM (46,511.71875 S/s) rate.
    pub fn pipe_cf32(&self, samples: &[c_float]) -> Result<(), NrsError> {
        let len = c_uint::try_from(samples.len()).map_err(|_| NrsError::Pipe)?;
        if unsafe { nrsc5_pipe_samples_cf32(self.st, samples.as_ptr(), len) } != 0 {
            return Err(NrsError::Pipe);
        }
        Ok(())
    }

    /// Returns the next queued event without waiting.
    pub fn poll(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    /// Blocks until the next event arrives (or the receiver is closed).
    pub fn wait(&self) -> Option<Event> {
        self.rx.recv().ok()
    }

    /// A non-blocking iterator over whatever is queued right now.
    pub fn drain(&self) -> Drain<'_> {
        Drain { rx: &self.rx }
    }
}

impl Drop for HdReceiver {
    fn drop(&mut self) {
        // Detach the callback first: the sink boxed on the receive side must
        // not be reachable once the worker is joined below.
        unsafe { nrsc5_set_callback(self.st, None, std::ptr::null_mut()) };
        unsafe { nrsc5_stop(self.st) };
        unsafe { nrsc5_close(self.st) };
    }
}

/// Iterator over the events queued at the moment `drain` was called.
pub struct Drain<'a> {
    rx: &'a Receiver<Event>,
}

impl Iterator for Drain<'_> {
    type Item = Event;
    fn next(&mut self) -> Option<Event> {
        self.rx.try_recv().ok()
    }
}

// Safety contract: `opaque` is the `Box<CbSink>` registered on `open`, and it
// outlives the worker because `drop` detaches the callback before joining.
struct CbSink {
    tx: mpsc::Sender<Event>,
}

unsafe extern "C" fn trampoline(evt: *const NrsEvent, opaque: *mut c_void) {
    let sink = unsafe { &*(opaque as *const CbSink) };
    if let Some(ev) = unsafe { translate(evt) } {
        let _ = sink.tx.send(ev);
    }
}

// NRSC5_EVENT_* values, in the order nrsc5.h declares them: LOST_DEVICE,
// IQ, SYNC, LOST_SYNC, MER, BER, HDC, AUDIO, ID3, SIG, LOT, SIS, STREAM,
// PACKET, AUDIO_SERVICE, STATION_ID, STATION_NAME, STATION_SLOGAN,
// STATION_MESSAGE, STATION_LOCATION, ...
const NRS_EVENT_SYNC: c_uint = 2;
const NRS_EVENT_LOST_SYNC: c_uint = 3;
const NRS_EVENT_MER: c_uint = 4;
const NRS_EVENT_BER: c_uint = 5;
const NRS_EVENT_AUDIO: c_uint = 7;
const NRS_EVENT_AUDIO_SERVICE: c_uint = 14;
const NRS_EVENT_STATION_NAME: c_uint = 16;
const NRS_EVENT_STATION_SLOGAN: c_uint = 17;
const NRS_EVENT_STATION_MESSAGE: c_uint = 18;

/// Translates the C event into a Rust one, copying every borrow out.
unsafe fn translate(evt: *const NrsEvent) -> Option<Event> {
    if evt.is_null() {
        return None;
    }
    let e = unsafe { &*evt };
    match e.event {
        NRS_EVENT_SYNC => Some(Event::Sync {
            freq_offset: unsafe { e.u.sync.freq_offset },
            psmi: unsafe { e.u.sync.psmi },
        }),
        NRS_EVENT_LOST_SYNC => Some(Event::LostSync),
        NRS_EVENT_MER => Some(Event::Mer {
            lower: unsafe { e.u.mer.lower },
            upper: unsafe { e.u.mer.upper },
        }),
        NRS_EVENT_BER => Some(Event::Ber {
            cber: unsafe { e.u.ber.cber },
        }),
        NRS_EVENT_AUDIO => {
            let a = unsafe { e.u.audio };
            if a.data.is_null() || a.count == 0 {
                return None;
            }
            let data = unsafe { std::slice::from_raw_parts(a.data, a.count) }.to_vec();
            Some(Event::Audio { program: a.program as u8, data, flags: a.flags })
        }
        NRS_EVENT_AUDIO_SERVICE => {
            let a = unsafe { e.u.audio_service };
            Some(Event::AudioService {
                program: a.program as u8,
                access: a.access as u8,
                codec_mode: a.codec_mode as u8,
            })
        }
        NRS_EVENT_STATION_NAME => unsafe { cstr(e.u.station_name.name) }.map(Event::StationName),
        NRS_EVENT_STATION_SLOGAN => {
            unsafe { cstr(e.u.station_slogan.name) }.map(Event::StationSlogan)
        }
        NRS_EVENT_STATION_MESSAGE => {
            unsafe { cstr(e.u.station_message.name) }.map(Event::StationMessage)
        }
        _ => None,
    }
}

/// Copies a NUL-terminated C string.
unsafe fn cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let c = unsafe { std::ffi::CStr::from_ptr(p) };
    Some(c.to_string_lossy().into_owned())
}

/// Opaque receiver handle, from the C library's point of view.
#[repr(C)]
struct NrsCtx {
    _private: [u8; 0],
}

#[repr(C)]
struct NrsEvent {
    event: c_uint,
    u: NrsUnion,
}

// A union of the members this crate reads. Union members not declared here
// (id3, sig, lot, ...) are simply never read; all union arms share the offset
// of the enclosing struct, so the missing ones cost nothing.
#[repr(C)]
union NrsUnion {
    sync: NrsSync,
    ber: NrsBer,
    mer: NrsMer,
    #[allow(dead_code)]
    hdc: NrsHdc,
    audio: NrsAudio,
    audio_service: NrsAudioService,
    station_name: NrsName,
    station_slogan: NrsName,
    station_message: NrsName,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NrsSync {
    freq_offset: c_float,
    psmi: c_int,
    pli: c_int,
    hppi: c_int,
    aabi: c_int,
    rdbi: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NrsBer {
    cber: c_float,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NrsMer {
    lower: c_float,
    upper: c_float,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct NrsHdc {
    program: c_uint,
    data: *const u8,
    count: usize,
    flags: c_uint,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NrsAudio {
    program: c_uint,
    data: *const i16,
    count: usize,
    flags: c_uint,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct NrsAudioService {
    program: c_uint,
    access: c_uint,
    type_: c_uint,
    codec_mode: c_uint,
    blend_control: c_uint,
    digital_audio_gain: c_int,
    common_delay: c_uint,
    latency: c_uint,
}

/// Mirrors `struct { const char *name; }`, `{ ... *slogan; }` and
/// `{ ... *message; }` — one pointer at offset 0 in each case.
#[repr(C)]
#[derive(Clone, Copy)]
struct NrsName {
    name: *const c_char,
}

#[allow(clippy::type_complexity)]
type NrsCallback =
    unsafe extern "C" fn(evt: *const NrsEvent, opaque: *mut c_void);

unsafe extern "C" {
    fn nrsc5_open_pipe(st: *mut *mut NrsCtx) -> c_int;
    fn nrsc5_close(st: *mut NrsCtx);
    fn nrsc5_start(st: *mut NrsCtx);
    fn nrsc5_stop(st: *mut NrsCtx);
    fn nrsc5_set_mode(st: *mut NrsCtx, mode: c_int) -> c_int;
    fn nrsc5_set_callback(st: *mut NrsCtx, callback: Option<NrsCallback>, opaque: *mut c_void);
    fn nrsc5_pipe_samples_cu8(st: *mut NrsCtx, samples: *const u8, length: c_uint) -> c_int;
    fn nrsc5_pipe_samples_cs16(st: *mut NrsCtx, samples: *const i16, length: c_uint) -> c_int;
    fn nrsc5_pipe_samples_cf32(st: *mut NrsCtx, samples: *const f32, length: c_uint) -> c_int;
}