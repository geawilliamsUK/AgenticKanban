//! Subscription usage: the account's rolling limit windows as Claude Code
//! reports them, and estimates of how much of each a run used.
//!
//! Claude Code prints a `rate_limit_event` near the start of every headless
//! run, and again when the numbers move. Its `unifiedWindows` object (not in
//! the documentation, checked against 2.1.302 and 2.1.314 on 2026-10-30)
//! carries each window's utilization as a fraction and its reset time in
//! epoch seconds. `claude -p /usage` prints the same windows as text without
//! calling a model, which is how the app refreshes them while nothing runs.
//!
//! The account is shared by everything the human does with Claude, so a run
//! cannot be read off directly. Instead the observed rise across runs that
//! ran alone is set against their API-equivalent cost, per window and model
//! family, and every run's cost is converted with that rate.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Timestamp;

/// One of the account's rolling limit windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Window {
    FiveHour,
    Week,
    /// The weekly window that only Fable draws on.
    WeekFable,
}

impl Window {
    pub const ALL: [Window; 3] = [Window::FiveHour, Window::Week, Window::WeekFable];

    pub fn label(&self) -> &'static str {
        match self {
            Window::FiveHour => "5-hour",
            Window::Week => "Week",
            Window::WeekFable => "Fable week",
        }
    }

    /// The key Claude Code uses in `unifiedWindows` and `rateLimitType`.
    /// `seven_day_overage_included` appears only on Fable runs and matched
    /// `/usage`'s "Current week (Fable)" figure; the name does not say so.
    fn from_wire(key: &str) -> Option<Self> {
        match key {
            "five_hour" => Some(Window::FiveHour),
            "seven_day" => Some(Window::Week),
            "seven_day_overage_included" | "seven_day_fable" => Some(Window::WeekFable),
            _ => None,
        }
    }

    /// Whether a run on `model` draws on this window.
    pub fn applies_to(&self, model: Option<&str>) -> bool {
        match self {
            Window::WeekFable => model_family(model) == "fable",
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowUsage {
    /// Fraction of the window used, 0 to 1.
    pub used: f64,
    pub resets_at: Option<Timestamp>,
}

impl WindowUsage {
    /// What is used now: nothing once the reset has passed.
    pub fn current(&self, now: Timestamp) -> f64 {
        match self.resets_at {
            Some(t) if t <= now => 0.0,
            _ => self.used,
        }
    }

    /// Whether `later` is in a period that began after this one ended. Reset
    /// times from the two sources differ by a minute or so, hence the slack.
    fn period_ended_before(&self, later: &WindowUsage) -> bool {
        match (self.resets_at, later.resets_at) {
            (Some(a), Some(b)) => b > a + chrono::Duration::minutes(30),
            _ => false,
        }
    }
}

/// The account's windows as last reported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub observed_at: Timestamp,
    pub windows: BTreeMap<Window, WindowUsage>,
    /// Claude Code is refusing requests until then: a limit is reached.
    #[serde(default)]
    pub rejected_until: Option<Timestamp>,
}

impl UsageSnapshot {
    /// Reads the `rate_limit_info` object of a `rate_limit_event`.
    pub fn from_rate_limit_info(info: &Value, now: Timestamp) -> Option<Self> {
        let reset = |v: &Value| {
            v.get("resetsAt")
                .and_then(|t| t.as_i64())
                .and_then(|s| chrono::Utc.timestamp_opt(s, 0).single())
        };
        let mut windows = BTreeMap::new();
        if let Some(unified) = info.get("unifiedWindows").and_then(|u| u.as_object()) {
            for (key, w) in unified {
                let (Some(window), Some(used)) = (
                    Window::from_wire(key),
                    w.get("utilization").and_then(|u| u.as_f64()),
                ) else {
                    continue;
                };
                windows.insert(
                    window,
                    WindowUsage {
                        used,
                        resets_at: reset(w),
                    },
                );
            }
        }
        // Older shape: only the window nearest its limit, at the top level.
        if windows.is_empty() {
            let window = info
                .get("rateLimitType")
                .and_then(|t| t.as_str())
                .and_then(Window::from_wire)?;
            let used = info.get("utilization").and_then(|u| u.as_f64())?;
            windows.insert(
                window,
                WindowUsage {
                    used,
                    resets_at: reset(info),
                },
            );
        }
        let rejected = info.get("status").and_then(|s| s.as_str()) == Some("rejected");
        Some(Self {
            observed_at: now,
            windows,
            rejected_until: if rejected { reset(info) } else { None },
        })
    }

    /// Reads the text `claude -p /usage` prints. Reset times are written in
    /// the human's local time without a year; `now` supplies both.
    pub fn from_usage_text(text: &str, now: DateTime<FixedOffset>) -> Option<Self> {
        let mut windows = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            let Some((head, rest)) = line.split_once(':') else {
                continue;
            };
            let window = match head.trim() {
                "Current session" => Window::FiveHour,
                h if h.starts_with("Current week") => {
                    let scope = h
                        .trim_start_matches("Current week")
                        .trim()
                        .trim_matches(|c| c == '(' || c == ')')
                        .to_ascii_lowercase();
                    if scope.is_empty() || scope == "all models" {
                        Window::Week
                    } else if scope.contains("fable") {
                        Window::WeekFable
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };
            let Some(pct) = rest
                .split('%')
                .next()
                .and_then(|n| n.trim().parse::<f64>().ok())
            else {
                continue;
            };
            let resets_at = rest
                .split_once("resets")
                .and_then(|(_, r)| parse_reset(r, now));
            windows.insert(
                window,
                WindowUsage {
                    used: pct / 100.0,
                    resets_at,
                },
            );
        }
        if windows.is_empty() {
            return None;
        }
        Some(Self {
            observed_at: now.with_timezone(&chrono::Utc),
            windows,
            rejected_until: None,
        })
    }

    /// Takes a newer report. Windows it leaves out, as a non-Fable run leaves
    /// out the Fable week, keep their last value.
    pub fn merge(&mut self, newer: UsageSnapshot) {
        self.observed_at = newer.observed_at;
        self.rejected_until = newer.rejected_until;
        self.windows.extend(newer.windows);
    }

    pub fn used(&self, window: Window, now: Timestamp) -> Option<f64> {
        self.windows.get(&window).map(|w| w.current(now))
    }

    pub fn is_rejected(&self, now: Timestamp) -> bool {
        self.rejected_until.is_some_and(|t| t > now)
    }
}

/// Parses `" Oct 31, 12:59am (Europe/London)"` or `" 3pm"` into the next
/// matching instant at or after a day before `now`.
fn parse_reset(s: &str, now: DateTime<FixedOffset>) -> Option<Timestamp> {
    let s = s.split('(').next()?.trim().trim_end_matches(',');
    let (date_part, time_part) = match s.rsplit_once(',') {
        Some((d, t)) => (Some(d.trim()), t.trim()),
        None => (None, s),
    };
    let time = time_part.to_ascii_lowercase();
    let (clock, pm) = match time.strip_suffix("am") {
        Some(c) => (c, false),
        None => (time.strip_suffix("pm")?, true),
    };
    let (h, m) = match clock.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (clock.parse::<u32>().ok()?, 0),
    };
    if h == 0 || h > 12 || m > 59 {
        return None;
    }
    let hour = match (h, pm) {
        (12, false) => 0,
        (12, true) => 12,
        (h, false) => h,
        (h, true) => h + 12,
    };
    let at = |date: NaiveDate| {
        date.and_hms_opt(hour, m, 0)
            .and_then(|t| now.timezone().from_local_datetime(&t).single())
            .map(|t| t.with_timezone(&chrono::Utc))
    };
    let floor = now.with_timezone(&chrono::Utc) - chrono::Duration::days(1);
    match date_part {
        Some(d) => {
            let (month, day) = d.split_once(' ')?;
            let month = month_number(month)?;
            let day = day.trim().parse::<u32>().ok()?;
            [now.year(), now.year() + 1]
                .into_iter()
                .filter_map(|y| NaiveDate::from_ymd_opt(y, month, day))
                .filter_map(at)
                .find(|t| *t >= floor)
        }
        None => {
            let today = now.date_naive();
            [today, today.succ_opt()?]
                .into_iter()
                .filter_map(at)
                .find(|t| *t >= now.with_timezone(&chrono::Utc))
        }
    }
}

fn month_number(name: &str) -> Option<u32> {
    let names = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = name.trim().to_ascii_lowercase();
    names
        .iter()
        .position(|n| lower.starts_with(n))
        .map(|i| i as u32 + 1)
}

/// The model family a rate is learned for: fable, opus, sonnet, haiku, or
/// other (including Claude Code's default when no model was named).
pub fn model_family(model: Option<&str>) -> &'static str {
    let m = model.unwrap_or("").to_ascii_lowercase();
    ["fable", "opus", "sonnet", "haiku"]
        .into_iter()
        .find(|f| m.contains(f))
        .unwrap_or("other")
}

/// What the account's windows looked like either side of one run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunUsage {
    /// The model Claude Code reported in its init message.
    pub model: Option<String>,
    pub before: BTreeMap<Window, WindowUsage>,
    pub after: BTreeMap<Window, WindowUsage>,
    /// No other run of this app was in progress at any point during this one.
    pub solo: bool,
}

impl RunUsage {
    /// The rise in each window across the run. A window that reset during
    /// the run counts from zero; one not seen before the run is unknown.
    pub fn observed(&self) -> BTreeMap<Window, f64> {
        let mut out = BTreeMap::new();
        for (w, after) in &self.after {
            let Some(before) = self.before.get(w) else {
                continue;
            };
            let rise = if before.period_ended_before(after) {
                after.used
            } else {
                after.used - before.used
            };
            out.insert(*w, rise.max(0.0));
        }
        out
    }
}

/// Window fraction per API-equivalent dollar, learned from solo runs.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rate {
    pub per_usd: f64,
    pub runs: u32,
    pub usd: f64,
    /// Total observed rise behind the rate.
    pub rise: f64,
}

impl Rate {
    /// Reports come in whole percent, so a rate needs a few points of rise,
    /// or enough spend that seeing none is itself evidence, before use.
    pub fn is_usable(&self) -> bool {
        self.runs > 0 && (self.rise >= 0.03 || self.usd >= 5.0)
    }

    fn add(&mut self, usd: f64, rise: f64) {
        self.runs += 1;
        self.usd += usd;
        self.rise += rise;
        self.per_usd = self.rise / self.usd;
    }
}

/// Learned rates per window, by model family and overall.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Calibration {
    pub by_family: BTreeMap<(Window, &'static str), Rate>,
    pub overall: BTreeMap<Window, Rate>,
}

impl Calibration {
    /// Learns from runs with their cost. Only runs that ran alone and cost
    /// something count; the Fable week learns only from Fable runs.
    pub fn fit<'a>(runs: impl IntoIterator<Item = (&'a RunUsage, f64)>) -> Self {
        let mut c = Self::default();
        for (usage, usd) in runs {
            if !usage.solo || usd <= 0.0 {
                continue;
            }
            let family = model_family(usage.model.as_deref());
            for (w, rise) in usage.observed() {
                if !w.applies_to(usage.model.as_deref()) {
                    continue;
                }
                c.by_family.entry((w, family)).or_default().add(usd, rise);
                c.overall.entry(w).or_default().add(usd, rise);
            }
        }
        c
    }

    /// The rate for runs on `model`: its family's when there is enough data,
    /// else the overall one.
    pub fn rate(&self, window: Window, model: Option<&str>) -> Option<Rate> {
        let family = model_family(model);
        self.by_family
            .get(&(window, family))
            .filter(|r| r.is_usable())
            .or_else(|| self.overall.get(&window).filter(|r| r.is_usable()))
            .copied()
    }

    /// Estimated fraction of `window` a run on `model` costing `usd` used.
    pub fn estimate(&self, window: Window, model: Option<&str>, usd: f64) -> Option<f64> {
        if !window.applies_to(model) {
            return Some(0.0);
        }
        self.rate(window, model).map(|r| r.per_usd * usd)
    }

    /// Estimated share of each window used by runs, given as model and cost.
    /// A window no rate is known for yet is left out.
    pub fn estimate_total<'a>(
        &self,
        runs: impl IntoIterator<Item = (Option<&'a str>, f64)>,
    ) -> BTreeMap<Window, f64> {
        let mut out = BTreeMap::new();
        for (model, usd) in runs {
            for w in Window::ALL {
                if let Some(e) = self.estimate(w, model, usd) {
                    *out.entry(w).or_insert(0.0) += e;
                }
            }
        }
        // Only the Fable week was answered (with zero, by the non-Fable
        // runs); that says nothing when no rate is known at all.
        if self.overall.values().all(|r| !r.is_usable()) {
            out.clear();
        }
        out
    }
}

/// Thresholds, in percent, past which the scheduler starts no new runs.
/// Zero turns one off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageHolds {
    pub five_hour_pct: u32,
    pub week_pct: u32,
    pub fable_week_pct: u32,
}

impl Default for UsageHolds {
    fn default() -> Self {
        Self {
            five_hour_pct: 90,
            week_pct: 95,
            fable_week_pct: 95,
        }
    }
}

impl UsageHolds {
    fn threshold(&self, window: Window) -> u32 {
        match window {
            Window::FiveHour => self.five_hour_pct,
            Window::Week => self.week_pct,
            Window::WeekFable => self.fable_week_pct,
        }
    }

    /// Why a run on `model` should wait, if it should.
    pub fn hold_reason(
        &self,
        snapshot: &UsageSnapshot,
        model: Option<&str>,
        now: Timestamp,
    ) -> Option<String> {
        if let Some(until) = snapshot.rejected_until.filter(|t| *t > now) {
            return Some(format!(
                "Claude reports a usage limit reached until {}",
                until.format("%Y-%m-%d %H:%M UTC")
            ));
        }
        for window in Window::ALL {
            let limit = self.threshold(window);
            if limit == 0 || !window.applies_to(model) {
                continue;
            }
            let Some(w) = snapshot.windows.get(&window) else {
                continue;
            };
            let used = w.current(now) * 100.0;
            if used >= limit as f64 {
                let resets = w
                    .resets_at
                    .map(|t| format!("; resets {}", t.format("%Y-%m-%d %H:%M UTC")))
                    .unwrap_or_default();
                return Some(format!(
                    "the {} window is at {used:.0}%, holding at {limit}%{resets}",
                    window.label()
                ));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(s: i64) -> Timestamp {
        chrono::Utc.timestamp_opt(s, 0).unwrap()
    }

    /// The event from a real Fable run on 2026-10-30.
    fn fable_event() -> Value {
        json!({
            "status": "allowed_warning", "resetsAt": 1793534400, "rateLimitType": "seven_day",
            "utilization": 0.78, "overageStatus": "rejected", "isUsingOverage": false,
            "surpassedThreshold": 0.75,
            "unifiedWindows": {
                "five_hour": {"utilization": 0.01, "resetsAt": 1793408400, "status": "allowed"},
                "seven_day": {"utilization": 0.78, "resetsAt": 1793534400, "status": "allowed_warning"},
                "seven_day_overage_included": {"utilization": 0.1, "resetsAt": 1793534400, "status": "allowed"}
            },
            "overageEnabled": true
        })
    }

    #[test]
    fn reads_unified_windows() {
        let now = ts(1793400000);
        let s = UsageSnapshot::from_rate_limit_info(&fable_event(), now).unwrap();
        assert_eq!(s.windows.len(), 3);
        assert_eq!(s.used(Window::FiveHour, now), Some(0.01));
        assert_eq!(s.used(Window::Week, now), Some(0.78));
        assert_eq!(s.used(Window::WeekFable, now), Some(0.1));
        assert_eq!(s.windows[&Window::FiveHour].resets_at, Some(ts(1793408400)));
        assert!(!s.is_rejected(now));
        // After the 5-hour reset, nothing is used in it.
        assert_eq!(s.used(Window::FiveHour, ts(1793408401)), Some(0.0));
    }

    #[test]
    fn falls_back_to_the_top_level_window_and_notes_a_rejection() {
        let info = json!({"status": "rejected", "resetsAt": 1793408400,
                          "rateLimitType": "five_hour", "utilization": 1.0});
        let now = ts(1793400000);
        let s = UsageSnapshot::from_rate_limit_info(&info, now).unwrap();
        assert_eq!(s.used(Window::FiveHour, now), Some(1.0));
        assert!(s.is_rejected(now));
        assert!(!s.is_rejected(ts(1793408400)));
        assert!(UsageSnapshot::from_rate_limit_info(&json!({"status": "allowed"}), now).is_none());
    }

    #[test]
    fn reads_usage_text_from_both_versions() {
        // 2026-10-30 22:10 in London, which is UTC+0 by then.
        let now = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 30, 22, 10, 0)
            .unwrap();
        let text_302 = "You are currently using your subscription to power your Claude Code usage\n\n\
            Current session: 2% used · resets Oct 31, 12:59am (Europe/London)\n\
            Current week (all models): 79% used · resets Nov 1, 11:59am (Europe/London)\n\
            Current week (Fable): 11% used · resets Nov 1, 11:59am (Europe/London)\n\n\
            What's contributing to your limits usage?\n  100% of your usage came from subagent-heavy sessions\n";
        let s = UsageSnapshot::from_usage_text(text_302, now).unwrap();
        let utc = now.with_timezone(&chrono::Utc);
        assert_eq!(s.used(Window::FiveHour, utc), Some(0.02));
        assert_eq!(s.used(Window::Week, utc), Some(0.79));
        assert_eq!(s.used(Window::WeekFable, utc), Some(0.11));
        assert_eq!(
            s.windows[&Window::FiveHour].resets_at.unwrap().to_rfc3339(),
            "2026-10-31T00:59:00+00:00"
        );
        assert_eq!(
            s.windows[&Window::Week].resets_at.unwrap().to_rfc3339(),
            "2026-11-01T11:59:00+00:00"
        );

        let text_314 = "Current session: 2% used · resets Oct 31, 1am (Europe/London)\n\
            Current week (all models): 79% used · resets Nov 1, 12pm (Europe/London)\n";
        let s = UsageSnapshot::from_usage_text(text_314, now).unwrap();
        assert_eq!(
            s.windows[&Window::FiveHour].resets_at.unwrap().to_rfc3339(),
            "2026-10-31T01:00:00+00:00"
        );
        assert_eq!(
            s.windows[&Window::Week].resets_at.unwrap().to_rfc3339(),
            "2026-11-01T12:00:00+00:00"
        );
        assert!(UsageSnapshot::from_usage_text("Not logged in", now).is_none());
    }

    #[test]
    fn a_reset_in_january_seen_in_december_is_next_year() {
        let now = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 12, 30, 10, 0, 0)
            .unwrap();
        let t = parse_reset(" Jan 2, 9am (Europe/London)", now).unwrap();
        assert_eq!(t.to_rfc3339(), "2027-01-02T09:00:00+00:00");
        let t = parse_reset(" 3pm", now).unwrap();
        assert_eq!(t.to_rfc3339(), "2026-12-30T15:00:00+00:00");
    }

    #[test]
    fn merge_keeps_windows_a_newer_report_leaves_out() {
        let now = ts(1793400000);
        let mut s = UsageSnapshot::from_rate_limit_info(&fable_event(), now).unwrap();
        let later = json!({"status": "allowed", "unifiedWindows": {
            "five_hour": {"utilization": 0.05, "resetsAt": 1793408400},
            "seven_day": {"utilization": 0.8, "resetsAt": 1793534400}}});
        s.merge(UsageSnapshot::from_rate_limit_info(&later, ts(1793400600)).unwrap());
        assert_eq!(s.observed_at, ts(1793400600));
        assert_eq!(s.used(Window::FiveHour, now), Some(0.05));
        assert_eq!(s.used(Window::WeekFable, now), Some(0.1));
    }

    fn window(used: f64, resets: i64) -> WindowUsage {
        WindowUsage {
            used,
            resets_at: Some(ts(resets)),
        }
    }

    fn run(model: &str, before: f64, after: f64, solo: bool) -> RunUsage {
        RunUsage {
            model: Some(model.into()),
            before: [(Window::FiveHour, window(before, 1000))].into(),
            after: [(Window::FiveHour, window(after, 1000))].into(),
            solo,
        }
    }

    #[test]
    fn observed_rise_handles_a_reset_during_the_run() {
        let r = RunUsage {
            model: None,
            before: [
                (Window::FiveHour, window(0.40, 1000)),
                (Window::Week, window(0.50, 9000)),
            ]
            .into(),
            after: [
                (Window::FiveHour, window(0.03, 1000 + 5 * 3600)),
                (Window::Week, window(0.52, 9000)),
                (Window::WeekFable, window(0.1, 9000)),
            ]
            .into(),
            solo: true,
        };
        let o = r.observed();
        assert!((o[&Window::FiveHour] - 0.03).abs() < 1e-9);
        assert!((o[&Window::Week] - 0.02).abs() < 1e-9);
        // Not seen before the run: unknown, not ten points.
        assert!(!o.contains_key(&Window::WeekFable));
    }

    #[test]
    fn calibration_learns_from_solo_runs_by_family() {
        let runs = [
            (run("claude-sonnet-5-5", 0.10, 0.12, true), 1.0),
            (run("claude-sonnet-5-5", 0.12, 0.14, true), 1.0),
            // Overlapped another run: its rise is not its own.
            (run("claude-sonnet-5-5", 0.14, 0.30, false), 1.0),
            (run("claude-fable-5-1", 0.30, 0.31, true), 0.5),
        ];
        let c = Calibration::fit(runs.iter().map(|(u, c)| (u, *c)));
        let sonnet = c.rate(Window::FiveHour, Some("sonnet")).unwrap();
        assert_eq!(sonnet.runs, 2);
        assert!((sonnet.per_usd - 0.02).abs() < 1e-9);
        let est = c
            .estimate(Window::FiveHour, Some("claude-sonnet-5-5"), 2.0)
            .unwrap();
        assert!((est - 0.04).abs() < 1e-9);
        // Fable has one point of rise: not enough of its own, so the overall
        // rate (5 points over $2.50) stands in.
        let fable = c.rate(Window::FiveHour, Some("fable")).unwrap();
        assert_eq!(fable.runs, 3);
        assert!((fable.per_usd - 0.02).abs() < 1e-9);
        // Non-Fable runs never touch the Fable week.
        assert_eq!(
            c.estimate(Window::WeekFable, Some("sonnet"), 3.0),
            Some(0.0)
        );
        assert_eq!(c.estimate(Window::Week, Some("sonnet"), 3.0), None);
        assert_eq!(
            Calibration::default().estimate(Window::FiveHour, None, 1.0),
            None
        );

        let total = c.estimate_total([(Some("sonnet"), 1.0), (None, 0.5)]);
        assert!((total[&Window::FiveHour] - 0.03).abs() < 1e-9);
        assert_eq!(total[&Window::WeekFable], 0.0);
        assert!(!total.contains_key(&Window::Week));
        assert!(Calibration::default()
            .estimate_total([(Some("sonnet"), 1.0)])
            .is_empty());
    }

    #[test]
    fn holds_respect_thresholds_model_and_resets() {
        let now = ts(1793400000);
        let s = UsageSnapshot::from_rate_limit_info(&fable_event(), now).unwrap();
        let holds = UsageHolds::default();
        assert_eq!(holds.hold_reason(&s, Some("sonnet"), now), None);
        let tight = UsageHolds {
            week_pct: 75,
            ..holds
        };
        let why = tight.hold_reason(&s, Some("sonnet"), now).unwrap();
        assert!(why.contains("Week window is at 78%"), "{why}");
        // Past the weekly reset nothing holds.
        assert_eq!(tight.hold_reason(&s, Some("sonnet"), ts(1793534400)), None);
        let fable_only = UsageHolds {
            five_hour_pct: 0,
            week_pct: 0,
            fable_week_pct: 10,
        };
        assert!(fable_only
            .hold_reason(&s, Some("claude-fable-5-1"), now)
            .is_some());
        assert_eq!(fable_only.hold_reason(&s, Some("opus"), now), None);
    }
}
