//! Station profiles through the engine (issue #197).
//!
//! The store itself is plain config; what only the engine can show is that a
//! profile put back on actually moves the station to where it was saved — in
//! particular that a profile saved while VFO B was in use comes back on B's
//! dial and not A's.

use std::time::{Duration, Instant};

use sdroxide_radio::{
    AudioParams, Complex32, EngineConfig, EngineHandles, IqSource, Result, rtrb, start_engine,
};
use sdroxide_types::{Command, DeviceCaps, Mode, RadioEvent, RadioState, RxId, Vfo};

const A_HZ: f64 = 14_074_000.0;
const B_HZ: f64 = 7_100_000.0;
const MOVED_A_HZ: f64 = 14_200_000.0;

static CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Quiet;

impl IqSource for Quiet {
    fn sample_rate(&self) -> f64 {
        48_000.0
    }
    fn center_hz(&self) -> f64 {
        A_HZ
    }
    fn set_center_hz(&mut self, _hz: f64) -> Result<()> {
        Ok(())
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        std::thread::sleep(Duration::from_millis(5));
        let n = buf.len().min(480);
        for z in buf[..n].iter_mut() {
            *z = Complex32::default();
        }
        Ok(n)
    }
    fn describe(&self) -> String {
        "quiet mock".into()
    }
}

fn caps() -> DeviceCaps {
    DeviceCaps {
        driver: "mock".into(),
        label: "mock".into(),
        rx_channels: 1,
        sample_rates: vec![48_000.0],
        freq_ranges_rx: vec![(1_000_000.0, 30_000_000.0)],
        ..DeviceCaps::default()
    }
}

fn isolate(name: &str) {
    let root = std::env::temp_dir().join(format!("sdroxide-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    unsafe { std::env::set_var("SDROXIDE_CONFIG_DIR", &root) };
}

fn start() -> EngineHandles {
    let (producer, _consumer) = rtrb::RingBuffer::<f32>::new(48_000);
    start_engine(
        Box::new(Quiet),
        caps(),
        EngineConfig {
            audio: Some(AudioParams { producer, out_rate: 48_000.0 }),
            initial_mode: Some(Mode::Usb),
            remember_session: true,
            ..Default::default()
        },
    )
}

fn stop(mut h: EngineHandles) {
    let thread = h.thread.take();
    drop(h);
    if let Some(t) = thread {
        let _ = t.join();
    }
}

fn send(h: &EngineHandles, c: Command) {
    h.cmd_tx.send(c).unwrap();
}

fn wait_for(h: &EngineHandles, what: &str, f: impl Fn(&RadioState) -> bool) -> RadioState {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last: Option<RadioState> = None;
    while Instant::now() < deadline {
        while let Ok(ev) = h.event_rx.try_recv() {
            if let RadioEvent::State(s) = ev {
                if f(&s) {
                    return s;
                }
                last = Some(s);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the state never showed {what}; last: {:?}", last.map(|s| (s.active_vfo, s.active_freq_hz())));
}

/// The profile list announcement, which is how the engine answers a save.
fn wait_saved(h: &EngineHandles, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        while let Ok(ev) = h.event_rx.try_recv() {
            if let RadioEvent::Profiles(names) = ev {
                if names.iter().any(|n| n == name) {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the profile list never carried {name}");
}

#[test]
fn a_profile_saved_on_vfo_b_comes_back_on_vfo_b() {
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    isolate("profiles-vfo-b");
    let h = start();

    // Put B where it should be and work from it.
    send(&h, Command::SetVfo { vfo: Vfo::B, hz: B_HZ });
    send(&h, Command::SelectVfo(Vfo::B));
    let _ = wait_for(&h, "VFO B in use", |s| s.active_vfo == Vfo::B);
    send(&h, Command::ProfileSave("b-profile".into()));
    wait_saved(&h, "b-profile");

    // Leave: A moved and in use.
    send(&h, Command::SetVfo { vfo: Vfo::A, hz: MOVED_A_HZ });
    send(&h, Command::SelectVfo(Vfo::A));
    let _ = wait_for(&h, "VFO A in use", |s| s.active_vfo == Vfo::A);

    // Apply: the station goes back to B, on B's dial — not A's.
    send(&h, Command::ProfileApply("b-profile".into()));
    let s = wait_for(&h, "the profile applied", |s| {
        s.active_vfo == Vfo::B && (s.active_freq_hz() - B_HZ).abs() < 1.0
    });
    assert!(
        (s.active_freq_hz() - B_HZ).abs() < 1.0,
        "B was saved at {B_HZ}, came back at {}",
        s.active_freq_hz()
    );
    assert_ne!(s.active_freq_hz(), MOVED_A_HZ);

    stop(h);
}
