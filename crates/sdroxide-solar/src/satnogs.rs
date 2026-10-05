//! SatNOGS transmitter list: the frequencies a TLE never carries.
//!
//! CelesTrak says where a satellite is. SatNOGS DB says what it listens and
//! talks on. The built-in table in [`crate::satfreq`] is only a handful of
//! well-known birds; everything else in a subscription listing would otherwise
//! sit in the picker with no dial to go to.
//!
//! Fetched and cached the same way as a TLE subscription, so a clone from git
//! fills itself in on the first run and keeps working offline.

use sdroxide_types::{Passband, SatFreqs, SatLink};

/// SatNOGS DB transmitters, active only. Paginated; [`refresh`] follows `next`.
pub const TRANSMITTERS_URL: &str =
    "https://db.satnogs.org/api/transmitters/?format=json&status=active&page_size=1000";

const CACHE_NAME: &str = "satnogs_transmitters.json";
const BODY_LIMIT: u64 = 16 * 1024 * 1024;
const MAX_PAGES: usize = 20;

/// Parse a SatNOGS transmitters response: either a bare array or a DRF page
/// `{results, next}`.
pub fn parse_transmitters(json: &str) -> Result<(Vec<SatFreqs>, Option<String>), String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("satnogs json: {e}"))?;
    let (rows, next) = match v {
        serde_json::Value::Array(a) => (a, None),
        serde_json::Value::Object(map) => {
            let rows = match map.get("results") {
                Some(serde_json::Value::Array(a)) => a.clone(),
                _ => return Err("satnogs page has no results array".into()),
            };
            let next = map.get("next").and_then(|n| n.as_str()).map(|s| s.to_string());
            (rows, next)
        }
        _ => return Err("satnogs body is not an array or a page".into()),
    };
    Ok((group_transmitters(&rows), next))
}

fn group_transmitters(rows: &[serde_json::Value]) -> Vec<SatFreqs> {
    let mut by_id: std::collections::BTreeMap<u64, SatFreqs> = std::collections::BTreeMap::new();
    for row in rows {
        let Some(link) = link_from_row(row) else { continue };
        let Some(id) = row.get("norad_cat_id").and_then(|v| v.as_u64()) else { continue };
        if id == 0 {
            continue;
        }
        let entry = by_id.entry(id).or_insert_with(|| SatFreqs::new(id, "", Vec::new()));
        entry.links.push(link);
    }
    by_id.into_values().filter(|f| f.usable_links().next().is_some()).collect()
}

fn link_from_row(row: &serde_json::Value) -> Option<SatLink> {
    let status = row.get("status").and_then(|v| v.as_str()).unwrap_or("");
    if !status.is_empty() && status != "active" {
        return None;
    }
    let up = passband(row.get("uplink_low"), row.get("uplink_high"));
    let down = passband(row.get("downlink_low"), row.get("downlink_high"));
    if up.is_none() && down.is_none() {
        return None;
    }
    let label = row
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Downlink")
        .to_string();
    let mut mode = row.get("mode").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if let Some(baud) = row.get("baud").and_then(|v| v.as_f64()).filter(|b| *b > 0.0) {
        if mode.is_empty() {
            mode = format!("{baud:.0} bd");
        } else if !mode.to_ascii_lowercase().contains(&format!("{baud:.0}")) {
            mode = format!("{mode} {baud:.0}");
        }
    }
    let invert = row.get("invert").and_then(|v| v.as_bool()).unwrap_or(false);
    Some(SatLink {
        label,
        mode,
        uplink: up,
        downlink: down,
        note: String::new(),
        inverting: invert,
    })
}

fn passband(lo: Option<&serde_json::Value>, hi: Option<&serde_json::Value>) -> Option<Passband> {
    let to_mhz = |v: &serde_json::Value| -> Option<f64> {
        let hz = v.as_f64().or_else(|| v.as_u64().map(|n| n as f64))?;
        if hz <= 0.0 {
            return None;
        }
        Some(hz / 1e6)
    };
    let lo_mhz = lo.and_then(to_mhz)?;
    let hi_mhz = hi.and_then(to_mhz).unwrap_or(lo_mhz);
    Some(if (hi_mhz - lo_mhz).abs() < 1e-9 {
        Passband::at(lo_mhz)
    } else {
        Passband::range(lo_mhz.min(hi_mhz), lo_mhz.max(hi_mhz))
    })
}

/// Merge later pages into the first page's grouping, last write per NORAD
/// concatenates links.
pub fn merge_pages(mut acc: Vec<SatFreqs>, extra: Vec<SatFreqs>) -> Vec<SatFreqs> {
    for f in extra {
        if let Some(have) = acc.iter_mut().find(|h| h.norad_id == f.norad_id) {
            have.links.extend(f.links);
        } else {
            acc.push(f);
        }
    }
    acc
}

/// The cached SatNOGS table, or empty when nothing has been fetched yet.
#[cfg(not(target_arch = "wasm32"))]
pub fn cached() -> Vec<SatFreqs> {
    let cache = crate::cache::Cache::open();
    cached_from(&cache)
}

#[cfg(not(target_arch = "wasm32"))]
fn cached_from(cache: &crate::cache::Cache) -> Vec<SatFreqs> {
    let Some(text) = cache.read_string(CACHE_NAME) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// True when the cache is missing or older than the TLE cadence.
#[cfg(not(target_arch = "wasm32"))]
pub fn cache_stale() -> bool {
    let cache = crate::cache::Cache::open();
    let age = crate::feed::now_unix() - cache.fetched_at(TRANSMITTERS_URL);
    cached_from(&cache).is_empty() || age > crate::data::TLE_PERIOD_S
}

/// Fetch (or 304) the SatNOGS transmitter list and return what is now on disk.
#[cfg(not(target_arch = "wasm32"))]
pub fn refresh() -> Vec<SatFreqs> {
    let agent = crate::tlesub::agent();
    let mut cache = crate::cache::Cache::open();
    let now = crate::feed::now_unix();
    match fetch_all(&agent, &mut cache, now) {
        Ok(freqs) => freqs,
        Err(e) => {
            tracing::warn!("SatNOGS transmitters: {e}");
            cached_from(&cache)
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn fetch_all(
    agent: &ureq::Agent,
    cache: &mut crate::cache::Cache,
    now_unix: i64,
) -> Result<Vec<SatFreqs>, String> {
    let mut url = Some(TRANSMITTERS_URL.to_string());
    let mut acc = Vec::new();
    let mut first_validators = crate::cache::Validators::default();
    for page in 0..MAX_PAGES {
        let Some(this) = url.take() else { break };
        let validators = cache.validators(&this);
        match crate::feed::http_get(agent, &this, &validators, BODY_LIMIT) {
            Ok(None) => {
                cache.touch(TRANSMITTERS_URL, now_unix);
                return Ok(cached_from(cache));
            }
            Ok(Some((bytes, validators, _))) => {
                if page == 0 {
                    first_validators = validators;
                }
                let text = String::from_utf8(bytes).map_err(|_| "satnogs body is not utf-8")?;
                let (page_freqs, next) = parse_transmitters(&text)?;
                acc = merge_pages(acc, page_freqs);
                url = next.filter(|n| !n.is_empty());
            }
            Err(e) => return Err(e),
        }
    }
    if acc.is_empty() {
        return Err("no transmitters in the response".into());
    }
    let body = serde_json::to_vec(&acc).map_err(|e| e.to_string())?;
    cache.write(
        CACHE_NAME,
        TRANSMITTERS_URL,
        &body,
        crate::cache::Validators { fetched_unix: now_unix, ..first_validators },
    );
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"{
        "count": 2,
        "next": "https://db.satnogs.org/api/transmitters/?page=2",
        "results": [
            {
                "norad_cat_id": 27607,
                "description": "FM Voice Repeater",
                "mode": "FM",
                "uplink_low": 145850000,
                "uplink_high": null,
                "downlink_low": 436795000,
                "downlink_high": null,
                "invert": false,
                "baud": null,
                "status": "active"
            },
            {
                "norad_cat_id": 25338,
                "description": "APT",
                "mode": "AFSK",
                "uplink_low": null,
                "downlink_low": 137620000,
                "downlink_high": null,
                "invert": false,
                "baud": 2400,
                "status": "active"
            },
            {
                "norad_cat_id": 1,
                "description": "dead",
                "downlink_low": 145000000,
                "status": "inactive"
            }
        ]
    }"#;

    #[test]
    fn a_satnogs_page_becomes_grouped_links() {
        let (freqs, next) = parse_transmitters(PAGE).expect("page");
        assert_eq!(next.as_deref(), Some("https://db.satnogs.org/api/transmitters/?page=2"));
        assert_eq!(freqs.len(), 2, "inactive row dropped");
        let so50 = freqs.iter().find(|f| f.norad_id == 27607).expect("SO-50");
        let fm = so50.usable_links().next().expect("fm");
        assert_eq!(fm.uplink, Some(Passband::at(145.850)));
        assert_eq!(fm.downlink, Some(Passband::at(436.795)));
        assert_eq!(fm.mode, "FM");
        let noaa = freqs.iter().find(|f| f.norad_id == 25338).expect("NOAA-15");
        let apt = noaa.usable_links().next().expect("apt");
        assert_eq!(apt.downlink, Some(Passband::at(137.620)));
        assert!(apt.mode.contains("2400"), "{}", apt.mode);
        assert!(apt.uplink.is_none());
    }

    #[test]
    fn a_bare_array_is_also_a_listing() {
        let (freqs, next) = parse_transmitters(
            r#"[{"norad_cat_id":44909,"description":"Beacon","mode":"CW",
                "downlink_low":435605000,"status":"active"}]"#,
        )
        .expect("array");
        assert!(next.is_none());
        assert_eq!(freqs[0].norad_id, 44909);
        assert_eq!(freqs[0].links[0].downlink, Some(Passband::at(435.605)));
    }

    #[test]
    fn an_inverting_transponder_keeps_its_flag() {
        let (freqs, _) = parse_transmitters(
            r#"[{"norad_cat_id":44909,"description":"U/V","mode":"USB",
                "uplink_low":145935000,"uplink_high":145995000,
                "downlink_low":435610000,"downlink_high":435670000,
                "invert":true,"status":"active"}]"#,
        )
        .expect("inv");
        let l = &freqs[0].links[0];
        assert!(l.inverting);
        assert_eq!(l.uplink, Some(Passband::range(145.935, 145.995)));
        assert_eq!(l.downlink, Some(Passband::range(435.610, 435.670)));
    }
}
