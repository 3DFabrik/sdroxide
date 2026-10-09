//! The Antenna Remote switch: an ESP32 antenna switch on the station's network.
//!
//! sdroxide tells it where the dial is, and it picks the antenna. The switch
//! keeps its own band-to-antenna table and its own automatic/manual mode, so
//! this side only has to say "the radio is on 14.074 MHz" and read back what
//! the switch chose.
//!
//! # The wire protocol
//!
//! Plain text over TCP, one command per line, in the manner of Hamlib's
//! `rotctld` so that `nc switch 4540` is enough to try it by hand. The switch
//! is the server; every reply ends in an `RPRT` line, `RPRT 0` for success.
//!
//! | Request    | Reply                                          |
//! |------------|------------------------------------------------|
//! | `v`        | `AntennaRemote 1` then `RPRT 0`                |
//! | `F <hz>`   | `RPRT 0` — the radio's frequency in whole hertz |
//! | `s`        | `ant=<1-8> auto=<0\|1> band=<m> n=<count> name=<text>` then `RPRT 0` |
//! | `M <0\|1>`  | `RPRT 0` — automatic mode off or on                |
//! | `A <n>`    | `RPRT 0` — selects antenna `n` and switches to manual |
//!
//! `name=` is last on its line because an antenna's name may contain spaces.
//! `band` is `0` when the frequency is outside every band, and `n=` is how many
//! antennas the switch has (a firmware that predates it leaves it out). A refusal
//! is `RPRT -1`, which is also what a firmware that predates `M` or `A` answers;
//! sdroxide keeps the connection in that case.
//!
//! sdroxide sends `v` once after connecting, `F` whenever the frequency has
//! settled on a new value, `M` and `A` when the operator asks, and `s` once a
//! second — which doubles as the keep-alive, so the switch can fall back to a
//! safe antenna when it stops arriving.

use serde::{Deserialize, Serialize};

/// Where the switch is, and whether this radio drives it.
///
/// Per radio, like the servers' configuration: each radio decides for itself
/// whether its dial steers the switch. The switch serves one client at a
/// time, so two radios both ticked would fight over it and the second is told
/// the switch is busy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AntennaRemoteConfig {
    /// "Use Antenna Remote". Off, nothing connects anywhere.
    pub enabled: bool,
    /// The switch's address on the local network.
    pub host: String,
    pub port: u16,
}

impl AntennaRemoteConfig {
    /// The port the protocol above is served on.
    pub const DEFAULT_PORT: u16 = 4540;

    /// What to dial, as one string.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host.trim(), self.port)
    }

    /// Whether there is anywhere to connect to.
    pub fn has_host(&self) -> bool {
        !self.host.trim().is_empty()
    }
}

impl Default for AntennaRemoteConfig {
    fn default() -> Self {
        AntennaRemoteConfig { enabled: false, host: String::new(), port: Self::DEFAULT_PORT }
    }
}

/// What the switch last said about itself.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AntennaRemoteStatus {
    /// Whether the connection is up and the switch is answering.
    pub connected: bool,
    /// The antenna it has selected, from 1; `0` until it has said.
    pub antenna: u8,
    /// Whether the switch is following the frequency by itself.
    pub auto: bool,
    /// The name the switch gives that antenna.
    pub name: String,
    /// The band the switch put the frequency in, in metres; `0` for none.
    pub band: u16,
    /// Why it is not connected, when it is not.
    pub error: Option<String>,
    /// How many antennas the switch has, from its `n=`; `0` when the firmware does
    /// not say. Appended last, so it is the tail of the struct on the wire.
    pub count: u8,
}

impl AntennaRemoteStatus {
    /// Read an `s` reply line: `ant=3 auto=1 band=20 n=8 name=Dipole 20 m`.
    ///
    /// Unknown keys are skipped, so a later firmware can say more. `None` when
    /// the line carries no antenna number at all, which is not a status.
    pub fn parse_line(line: &str) -> Option<AntennaRemoteStatus> {
        let (head, name) = match line.find("name=") {
            Some(i) => (&line[..i], line[i + 5..].trim()),
            None => (line, ""),
        };
        let mut st = AntennaRemoteStatus { name: name.to_string(), ..Default::default() };
        let mut found = false;
        for tok in head.split_whitespace() {
            let Some((k, v)) = tok.split_once('=') else { continue };
            match k {
                "ant" => {
                    st.antenna = v.parse().ok()?;
                    found = true;
                }
                "auto" => st.auto = v == "1",
                "band" => st.band = v.parse().unwrap_or(0),
                "n" => st.count = v.parse().unwrap_or(0),
                _ => {}
            }
        }
        found.then_some(st)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_line_is_read_with_a_name_containing_spaces() {
        let s = AntennaRemoteStatus::parse_line("ant=3 auto=1 band=20 name=Dipole 20 m").unwrap();
        assert_eq!((s.antenna, s.auto, s.band), (3, true, 20));
        assert_eq!(s.name, "Dipole 20 m");
    }

    #[test]
    fn a_line_without_an_antenna_is_not_a_status() {
        assert_eq!(AntennaRemoteStatus::parse_line("auto=1 band=20"), None);
        assert_eq!(AntennaRemoteStatus::parse_line("RPRT 0"), None);
        assert_eq!(AntennaRemoteStatus::parse_line("ant=x"), None);
    }

    #[test]
    fn the_antenna_count_is_read_when_the_firmware_gives_one() {
        let s = AntennaRemoteStatus::parse_line("ant=2 auto=0 band=40 n=4 name=City Windom").unwrap();
        assert_eq!((s.antenna, s.count), (2, 4));
        assert_eq!(s.name, "City Windom");
        let old = AntennaRemoteStatus::parse_line("ant=2 auto=0 band=40 name=City Windom").unwrap();
        assert_eq!(old.count, 0);
    }

    #[test]
    fn unknown_keys_and_a_missing_name_are_tolerated() {
        let s = AntennaRemoteStatus::parse_line("ant=8 auto=0 band=0 tune=1").unwrap();
        assert_eq!((s.antenna, s.auto, s.band), (8, false, 0));
        assert!(s.name.is_empty());
    }
}
