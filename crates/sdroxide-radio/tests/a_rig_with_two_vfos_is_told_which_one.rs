//! A front end that has its own pair of VFOs is told which one is in use.
//!
//! Only a handful of radios have a second VFO to select — an ELAD FDM-DUO is
//! the first — but the two things this pins are not ELAD's:
//!
//! * the source hears about the switch at all, with the frequency beside it,
//!   so selecting a VFO holding a stale number is one command wide rather than
//!   lasting until the next retune;
//! * the number is the **rig's**, not the dial. In CW a radio that keys its own
//!   transmitter sits a sidetone above the dial, which is how `follow_dial`
//!   commands it. Handing over the bare dial puts such a rig one pitch low, and
//!   on a rig whose dial is read back and believed the error compounds: the
//!   frequency walks down by one pitch on every switch. That was measured on an
//!   FDM-DUO before this test existed — 1200.0, then 1199.3, then 1198.6 kHz.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use sdroxide_radio::{Complex32, EngineConfig, IqSource, Result, start_engine};
use sdroxide_types::{Command, DeviceCaps, Mode, RxId, Vfo};

const RATE: f64 = 192_000.0;
const PITCH: f64 = 700.0;

/// A rig whose window is its VFO and which keys its own transmitter — the
/// shape that makes the CW offset apply, as on the FDM-DUO.
#[derive(Clone)]
struct RigWithTwoVfos {
    center: f64,
    told: Arc<Mutex<Vec<(Vfo, f64)>>>,
}

impl IqSource for RigWithTwoVfos {
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_hz(&self) -> f64 {
        self.center
    }
    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.center = hz;
        Ok(())
    }
    fn center_is_dial(&self) -> bool {
        true
    }
    fn cw_iq_on_vfo(&self) -> bool {
        true
    }
    fn select_vfo(&mut self, vfo: Vfo, hz: f64) {
        self.told.lock().unwrap().push((vfo, hz));
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        std::thread::sleep(Duration::from_millis(5));
        let n = buf.len().min(2048);
        buf[..n].fill(Complex32::new(0.0, 0.0));
        Ok(n)
    }
    fn describe(&self) -> String {
        "mock rig with two VFOs".into()
    }
}

fn isolate_config() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let root = std::env::temp_dir().join(format!("sdroxide-rig-vfo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        unsafe { std::env::set_var("SDROXIDE_CONFIG_DIR", &root) };
    });
}

fn caps() -> DeviceCaps {
    DeviceCaps {
        driver: "mock".into(),
        label: "mock".into(),
        rx_channels: 1,
        sample_rates: vec![RATE],
        freq_ranges_rx: vec![(0.0, 1_000_000_000.0)],
        ..DeviceCaps::default()
    }
}

/// Run `cmds` and report every `select_vfo` the source was given.
fn told_after(cmds: &[Command]) -> Vec<(Vfo, f64)> {
    isolate_config();
    let told = Arc::new(Mutex::new(Vec::new()));
    let mut h = start_engine(
        Box::new(RigWithTwoVfos { center: 940_000.0, told: Arc::clone(&told) }),
        caps(),
        EngineConfig { tx_ham_only: false, ..Default::default() },
    );
    let thread = h.thread.take();
    std::thread::sleep(Duration::from_millis(150));
    for c in cmds {
        h.cmd_tx.send(c.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(80));
    }
    std::thread::sleep(Duration::from_millis(150));
    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
    let out = told.lock().unwrap().clone();
    out
}

#[test]
fn taking_up_the_other_vfo_tells_the_rig_which_one_and_where() {
    let told = told_after(&[
        Command::SetVfo { vfo: Vfo::A, hz: 940_000.0 },
        Command::SetMode { rx: RxId::Main, mode: Mode::Am },
        Command::SelectVfo(Vfo::B),
        Command::SetVfo { vfo: Vfo::B, hz: 1_200_000.0 },
        Command::SelectVfo(Vfo::A),
    ]);
    let vfos: Vec<Vfo> = told.iter().map(|(v, _)| *v).collect();
    assert_eq!(vfos, vec![Vfo::B, Vfo::A], "each switch is announced once, in order");
    let (_, back_on_a) = told.last().expect("a switch back to A");
    assert!((back_on_a - 940_000.0).abs() < 1.0, "A is on 940 kHz, got {back_on_a}");
}

/// The regression: in CW the rig is told the frequency it must sit on, which is
/// one sidetone above the dial, so a dial read back from the rig means the same
/// thing on the way in as on the way out.
#[test]
fn a_cw_vfo_is_handed_the_frequency_the_rig_must_sit_on() {
    let told = told_after(&[
        Command::SetVfo { vfo: Vfo::A, hz: 940_000.0 },
        Command::SetMode { rx: RxId::Main, mode: Mode::Am },
        Command::SelectVfo(Vfo::B),
        Command::SetVfo { vfo: Vfo::B, hz: 1_200_000.0 },
        Command::SetMode { rx: RxId::Main, mode: Mode::Cw },
        Command::SelectVfo(Vfo::A),
        Command::SelectVfo(Vfo::B),
    ]);
    let (vfo, hz) = *told.last().expect("a switch onto the CW VFO");
    assert_eq!(vfo, Vfo::B);
    assert!(
        (hz - (1_200_000.0 + PITCH)).abs() < 1.0,
        "a CW VFO on 1200 kHz puts the rig on 1200 kHz + one pitch, got {hz}"
    );

    // And the AM VFO is still handed its bare dial: the offset belongs to CW,
    // not to switching.
    let (vfo, hz) = told[told.len() - 2];
    assert_eq!(vfo, Vfo::A);
    assert!((hz - 940_000.0).abs() < 1.0, "an AM VFO takes no sidetone step, got {hz}");
}
