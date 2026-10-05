//! A recorded APT pass as a WAV file WXtoImg can open.
//!
//! What this writes is the **discriminator output** — the 2400 Hz AM subcarrier
//! that comes out of the FM demodulator, which is the signal a soundcard-era
//! APT decoder was always fed. It is not the picture: WXtoImg does its own
//! sync hunting, its own line timing, and from that its map overlays,
//! projections and colour enhancements, none of which can be recovered from a
//! decoded image. So a pass is kept twice — the picture this program made, and
//! the audio somebody else's program can make a better one from.
//!
//! 16-bit PCM mono at 11025 Hz, because that is what WXtoImg and the decoders
//! that came before it expect, and because [`sdroxide_dsp::apt`] needs nothing
//! more: the subcarrier is at 2400 Hz and the whole baseband fits under 5 kHz.
//! Deliberately not the float32 stereo of [`crate::iq_wav`], which exists to
//! hand a *baseband* capture to an SDR program — this is audio, and a WAV that
//! is not the shape the receiving program expects is a file nobody can use.
//!
//! No RF64 reservation either, for the arithmetic: 11025 samples a second at
//! two bytes is 22 kB/s, so the 4 GB a RIFF file can hold is fifty hours. A
//! pass is fifteen minutes.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The rate WXtoImg and its predecessors read. See the module note.
pub const RATE_HZ: u32 = 11_025;

/// Bytes one frame occupies: one channel of `i16`.
const FRAME_BYTES: u64 = 2;

/// Where the 32-bit `RIFF` size lives.
const RIFF_SIZE_AT: u64 = 4;

/// A recording in progress.
pub struct AptWavWriter {
    file: BufWriter<File>,
    path: PathBuf,
    /// Byte offset of the `data` chunk's own size field, patched on close.
    data_size_at: u64,
    frames: u64,
    scratch: Vec<u8>,
}

impl AptWavWriter {
    /// Open `path` and write the header for a mono PCM16 stream at
    /// [`RATE_HZ`].
    pub fn create(path: &Path) -> std::io::Result<AptWavWriter> {
        let file = File::create(path)?;
        // Modest next to the I/Q writer's two megabytes: this is 22 kB/s, and
        // a second of buffering is already more than the engine tick needs.
        let mut w = BufWriter::with_capacity(1 << 16, file);
        let (bytes, data_size_at) = header_bytes();
        w.write_all(&bytes)?;
        Ok(AptWavWriter {
            file: w,
            path: path.to_path_buf(),
            data_size_at,
            frames: 0,
            scratch: Vec::new(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Seconds of audio written so far — what the operator is shown.
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / f64::from(RATE_HZ)
    }

    pub fn bytes(&self) -> u64 {
        self.frames * FRAME_BYTES
    }

    /// Append a block of audio, already at [`RATE_HZ`].
    ///
    /// Clipped rather than wrapped. A discriminator that has been driven past
    /// full scale is a pass recorded too hot, and a sample that wrapped to the
    /// opposite rail is a click in the picture; the clip at least keeps the
    /// line readable.
    pub fn write(&mut self, audio: &[f32]) -> std::io::Result<()> {
        self.scratch.clear();
        self.scratch.reserve(audio.len() * FRAME_BYTES as usize);
        for &s in audio {
            let v = (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
            self.scratch.extend_from_slice(&v.to_le_bytes());
        }
        self.file.write_all(&self.scratch)?;
        self.frames += audio.len() as u64;
        Ok(())
    }

    /// Close the file, patching the two sizes in.
    ///
    /// Returns rather than doing this in `Drop` for the reason
    /// [`crate::iq_wav::IqWavWriter::finish`] does: a failure here leaves a
    /// file whose header says it is empty, which every program reads as a
    /// zero-length recording, and that is worth saying out loud.
    pub fn finish(mut self) -> std::io::Result<PathBuf> {
        self.file.flush()?;
        let data_bytes = self.frames * FRAME_BYTES;
        let riff_bytes = self.data_size_at + 4 + data_bytes - 8;
        let f = self.file.get_mut();
        f.seek(SeekFrom::Start(RIFF_SIZE_AT))?;
        f.write_all(&(riff_bytes.min(u64::from(u32::MAX)) as u32).to_le_bytes())?;
        f.seek(SeekFrom::Start(self.data_size_at))?;
        f.write_all(&(data_bytes.min(u64::from(u32::MAX)) as u32).to_le_bytes())?;
        f.flush()?;
        Ok(self.path)
    }
}

/// The header up to and including the `data` chunk's size field, and where that
/// field is.
fn header_bytes() -> (Vec<u8>, u64) {
    let mut b: Vec<u8> = Vec::with_capacity(44);
    let channels: u16 = 1;
    let bits: u16 = 16;
    let block_align = u32::from(channels) * u32::from(bits) / 8;

    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&0u32.to_le_bytes()); // patched on close
    b.extend_from_slice(b"WAVE");

    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&RATE_HZ.to_le_bytes());
    b.extend_from_slice(&(RATE_HZ * block_align).to_le_bytes());
    b.extend_from_slice(&(block_align as u16).to_le_bytes());
    b.extend_from_slice(&bits.to_le_bytes());

    b.extend_from_slice(b"data");
    let data_size_at = b.len() as u64;
    b.extend_from_slice(&0u32.to_le_bytes()); // patched on close
    (b, data_size_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sdroxide-aptwav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// The shape WXtoImg opens: mono PCM16 at 11025 Hz, with both sizes
    /// patched to what the file really holds. A header that still claims zero
    /// bytes is the failure mode this guards — the recording is on the disk and
    /// every player reports an empty file.
    #[test]
    fn a_pass_is_a_mono_pcm16_wav_at_the_rate_wxtoimg_reads() {
        let path = tmp("pass.wav");
        let mut w = AptWavWriter::create(&path).unwrap();
        let block: Vec<f32> = (0..RATE_HZ).map(|i| (i as f32 * 0.001).sin() * 0.5).collect();
        w.write(&block).unwrap();
        assert_eq!(w.bytes(), u64::from(RATE_HZ) * 2);
        assert!((w.seconds() - 1.0).abs() < 1e-9);
        w.finish().unwrap();

        let raw = std::fs::read(&path).unwrap();
        assert_eq!(&raw[..4], b"RIFF");
        assert_eq!(&raw[8..12], b"WAVE");
        assert_eq!(
            u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize,
            raw.len() - 8,
            "the RIFF size is everything after it"
        );
        assert_eq!(&raw[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes(raw[20..22].try_into().unwrap()), 1, "PCM");
        assert_eq!(u16::from_le_bytes(raw[22..24].try_into().unwrap()), 1, "mono");
        assert_eq!(u32::from_le_bytes(raw[24..28].try_into().unwrap()), RATE_HZ);
        assert_eq!(u32::from_le_bytes(raw[28..32].try_into().unwrap()), RATE_HZ * 2, "byte rate");
        assert_eq!(u16::from_le_bytes(raw[34..36].try_into().unwrap()), 16, "bits");
        assert_eq!(&raw[36..40], b"data");
        let data = u32::from_le_bytes(raw[40..44].try_into().unwrap()) as usize;
        assert_eq!(data, RATE_HZ as usize * 2);
        assert_eq!(44 + data, raw.len(), "the samples are all of the rest of the file");
        let _ = std::fs::remove_file(&path);
    }

    /// A discriminator driven past full scale clips rather than wrapping to the
    /// opposite rail, which would put a click in every line of the picture.
    #[test]
    fn an_overdriven_sample_clips_instead_of_wrapping() {
        let path = tmp("hot.wav");
        let mut w = AptWavWriter::create(&path).unwrap();
        w.write(&[2.0, -2.0, 0.0]).unwrap();
        w.finish().unwrap();

        let raw = std::fs::read(&path).unwrap();
        let s = |i: usize| i16::from_le_bytes(raw[44 + i * 2..46 + i * 2].try_into().unwrap());
        assert_eq!(s(0), i16::MAX);
        assert_eq!(s(1), -i16::MAX, "the negative rail, not a wrap to the positive one");
        assert_eq!(s(2), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// A pass nothing ever arrived for still closes into a legal, empty WAV
    /// rather than a file with an unpatched header.
    #[test]
    fn a_recording_with_no_audio_is_still_a_legal_file() {
        let path = tmp("empty.wav");
        AptWavWriter::create(&path).unwrap().finish().unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(raw.len(), 44);
        assert_eq!(u32::from_le_bytes(raw[4..8].try_into().unwrap()), 36);
        assert_eq!(u32::from_le_bytes(raw[40..44].try_into().unwrap()), 0);
        let _ = std::fs::remove_file(&path);
    }
}
