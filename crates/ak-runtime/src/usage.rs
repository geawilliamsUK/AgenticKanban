//! The account's subscription usage, as every run and refresh reports it.
//!
//! One tracker serves every project, because the windows belong to the
//! Claude login, not to a project. It keeps the latest snapshot on disk so
//! the meter has something to show at startup, and notes which runs
//! overlapped so that only runs that ran alone teach the calibration.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;

use ak_core::ids::RunId;
use ak_core::usage::{Calibration, UsageHolds, UsageSnapshot, Window, WindowUsage};
use ak_core::Timestamp;
use tokio::sync::watch;

/// A snapshot older than this is no baseline for a run starting now: other
/// use of the account may have moved it since.
const FRESH_FOR: chrono::Duration = chrono::Duration::minutes(10);

pub struct UsageTracker {
    file: PathBuf,
    state: Mutex<State>,
    /// The rates that turn a run's cost into shares, and the holds in
    /// force, as the app last set them; for the board's usage_get.
    calibration: Mutex<Calibration>,
    holds: Mutex<UsageHolds>,
    tx: watch::Sender<Option<UsageSnapshot>>,
}

struct State {
    latest: Option<UsageSnapshot>,
    /// Runs in progress, and whether each has been alone so far.
    active: HashMap<RunId, bool>,
}

impl UsageTracker {
    /// Starts from the snapshot saved at `file`, if any.
    pub fn load(file: PathBuf) -> Self {
        let latest = std::fs::read_to_string(&file)
            .ok()
            .and_then(|s| serde_json::from_str::<UsageSnapshot>(&s).ok());
        let (tx, _) = watch::channel(latest.clone());
        Self {
            file,
            state: Mutex::new(State {
                latest,
                active: HashMap::new(),
            }),
            tx,
            calibration: Mutex::new(Calibration::default()),
            holds: Mutex::new(UsageHolds::default()),
        }
    }

    pub fn set_calibration(&self, c: Calibration) {
        *self.calibration.lock().expect("usage") = c;
    }

    pub fn calibration(&self) -> Calibration {
        self.calibration.lock().expect("usage").clone()
    }

    pub fn set_holds(&self, h: UsageHolds) {
        *self.holds.lock().expect("usage") = h;
    }

    pub fn holds(&self) -> UsageHolds {
        *self.holds.lock().expect("usage")
    }

    pub fn latest(&self) -> Option<UsageSnapshot> {
        self.state.lock().expect("usage").latest.clone()
    }

    /// Fires whenever a new report lands.
    pub fn subscribe(&self) -> watch::Receiver<Option<UsageSnapshot>> {
        self.tx.subscribe()
    }

    /// Runs of this app in progress.
    pub fn active(&self) -> usize {
        self.state.lock().expect("usage").active.len()
    }

    /// Takes a report from a run or a refresh.
    pub fn observe(&self, snapshot: UsageSnapshot) {
        let merged = {
            let mut state = self.state.lock().expect("usage");
            match state.latest.as_mut() {
                Some(latest) => latest.merge(snapshot),
                None => state.latest = Some(snapshot),
            }
            state.latest.clone()
        };
        if let Some(s) = &merged {
            if let Ok(json) = serde_json::to_string_pretty(s) {
                if let Err(e) = std::fs::write(&self.file, json) {
                    tracing::warn!("could not save usage to {}: {e}", self.file.display());
                }
            }
        }
        self.tx.send_replace(merged);
    }

    /// Notes a run starting and returns its baseline: the windows as last
    /// reported, if that was recent enough to stand for "before".
    pub fn begin(&self, run: RunId, now: Timestamp) -> Option<BTreeMap<Window, WindowUsage>> {
        let mut state = self.state.lock().expect("usage");
        let alone = state.active.is_empty();
        for solo in state.active.values_mut() {
            *solo = false;
        }
        state.active.insert(run, alone);
        state
            .latest
            .as_ref()
            .filter(|s| now - s.observed_at <= FRESH_FOR)
            .map(|s| s.windows.clone())
    }

    /// Notes a run ending. Returns the windows as last reported and whether
    /// the run was alone throughout.
    pub fn end(&self, run: RunId) -> (BTreeMap<Window, WindowUsage>, bool) {
        let mut state = self.state.lock().expect("usage");
        let solo = state.active.remove(&run).unwrap_or(false);
        let windows = state
            .latest
            .as_ref()
            .map(|s| s.windows.clone())
            .unwrap_or_default();
        (windows, solo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn snapshot(five_hour: f64, at: Timestamp) -> UsageSnapshot {
        UsageSnapshot::from_rate_limit_info(
            &json!({"status": "allowed", "unifiedWindows": {
                "five_hour": {"utilization": five_hour, "resetsAt": 1893456000}}}),
            at,
        )
        .unwrap()
    }

    #[test]
    fn overlapping_runs_are_not_solo_and_the_snapshot_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("usage.json");
        let t = UsageTracker::load(file.clone());
        let now = Utc::now();
        let first = RunId::new();
        assert!(t.begin(first, now).is_none(), "nothing reported yet");
        assert!(t.end(first).0.is_empty());

        t.observe(snapshot(0.2, now));
        let (a, b, c) = (RunId::new(), RunId::new(), RunId::new());
        // An old report is no baseline.
        let stale = t.begin(a, now + chrono::Duration::minutes(30));
        assert!(stale.is_none());
        let (_, solo_a) = t.end(a);
        assert!(solo_a);

        assert!(t.begin(b, now).is_some());
        t.begin(c, now);
        t.observe(snapshot(0.25, now));
        let (after, solo_b) = t.end(b);
        assert!(!solo_b);
        assert_eq!(after[&Window::FiveHour].used, 0.25);
        assert!(!t.end(c).1);
        assert_eq!(t.active(), 0);

        let reloaded = UsageTracker::load(file);
        assert_eq!(
            reloaded.latest().unwrap().used(Window::FiveHour, now),
            Some(0.25)
        );
    }
}
