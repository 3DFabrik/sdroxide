//! The weather-satellite pass scheduler's arithmetic: which passes are coming,
//! which of them the operator armed, and which one the radio owes its attention
//! to right now.
//!
//! Separated from the engine because none of it needs a radio. Everything here
//! is a function of the element sets, the observer, the clock and the operator's
//! ticks — which is also what makes it testable, and this is logic worth
//! testing: an off-by-one in "is this pass the one that was armed" is a
//! recording that silently never happens, and nobody finds out until the
//! picture is not there.
//!
//! The engine side — tuning, the decoder, the files — is
//! `Engine::poll_wx_sched`.

use sdroxide_types::{WxJobState, WxPass, WxSchedConfig, WxSchedule};

/// How often the prediction is recomputed, seconds.
///
/// A LEO pass is ten to fifteen minutes long and a fresh element set moves its
/// AOS by seconds, so a minute is far finer than anything that can change
/// underneath it. The cost is a few hundred sgp4 runs, off the audio path.
pub const SWEEP_INTERVAL_S: i64 = 60;

/// Predict every pass inside the window that clears the filter.
///
/// `sats` is one [`sdroxide_solar::Satellite`] per bird, already resolved to the
/// freshest element set available; `freq_for` answers what a bird's downlink is
/// (zero when nothing knows). A bird with no frequency is still listed — the
/// row explains itself — but [`WxPass::is_tunable`] refuses to arm it.
pub fn predict(
    sats: &[sdroxide_solar::Satellite],
    cfg: &WxSchedConfig,
    observer: (f64, f64),
    now_unix: i64,
    freq_for: impl Fn(u64) -> f64,
) -> Vec<WxPass> {
    // A pass every hundred minutes per bird, four birds, a week: comfortably
    // more than any horizon can produce, and a ceiling all the same.
    const MAX_PER_SAT: usize = 128;

    let mut out = Vec::new();
    for sat in sats {
        let found =
            sat.next_passes(observer.0, observer.1, now_unix as f64, cfg.horizon_s(), MAX_PER_SAT);
        let sdroxide_solar::PassSearch::Passes(passes) = found else {
            // An APT bird is in a polar LEO: it neither sits still nor stays
            // away. Either answer here means the element set is unusable.
            continue;
        };
        let downlink_hz = freq_for(sat.norad_id);
        for p in passes {
            if (p.max_el as f32) < cfg.min_max_el {
                continue;
            }
            out.push(WxPass {
                norad_id: sat.norad_id,
                name: sat.name.clone(),
                aos_unix: p.rise_unix,
                los_unix: p.set_unix,
                max_el: p.max_el as f32,
                rise_az: p.rise_az as f32,
                set_az: p.set_az as f32,
                downlink_hz,
                armed: false,
                state: WxJobState::default(),
                png: String::new(),
                wav: String::new(),
                note: String::new(),
            });
        }
    }
    out.sort_by(|a, b| a.aos_unix.cmp(&b.aos_unix).then(a.norad_id.cmp(&b.norad_id)));
    out
}

/// Fold the operator's ticks into a prediction, and carry forward any armed
/// pass the prediction no longer has.
///
/// The second half is the point. The filter is a view, not a commitment: an
/// operator who arms a 15° pass and then raises the minimum to 30° has not
/// cancelled it, and a row that quietly disappeared would be a recording they
/// are still expecting. The same goes for a finished job whose pass has slid
/// out of the back of the window — that is where the picture is.
pub fn merge(
    mut passes: Vec<WxPass>,
    sched: &WxSchedule,
    freq_for: impl Fn(u64) -> f64,
) -> Vec<WxPass> {
    for p in &mut passes {
        let Some(j) = sched.job(p.norad_id, p.aos_unix) else { continue };
        p.armed = true;
        p.state = j.state;
        p.png = j.png.clone();
        p.wav = j.wav.clone();
        p.note = j.note.clone();
    }
    for j in &sched.jobs {
        if passes.iter().any(|p| j.is(p.norad_id, p.aos_unix)) {
            continue;
        }
        passes.push(WxPass {
            norad_id: j.norad_id,
            name: j.name.clone(),
            aos_unix: j.aos_unix,
            los_unix: j.los_unix,
            max_el: j.max_el,
            // Unknown: the pass is no longer predicted, so there is no geometry
            // left to read these off. The frequency still resolves, which is
            // what a finished row needs in order to say what it listened on.
            rise_az: 0.0,
            set_az: 0.0,
            downlink_hz: freq_for(j.norad_id),
            armed: true,
            state: j.state,
            png: j.png.clone(),
            wav: j.wav.clone(),
            note: j.note.clone(),
        });
    }
    passes.sort_by(|a, b| a.aos_unix.cmp(&b.aos_unix).then(a.norad_id.cmp(&b.norad_id)));
    passes
}

/// When an armed pass's recording should start and stop, with the pre-roll and
/// post-roll applied.
pub fn window(job: &sdroxide_types::WxJob, cfg: &WxSchedConfig) -> (i64, i64) {
    (job.aos_unix - i64::from(cfg.lead_s), job.los_unix + i64::from(cfg.trail_s))
}

/// Index of the armed pass the radio should be on at `now`, if any.
///
/// Earliest start wins. Two birds genuinely overlap several times a week, and
/// there is one receiver: picking the one that started first means the pass
/// already being recorded is seen through to its end rather than abandoned
/// half-decoded for a newer one.
pub fn due(sched: &WxSchedule, now: i64) -> Option<usize> {
    sched
        .jobs
        .iter()
        .enumerate()
        .filter(|(_, j)| j.state.is_pending())
        .filter(|(_, j)| {
            let (from, to) = window(j, &sched.cfg);
            now >= from && now <= to
        })
        .min_by_key(|(_, j)| (window(j, &sched.cfg).0, j.norad_id))
        .map(|(i, _)| i)
}

/// Armed passes whose window has gone by without anything being recorded.
///
/// Marked rather than dropped: "the radio was busy" and "the bird never
/// appeared" look identical from the gallery, and a row that says *missed* is
/// the only way an operator finds out that an overnight schedule did not run.
pub fn missed(sched: &WxSchedule, now: i64) -> Vec<usize> {
    sched
        .jobs
        .iter()
        .enumerate()
        .filter(|(_, j)| j.state == WxJobState::Planned && window(j, &sched.cfg).1 < now)
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdroxide_types::WxJob;

    /// NOAA 19, epoch 2026-01-31. Real elements, so the geometry below is the
    /// geometry — a made-up TLE propagates to nonsense and tests nothing.
    const NOAA19: &str = "NOAA 19\n\
         1 33591U 09005A   26031.51268519  .00000271  00000-0  16472-3 0  9992\n\
         2 33591  99.0361 121.3384 0013431 262.5195  97.4595 14.13096410877269";

    fn noaa19() -> sdroxide_solar::Satellite {
        sdroxide_solar::satellites::parse_pasted_tles(NOAA19).into_iter().next().expect("elements")
    }

    /// Epoch day of the element set above, so the prediction runs where the
    /// propagator is accurate rather than years away from it.
    const AT_EPOCH: i64 = 1_769_904_000; // 2026-01-31T00:00:00Z

    /// Cologne-ish, which is where the station this was written for is.
    const QTH: (f64, f64) = (50.94, 6.96);

    /// A day's worth of NOAA 19 over one QTH: several passes, each one a real
    /// pass (AOS before LOS, culminating in between), and every one of them
    /// above the filter.
    #[test]
    fn a_day_of_passes_comes_back_in_order_and_above_the_floor() {
        let cfg = WxSchedConfig { horizon_h: 24, min_max_el: 20.0, ..Default::default() };
        let passes = predict(&[noaa19()], &cfg, QTH, AT_EPOCH, |_| 137.1e6);

        assert!(!passes.is_empty(), "a polar bird passes a mid-latitude QTH several times a day");
        assert!(passes.len() <= 8, "a day cannot hold more than this: {}", passes.len());
        for p in &passes {
            assert!(p.aos_unix < p.los_unix, "a pass has to end after it starts");
            assert!(p.duration_s() > 120, "a 20°+ pass is minutes long, not seconds");
            assert!(p.duration_s() < 2000);
            assert!(p.max_el >= cfg.min_max_el, "{} is below the floor", p.max_el);
            assert_eq!(p.downlink_hz, 137.1e6);
            assert!(p.is_tunable());
            assert!(!p.armed, "nothing is armed until the operator says so");
        }
        for w in passes.windows(2) {
            assert!(w[0].aos_unix <= w[1].aos_unix, "earliest first");
        }

        // Raising the floor can only take passes away, never add them.
        let higher = predict(
            &[noaa19()],
            &WxSchedConfig { min_max_el: 60.0, ..cfg.clone() },
            QTH,
            AT_EPOCH,
            |_| 137.1e6,
        );
        assert!(higher.len() < passes.len(), "a 60° floor is stricter than a 20° one");
        assert!(higher.iter().all(|p| p.max_el >= 60.0));
    }

    /// A bird nothing can name a frequency for is still listed, so the row can
    /// explain itself — but it cannot be armed, because there is nowhere to
    /// point the radio.
    #[test]
    fn a_pass_with_no_frequency_is_listed_but_not_tunable() {
        let passes = predict(&[noaa19()], &WxSchedConfig::default(), QTH, AT_EPOCH, |_| 0.0);
        assert!(!passes.is_empty());
        assert!(passes.iter().all(|p| !p.is_tunable()));
    }

    /// The bug this exists to catch: the prediction is recomputed every minute
    /// from whatever element set is freshest, so an armed pass's AOS moves. If
    /// the tick did not follow it, the row would come back unarmed and the
    /// recording would never happen.
    #[test]
    fn an_armed_tick_follows_its_pass_through_a_refresh() {
        let cfg = WxSchedConfig::default();
        let passes = predict(&[noaa19()], &cfg, QTH, AT_EPOCH, |_| 137.1e6);
        let first = passes[0].clone();

        // Armed against the AOS as it was predicted a minute ago.
        let sched = WxSchedule {
            cfg: cfg.clone(),
            jobs: vec![WxJob {
                norad_id: first.norad_id,
                name: first.name.clone(),
                aos_unix: first.aos_unix - 37,
                los_unix: first.los_unix - 37,
                max_el: first.max_el,
                state: WxJobState::Planned,
                ..Default::default()
            }],
        };
        let merged = merge(passes.clone(), &sched, |_| 137.1e6);
        assert_eq!(merged.len(), passes.len(), "no phantom row was added");
        assert!(merged[0].armed, "the tick followed its pass");
        assert!(merged[1..].iter().all(|p| !p.armed), "and only that one");
    }

    /// Tightening the filter is a change of view, not a cancellation: the pass
    /// the operator already ticked stays on the list, or they are waiting for a
    /// recording that is not coming.
    #[test]
    fn an_armed_pass_outside_the_filter_is_still_shown() {
        let sched = WxSchedule {
            cfg: WxSchedConfig::default(),
            jobs: vec![WxJob {
                norad_id: 25_338,
                name: "NOAA 15".into(),
                aos_unix: AT_EPOCH + 500,
                los_unix: AT_EPOCH + 1200,
                max_el: 12.0,
                state: WxJobState::Done,
                png: "apt-1-NOAA_15.png".into(),
                wav: "apt-1-NOAA_15.wav".into(),
                note: "640 lines".into(),
            }],
        };
        // A prediction that knows nothing about it: a different bird entirely.
        let merged =
            merge(predict(&[noaa19()], &sched.cfg, QTH, AT_EPOCH, |_| 137.1e6), &sched, |_| {
                137.62e6
            });
        let kept = merged.iter().find(|p| p.norad_id == 25_338).expect("the armed pass survived");
        assert!(kept.armed);
        assert_eq!(kept.state, WxJobState::Done);
        assert_eq!(kept.png, "apt-1-NOAA_15.png");
        assert_eq!(kept.wav, "apt-1-NOAA_15.wav");
        assert_eq!(kept.downlink_hz, 137.62e6, "what it listened on, resolved afresh");
        for w in merged.windows(2) {
            assert!(w[0].aos_unix <= w[1].aos_unix, "the carried row sorted into place");
        }
    }

    /// One receiver, so two overlapping passes are not a choice to be made
    /// every tick: the one already running wins until it is over.
    #[test]
    fn overlapping_passes_do_not_fight_over_the_radio() {
        let now = 1_800_000_000;
        let sched = WxSchedule {
            cfg: WxSchedConfig { lead_s: 60, trail_s: 60, ..Default::default() },
            jobs: vec![
                WxJob {
                    norad_id: 33_591,
                    aos_unix: now + 120,
                    los_unix: now + 900,
                    state: WxJobState::Planned,
                    ..Default::default()
                },
                WxJob {
                    norad_id: 25_338,
                    aos_unix: now,
                    los_unix: now + 600,
                    state: WxJobState::Planned,
                    ..Default::default()
                },
            ],
        };
        // Before either lead-in: nothing is due.
        assert_eq!(due(&sched, now - 300), None);
        // Inside NOAA 15's lead-in only.
        assert_eq!(due(&sched, now - 30).map(|i| sched.jobs[i].norad_id), Some(25_338));
        // Both windows open: the earlier start keeps the radio.
        assert_eq!(due(&sched, now + 300).map(|i| sched.jobs[i].norad_id), Some(25_338));
        // NOAA 15 is over; NOAA 19 is still running.
        assert_eq!(due(&sched, now + 700).map(|i| sched.jobs[i].norad_id), Some(33_591));
        // And once the post-roll has gone by, nothing again.
        assert_eq!(due(&sched, now + 2000), None);
    }

    /// An overnight schedule that did not run has to say so. A `Planned` job
    /// whose window closed is reported, and one that actually recorded is left
    /// alone however old.
    #[test]
    fn a_window_that_closed_without_recording_is_reported_missed() {
        let now = 1_800_000_000;
        let sched = WxSchedule {
            cfg: WxSchedConfig { lead_s: 60, trail_s: 60, ..Default::default() },
            jobs: vec![
                WxJob {
                    norad_id: 25_338,
                    aos_unix: now - 5000,
                    los_unix: now - 4000,
                    state: WxJobState::Planned,
                    ..Default::default()
                },
                WxJob {
                    norad_id: 28_654,
                    aos_unix: now - 5000,
                    los_unix: now - 4000,
                    state: WxJobState::Done,
                    ..Default::default()
                },
                WxJob {
                    norad_id: 33_591,
                    aos_unix: now + 500,
                    los_unix: now + 1200,
                    state: WxJobState::Planned,
                    ..Default::default()
                },
            ],
        };
        let missed: Vec<u64> =
            missed(&sched, now).into_iter().map(|i| sched.jobs[i].norad_id).collect();
        assert_eq!(missed, vec![25_338], "only the one that was planned and is now past");
    }
}
