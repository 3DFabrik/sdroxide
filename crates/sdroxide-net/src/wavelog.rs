//! Wavelog (and Cloudlog) integration: QSO upload through `api/qso`, a key
//! check, and the radio interface (`api/radio`) that tells the logger what the
//! dial is on so its entry form follows the radio.
//!
//! Wavelog's v1 API is JSON over POST with the key inside the document, not in a
//! header. Cloudlog, which Wavelog forked from, speaks the same three endpoints.

use std::time::{Duration, Instant};

use sdroxide_types::{Mode, NetworkConfig};
use serde_json::{Value, json};

use crate::http;
use crate::upload::urlencode;

/// `{base}/api/{endpoint}` from what the operator typed.
///
/// Plain `http://` is allowed on purpose: Wavelog is commonly self-hosted on a
/// LAN without a certificate.
fn api_url(base: &str, endpoint: &str) -> Result<String, String> {
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("Wavelog URL not set".into());
    }
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return Err("Wavelog URL must start with http:// or https://".into());
    }
    Ok(format!("{base}/api/{endpoint}"))
}

/// What Wavelog said was wrong, from whichever of its error shapes is present.
fn reason(status: u16, body: &str) -> String {
    let text = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        if let Some(r) = v.get("reason").and_then(Value::as_str) {
            return Some(r.to_string());
        }
        if let Some(m) = v.get("message").and_then(Value::as_str) {
            return Some(m.to_string());
        }
        let msgs: Vec<&str> = v
            .get("messages")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .collect();
        (!msgs.is_empty()).then(|| msgs.join("; "))
    });
    match text {
        Some(t) => t.chars().take(200).collect(),
        None => format!("HTTP {status}"),
    }
}

fn key(cfg: &NetworkConfig) -> Result<&str, String> {
    let k = cfg.wavelog_api_key.trim();
    if k.is_empty() { Err("API key not set".into()) } else { Ok(k) }
}

/// Send one QSO's ADIF to the configured station profile.
pub fn upload(cfg: &NetworkConfig, adif: &str) -> Result<String, String> {
    let key = key(cfg).map_err(|e| format!("Wavelog: {e}"))?;
    let station = cfg.wavelog_station_id.trim();
    if station.is_empty() || station.parse::<u32>().is_err() {
        return Err("Wavelog: station ID not set (it is the number in the station profile's URL)".into());
    }
    let url = api_url(&cfg.wavelog_url, "qso").map_err(|e| format!("Wavelog: {e}"))?;
    let body = json!({
        "key": key,
        "station_profile_id": station,
        "type": "adif",
        "string": adif,
    })
    .to_string();
    let (status, reply) = http::post_json_plain_status(&url, &body)?;
    let parsed: Option<Value> = serde_json::from_str(&reply).ok();
    let created = parsed.as_ref().and_then(|v| v.get("status")).and_then(Value::as_str)
        == Some("created");
    if (status == 200 || status == 201) && created {
        let count = parsed.as_ref().and_then(|v| v.get("adif_count")).and_then(Value::as_u64);
        return match count {
            Some(0) => Err("Wavelog: nothing was imported".into()),
            _ => Ok("Wavelog: logged".into()),
        };
    }
    Err(match status {
        401 => format!("Wavelog: {}", reason(status, &reply)),
        403 => "Wavelog: the API key has no write permission — create a read+write key".into(),
        _ => format!("Wavelog: {}", reason(status, &reply)),
    })
}

/// Check the key (and station ID) with read-only calls; logs nothing.
///
/// `api/auth/{key}` exists in both Wavelog and Cloudlog and says whether the
/// key is read-only, which matters because both uploading and the radio
/// interface need write rights.
pub fn test(cfg: &NetworkConfig) -> Result<String, String> {
    let key = key(cfg)?;
    let (_, xml) = http::get_status(&api_url(&cfg.wavelog_url, &format!("auth/{}", urlencode(key)))?)?;
    let doc = roxmltree::Document::parse(&xml)
        .map_err(|_| "unexpected reply — is the URL the Wavelog base address?".to_string())?;
    let text_of = |tag: &str| {
        doc.descendants()
            .find(|n| n.has_tag_name(tag))
            .and_then(|n| n.text())
            .map(|t| t.trim().to_string())
    };
    if text_of("status").as_deref() != Some("Valid") {
        return Err(text_of("message").unwrap_or_else(|| "key rejected".into()));
    }
    if text_of("rights").as_deref() == Some("r") {
        return Err("key is read-only — create a read+write key".into());
    }
    let station = cfg.wavelog_station_id.trim();
    if station.is_empty() {
        return Ok("key accepted (read+write); station ID not set yet".into());
    }
    // Best effort: an older Cloudlog may not have the endpoint at all.
    let listed = http::get_status(&api_url(
        &cfg.wavelog_url,
        &format!("station_info/{}", urlencode(key)),
    )?)
    .ok()
    .filter(|(s, _)| *s == 200)
    .and_then(|(_, b)| serde_json::from_str::<Vec<Value>>(&b).ok());
    let Some(stations) = listed else {
        return Ok("key accepted (read+write)".into());
    };
    let id_of = |s: &Value| match s.get("station_id") {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    match stations.iter().find(|s| id_of(s) == station) {
        Some(s) => {
            let name = s.get("station_profile_name").and_then(Value::as_str).unwrap_or("");
            let call = s.get("station_callsign").and_then(Value::as_str).unwrap_or("");
            Ok(format!("key accepted; station {station}: {name} ({call})"))
        }
        None => {
            let ids: Vec<String> = stations.iter().map(id_of).collect();
            Err(format!("key accepted, but station ID {station} is not one of: {}", ids.join(", ")))
        }
    }
}

/// What the radio is doing, as Wavelog wants to hear it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Radio {
    /// Where a contact would be logged: the transmit frequency under split.
    pub freq_hz: u64,
    /// The receive frequency, only when it differs (split).
    pub rx_hz: Option<u64>,
    pub mode: &'static str,
}

/// Wavelog's name for a mode.
///
/// Wavelog turns USB/LSB into SSB itself. The data and keyboard modes are sent
/// by their own name where ADIF has one; everything without a logging name of
/// its own goes up as the sideband it rides, which is what the operator would
/// type into the form.
pub fn mode_name(mode: Mode, dial_hz: f64) -> &'static str {
    match mode {
        Mode::Lsb => "LSB",
        Mode::Usb => "USB",
        Mode::Cw => "CW",
        Mode::Am | Mode::Sam | Mode::Dsb => "AM",
        Mode::Nfm | Mode::Wfm | Mode::Packet | Mode::Aprs | Mode::SstvFm => "FM",
        Mode::Ft8 => "FT8",
        Mode::Ft4 => "FT4",
        Mode::Ft2 => "FT2",
        Mode::Psk => "PSK31",
        Mode::Rtty | Mode::RttyFm => "RTTY",
        Mode::Js8 => "JS8",
        Mode::Wspr => "WSPR",
        Mode::Olivia => "OLIVIA",
        Mode::Thor => "THOR",
        Mode::Fsq => "FSQ",
        Mode::Hell => "HELL",
        Mode::Sstv => "SSTV",
        other => {
            if other.is_lower_sideband_at(dial_hz) {
                "LSB"
            } else {
                "USB"
            }
        }
    }
}

fn radio_json(key: &str, name: &str, r: &Radio) -> String {
    let mut v = json!({
        "key": key,
        "radio": name,
        "frequency": r.freq_hz,
        "mode": r.mode,
    });
    if let Some(rx) = r.rx_hz {
        v["frequency_rx"] = json!(rx);
        v["mode_rx"] = json!(r.mode);
    }
    v.to_string()
}

/// Post the radio's state to `api/radio`.
pub fn push_radio(cfg: &NetworkConfig, r: &Radio) -> Result<(), String> {
    let key = key(cfg).map_err(|e| format!("Wavelog: {e}"))?;
    let url = api_url(&cfg.wavelog_url, "radio").map_err(|e| format!("Wavelog: {e}"))?;
    let name = match cfg.wavelog_radio_name.trim() {
        "" => "SDROxide",
        n => n,
    };
    let (status, reply) = http::post_json_plain_status(&url, &radio_json(key, name, r))?;
    if status == 200 {
        return Ok(());
    }
    Err(format!("Wavelog radio: {}", reason(status, &reply)))
}

/// Wavelog asks to be told when frequency or mode *changes*, not every second.
/// A dial being spun is therefore sent once it comes to rest, and never faster
/// than [`MIN_GAP`].
const SETTLE: Duration = Duration::from_millis(500);
const MIN_GAP: Duration = Duration::from_secs(1);
/// Resend an unchanged state this often so Wavelog keeps listing the radio as live.
const KEEPALIVE: Duration = Duration::from_secs(300);

/// Decides when a [`Radio`] is worth sending. Pure, so the timing is testable.
#[derive(Default)]
pub struct QrgPusher {
    sent: Option<(Radio, Instant)>,
    candidate: Option<(Radio, Instant)>,
}

impl QrgPusher {
    /// Forget what was sent, so the next settled state goes out.
    pub fn reset(&mut self) {
        *self = QrgPusher::default();
    }

    /// Called every engine tick with the current state; true means send it now.
    pub fn tick(&mut self, now: Instant, cur: Radio) -> bool {
        let since = match self.candidate {
            Some((c, t)) if c == cur => t,
            _ => {
                self.candidate = Some((cur, now));
                now
            }
        };
        if now.duration_since(since) < SETTLE {
            return false;
        }
        let due = match self.sent {
            None => true,
            Some((s, at)) if s != cur => now.duration_since(at) >= MIN_GAP,
            Some((_, at)) => now.duration_since(at) >= KEEPALIVE,
        };
        if due {
            self.sent = Some((cur, now));
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn radio(freq_hz: u64) -> Radio {
        Radio { freq_hz, rx_hz: None, mode: "USB" }
    }

    #[test]
    fn url_is_the_base_plus_api() {
        assert_eq!(api_url(" https://log.example.com/ ", "qso").unwrap(), "https://log.example.com/api/qso");
        assert_eq!(
            api_url("http://192.168.1.5/wavelog", "radio").unwrap(),
            "http://192.168.1.5/wavelog/api/radio"
        );
        assert!(api_url("", "qso").is_err());
        assert!(api_url("ftp://x", "qso").is_err());
        assert!(api_url("log.example.com", "qso").is_err());
    }

    #[test]
    fn errors_are_read_from_either_wavelog_shape() {
        assert_eq!(reason(401, r#"{"status":"failed","reason":"missing or wrong api key"}"#), "missing or wrong api key");
        assert_eq!(
            reason(400, r#"{"status":"abort","messages":["","Station mismatch"]}"#),
            "Station mismatch"
        );
        assert_eq!(reason(404, "<html>not found</html>"), "HTTP 404");
    }

    #[test]
    fn radio_payload_carries_only_documented_fields() {
        let v: Value = serde_json::from_str(&radio_json("K", "SDROxide", &radio(14_074_000))).unwrap();
        assert_eq!(v["frequency"], 14_074_000);
        assert_eq!(v["mode"], "USB");
        assert_eq!(v["radio"], "SDROxide");
        assert!(v.get("frequency_rx").is_none() && v.get("timestamp").is_none());

        let split = Radio { freq_hz: 14_195_000, rx_hz: Some(14_200_000), mode: "LSB" };
        let v: Value = serde_json::from_str(&radio_json("K", "R", &split)).unwrap();
        assert_eq!(v["frequency_rx"], 14_200_000);
        assert_eq!(v["mode_rx"], "LSB");
    }

    #[test]
    fn modes_use_the_names_a_log_form_knows() {
        assert_eq!(mode_name(Mode::Usb, 14.2e6), "USB");
        assert_eq!(mode_name(Mode::Nfm, 145.5e6), "FM");
        assert_eq!(mode_name(Mode::Ft8, 14.074e6), "FT8");
        assert_eq!(mode_name(Mode::Digl, 7.04e6), "LSB");
        assert_eq!(mode_name(Mode::Digu, 14.07e6), "USB");
        // Phone-practice modes follow the band.
        assert_eq!(mode_name(Mode::Rade, 7.1e6), "LSB");
        assert_eq!(mode_name(Mode::Rade, 14.2e6), "USB");
    }

    #[test]
    fn a_spinning_dial_is_sent_once_at_rest() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut p = QrgPusher::default();
        // First sight is held back until it has been steady for SETTLE.
        assert!(!p.tick(ms(0), radio(14_000_000)));
        assert!(p.tick(ms(500), radio(14_000_000)));
        // Unchanged: nothing more is sent.
        assert!(!p.tick(ms(1_000), radio(14_000_000)));
        // Dial moving every tick: nothing goes out while it moves.
        for i in 0..20u64 {
            assert!(!p.tick(ms(2_000 + i * 50), radio(14_000_000 + i * 100)));
        }
        // Rests: exactly one send.
        assert!(!p.tick(ms(3_000), radio(14_002_000)));
        assert!(p.tick(ms(3_600), radio(14_002_000)));
        assert!(!p.tick(ms(3_700), radio(14_002_000)));
    }

    #[test]
    fn an_idle_radio_is_refreshed_and_reset_resends() {
        let t0 = Instant::now();
        let mut p = QrgPusher::default();
        assert!(!p.tick(t0, radio(7_100_000)));
        assert!(p.tick(t0 + SETTLE, radio(7_100_000)));
        assert!(!p.tick(t0 + KEEPALIVE, radio(7_100_000)));
        assert!(p.tick(t0 + SETTLE + KEEPALIVE, radio(7_100_000)));
        p.reset();
        let t1 = t0 + SETTLE + KEEPALIVE + Duration::from_secs(1);
        assert!(!p.tick(t1, radio(7_100_000)));
        assert!(p.tick(t1 + SETTLE, radio(7_100_000)));
    }
}
