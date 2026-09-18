//! ACARS — the VHF aircraft datalink, around 130 MHz (issue #436).
//!
//! An AM carrier in the airband carries a 2400-baud MSK signal centred on
//! 1800 Hz (tones 1200/2400 Hz), NRZI-coded, character-oriented: 7-bit ASCII,
//! least-significant bit first, plus odd parity. A frame is a pre-key of ones,
//! the bit- and character-sync pattern, `SOH`, a header (mode, address,
//! acknowledgement, label, block id), `STX`, up to 220 characters of text,
//! `ETX`/`ETB`, a 16-bit block check sequence and `DEL`.
//!
//! This file is in two layers so the testable part is the one that matters:
//! [`parse_frame`] turns an NRZI-decoded bit stream into a message and needs no
//! radio at all, and [`AcarsRx`] turns the demodulated audio into that bit
//! stream. **The demodulator has no timing recovery yet** — it samples on a
//! fixed grid from the start of each block — so it is verified against a
//! synthetic signal and **not yet against a real one**; feeding it real audio
//! is the next step ([`crates/sdroxide-dsp`] has no capture to test with here).

use crate::fir::RealFir;

/// ACARS' audio centre.
pub const CENTER_HZ: f64 = 1800.0;
/// Baud rate.
pub const BAUD: f64 = 2400.0;

/// One decoded ACARS message.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AcarsFrame {
    /// The mode character, as text.
    pub mode: String,
    /// The 7-character aircraft address, trimmed.
    pub address: String,
    /// The technical acknowledgement character.
    pub ack: String,
    /// The 2-character label.
    pub label: String,
    /// The block identifier.
    pub block_id: String,
    /// The message text.
    pub text: String,
    /// Whether the block-check sequence matched.
    pub crc_ok: bool,
}

/// What the decoder produces.
#[derive(Debug, Clone, PartialEq)]
pub enum AcarsEvent {
    Message(AcarsFrame),
}

/// Odd parity over the low seven bits, compared with bit 7.
fn parity_ok(b: u8) -> bool {
    (b.count_ones() & 1) == 1
}

/// CCITT CRC-16 (polynomial 0x1021, initial 0xFFFF) over `data`.
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// Assemble bytes from a bit slice, least-significant bit first, eight bits each.
fn bytes_lsb_first(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .filter(|c| c.len() == 8)
        .map(|c| c.iter().enumerate().fold(0u8, |b, (i, &v)| b | ((v & 1) << i)))
        .collect()
}

/// Parse an NRZI-decoded bit stream into a message, hunting for the start of
/// the frame.
///
/// Returns `None` when no frame with a valid block check is found — the parse
/// is what rejects noise, so a false positive costs a check rather than a
/// phantom message.
pub fn parse_frame(bits: &[u8]) -> Option<AcarsFrame> {
    // Byte-align every shift of the stream and keep the headings; the real one
    // is whichever ends with a block check that matches.
    for shift in 0..8 {
        let aligned = &bits[shift..];
        let bytes = bytes_lsb_first(aligned);
        for (i, &b) in bytes.iter().enumerate() {
            // SOH, with odd parity, marks the header.
            if b & 0x7f != 0x01 || !parity_ok(b) {
                continue;
            }
            if let Some(msg) = parse_from(&bytes[i + 1..]) {
                return Some(msg);
            }
        }
    }
    None
}

/// Parse the header and text that follow `SOH`.
fn parse_from(rest: &[u8]) -> Option<AcarsFrame> {
    // mode(1) address(7) ack(1) label(2) block_id(1) STX(1)
    if rest.len() < 13 {
        return None;
    }
    let ch = |b: u8| (b & 0x7f) as char;
    let ck = |b: u8| -> Option<()> { parity_ok(b).then_some(()) };
    ck(rest[0])?;
    let mode = ch(rest[0]).to_string();
    let mut address = String::new();
    for &b in &rest[1..8] {
        ck(b)?;
        address.push(ch(b));
    }
    ck(rest[8])?;
    let ack = ch(rest[8]).to_string();
    ck(rest[9])?;
    ck(rest[10])?;
    let label: String = [ch(rest[9]), ch(rest[10])].iter().collect();
    ck(rest[11])?;
    let block_id = ch(rest[11]).to_string();
    ck(rest[12])?;
    if rest[12] & 0x7f != 0x02 {
        return None;
    }

    // Text up to ETX/ETB, which carries parity like the rest.
    let mut text = String::new();
    let mut idx = 13;
    let mut ended = false;
    while idx < rest.len().min(13 + 220) {
        let b = rest[idx];
        ck(b)?;
        idx += 1;
        if b & 0x7f == 0x03 || b & 0x7f == 0x17 {
            ended = true;
            break;
        }
        text.push(ch(b));
    }
    if !ended {
        return None;
    }

    // The block check is two unparitied bytes, then DEL. The CRC covers the
    // header and text bytes as they were received (parity stripped is what the
    // senders build it over, so strip it here too).
    if rest.len() < idx + 3 {
        return None;
    }
    let bcs = u16::from_be_bytes([rest[idx], rest[idx + 1]]);
    let mut covered: Vec<u8> = rest[..idx].iter().map(|b| b & 0x7f).collect();
    // The standard builds the check over the header from SOH; the caller has
    // already skipped it, so the SOH byte is prepended back.
    covered.insert(0, 0x01);
    let crc_ok = crc16(&covered) == bcs;

    Some(AcarsFrame {
        mode,
        address: address.trim().to_string(),
        ack,
        label,
        block_id,
        text,
        crc_ok,
    })
}

/// Undo the NRZI line coding: a *level* change is a data 0, no change a data 1.
pub fn nrzi_decode(levels: &[u8]) -> Vec<u8> {
    let mut prev = levels.first().copied().unwrap_or(1);
    let mut out = Vec::with_capacity(levels.len());
    for &l in levels {
        out.push(u8::from(l == prev));
        prev = l;
    }
    out
}

/// The ACARS demodulator: real audio in, bits out.
pub struct AcarsRx {
    /// Mixes the 1800 Hz centre down to DC.
    phase: f64,
    dphase: f64,
    /// Real and imaginary low-pass against 2400 Hz.
    lp_re: RealFir,
    lp_im: RealFir,
    /// Scratch buffers for the mixed and filtered block.
    mix_re: Vec<f32>,
    mix_im: Vec<f32>,
    filt_re: Vec<f32>,
    filt_im: Vec<f32>,
    /// The previous sample, for the instantaneous-frequency term.
    prev: (f32, f32),
    /// Samples per symbol.
    sps: f64,
    /// Symbol accumulator and its sample count.
    acc: f32,
    acc_n: f64,
    /// Levels since the last frame attempt, trimmed to a sane maximum.
    levels: Vec<u8>,
    /// Samples to skip while the low-pass fills, so the symbol grid lines up
    /// with the signal rather than with the filter's group delay.
    warmup: usize,
    /// Smoothed audio level, for the panel's meter.
    level: f32,
    /// Frames decoded with a good block check, and frames whose check failed.
    frames: u64,
    bad: u64,
}

impl AcarsRx {
    pub fn new(rate: f64) -> Self {
        AcarsRx {
            phase: 0.0,
            dphase: std::f64::consts::TAU * CENTER_HZ / rate,
            lp_re: RealFir::lowpass(63, 2_400.0, rate),
            lp_im: RealFir::lowpass(63, 2_400.0, rate),
            mix_re: Vec::new(),
            mix_im: Vec::new(),
            filt_re: Vec::new(),
            filt_im: Vec::new(),
            prev: (0.0, 0.0),
            sps: rate / BAUD,
            acc: 0.0,
            acc_n: 0.0,
            levels: Vec::new(),
            // Half the 63-tap low-pass, in samples.
            warmup: 31,
            level: 0.0,
            frames: 0,
            bad: 0,
        }
    }

    /// Smoothed audio level, 0-1-ish.
    pub fn level(&self) -> f32 {
        self.level
    }

    /// Frames decoded with a good block check.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Frames whose block check failed.
    pub fn bad(&self) -> u64 {
        self.bad
    }

    /// Consume audio, appending any messages found.
    pub fn process(&mut self, audio: &[f32], out: &mut Vec<AcarsEvent>) {
        // A running level for the meter.
        let ms = audio.iter().map(|s| s * s).sum::<f32>() / audio.len().max(1) as f32;
        self.level += 0.3 * (ms.sqrt() - self.level);
        // Mix the 1800 Hz centre down to DC, then low-pass.
        self.mix_re.clear();
        self.mix_im.clear();
        self.mix_re.reserve(audio.len());
        self.mix_im.reserve(audio.len());
        for &s in audio {
            self.phase += self.dphase;
            if self.phase > std::f64::consts::TAU {
                self.phase -= std::f64::consts::TAU;
            }
            self.mix_re.push(s * self.phase.cos() as f32);
            self.mix_im.push(-s * self.phase.sin() as f32);
        }
        self.filt_re.clear();
        self.filt_im.clear();
        self.lp_re.process(&self.mix_re, &mut self.filt_re);
        self.lp_im.process(&self.mix_im, &mut self.filt_im);

        for k in 0..self.filt_re.len() {
            let (r, m) = (self.filt_re[k], self.filt_im[k]);
            // Instantaneous frequency as the phase step between samples.
            // The phase step from the previous sample: the negative of
            // atan2 of the cross product, so a tone above the centre reads
            // positive.
            let cross = self.prev.1 * r - self.prev.0 * m;
            let dot = self.prev.0 * r + self.prev.1 * m;
            let dphi = -cross.atan2(dot);
            self.prev = (r, m);
            if self.warmup > 0 {
                self.warmup -= 1;
                continue;
            }

            self.acc += dphi;
            self.acc_n += 1.0;
            if self.acc_n >= self.sps {
                // MSK: the 2400 Hz tone sits above the 1800 Hz centre and is
                // the higher level; the 1200 Hz tone is the lower one.
                let level = u8::from(self.acc > 0.0);
                self.acc = 0.0;
                // Carry the overshoot instead of clearing it. `sps` is
                // fractional at most rates — 51.2 kHz / 2400 baud is 21.33… —
                // and dropping the remainder on every symbol walks the sampling
                // grid off the signal, so the far end of a long frame is read on
                // the wrong samples and the block check never matches.
                self.acc_n -= self.sps;
                if self.levels.len() < 8 * 300 {
                    self.levels.push(level);
                }
            }
        }

        // Try to parse whenever the buffer has a plausible frame in it.
        if self.levels.len() >= 8 * 32 {
            let bits = nrzi_decode(&self.levels);
            if let Some(msg) = parse_frame(&bits) {
                if msg.crc_ok {
                    self.frames += 1;
                } else {
                    self.bad += 1;
                }
                out.push(AcarsEvent::Message(msg));
                self.levels.clear();
            } else if self.levels.len() > 8 * 280 {
                self.levels.drain(..8 * 40);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a frame's bit stream from a message, the way a transmitter would:
    /// SOH, header, STX, text, ETX, BCS, DEL, each character 7 bits LSB-first
    /// plus odd parity — then NRZI-encode it.
    pub(super) fn encode(msg: &AcarsFrame) -> Vec<u8> {
        let mut bytes: Vec<u8> = Vec::new();
        let ch = |c: char| -> u8 {
            let d = (c as u8) & 0x7f;
            d | if (d.count_ones() & 1) == 0 { 0x80 } else { 0 }
        };
        // Bit sync and character sync, then SOH.
        for c in ['+', '*'] {
            bytes.push(ch(c));
        }
        for _ in 0..2 {
            bytes.push(0x16);
        }
        bytes.push(ch('\u{1}'));
        bytes.push(ch(msg.mode.chars().next().unwrap_or('2')));
        for c in msg.address.chars().take(7) {
            bytes.push(ch(c));
        }
        for _ in 0..(7 - msg.address.chars().count().min(7)) {
            bytes.push(ch(' '));
        }
        bytes.push(ch(msg.ack.chars().next().unwrap_or(' ')));
        for c in msg.label.chars().take(2) {
            bytes.push(ch(c));
        }
        bytes.push(ch(msg.block_id.chars().next().unwrap_or(' ')));
        bytes.push(ch('\u{2}'));
        for c in msg.text.chars() {
            bytes.push(ch(c));
        }
        bytes.push(ch('\u{3}'));

        // The BCS covers the header from SOH onwards, parity stripped.
        let mut covered: Vec<u8> = vec![0x01];
        covered.extend(bytes[5..].iter().map(|b| b & 0x7f));
        let bcs = crc16(&covered);
        bytes.push((bcs >> 8) as u8);
        bytes.push((bcs & 0xff) as u8);
        bytes.push(0x7f);

        // Bits, LSB-first per byte, then NRZI levels: 0 = change, 1 = no change.
        let mut levels: Vec<u8> = vec![1; 16 * 8]; // pre-key of ones
        let mut last = 1u8;
        for byte in bytes {
            for i in 0..8 {
                let bit = (byte >> i) & 1;
                let level = if bit == 0 { 1 - last } else { last };
                levels.push(level);
                last = level;
            }
        }
        levels
    }

    fn sample() -> AcarsFrame {
        AcarsFrame {
            mode: "2".into(),
            address: ".N12345".into(),
            ack: " ".into(),
            label: "H1".into(),
            block_id: "1".into(),
            text: "HELLO FROM ACARS".into(),
            crc_ok: true,
        }
    }

    #[test]
    fn a_frame_round_trips_through_nrzi_and_the_parser() {
        let msg = sample();
        let bits = nrzi_decode(&encode(&msg));
        let got = parse_frame(&bits).expect("a frame");
        assert_eq!(got.address, ".N12345");
        assert_eq!(got.label, "H1");
        assert_eq!(got.text, "HELLO FROM ACARS");
        assert!(got.crc_ok, "the block check must verify");
    }

    #[test]
    fn a_corrupt_block_check_is_reported_not_hidden() {
        let mut bits = nrzi_decode(&encode(&sample()));
        // Flip a text bit; the CRC must catch it.
        let n = bits.len();
        bits[n / 2] ^= 1;
        match parse_frame(&bits) {
            Some(m) => assert!(!m.crc_ok, "a corrupted frame must not claim a good check"),
            None => {}
        }
    }

    #[test]
    fn noise_yields_no_message() {
        // A pseudo-random stream should not produce a frame with a good check.
        let mut x = 0x12345678u32;
        let bits: Vec<u8> = (0..4000)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                ((x >> 16) & 1) as u8
            })
            .collect();
        assert!(parse_frame(&bits).is_none(), "noise must not fabricate a frame");
    }

    /// Turn NRZI levels into the AM-detected audio: a tone at 2400 Hz for a
    /// high level and 1200 Hz for a low one, continuous phase.
    pub(super) fn levels_to_audio(levels: &[u8], rate: f64, sps: usize) -> Vec<f32> {
        let t = std::f64::consts::TAU;
        let mut phase = 0.0f64;
        let mut out = Vec::with_capacity(levels.len() * sps);
        for &l in levels {
            let f = if l == 1 { 2400.0 } else { 1200.0 };
            for _ in 0..sps {
                out.push(phase.cos() as f32);
                phase = (phase + t * f / rate) % t;
            }
        }
        out
    }

    #[test]
    fn a_synthetic_signal_decodes_end_to_end() {
        let msg = sample();
        let levels = encode(&msg);
        let rate = 48_000.0;
        let sps = (rate / BAUD).round() as usize;
        let mut audio = levels_to_audio(&levels, rate, sps);
        // A little more of the last tone, so the final symbol and its block
        // check are inside the audio rather than cut off by its end.
        audio.extend(std::iter::repeat_n(0.0f32, sps * 4));
        let mut rx = AcarsRx::new(rate);
        let mut events = Vec::new();
        // Feed in blocks, as the engine would.
        for chunk in audio.chunks(4096) {
            rx.process(chunk, &mut events);
        }
        let got: Vec<&AcarsFrame> = events
            .iter()
            .filter_map(|e| match e {
                AcarsEvent::Message(m) => Some(m),
                _ => None,
            })
            .collect();
        assert!(!got.is_empty(), "a synthetic frame must decode");
        assert_eq!(got[0].text, "HELLO FROM ACARS");
        assert!(got[0].crc_ok, "and its block check must verify");
    }

    /// The symbol clock carries its fractional remainder. At 51.2 kHz a symbol
    /// is 21.33… samples, and rounding that away each symbol walks the sampling
    /// grid off a long frame — the far end reads the wrong samples and the
    /// block check never matches. 48 kHz divides evenly and hid it.
    #[test]
    fn a_fractional_symbol_rate_still_decodes() {
        let levels = encode(&sample());
        let rate = 51_200.0;
        // Symbol boundaries at their true fractional positions.
        let t = std::f64::consts::TAU;
        let mut phase = 0.0f64;
        let mut audio = Vec::new();
        for (k, &l) in levels.iter().enumerate() {
            let start = (k as f64 * rate / BAUD).round() as usize;
            let end = (((k + 1) as f64) * rate / BAUD).round() as usize;
            let f = if l == 1 { 2400.0 } else { 1200.0 };
            for _ in start..end {
                audio.push(phase.cos() as f32);
                phase = (phase + t * f / rate) % t;
            }
        }
        audio.extend(std::iter::repeat_n(0.0f32, 200));
        let mut rx = AcarsRx::new(rate);
        let mut events = Vec::new();
        for chunk in audio.chunks(4096) {
            rx.process(chunk, &mut events);
        }
        let got = events.iter().find_map(|e| match e {
            AcarsEvent::Message(m) => Some(m),
            _ => None,
        });
        let got = got.expect("a frame at a fractional symbol rate must decode");
        assert!(got.crc_ok, "and its block check must verify");
    }

    #[test]
    fn the_crc_matches_the_ccitt_known_answer() {
        // "123456789" under CRC-16/CCITT-FALSE is 0x29B1.
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }
}
