//! NOAA APT weather-satellite decoder.
//!
//! After the FM discriminator the picture is a 2400 Hz AM subcarrier. Two
//! video channels (A visible, B infrared) are interleaved at two lines per
//! second. A line begins with seven cycles of 1040 Hz (channel A) or 832 Hz
//! (channel B). This receiver envelope-detects the subcarrier, locks those
//! sync bursts, and emits one grayscale line at a time.

use std::f64::consts::TAU;

/// Peak FM deviation of the 137 MHz downlink, Hz. NOAA KLM §4.2.
pub const DEVIATION_HZ: f64 = 17_000.0;
/// Video subcarrier, Hz.
pub const CARRIER_HZ: f64 = 2400.0;
/// Channel-A sync, Hz — seven cycles mark the start of a visible line.
pub const SYNC_A_HZ: f64 = 1040.0;
/// Channel-B sync, Hz — seven cycles mark the start of an IR line.
pub const SYNC_B_HZ: f64 = 832.0;
/// Whole multiplex line, seconds (two video channels).
pub const LINE_S: f64 = 0.5;
/// Pixels in one multiplex line. NOAA KLM §4.2.
pub const LINE_PIXELS: usize = 2080;
/// Image pixels kept from each half of the line (sync/telemetry stripped).
pub const CHANNEL_PIXELS: usize = 909;
/// Offset of channel A's image inside the 2080-pixel line.
const CHAN_A_START: usize = 86;
/// Offset of channel B's image.
const CHAN_B_START: usize = 1126;

/// One decoded event.
#[derive(Debug, Clone, PartialEq)]
pub enum AptEvent {
    /// A new pass has started (first sync after silence).
    Start,
    /// One video line of one channel.
    Line {
        channel: AptChannel,
        y: u16,
        gray: Vec<u8>,
    },
    /// The decoder has been idle long enough that the picture is finished.
    Complete,
}

/// Which of the two interleaved pictures a line belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AptChannel {
    A,
    B,
}

/// Streaming APT receiver. Fed demodulated FM audio.
pub struct AptRx {
    _rate: f64,
    /// One-pole envelope after a 2400 Hz mix-down.
    env_re: f64,
    env_im: f64,
    env_alpha: f64,
    mix_phase: f64,
    mix_dphi: f64,
    /// Envelope history for sync correlation, newest last.
    env: Vec<f32>,
    env_len: usize,
    sync_a: Vec<f32>,
    sync_b: Vec<f32>,
    /// Samples remaining to collect for the current line, or `None` hunting.
    remain: Option<usize>,
    line: Vec<f32>,
    line_need: usize,
    y_a: u16,
    y_b: u16,
    last_sync: Option<AptChannel>,
    idle: usize,
    idle_limit: usize,
    started: bool,
    level: f32,
}

impl AptRx {
    pub fn new(rate: f64) -> Self {
        let rate = rate.max(8_000.0);
        let env_len = ((rate * 0.020).round() as usize).max(16);
        let line_need = (rate * LINE_S).round() as usize;
        let idle_limit = line_need * 8;
        let env_alpha = 1.0 - (-TAU * 800.0 / rate).exp();
        AptRx {
            _rate: rate,
            env_re: 0.0,
            env_im: 0.0,
            env_alpha,
            mix_phase: 0.0,
            mix_dphi: TAU * CARRIER_HZ / rate,
            env: Vec::with_capacity(env_len + 8),
            env_len,
            sync_a: sync_template(rate, SYNC_A_HZ, env_len),
            sync_b: sync_template(rate, SYNC_B_HZ, env_len),
            remain: None,
            line: Vec::with_capacity(line_need),
            line_need,
            y_a: 0,
            y_b: 0,
            last_sync: None,
            idle: 0,
            idle_limit,
            started: false,
            level: 0.0,
        }
    }

    pub fn level(&self) -> f32 {
        self.level
    }

    pub fn receiving(&self) -> bool {
        self.started
    }

    pub fn lines_a(&self) -> u16 {
        self.y_a
    }

    pub fn lines_b(&self) -> u16 {
        self.y_b
    }

    /// Reset as if the operator pressed STOP / a new pass is about to start.
    pub fn reset(&mut self) {
        self.remain = None;
        self.line.clear();
        self.y_a = 0;
        self.y_b = 0;
        self.last_sync = None;
        self.idle = 0;
        self.started = false;
        self.env.clear();
    }

    /// Begin a picture now, without waiting for the first sync — useful when
    /// the operator tuned in mid-pass.
    pub fn force_start(&mut self) {
        self.started = true;
        self.idle = 0;
    }

    pub fn process(&mut self, audio: &[f32], out: &mut Vec<AptEvent>) {
        for &s in audio {
            let env = self.envelope(s);
            self.level = self.level * 0.995 + env.abs() * 0.005;
            if let Some(left) = self.remain {
                self.line.push(env);
                if left <= 1 {
                    self.emit_line(out);
                    self.remain = None;
                } else {
                    self.remain = Some(left - 1);
                }
                continue;
            }
            self.env.push(env);
            if self.env.len() > self.env_len {
                self.env.remove(0);
            }
            if self.env.len() < self.env_len {
                continue;
            }
            let (which, score) = self.best_sync();
            // A real sync is several times the mean envelope energy.
            let thresh = (self.level * 4.0).max(0.02);
            if score > thresh {
                if !self.started {
                    self.started = true;
                    out.push(AptEvent::Start);
                }
                self.last_sync = Some(which);
                self.idle = 0;
                self.line.clear();
                self.line.extend_from_slice(&self.env);
                self.remain = Some(self.line_need.saturating_sub(self.line.len()));
                self.env.clear();
            } else if self.started {
                self.idle += 1;
                if self.idle > self.idle_limit {
                    out.push(AptEvent::Complete);
                    self.reset();
                }
            }
        }
    }

    fn envelope(&mut self, s: f32) -> f32 {
        let (sn, cs) = self.mix_phase.sin_cos();
        self.mix_phase += self.mix_dphi;
        if self.mix_phase > TAU {
            self.mix_phase -= TAU;
        }
        let x = s as f64;
        self.env_re += self.env_alpha * (x * cs - self.env_re);
        self.env_im += self.env_alpha * (x * sn - self.env_im);
        ((self.env_re * self.env_re + self.env_im * self.env_im).sqrt()) as f32
    }

    fn best_sync(&self) -> (AptChannel, f32) {
        let a = correlate(&self.env, &self.sync_a);
        let b = correlate(&self.env, &self.sync_b);
        if a >= b { (AptChannel::A, a) } else { (AptChannel::B, b) }
    }

    fn emit_line(&mut self, out: &mut Vec<AptEvent>) {
        let Some(ch) = self.last_sync else { return };
        let pixels = resample_line(&self.line, LINE_PIXELS);
        let (start, y) = match ch {
            AptChannel::A => {
                let y = self.y_a;
                self.y_a = self.y_a.saturating_add(1);
                (CHAN_A_START, y)
            }
            AptChannel::B => {
                let y = self.y_b;
                self.y_b = self.y_b.saturating_add(1);
                (CHAN_B_START, y)
            }
        };
        let end = (start + CHANNEL_PIXELS).min(pixels.len());
        if end <= start {
            return;
        }
        let slice = &pixels[start..end];
        let (lo, hi) = min_max(slice);
        let span = (hi - lo).max(1e-6);
        let mut gray = Vec::with_capacity(CHANNEL_PIXELS);
        for &p in slice {
            let v = ((p - lo) / span * 255.0).round().clamp(0.0, 255.0);
            gray.push(v as u8);
        }
        while gray.len() < CHANNEL_PIXELS {
            gray.push(*gray.last().unwrap_or(&0));
        }
        out.push(AptEvent::Line { channel: ch, y, gray });
        self.line.clear();
    }
}

fn sync_template(rate: f64, hz: f64, len: usize) -> Vec<f32> {
    // Seven cycles of a raised square, matching the APT sync pulse train.
    let mut v = vec![0.0f32; len];
    let cycles = 7.0;
    let period = rate / hz;
    let used = (period * cycles).min(len as f64);
    for (i, s) in v.iter_mut().enumerate() {
        if (i as f64) >= used {
            break;
        }
        let phase = (i as f64) / period;
        *s = if phase.fract() < 0.5 { 1.0 } else { -0.2 };
    }
    v
}

fn correlate(hay: &[f32], needle: &[f32]) -> f32 {
    if hay.len() < needle.len() || needle.is_empty() {
        return 0.0;
    }
    let start = hay.len() - needle.len();
    let mut acc = 0.0f32;
    let mut n_energy = 0.0f32;
    for (i, &n) in needle.iter().enumerate() {
        acc += hay[start + i] * n;
        n_energy += n * n;
    }
    if n_energy < 1e-9 { 0.0 } else { acc / n_energy.sqrt() }
}

fn resample_line(samples: &[f32], pixels: usize) -> Vec<f32> {
    if samples.is_empty() || pixels == 0 {
        return vec![0.0; pixels];
    }
    let mut out = vec![0.0f32; pixels];
    let last = (samples.len() - 1) as f64;
    for (i, p) in out.iter_mut().enumerate() {
        let x = i as f64 / (pixels - 1).max(1) as f64 * last;
        let lo = x.floor() as usize;
        let hi = (lo + 1).min(samples.len() - 1);
        let t = (x - lo as f64) as f32;
        *p = samples[lo] * (1.0 - t) + samples[hi] * t;
    }
    out
}

fn min_max(v: &[f32]) -> (f32, f32) {
    let mut lo = f32::MAX;
    let mut hi = f32::MIN;
    for &x in v {
        lo = lo.min(x);
        hi = hi.max(x);
    }
    if !lo.is_finite() { (0.0, 1.0) } else { (lo, hi) }
}

/// Build one multiplex line of FM-demodulated APT audio for tests.
pub fn synthesize_line(rate: f64, channel: AptChannel, brightness: f32) -> Vec<f32> {
    let n = (rate * LINE_S).round() as usize;
    let sync_hz = match channel {
        AptChannel::A => SYNC_A_HZ,
        AptChannel::B => SYNC_B_HZ,
    };
    let mut out = vec![0.0f32; n];
    let sync_s = 7.0 / sync_hz;
    for (i, s) in out.iter_mut().enumerate() {
        let t = i as f64 / rate;
        let video = if t < sync_s {
            if (t * sync_hz).fract() < 0.5 { 0.9 } else { 0.1 }
        } else {
            brightness.clamp(0.1, 0.9) as f64
        };
        *s = ((TAU * CARRIER_HZ * t).sin() * video) as f32;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_synthetic_a_line_decodes_as_channel_a() {
        let rate = 12_000.0;
        let mut rx = AptRx::new(rate);
        let mut events = Vec::new();
        // Two lines so the correlator sees a clean burst after warmup.
        let mut audio = synthesize_line(rate, AptChannel::A, 0.7);
        audio.extend(synthesize_line(rate, AptChannel::A, 0.7));
        rx.process(&audio, &mut events);
        assert!(events.iter().any(|e| matches!(e, AptEvent::Start)), "{events:?}");
        let lines: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AptEvent::Line { channel, gray, .. } => Some((*channel, gray.len())),
                _ => None,
            })
            .collect();
        assert!(
            lines.iter().any(|(c, n)| *c == AptChannel::A && *n == CHANNEL_PIXELS),
            "{lines:?}"
        );
    }
}
