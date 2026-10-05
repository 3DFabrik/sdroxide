//! The weather-satellite pass scheduler: which overflights to record while
//! nobody is watching.
//!
//! A NOAA APT pass is fifteen minutes long, happens four times a day per bird,
//! and cannot be asked to come back later. Recording one by hand means being at
//! the radio at the right minute with the right mode selected, which is the
//! part a program should be doing. So the operator picks a window, a set of
//! birds and a minimum culmination, ticks the passes worth having, and the
//! engine does the rest.
//!
//! The ticks live on the engine host, not in a browser tab. That is the whole
//! point: a schedule that only runs while a page is open is not a schedule.
//! These are the types both ends of that conversation share.

use serde::{Deserialize, Serialize};

/// The analog APT birds. Deliberately not [`crate::WEATHER_APT_CATNRS`], which
/// also carries the Meteor-M satellites: those are LRPT, a digital downlink
/// this decoder cannot read and WXtoImg cannot either.
pub const WX_APT_CATNRS: &[u64] = &[
    25338, // NOAA 15
    28654, // NOAA 18
    33591, // NOAA 19
];

/// The same birds with the names they are known by, for a selector that has to
/// be drawn before any element set has been loaded.
///
/// A catalogue number is not a satellite anybody recognises, and the names
/// cannot come from the prediction: the list has to offer a bird the operator
/// has *not* subscribed to the elements for, which is the case where picking it
/// is the useful thing to do.
pub const WX_APT_BIRDS: &[(u64, &str)] =
    &[(25338, "NOAA 15"), (28654, "NOAA 18"), (33591, "NOAA 19")];

/// What to call a bird whose elements have not been loaded.
pub fn wx_bird_name(norad_id: u64) -> String {
    WX_APT_BIRDS
        .iter()
        .find(|(n, _)| *n == norad_id)
        .map_or_else(|| format!("#{norad_id}"), |(_, name)| name.to_string())
}

/// Bounds on how far ahead the list looks. The lower end is a single day's
/// worth of passes; the upper is a week, past which the element sets the
/// prediction rests on are older than the answer they give.
pub const WX_HORIZON_MIN_H: u16 = 6;
pub const WX_HORIZON_MAX_H: u16 = 168;

/// Bounds on the pre-roll and post-roll, seconds. Some lead is essential — the
/// radio has to be tuned and the decoder hunting before the first sync arrives
/// — and ten minutes of it is more than any pass needs.
pub const WX_PAD_MAX_S: u16 = 600;

/// Most jobs kept at once, so a horizon of a week cannot grow the file without
/// limit. Older finished jobs are dropped first.
pub const WX_JOBS_MAX: usize = 256;

/// What the scheduler offers and how it records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WxSchedConfig {
    /// How far into the future passes are listed, hours.
    pub horizon_h: u16,
    /// Only passes whose highest point reaches this, degrees. A pass that
    /// barely clears the horizon is a few streaks of noise: the signal is in
    /// the ground clutter for most of it and the picture is not worth the disk.
    pub min_max_el: f32,
    /// Catalogue numbers offered. Empty means [`WX_APT_CATNRS`].
    pub sats: Vec<u64>,
    /// Pre-roll before AOS, seconds: the radio is tuned and the decoder is
    /// hunting this long before the bird is due over the horizon.
    pub lead_s: u16,
    /// Post-roll after LOS, seconds.
    pub trail_s: u16,
    /// Keep the discriminator audio as a WAV for an external decoder. On by
    /// default — it is the only artefact WXtoImg can do anything with — and
    /// worth switching off on a small card, at twenty megabytes a pass.
    pub keep_wav: bool,
}

impl Default for WxSchedConfig {
    fn default() -> Self {
        WxSchedConfig {
            horizon_h: 24,
            min_max_el: 20.0,
            sats: WX_APT_CATNRS.to_vec(),
            lead_s: 60,
            trail_s: 60,
            keep_wav: true,
        }
    }
}

impl WxSchedConfig {
    /// Clamp a configuration that arrived from a socket or an edited file into
    /// something the scheduler can act on.
    pub fn sane(mut self) -> Self {
        self.horizon_h = self.horizon_h.clamp(WX_HORIZON_MIN_H, WX_HORIZON_MAX_H);
        // Below the horizon is not a pass, and a bird that has to reach 90° is
        // one that will never be recorded.
        self.min_max_el = self.min_max_el.clamp(0.0, 80.0);
        self.lead_s = self.lead_s.min(WX_PAD_MAX_S);
        self.trail_s = self.trail_s.min(WX_PAD_MAX_S);
        self.sats.retain(|id| WX_APT_CATNRS.contains(id));
        self.sats.dedup();
        if self.sats.is_empty() {
            self.sats = WX_APT_CATNRS.to_vec();
        }
        self
    }

    /// The window the prediction runs over, seconds.
    pub fn horizon_s(&self) -> f64 {
        f64::from(self.horizon_h) * 3600.0
    }
}

/// How far an armed pass has got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WxJobState {
    /// Armed and waiting for its AOS.
    #[default]
    Planned,
    /// The radio is on it now.
    Recording,
    /// Finished, and something came of it.
    Done,
    /// Finished with nothing to show, or never started. `note` says why.
    Failed,
}

impl WxJobState {
    pub fn label(self) -> &'static str {
        match self {
            WxJobState::Planned => "planned",
            WxJobState::Recording => "recording",
            WxJobState::Done => "done",
            WxJobState::Failed => "failed",
        }
    }

    /// Whether the scheduler is still going to do something about this one.
    pub fn is_pending(self) -> bool {
        matches!(self, WxJobState::Planned | WxJobState::Recording)
    }
}

/// One armed pass, as the engine persists it.
///
/// Keyed by catalogue number and AOS rather than by position: the prediction is
/// recomputed from fresh element sets every few minutes, and a pass that moved
/// thirty seconds has to still be the pass the operator ticked. See
/// [`WxJob::is`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WxJob {
    pub norad_id: u64,
    /// Display name as the listing had it, so a finished job can still name the
    /// bird after its element set has gone.
    pub name: String,
    pub aos_unix: i64,
    pub los_unix: i64,
    pub max_el: f32,
    pub state: WxJobState,
    /// What was written, once it has been. Empty until then.
    pub png: String,
    pub wav: String,
    /// Why it failed, or what it produced.
    pub note: String,
}

/// How far a recomputed AOS may move and still be the same pass, seconds.
///
/// A fresh element set shifts a prediction by seconds, not minutes; two
/// genuinely different passes of the same bird are an orbit apart. Anything in
/// between is the same pass, seen through better numbers.
pub const WX_SAME_PASS_S: i64 = 600;

impl WxJob {
    /// Whether this job is the pass at `aos_unix` for `norad_id`.
    pub fn is(&self, norad_id: u64, aos_unix: i64) -> bool {
        self.norad_id == norad_id && (self.aos_unix - aos_unix).abs() <= WX_SAME_PASS_S
    }
}

/// One row of the scheduler's list: a predicted pass, and whatever the operator
/// and the engine have already made of it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WxPass {
    pub norad_id: u64,
    pub name: String,
    pub aos_unix: i64,
    pub los_unix: i64,
    pub max_el: f32,
    /// Azimuths at the horizon, degrees clockwise from north — which way to
    /// look, and where it ends up.
    pub rise_az: f32,
    pub set_az: f32,
    /// The downlink the recorder will tune, Hz. Zero when no frequency could be
    /// resolved for the bird, which is also why the row cannot be armed.
    pub downlink_hz: f64,
    /// Ticked by the operator.
    pub armed: bool,
    pub state: WxJobState,
    pub png: String,
    pub wav: String,
    pub note: String,
}

impl WxPass {
    pub fn duration_s(&self) -> i64 {
        (self.los_unix - self.aos_unix).max(0)
    }

    /// Whether this row can be armed at all: a pass with no frequency is a pass
    /// the radio cannot be pointed at.
    pub fn is_tunable(&self) -> bool {
        self.downlink_hz > 0.0
    }
}

/// The scheduler as a client sees it: the configuration, the list, and what the
/// engine is doing about it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WxSchedStatus {
    pub cfg: WxSchedConfig,
    /// Predicted passes inside the window, earliest first, with any armed job
    /// folded in. Also carries armed jobs that have fallen outside the window
    /// or outside the filter, so a tick never silently disappears.
    pub passes: Vec<WxPass>,
    /// Why the list is empty or short — no grid, no element sets, no
    /// frequencies. Empty when there is nothing to explain.
    pub note: String,
    /// The pass being recorded right now, as `(norad_id, aos_unix)`.
    pub recording: Option<(u64, i64)>,
}

impl Default for WxSchedStatus {
    fn default() -> Self {
        WxSchedStatus {
            cfg: WxSchedConfig::default(),
            passes: Vec::new(),
            note: String::new(),
            recording: None,
        }
    }
}

/// The scheduler as the engine persists it: the configuration and the ticks.
///
/// The predicted passes are not in here. They are derived from element sets
/// that change under the program, so keeping them would mean keeping a
/// prediction that is wrong by the time it is read back.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WxSchedule {
    pub cfg: WxSchedConfig,
    pub jobs: Vec<WxJob>,
}

impl WxSchedule {
    /// Find the job for a pass, if it has been armed.
    pub fn job(&self, norad_id: u64, aos_unix: i64) -> Option<&WxJob> {
        self.jobs.iter().find(|j| j.is(norad_id, aos_unix))
    }

    pub fn job_mut(&mut self, norad_id: u64, aos_unix: i64) -> Option<&mut WxJob> {
        self.jobs.iter_mut().find(|j| j.is(norad_id, aos_unix))
    }

    /// Drop finished jobs once there are too many, oldest first.
    ///
    /// Pending ones are never dropped however old: a job still marked
    /// `Recording` after a restart is a crash to be reported, not a row to be
    /// quietly forgotten.
    pub fn prune(&mut self) {
        if self.jobs.len() <= WX_JOBS_MAX {
            return;
        }
        self.jobs.sort_by_key(|j| j.aos_unix);
        let excess = self.jobs.len() - WX_JOBS_MAX;
        let mut dropped = 0;
        self.jobs.retain(|j| {
            if dropped < excess && !j.state.is_pending() {
                dropped += 1;
                return false;
            }
            true
        });
    }
}

/// The file name a recorded pass is written under, without its extension.
///
/// Two files share it — the picture and the audio — so the pair can be found
/// together, and the leading `apt-<millis>` is what
/// [`crate::received_at`] already reads a stored picture's date out of. The
/// label is the bird, flattened to what [`crate::safe_name`] accepts.
pub fn pass_stem(unix_millis: i64, name: &str) -> String {
    let label: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    if label.is_empty() {
        format!("apt-{unix_millis}")
    } else {
        format!("apt-{unix_millis}-{label}")
    }
}

/// Accept a recorded-audio file name from a client, or refuse it.
///
/// [`crate::safe_name`]'s rules, with `.wav` in place of `.png`: these names
/// arrive over the same socket and are joined to a directory this program will
/// read, and the picture store's check would refuse every one of them on the
/// extension alone.
pub fn safe_wav_name(name: &str) -> Option<&str> {
    if name.is_empty() || name.len() > crate::IMAGE_NAME_MAX {
        return None;
    }
    if name.starts_with('.') || name.contains("..") {
        return None;
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')) {
        return None;
    }
    let dot = name.rfind('.')?;
    name[dot..].eq_ignore_ascii_case(".wav").then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configuration that arrived from a socket cannot be trusted to be
    /// operable: a zero horizon lists nothing, a 90° culmination is never
    /// reached, and a Meteor bird would be tuned to an LRPT downlink this
    /// decoder reads as noise.
    #[test]
    fn a_configuration_from_outside_is_clamped_into_range() {
        let wild = WxSchedConfig {
            horizon_h: 0,
            min_max_el: 95.0,
            sats: vec![40069, 25338, 99999],
            lead_s: 9999,
            trail_s: 9999,
            keep_wav: true,
        }
        .sane();
        assert_eq!(wild.horizon_h, WX_HORIZON_MIN_H);
        assert_eq!(wild.min_max_el, 80.0);
        assert_eq!(wild.lead_s, WX_PAD_MAX_S);
        assert_eq!(wild.trail_s, WX_PAD_MAX_S);
        assert_eq!(wild.sats, vec![25338], "Meteor is LRPT, and 99999 is nothing");

        // Filtering every satellite out leaves the default set rather than a
        // scheduler that can never list anything.
        let empty = WxSchedConfig { sats: vec![40069], ..Default::default() }.sane();
        assert_eq!(empty.sats, WX_APT_CATNRS);
    }

    /// The bug this guards: the prediction is recomputed from whatever element
    /// set is freshest, so an armed pass's AOS moves by seconds between one
    /// listing and the next. Matching on the exact second would un-tick the
    /// operator's choice and never record it.
    #[test]
    fn a_pass_stays_the_same_pass_when_the_elements_refresh() {
        let job = WxJob { norad_id: 33591, aos_unix: 1_800_000_000, ..Default::default() };
        assert!(job.is(33591, 1_800_000_000));
        assert!(job.is(33591, 1_800_000_000 + 45), "a fresher TLE moved it a little");
        assert!(job.is(33591, 1_800_000_000 - 45));
        // An orbit later is a different pass.
        assert!(!job.is(33591, 1_800_000_000 + 6000));
        // And so is another bird at the same minute.
        assert!(!job.is(25338, 1_800_000_000));
    }

    /// Both files of a pass share a stem, and the picture's half still carries
    /// the date the gallery sorts by.
    #[test]
    fn a_pass_is_named_for_its_bird_and_its_minute() {
        let stem = pass_stem(1_753_795_200_000, "NOAA 19");
        assert_eq!(stem, "apt-1753795200000-NOAA_19");
        let png = format!("{stem}.png");
        assert_eq!(crate::safe_name(&png), Some(png.as_str()));
        assert_eq!(crate::received_at(crate::ImageKind::Apt, &png), Some(1_753_795_200));
        assert!(safe_wav_name(&format!("{stem}.wav")).is_some());

        // A name with nothing usable in it still yields a legal stem.
        assert_eq!(pass_stem(42, "///"), "apt-42");
    }

    /// The audio name is a fetch key off an unauthenticated socket, so it gets
    /// the same treatment the picture store's already gives.
    #[test]
    fn an_audio_name_from_outside_the_store_is_refused() {
        for bad in ["", "../../etc/passwd", "..", "a/b.wav", "a\\b.wav", ".hidden.wav", "pass.png"]
        {
            assert_eq!(safe_wav_name(bad), None, "{bad:?} should be refused");
        }
        assert_eq!(safe_wav_name("apt-1-NOAA_19.wav"), Some("apt-1-NOAA_19.wav"));
        assert_eq!(safe_wav_name("apt-1.WAV"), Some("apt-1.WAV"));
    }

    /// A week-long horizon ticked all the way through must not grow the file
    /// without limit — but a job the engine still owes an answer for survives
    /// the cull however old it is.
    #[test]
    fn pruning_drops_finished_jobs_and_keeps_pending_ones() {
        let mut s = WxSchedule::default();
        for i in 0..(WX_JOBS_MAX as i64 + 10) {
            s.jobs.push(WxJob {
                norad_id: 33591,
                aos_unix: i * 10_000,
                state: if i == 0 { WxJobState::Recording } else { WxJobState::Done },
                ..Default::default()
            });
        }
        s.prune();
        assert_eq!(s.jobs.len(), WX_JOBS_MAX);
        assert!(
            s.jobs.iter().any(|j| j.aos_unix == 0),
            "the oldest job was still running: it is a crash report, not spare space"
        );
    }
}
