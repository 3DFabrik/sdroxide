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
//! [`parse_frame`] turns the demodulated bit stream into a message and needs no
//! radio at all, and [`AcarsRx`] turns the demodulated audio into that bit
//! stream.
//!
//! The demodulator follows acarsdec's `msk.c`: a half-sine matched filter, a
//! bit clock, and a PI loop that tracks the carrier. The fixed-grid integrator
//! it started as only worked on this decoder's own encoder — real audio is not
//! symbol-aligned to the first sample and its carrier is not exactly 1800 Hz,
//! so both have to be recovered. It is checked against an off-air recording of
//! acarsdec's, not only its own encoder.

use crate::resample::MonoResampler;
use num_complex::Complex32;

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

/// The ACARS block-check sequence: CRC-16/reflected with polynomial 0x8408 and
/// initial value 0, over the bytes **as received, parity bits included**, from
/// the mode character through `ETX`/`ETB`. The transmitted BCS is this value
/// with its low byte first.
///
/// Not CCITT-FALSE. acarsdec — the reference decoder, and the source of the
/// off-air fixture this is checked against — builds the check this way, and a
/// real frame's bytes verify to zero under it. The VDL2 parser's Kermit CRC is
/// the same reflected polynomial; the difference here is the initial value and
/// that ACARS carries the parity bits into the sum.
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x8408 } else { crc >> 1 };
        }
    }
    crc
}

/// Assemble bytes from a bit slice, least-significant bit first, eight bits
/// each — every bit inverted when `invert` is set.
fn bytes_lsb_first(bits: &[u8], invert: bool) -> Vec<u8> {
    let flip = u8::from(invert);
    bits.chunks(8)
        .filter(|c| c.len() == 8)
        .map(|c| c.iter().enumerate().fold(0u8, |b, (i, &v)| b | (((v ^ flip) & 1) << i)))
        .collect()
}

/// Parse a demodulated bit stream into a message, hunting for the start of
/// the frame.
///
/// Returns a frame whose block check matches if the stream holds one, and
/// otherwise the first heading that framed at all, with `crc_ok` false. `None`
/// when nothing frames — the parse is what rejects noise, so a false positive
/// costs a check rather than a phantom message.
pub fn parse_frame(bits: &[u8]) -> Option<AcarsFrame> {
    find_frame(bits).map(|(msg, _)| msg)
}

/// [`parse_frame`], with the bit offset of the frame's `SOH` in `bits`.
///
/// Both polarities are tried. The carrier loop settles with either sign — an
/// MSK phase detector cannot tell the two apart — and a burst demodulated
/// upside down is every bit inverted. acarsdec reads the sign off an inverted
/// SYN and flips its demodulator; reading the window both ways and keeping
/// the block check that matches comes to the same thing. Without it, half the
/// bursts in acarsdec's own recording never framed.
///
/// A heading whose block check matches wins over any that does not, wherever
/// either sits: a coincidental heading read the wrong way up must not hide
/// the real frame read the right way.
fn find_frame(bits: &[u8]) -> Option<(AcarsFrame, usize)> {
    let mut failed = None;
    for invert in [false, true] {
        for shift in 0..8 {
            let Some(aligned) = bits.get(shift..) else { break };
            let bytes = bytes_lsb_first(aligned, invert);
            for (i, &b) in bytes.iter().enumerate() {
                // SOH, with odd parity, marks the header.
                if b & 0x7f != 0x01 || !parity_ok(b) {
                    continue;
                }
                if let Some(msg) = parse_from(&bytes[i + 1..]) {
                    let at = shift + 8 * i;
                    if msg.crc_ok {
                        return Some((msg, at));
                    }
                    failed.get_or_insert((msg, at));
                }
            }
        }
    }
    failed
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
    // STX when text follows, ETX when the block has none — an uplink's
    // acknowledgement or a `_d` general response is only the header. Requiring
    // STX dropped every one of those.
    let mut ended = match rest[12] & 0x7f {
        0x02 => false,
        0x03 => true,
        _ => return None,
    };

    // Text up to ETX/ETB, which carries parity like the rest.
    let mut text = String::new();
    let mut idx = 13;
    while !ended && idx < rest.len().min(13 + 220) {
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
    // The check runs over the header and text exactly as received — parity and
    // all — from the mode character (the caller skipped SOH) through ETX, and
    // the transmitted BCS carries the low byte first.
    let bcs = u16::from_le_bytes([rest[idx], rest[idx + 1]]);
    let crc_ok = crc16(&rest[..idx]) == bcs;

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
///
/// Ported from acarsdec's `msk.c`: the mixer runs off a VCO whose phase is
/// corrected by a PI carrier loop, the symbol decision is a half-sine matched
/// filter read at the bit clock's fractional position, and the clock itself is
/// the VCO phase crossing 3π/2 — so it tracks a real signal's timing and
/// frequency rather than assuming both.
pub struct AcarsRx {
    /// Audio rate, for the VCO's centre-frequency step.
    rate: f32,
    /// Matched filter length in samples: one 1200 Hz half-cycle.
    bitlen: usize,
    /// Oversampling of the filter table.
    over: usize,
    /// Half-sine matched filter, `bitlen * over + 1` taps.
    mflt: Vec<f32>,
    /// The last `bitlen` mixed samples, newest overwriting oldest.
    inb: Vec<Complex32>,
    inb_idx: usize,
    /// VCO phase (the carrier) and the bit-clock accumulator.
    phi: f32,
    clk: f32,
    /// PI carrier loop: frequency and phase corrections.
    df: f32,
    dphi: f32,
    /// Symbols emitted: selects the I/Q arm and the two-symbol sign flip.
    symbols: u32,
    /// Bits since the last framing attempt, trimmed to a sane maximum.
    bits: Vec<u8>,
    /// Resamples the incoming audio to the 12 kHz the demodulator runs at.
    /// `None` when it already is.
    rs: Option<MonoResampler>,
    rs_buf: Vec<f32>,
    /// Smoothed audio level, for the panel's meter.
    level: f32,
    /// Frames decoded with a good block check, and frames whose check failed.
    frames: u64,
    bad: u64,
}

impl AcarsRx {
    pub fn new(rate: f64) -> Self {
        // acarsdec's demodulator is defined at 12 kHz and reaches it by
        // averaging the input down; a resampler does the same for any rate, so
        // the 12 kHz constants below stay exactly what the reference uses
        // instead of being re-derived (wrongly) per rate.
        let demod_rate = 12_000.0f64;
        let over = 240usize;
        let bitlen = (demod_rate / 1200.0).ceil().max(1.0) as usize;
        let mut mflt = vec![0.0f32; bitlen * over + 1];
        for (i, h) in mflt.iter_mut().enumerate() {
            *h = (std::f32::consts::PI * 1200.0 * i as f32 / demod_rate as f32 / over as f32).sin();
        }
        AcarsRx {
            rate: demod_rate as f32,
            bitlen,
            over,
            mflt,
            inb: vec![Complex32::default(); bitlen],
            inb_idx: 0,
            phi: 0.0,
            clk: 0.0,
            df: 0.0,
            dphi: 0.0,
            symbols: 0,
            bits: Vec::new(),
            rs: MonoResampler::new(rate, demod_rate),
            rs_buf: Vec::new(),
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
        let ms = audio.iter().map(|s| s * s).sum::<f32>() / audio.len().max(1) as f32;
        self.level += 0.3 * (ms.sqrt() - self.level);

        // acarsdec's loop gains. It scales them by its own 12 kHz filter length
        // (10 samples); the loop steps once per symbol and the symbol rate is
        // fixed, so the per-symbol gains are what matter and they must not be
        // re-scaled by *this* rate's filter length — that made an engine-fed
        // 48 kHz signal far too slow to lock.
        let ki = 71e-7 / 10.0;
        let kp = 60e-3 / 10.0;
        let tau = std::f32::consts::TAU;
        let bit_clock = 1.5 * std::f32::consts::PI;

        if let Some(rs) = self.rs.as_mut() {
            self.rs_buf.clear();
            rs.push(audio, &mut self.rs_buf);
        }
        let resampled = self.rs.is_some();
        let n = if resampled { self.rs_buf.len() } else { audio.len() };

        for k in 0..n {
            let sample = if resampled { self.rs_buf[k] } else { audio[k] };
            // VCO frequency: the 1800 Hz centre plus the loop's correction.
            let s = tau * CENTER_HZ as f32 / self.rate + self.dphi;

            self.clk += s;
            if self.clk > bit_clock {
                self.clk -= bit_clock;

                // The matched filter, read at the clock's fractional position.
                let mut o = (self.over as f32 * (self.clk / s)) as usize;
                if o > self.over {
                    o = self.over;
                }
                let mut v = Complex32::new(0.0, 0.0);
                for j in 0..self.bitlen {
                    let tap = self.mflt[o + j * self.over];
                    v += self.inb[(j + self.inb_idx) % self.bitlen] * tap;
                }
                // Normalise, so the decision is a sign and not a level.
                let v = v / (v.norm() + 1e-8);

                let (vo, dphi) = if self.symbols & 1 == 1 {
                    let vo = v.im;
                    (vo, if vo >= 0.0 { -v.re } else { v.re })
                } else {
                    let vo = v.re;
                    (vo, if vo >= 0.0 { v.im } else { -v.im })
                };
                let bit = if self.symbols & 2 != 0 { -vo } else { vo };
                self.push_bit(bit > 0.0, out);
                self.symbols = self.symbols.wrapping_add(1);

                // The PI controller: frequency integrates the error, phase is
                // its output plus a proportional term.
                self.df += ki * dphi;
                self.dphi = self.df + kp * dphi;
            }

            self.phi += s;
            if self.phi >= tau {
                self.phi -= tau;
            }
            // Mix down by the VCO, newest sample into the circular buffer.
            self.inb[self.inb_idx] = Complex32::from_polar(1.0, -self.phi) * sample;
            self.inb_idx = (self.inb_idx + 1) % self.bitlen;
        }
    }

    /// One demodulated bit: buffer it, and try to frame once a byte is in.
    ///
    /// A heading whose block check fails is counted and stepped past, not
    /// thrown away with the rest of the buffer: on real audio there are several
    /// SOH-shaped coincidences per burst, and clearing on the first of them
    /// discards the real frame sitting behind it. Only a frame that checks out
    /// is emitted and clears the window.
    fn push_bit(&mut self, bit: bool, out: &mut Vec<AcarsEvent>) {
        if self.bits.len() < 8 * 512 {
            self.bits.push(u8::from(bit));
        }
        if self.bits.len() % 8 != 0 || self.bits.len() < 8 * 32 {
            return;
        }
        match parse_frame(&self.bits) {
            Some(msg) if msg.crc_ok => {
                self.frames += 1;
                out.push(AcarsEvent::Message(msg));
                self.bits.clear();
            }
            Some(_) => {
                self.bad += 1;
                self.bits.drain(..8);
            }
            None if self.bits.len() > 8 * 480 => {
                self.bits.drain(..8 * 40);
            }
            None => {}
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
        // A block with no text goes straight from the header to ETX.
        if !msg.text.is_empty() {
            bytes.push(ch('\u{2}'));
            for c in msg.text.chars() {
                bytes.push(ch(c));
            }
        }
        bytes.push(ch('\u{3}'));

        // The BCS covers the mode character through ETX as transmitted, parity
        // included, and goes out low byte first.
        let bcs = crc16(&bytes[5..]);
        bytes.push((bcs & 0xff) as u8);
        bytes.push((bcs >> 8) as u8);
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

    /// A block with no text has ETX where STX would be. acarsdec's recording
    /// has one (an uplink `_d` to LN-DYY), and it was the frame this decoder
    /// missed at either polarity.
    #[test]
    fn a_frame_with_no_text_ends_its_header_with_etx() {
        let msg = AcarsFrame {
            mode: "2".into(),
            address: ".LN-DYY".into(),
            ack: "5".into(),
            label: "_\u{7f}".into(),
            block_id: "A".into(),
            text: String::new(),
            crc_ok: true,
        };
        let got = parse_frame(&nrzi_decode(&encode(&msg))).expect("a header-only frame");
        assert_eq!(got.label, "_\u{7f}");
        assert!(got.text.is_empty());
        assert!(got.crc_ok, "the block check covers the header and ETX");
    }

    /// The carrier loop can settle with either sign, which turns every bit
    /// over. The frame must decode the same either way up.
    #[test]
    fn an_inverted_bit_stream_still_decodes() {
        let bits: Vec<u8> = nrzi_decode(&encode(&sample())).iter().map(|b| b ^ 1).collect();
        let got = parse_frame(&bits).expect("a frame read upside down");
        assert_eq!(got.text, "HELLO FROM ACARS");
        assert!(got.crc_ok);
    }

    /// A heading that frames but fails its check, ahead of a good frame in the
    /// same window, must not be what comes back.
    #[test]
    fn a_failed_heading_does_not_hide_a_good_frame_behind_it() {
        let mut bad = nrzi_decode(&encode(&sample()));
        // Two bits of the first text character: parity still holds, so the
        // heading frames, and only the block check can tell. The text starts
        // after 16 bytes of pre-key and 18 of sync and header.
        let text = (16 + 18) * 8;
        bad[text] ^= 1;
        bad[text + 1] ^= 1;
        let only = parse_frame(&bad).expect("the damaged frame still frames");
        assert!(!only.crc_ok);
        let mut good = sample();
        good.text = "SECOND".into();
        bad.extend(nrzi_decode(&encode(&good)));
        let got = parse_frame(&bad).expect("a frame");
        assert!(got.crc_ok, "the frame that checks out wins");
        assert_eq!(got.text, "SECOND");
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



    /// The symbol clock carries its fractional remainder. At 51.2 kHz a symbol
    /// is 21.33… samples, and rounding that away each symbol walks the sampling
    /// grid off a long frame — the far end reads the wrong samples and the
    /// block check never matches. 48 kHz divides evenly and hid it.

    /// The block check is the reflected CRC-16 with initial value 0 — the same
    /// form as CRC-16/KERMIT, whose check value for "123456789" is 0x2189.
    /// CCITT-FALSE would give 0x29B1 and does not verify real frames.

    #[test]
    fn the_block_check_is_the_reflected_crc() {
        assert_eq!(crc16(b"123456789"), 0x2189);
    }

    /// Decode an off-air recording, which is the only thing that can say
    /// whether the demodulator matches the air rather than its own encoder.
    ///
    /// Ignored by default: it needs a file that is not in the tree. Point
    /// `SDROXIDE_ACARS_SAMPLE` at a WAV of demodulated AM audio at any rate —
    /// acarsdec's `test.wav` (12.5 kHz, four receivers side by side) is one.
    /// Every channel is decoded as a receiver of its own; acarsdec finds seven
    /// frames in that file, 2 + 2 + 2 + 1, and so should this.
    ///
    /// `SDROXIDE_ACARS_SAMPLE=/path/test.wav cargo test -p sdroxide-dsp
    /// --release -- --ignored --nocapture`
    #[test]
    #[ignore = "needs an off-air recording; set SDROXIDE_ACARS_SAMPLE"]
    fn an_off_air_recording_decodes() {
        let Ok(path) = std::env::var("SDROXIDE_ACARS_SAMPLE") else {
            eprintln!("skipping: set SDROXIDE_ACARS_SAMPLE to a WAV recording");
            return;
        };
        let mut reader =
            hound::WavReader::open(&path).unwrap_or_else(|e| panic!("opening {path}: {e}"));
        let spec = reader.spec();
        let rate = f64::from(spec.sample_rate);
        let channels = spec.channels as usize;
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Int => {
                let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader.samples::<i32>().filter_map(Result::ok).map(|s| s as f32 * scale).collect()
            }
            hound::SampleFormat::Float => reader.samples::<f32>().filter_map(Result::ok).collect(),
        };
        let mut total = 0;
        for ch in 0..channels {
            let mono: Vec<f32> = samples.iter().skip(ch).step_by(channels).copied().collect();
            let mut rx = AcarsRx::new(rate);
            let mut events = Vec::new();
            for chunk in mono.chunks(4096) {
                rx.process(chunk, &mut events);
            }
            for AcarsEvent::Message(m) in &events {
                eprintln!(
                    "#{} ok  {} {} {}  {}",
                    ch + 1,
                    m.address,
                    m.label,
                    m.block_id,
                    m.text.trim()
                );
            }
            eprintln!("#{}: {} good frames, {} bad", ch + 1, rx.frames(), rx.bad());
            total += rx.frames();
        }
        assert!(total > 0, "the recording produced no frame with a good block check");
    }
}
