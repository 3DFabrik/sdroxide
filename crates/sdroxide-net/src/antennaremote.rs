//! Client for the Antenna Remote antenna switch.
//!
//! The switch is the server and this is the client: it dials the switch,
//! tells it where the radio's dial is, and reads back which antenna the switch
//! picked. The protocol is documented with [`AntennaRemoteConfig`] — plain
//! text lines in the manner of `rotctld`.
//!
//! The shape follows the rotator client: one worker thread owns the socket,
//! the engine talks to it through a small channel and reads its health back
//! from a shared status, and a connection that drops is retried with backoff
//! rather than treated as fatal.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use sdroxide_types::{AntennaRemoteConfig, AntennaRemoteStatus};
use tracing::{debug, warn};

/// What the switch must answer to the first `v`, so an address that is some
/// other service is refused instead of being fed frequencies.
const GREETING: &str = "AntennaRemote";

const RECONNECT_FIRST: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(5);
/// How often the switch is asked what it has selected. Also the keep-alive.
const STATUS_POLL: Duration = Duration::from_secs(1);
/// A frequency that keeps changing — a dial being spun — is sent at most this
/// often. The last value is always sent once it stops.
const MIN_SEND_GAP: Duration = Duration::from_millis(150);
const TICK: Duration = Duration::from_millis(100);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REPLY_TIMEOUT: Duration = Duration::from_secs(4);

enum Cmd {
    Freq(u64),
    Auto(bool),
    Antenna(u8),
}

/// What a refusal's message starts with, so a refused request can be told from
/// a connection that broke.
const REFUSED: &str = "the switch refused";

/// Handle to the worker thread. Dropping it closes the channel, which is the
/// worker's signal to stop; the drop joins the thread so a socket mid-write
/// cannot outlive the engine.
pub struct AntennaRemoteClient {
    tx: Option<Sender<Cmd>>,
    status: Arc<Mutex<AntennaRemoteStatus>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl AntennaRemoteClient {
    pub fn start(cfg: AntennaRemoteConfig) -> AntennaRemoteClient {
        let (tx, rx) = bounded(16);
        let status = Arc::new(Mutex::new(AntennaRemoteStatus::default()));
        let st = status.clone();
        let join = std::thread::Builder::new()
            .name("sdroxide-antenna-remote".into())
            .spawn(move || worker(cfg, rx, st))
            .map_err(|e| warn!("could not start the Antenna Remote client: {e}"))
            .ok();
        AntennaRemoteClient { tx: Some(tx), status, join }
    }

    /// Say where the radio is, in hertz. Safe to call on every engine tick:
    /// the worker only forwards a value that differs from the last one sent.
    pub fn set_freq(&self, hz: u64) {
        if let Some(tx) = &self.tx {
            // A full queue means the worker is busy reconnecting; a fresher
            // value follows on the next tick.
            let _ = tx.try_send(Cmd::Freq(hz));
        }
    }

    /// Switch the switch's automatic mode. Sent once, and only if the switch is
    /// connected when the worker gets to it: the next status poll shows whether
    /// it took.
    pub fn set_auto(&self, on: bool) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(Cmd::Auto(on));
        }
    }

    /// Select antenna `n` (from 1) on the switch, which also takes it out of
    /// automatic mode. Sent once, like [`Self::set_auto`].
    pub fn set_antenna(&self, n: u8) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(Cmd::Antenna(n));
        }
    }

    pub fn status(&self) -> AntennaRemoteStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl Drop for AntennaRemoteClient {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn set(status: &Mutex<AntennaRemoteStatus>, f: impl FnOnce(&mut AntennaRemoteStatus)) {
    if let Ok(mut s) = status.lock() {
        f(&mut s);
    }
}

fn worker(cfg: AntennaRemoteConfig, rx: Receiver<Cmd>, status: Arc<Mutex<AntennaRemoteStatus>>) {
    let mut conn: Option<Conn> = None;
    let mut retry_at = Instant::now();
    let mut retry_every = RECONNECT_FIRST;
    let mut want: Option<u64> = None;
    let mut want_auto: Option<bool> = None;
    let mut want_antenna: Option<u8> = None;
    // What the switch was last told on *this* connection: a fresh connection
    // starts from nothing, so the current frequency goes out straight away.
    let mut sent: Option<u64> = None;
    let mut last_send = Instant::now() - MIN_SEND_GAP;
    let mut next_poll = Instant::now();

    loop {
        if conn.is_none() && Instant::now() >= retry_at {
            match Conn::open(&cfg) {
                Ok(c) => {
                    debug!("Antenna Remote connected: {}", cfg.address());
                    retry_every = RECONNECT_FIRST;
                    sent = None;
                    next_poll = Instant::now();
                    set(&status, |s| {
                        s.connected = true;
                        s.error = None;
                    });
                    conn = Some(c);
                }
                Err(e) => {
                    set(&status, |s| *s = AntennaRemoteStatus { error: Some(e), ..Default::default() });
                    retry_at = Instant::now() + retry_every;
                    retry_every = (retry_every * 2).min(RECONNECT_MAX);
                }
            }
        }

        match rx.recv_timeout(TICK) {
            Ok(Cmd::Freq(hz)) => want = Some(hz),
            Ok(Cmd::Auto(on)) => want_auto = Some(on),
            Ok(Cmd::Antenna(n)) => want_antenna = Some(n),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Cmd::Freq(hz) => want = Some(hz),
                Cmd::Auto(on) => want_auto = Some(on),
                Cmd::Antenna(n) => want_antenna = Some(n),
            }
        }
        let Some(c) = conn.as_mut() else {
            // A switch that is not there cannot be told anything, and the
            // request must not wait around to surprise the next connection.
            want_auto = None;
            want_antenna = None;
            continue;
        };

        let now = Instant::now();
        let r = (|| -> Result<(), String> {
            if let Some(hz) = want.filter(|hz| Some(*hz) != sent)
                && now.duration_since(last_send) >= MIN_SEND_GAP
            {
                c.send_freq(hz)?;
                sent = Some(hz);
                last_send = now;
                // The answer to a retune is worth having at once.
                next_poll = now;
            }
            if let Some(on) = want_auto.take() {
                c.send_auto(on)?;
                next_poll = now;
            }
            if let Some(n) = want_antenna.take() {
                c.send_antenna(n)?;
                next_poll = now;
            }
            if now >= next_poll {
                next_poll = now + STATUS_POLL;
                let st = c.status()?;
                set(&status, |s| {
                    s.antenna = st.antenna;
                    s.auto = st.auto;
                    s.name = st.name;
                    s.band = st.band;
                    s.count = st.count;
                });
            }
            Ok(())
        })();
        if let Err(e) = r {
            warn!("Antenna Remote: {e}");
            set(&status, |s| *s = AntennaRemoteStatus { error: Some(e), ..Default::default() });
            conn = None;
            retry_at = Instant::now() + RECONNECT_FIRST;
            retry_every = RECONNECT_FIRST;
        }
    }
}

struct Conn {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Conn {
    fn open(cfg: &AntennaRemoteConfig) -> Result<Conn, String> {
        if !cfg.has_host() {
            return Err("no address set for the switch".into());
        }
        let addr = cfg.address();
        let sock = addr
            .to_socket_addrs()
            .map_err(|e| format!("{addr}: {e}"))?
            .next()
            .ok_or_else(|| format!("{addr}: no address"))?;
        let stream = TcpStream::connect_timeout(&sock, CONNECT_TIMEOUT)
            .map_err(|e| format!("{addr}: {e}"))?;
        let _ = stream.set_read_timeout(Some(REPLY_TIMEOUT));
        let _ = stream.set_write_timeout(Some(REPLY_TIMEOUT));
        let _ = stream.set_nodelay(true);
        let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut c = Conn { stream, reader };
        let body = c.command("v").map_err(|e| format!("{addr}: {e}"))?;
        if !body.first().is_some_and(|l| l.starts_with(GREETING)) {
            return Err(format!("{addr} is not an Antenna Remote switch"));
        }
        Ok(c)
    }

    /// Send one request and read its reply: the lines before the closing
    /// `RPRT n`, or an error when `n` is not zero.
    fn command(&mut self, line: &str) -> Result<Vec<String>, String> {
        self.stream.write_all(format!("{line}\n").as_bytes()).map_err(|e| e.to_string())?;
        let mut body = Vec::new();
        for _ in 0..16 {
            let mut l = String::new();
            match self.reader.read_line(&mut l) {
                Ok(0) => return Err("the switch closed the connection (is it busy?)".into()),
                Ok(_) => {}
                Err(e) => return Err(format!("no answer from the switch: {e}")),
            }
            let l = l.trim_end().to_string();
            if let Some(code) = l.strip_prefix("RPRT ") {
                return match code.trim().parse::<i32>() {
                    Ok(0) => Ok(body),
                    Ok(n) => Err(format!("{REFUSED} `{line}` (RPRT {n})")),
                    Err(_) => Err(format!("unreadable reply {l:?}")),
                };
            }
            body.push(l);
        }
        Err("the switch's reply did not end".into())
    }

    fn send_freq(&mut self, hz: u64) -> Result<(), String> {
        self.command(&format!("F {hz}")).map(|_| ())
    }

    /// A switch whose firmware predates `M` refuses it; that is no reason to
    /// drop a connection that is otherwise doing its job.
    fn send_auto(&mut self, on: bool) -> Result<(), String> {
        self.refusable(&format!("M {}", u8::from(on)))
    }

    fn send_antenna(&mut self, n: u8) -> Result<(), String> {
        self.refusable(&format!("A {n}"))
    }

    /// A request the switch may decline — an older firmware, or an antenna it
    /// does not have — without that being a reason to drop the connection.
    fn refusable(&mut self, line: &str) -> Result<(), String> {
        match self.command(line) {
            Err(e) if e.starts_with(REFUSED) => {
                warn!("Antenna Remote: {e}");
                Ok(())
            }
            r => r.map(|_| ()),
        }
    }

    fn status(&mut self) -> Result<AntennaRemoteStatus, String> {
        let body = self.command("s")?;
        body.iter()
            .find_map(|l| AntennaRemoteStatus::parse_line(l))
            .ok_or_else(|| "the switch's status reply had no antenna in it".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// A switch that speaks the protocol, on a loopback port. Reports every
    /// `F` it is sent.
    fn fake_switch(greeting: &'static str) -> (u16, mpsc::Receiver<u64>) {
        let (port, freq, _auto, _ant) = fake_switch_with(greeting, true);
        (port, freq)
    }

    /// The same, also reporting every `M` and every `A` it accepts (it has four
    /// antennas). With `knows_m` false it answers both the way a firmware that
    /// predates them does: `RPRT -1`.
    fn fake_switch_with(
        greeting: &'static str,
        knows_m: bool,
    ) -> (u16, mpsc::Receiver<u64>, mpsc::Receiver<bool>, mpsc::Receiver<u8>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        let (auto_tx, auto_rx) = mpsc::channel();
        let (ant_tx, ant_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else { return };
            let mut out = stream.try_clone().unwrap();
            let mut auto = true;
            let mut ant = 3u8;
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { return };
                let reply = match line.split_whitespace().next() {
                    Some("v") => format!("{greeting}\nRPRT 0\n"),
                    Some("F") => {
                        let hz = line[1..].trim().parse().unwrap_or(0);
                        let _ = tx.send(hz);
                        "RPRT 0\n".to_string()
                    }
                    Some("M") if knows_m => {
                        auto = line[1..].trim() == "1";
                        let _ = auto_tx.send(auto);
                        "RPRT 0\n".to_string()
                    }
                    Some("A") if knows_m => match line[1..].trim().parse::<u8>() {
                        Ok(n) if (1..=4).contains(&n) => {
                            ant = n;
                            auto = false;
                            let _ = ant_tx.send(n);
                            "RPRT 0\n".to_string()
                        }
                        _ => "RPRT -1\n".to_string(),
                    },
                    Some("s") => format!(
                        "ant={ant} auto={} band=20 n=4 name=Dipole 20 m\nRPRT 0\n",
                        u8::from(auto)
                    ),
                    _ => "RPRT -1\n".to_string(),
                };
                if out.write_all(reply.as_bytes()).is_err() {
                    return;
                }
            }
        });
        (port, rx, auto_rx, ant_rx)
    }

    fn cfg(port: u16) -> AntennaRemoteConfig {
        AntennaRemoteConfig { enabled: true, host: "127.0.0.1".into(), port }
    }

    fn wait_for(client: &AntennaRemoteClient, ok: impl Fn(&AntennaRemoteStatus) -> bool) -> bool {
        for _ in 0..60 {
            if ok(&client.status()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn the_frequency_goes_out_and_the_selected_antenna_comes_back() {
        let (port, got) = fake_switch("AntennaRemote 1");
        let client = AntennaRemoteClient::start(cfg(port));
        client.set_freq(14_074_000);
        assert_eq!(got.recv_timeout(Duration::from_secs(3)).unwrap(), 14_074_000);
        assert!(wait_for(&client, |s| s.connected && s.antenna == 3), "{:?}", client.status());
        let s = client.status();
        assert!(s.auto && s.band == 20 && s.name == "Dipole 20 m", "{s:?}");
        // An unchanged frequency is not sent again.
        client.set_freq(14_074_000);
        assert!(got.recv_timeout(Duration::from_millis(400)).is_err());
    }

    #[test]
    fn the_automatic_switch_goes_out_and_the_answer_comes_back() {
        let (port, _freq, autos, _ants) = fake_switch_with("AntennaRemote 1", true);
        let client = AntennaRemoteClient::start(cfg(port));
        assert!(wait_for(&client, |s| s.connected && s.auto), "{:?}", client.status());
        client.set_auto(false);
        assert!(!autos.recv_timeout(Duration::from_secs(3)).unwrap());
        assert!(wait_for(&client, |s| s.connected && !s.auto), "{:?}", client.status());
        client.set_auto(true);
        assert!(autos.recv_timeout(Duration::from_secs(3)).unwrap());
        assert!(wait_for(&client, |s| s.auto), "{:?}", client.status());
    }

    #[test]
    fn an_antenna_is_selected_and_the_switch_goes_manual() {
        let (port, _freq, _autos, ants) = fake_switch_with("AntennaRemote 1", true);
        let client = AntennaRemoteClient::start(cfg(port));
        assert!(wait_for(&client, |s| s.connected && s.auto && s.count == 4), "{:?}", client.status());
        client.set_antenna(2);
        assert_eq!(ants.recv_timeout(Duration::from_secs(3)).unwrap(), 2);
        assert!(wait_for(&client, |s| s.antenna == 2 && !s.auto), "{:?}", client.status());
        // An antenna the switch does not have is refused, and the connection stays.
        client.set_antenna(9);
        std::thread::sleep(Duration::from_millis(1300));
        let s = client.status();
        assert!(s.connected && s.antenna == 2, "{s:?}");
    }

    #[test]
    fn a_firmware_without_the_automatic_switch_keeps_its_connection() {
        let (port, _freq, _autos, _ants) = fake_switch_with("AntennaRemote 1", false);
        let client = AntennaRemoteClient::start(cfg(port));
        assert!(wait_for(&client, |s| s.connected), "{:?}", client.status());
        client.set_auto(false);
        // Longer than a status poll: had the refusal dropped the connection, the
        // fake (which serves one connection) would be gone and the status with it.
        std::thread::sleep(Duration::from_millis(1500));
        let s = client.status();
        assert!(s.connected && s.error.is_none() && s.antenna == 3, "{s:?}");
    }

    #[test]
    fn some_other_service_is_refused() {
        let (port, _got) = fake_switch("SSH-2.0-OpenSSH");
        let client = AntennaRemoteClient::start(cfg(port));
        assert!(
            wait_for(&client, |s| s.error.as_deref().is_some_and(|e| e.contains("not an Antenna Remote"))),
            "{:?}",
            client.status()
        );
        assert!(!client.status().connected);
    }

    #[test]
    fn nothing_listening_is_reported_as_an_error_not_a_hang() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let client = AntennaRemoteClient::start(cfg(port));
        assert!(wait_for(&client, |s| s.error.is_some()), "{:?}", client.status());
        assert!(!client.status().connected);
    }

    #[test]
    fn an_empty_address_says_so() {
        let client =
            AntennaRemoteClient::start(AntennaRemoteConfig { enabled: true, ..Default::default() });
        assert!(
            wait_for(&client, |s| s.error.as_deref().is_some_and(|e| e.contains("no address"))),
            "{:?}",
            client.status()
        );
    }
}
