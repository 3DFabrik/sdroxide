//! End-to-end decode check against nrsc5's own sample capture.
//!
//! `vendor/nrsc5/support/sample.xz` is upstream's CI fixture: a few seconds of
//! FM HD Radio, decoded by upstream's CLI to prove a build works (their
//! workflow watches the log for the station text). Piping it through this
//! crate instead proves the whole chain here — the vendored library, the FFTW
//! stand-in and the patched faad2 link all at once.
//!
//! Defaults to ignored: it decompresses ~48 MB and decodes in real-ish time
//! per sample, so it is not part of the ordinary `cargo test` run. Run with
//! `cargo test -p sdroxide-nrsc5 --release -- --ignored --nocapture`. The `xz`
//! binary must be on PATH.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use num_complex::Complex32;

use sdroxide_nrsc5::{Event, HdReceiver, Mode};

#[test]
#[ignore = "decompresses 48 MB and decodes for several seconds"]
fn decode_sample_capture() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let sample = manifest.join("../../vendor/nrsc5/support/sample.xz");
    if !sample.exists() {
        eprintln!("skipping: {} missing (submodules not fetched?)", sample.display());
        return;
    }

    // xz -dc streams the CU8 I/Q directly into the pipe.
    let mut child = match Command::new("xz")
        .args(["-dc", sample.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping: cannot run xz ({e}); needs the binary on PATH");
            return;
        }
    };
    let mut stdin_off = child.stdout.take().expect("xz stdout");
    let rx = HdReceiver::open(Mode::Fm).expect("open HD Radio receiver");

    let mut buf = vec![0u8; 1 << 16];
    let mut fed = 0u64;
    let mut got = Summary::default();
    loop {
        match stdin_off.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                rx.pipe_cu8(&buf[..n]).expect("pipe samples");
                fed += n as u64;
                // Drain whatever decoded during this chunk so the channel
                // never balloons.
                for ev in rx.drain() {
                    got.absorb(ev);
                }
            }
            Err(e) => panic!("failed reading xz stream: {e}"),
        }
    }
    let status = child.wait().expect("xz exit");
    assert!(status.success(), "xz failed: {status:?}");

    // Let the tail of the capture drain, then tear the receiver down.
    std::thread::sleep(std::time::Duration::from_millis(500));
    for ev in rx.drain() {
        got.absorb(ev);
    }
    drop(rx);

    eprintln!("fed {fed} bytes of CU8 I/Q");
    eprintln!("{got:#?}");

    assert!(got.syncs > 0, "no sync event — the receive path never locked onto the capture");
    assert!(!got.text.is_empty(), "no station name/slogan/message was decoded from the capture");
    assert!(got.audio_frames > 0, "no decoded audio — HDC/faad2 path never produced samples");
    eprintln!("station text: {}", got.text.join(" | "));
}

/// The same capture through [`HdDemod`], the way the receive chain drives it:
/// channel I/Q in blocks, audio out paced to real time, status taken on a
/// timer. That puts the decoder thread, the queue into it and the audio queue
/// out of it under test, which the raw receiver above does not touch.
///
/// Fed at about twice real time — the demod drops what its queue cannot hold,
/// as it would on a machine that cannot keep up, and a faster feed would test
/// that instead.
#[test]
#[ignore = "decompresses 48 MB and decodes for several seconds"]
fn decode_sample_capture_through_the_demod() {
    use sdroxide_dsp::Demodulator;
    use sdroxide_nrsc5::HdDemod;

    /// `NRSC5_SAMPLE_RATE_CU8`: the capture is at twice the decoder's own rate.
    const CU8_RATE: f64 = 1_488_375.0;

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let sample = manifest.join("../../vendor/nrsc5/support/sample.xz");
    if !sample.exists() {
        eprintln!("skipping: {} missing (submodules not fetched?)", sample.display());
        return;
    }
    let Ok(mut child) = Command::new("xz")
        .args(["-dc", sample.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        eprintln!("skipping: xz is not on PATH");
        return;
    };
    let mut stream = child.stdout.take().expect("xz stdout");

    let mut demod = HdDemod::new(CU8_RATE);
    let mut bytes = vec![0u8; 1 << 16];
    let mut iq = Vec::new();
    let mut audio = Vec::new();
    let (mut locked, mut station, mut loud) = (false, String::new(), 0usize);
    let mut absorb = |demod: &mut HdDemod, audio: &[f32]| {
        loud += audio.iter().filter(|a| a.abs() > 1e-3).count();
        if let Some(st) = demod.take_hd_radio() {
            locked |= st.locked;
            if !st.station_name.is_empty() {
                station = st.station_name;
            }
        }
    };
    loop {
        let n = stream.read(&mut bytes).expect("read the xz stream");
        if n == 0 {
            break;
        }
        iq.clear();
        iq.extend(
            bytes[..n & !1].chunks_exact(2).map(|p| {
                Complex32::new((p[0] as f32 - 127.5) / 127.5, (p[1] as f32 - 127.5) / 127.5)
            }),
        );
        audio.clear();
        demod.process(&iq, &mut audio);
        absorb(&mut demod, &audio);
        let block_s = iq.len() as f64 / CU8_RATE;
        std::thread::sleep(Duration::from_secs_f64(block_s / 2.0));
    }
    assert!(child.wait().expect("xz exit").success());
    // The decoder is behind the feed by its own latency; keep the chain
    // running on silence while it finishes, as a receiver would.
    let silence = vec![Complex32::new(0.0, 0.0); 32_768];
    for _ in 0..60 {
        audio.clear();
        demod.process(&silence, &mut audio);
        absorb(&mut demod, &audio);
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(absorb);

    eprintln!("locked {locked}, station {station:?}, {loud} audible samples");
    assert!(locked, "the decoder thread never locked onto the capture");
    assert_eq!(station.trim(), "KUT", "the station name came back through the status");
    assert!(loud > 44_100, "at least a second of decoded audio reached the chain, got {loud}");
    assert_eq!(demod.backlog_drops(), 0, "the audio queue was drained at the rate it filled");
}

/// What the assertions look at, without holding on to the PCM.
#[derive(Debug, Default)]
struct Summary {
    syncs: usize,
    lost_syncs: usize,
    audio_frames: usize,
    audio_samples: usize,
    text: Vec<String>,
}

impl Summary {
    fn absorb(&mut self, ev: Event) {
        match ev {
            Event::Sync { .. } => self.syncs += 1,
            Event::LostSync => self.lost_syncs += 1,
            Event::Audio { data, .. } => {
                self.audio_frames += 1;
                self.audio_samples += data.len();
            }
            Event::StationName(s) | Event::StationSlogan(s) | Event::StationMessage(s) => {
                if self.text.last() != Some(&s) {
                    self.text.push(s);
                }
            }
            _ => {}
        }
    }
}
