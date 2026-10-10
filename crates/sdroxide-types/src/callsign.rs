//! Callsign-lookup result, shared by the native lookup clients (QRZ/HamQTH in
//! `sdroxide-net`), the wire protocol, and the UI. Pure data + serde.

use serde::{Deserialize, Serialize};

/// Biographical / location data resolved for a callsign. Every field is
/// optional: providers return different subsets and some require a paid
/// subscription for the full record. The UI merges the populated fields into
/// the active log entry without overwriting what the operator already typed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CallsignInfo {
    /// The callsign this record is for (uppercased), echoed back so a late
    /// result can be matched to the right log entry.
    pub call: String,
    /// Operator name (first + last, as the provider formats it).
    pub name: Option<String>,
    /// City / town (QTH).
    pub qth: Option<String>,
    /// Maidenhead grid locator.
    pub grid: Option<String>,
    /// US state (2-letter) or other primary administrative subdivision.
    pub state: Option<String>,
    /// US county, if provided.
    pub county: Option<String>,
    /// Country / DXCC entity name.
    pub country: Option<String>,
    /// DXCC entity number, if the provider returns it.
    pub dxcc: Option<u16>,
    /// CQ zone (1..40).
    pub cq_zone: Option<u8>,
    /// ITU zone (1..90).
    pub itu_zone: Option<u8>,
    /// QSL routing hint / manager, if any.
    pub qsl_via: Option<String>,
}

impl CallsignInfo {
    pub fn for_call(call: &str) -> Self {
        CallsignInfo { call: call.trim().to_ascii_uppercase(), ..Default::default() }
    }

    /// True when the lookup produced nothing usable.
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.qth.is_none()
            && self.grid.is_none()
            && self.state.is_none()
            && self.county.is_none()
            && self.country.is_none()
            && self.dxcc.is_none()
            && self.cq_zone.is_none()
            && self.itu_zone.is_none()
            && self.qsl_via.is_none()
    }
}

/// Where to send a one-click QSO upload. Matches the credentials in
/// [`crate` network config] and the upload clients in `sdroxide-net`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UploadTarget {
    Eqsl,
    QrzLogbook,
    ClubLog,
    /// The HamQTH logbook, via its real-time QSO endpoint. Appended last:
    /// postcard numbers variants by declaration index, so inserting it next to
    /// QRZ (where it belongs on screen) would renumber Club Log on the wire.
    HamQth,
    /// The World Radio League logbook, via its developer API (issue #337).
    /// Appended for the reason [`UploadTarget::HamQth`] gives.
    Wrl,
    /// A Wavelog (or Cloudlog) logbook, via its `api/qso` endpoint. Appended
    /// for the reason [`UploadTarget::HamQth`] gives.
    Wavelog,
}

impl UploadTarget {
    pub fn label(self) -> &'static str {
        match self {
            UploadTarget::Eqsl => "eQSL",
            UploadTarget::QrzLogbook => "QRZ",
            UploadTarget::ClubLog => "Club Log",
            UploadTarget::HamQth => "HamQTH",
            UploadTarget::Wrl => "WRL",
            UploadTarget::Wavelog => "Wavelog",
        }
    }

    /// The service whose stored credentials this target's are, for a
    /// "Test this login" button beside it.
    ///
    /// One-way on purpose. Every upload target is testable, so this mapping is
    /// total; the reverse is not — LoTW can be tested and cannot be uploaded to
    /// — which is why [`LoginTarget`] stays an enum of its own rather than
    /// becoming a method on this one.
    pub fn login_target(self) -> LoginTarget {
        match self {
            UploadTarget::Eqsl => LoginTarget::Eqsl,
            UploadTarget::QrzLogbook => LoginTarget::QrzLogbook,
            UploadTarget::ClubLog => LoginTarget::ClubLog,
            UploadTarget::HamQth => LoginTarget::HamQth,
            UploadTarget::Wrl => LoginTarget::Wrl,
            UploadTarget::Wavelog => LoginTarget::Wavelog,
        }
    }

    /// Display order, which is deliberately not the declaration order above:
    /// the variants are numbered by the wire and appended to, while this is
    /// what the operator reads along a tab strip.
    pub const ALL: [UploadTarget; 6] = [
        UploadTarget::QrzLogbook,
        UploadTarget::Eqsl,
        UploadTarget::HamQth,
        UploadTarget::ClubLog,
        UploadTarget::Wrl,
        UploadTarget::Wavelog,
    ];
}

/// A service whose stored credentials can be checked without logging a QSO.
///
/// Deliberately a separate enum from [`UploadTarget`] rather than an extension
/// of it. LoTW is testable but is not an upload target (its upload needs TQSL
/// signing), and the two lists would drift apart the moment a download-only
/// service was added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LoginTarget {
    Eqsl,
    QrzLogbook,
    ClubLog,
    Lotw,
    /// The HamQTH account — the *same* username and password the callsign
    /// lookup uses, because HamQTH has one account per operator and the
    /// real-time logbook endpoint authenticates with it directly.
    HamQth,
    /// The World Radio League developer API key (issue #337). Appended for the
    /// reason [`UploadTarget::HamQth`] gives.
    Wrl,
    /// The Wavelog / Cloudlog API key. Appended for the reason
    /// [`UploadTarget::HamQth`] gives.
    Wavelog,
}

impl LoginTarget {
    pub fn label(self) -> &'static str {
        match self {
            LoginTarget::Eqsl => "eQSL",
            LoginTarget::QrzLogbook => "QRZ Logbook",
            LoginTarget::ClubLog => "Club Log",
            LoginTarget::Lotw => "LoTW",
            LoginTarget::HamQth => "HamQTH",
            LoginTarget::Wrl => "World Radio League",
            LoginTarget::Wavelog => "Wavelog",
        }
    }

    pub const ALL: [LoginTarget; 7] = [
        LoginTarget::Eqsl,
        LoginTarget::QrzLogbook,
        LoginTarget::HamQth,
        LoginTarget::ClubLog,
        LoginTarget::Lotw,
        LoginTarget::Wrl,
        LoginTarget::Wavelog,
    ];
}

/// The outcome of checking one service's stored credentials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoginTestResult {
    pub target: LoginTarget,
    pub ok: bool,
    /// What the service said, or why the check could not be made.
    pub message: String,
}

/// One station location in a Wavelog account, as `api/station_info` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WavelogStation {
    pub id: u32,
    pub name: String,
    pub callsign: String,
    pub grid: String,
    pub active: bool,
}

/// The station locations worth offering for `my_call`.
///
/// Inactive locations are dropped unless nothing else is left, and when several
/// remain the ones logged under the operator's own callsign win. One entry
/// means the choice is made; more means the operator has to pick.
pub fn wavelog_candidates<'a>(
    stations: &'a [WavelogStation],
    my_call: &str,
) -> Vec<&'a WavelogStation> {
    let mut c: Vec<&WavelogStation> = stations.iter().filter(|s| s.active).collect();
    if c.is_empty() {
        c = stations.iter().collect();
    }
    let call = my_call.trim();
    if c.len() > 1 && !call.is_empty() {
        let own: Vec<&WavelogStation> =
            c.iter().copied().filter(|s| s.callsign.trim().eq_ignore_ascii_case(call)).collect();
        if !own.is_empty() {
            c = own;
        }
    }
    c
}

/// The outcome of an upload attempt for one QSO to one target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadResult {
    /// The logbook id of the QSO this result is for.
    pub qso_id: u64,
    pub target: UploadTarget,
    pub ok: bool,
    /// Server message or error detail.
    pub message: String,
}

#[cfg(test)]
mod wavelog_tests {
    use super::*;

    fn st(id: u32, call: &str, active: bool) -> WavelogStation {
        WavelogStation {
            id,
            name: format!("loc{id}"),
            callsign: call.into(),
            grid: String::new(),
            active,
        }
    }

    #[test]
    fn single_station_is_chosen() {
        let s = [st(3, "DL1ABC", true)];
        assert_eq!(wavelog_candidates(&s, "").len(), 1);
    }

    #[test]
    fn inactive_dropped_unless_all_are() {
        let s = [st(1, "DL1ABC", false), st(2, "DL1ABC", true)];
        assert_eq!(wavelog_candidates(&s, "")[0].id, 2);
        let s = [st(1, "DL1ABC", false), st(2, "DL9XYZ", false)];
        assert_eq!(wavelog_candidates(&s, "").len(), 2);
    }

    #[test]
    fn own_callsign_narrows_case_insensitively() {
        let s = [st(1, "DL9XYZ", true), st(2, "DL1ABC", true), st(4, "DL1ABC/P", true)];
        let c = wavelog_candidates(&s, " dl1abc ");
        assert_eq!(c.iter().map(|x| x.id).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn unknown_callsign_leaves_the_choice_to_the_operator() {
        let s = [st(1, "DL9XYZ", true), st(2, "DL8AAA", true)];
        assert_eq!(wavelog_candidates(&s, "DL1ABC").len(), 2);
    }
}
