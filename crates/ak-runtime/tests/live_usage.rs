//! Live check that `claude -p /usage` still prints windows the app can read.
//! Ignored by default because it needs a logged-in Claude subscription; it
//! calls no model, so it costs nothing. Uses the binary from the app's own
//! settings when they name one.
//!
//! ```bash
//! cargo test -p ak-runtime --test live_usage -- --ignored --nocapture
//! ```

use ak_core::usage::{UsageSnapshot, Window};
use ak_runtime::{claude, config, HomePaths};

#[tokio::test]
#[ignore = "needs a Claude subscription login"]
async fn usage_report_parses_with_real_claude_code() {
    let configured = HomePaths::resolve()
        .ok()
        .and_then(|p| config::load_home_config(&p.config_file()).ok())
        .and_then(|c| c.settings.claude_binary);
    let binary = claude::locate(configured.as_deref()).expect("Claude Code binary");
    let tmp = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let text = claude::usage_report(&binary, tmp.path())
        .await
        .expect("/usage ran");
    println!(
        "binary: {}  took {:.1}s\n----- /usage -----\n{text}\n------------------",
        binary.display(),
        started.elapsed().as_secs_f32()
    );
    let snapshot = UsageSnapshot::from_usage_text(&text, chrono::Local::now().fixed_offset())
        .expect("windows in the report");
    for w in Window::ALL {
        if let Some(u) = snapshot.windows.get(&w) {
            println!(
                "{:<14} {:>5.1}%  resets {:?}",
                w.label(),
                u.used * 100.0,
                u.resets_at
            );
        }
    }
    let five = snapshot
        .windows
        .get(&Window::FiveHour)
        .expect("5-hour window");
    assert!(five.resets_at.is_some(), "the reset time parsed");
    assert!(snapshot.windows.contains_key(&Window::Week));
}
