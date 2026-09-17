//! End-to-end decode check against nrsc5's own sample capture.
//!
//! `vendor/nrsc5/support/sample.xz` is upstream's CI fixture: a few seconds of
//! FM HD Radio, decoded by upstream's CLI to prove a build works (their
//! workflow watches the log for the station text). Piping it through this
//! crate instead proves the whole chain here — the vendored library, the FFTW
//! stand-in and the combined faad2 link all at once.
//!
//! Defaults to ignored: it decompresses ~48 MB and decodes in real-ish time
//! per sample, so it is not part of the ordinary `cargo test` run. Run with
//! `cargo test -p sdroxide-nrsc5 --release -- --ignored --nocapture`. The `xz`
//! binary must be on PATH.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use sdroxide_nrsc5::{Event, HdReceiver, Mode};

// The HDC decode calls resolve to the single DRM+HDC faad2 archive that
// `sdroxide-drm` builds; keeping its rlib in the link forwards that archive.
use sdroxide_drm as _;

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

    assert!(
        got.syncs > 0,
        "no sync event — the receive path never locked onto the capture"
    );
    assert!(
        !got.text.is_empty(),
        "no station name/slogan/message was decoded from the capture"
    );
    assert!(
        got.audio_frames > 0,
        "no decoded audio — HDC/faad2 path never produced samples"
    );
    eprintln!("station text: {}", got.text.join(" | "));
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