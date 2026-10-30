//! A live check of the team analyst against the real Claude Code binary.
//! Ignored by default because it spends a real model call. Run deliberately:
//!
//! ```bash
//! cargo test -p ak-runtime --test live_analyst -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;

use ak_core::agent::AgentDefinition;
use ak_core::config::{GlobalSettings, GuardrailThresholds, PermissionProfile, ProjectPolicy};
use ak_runtime::analyst::{self, AgentStats, Digest, RunBrief, Target, TripLine};
use ak_runtime::{ClaudeCodeRunner, HomePaths, UtilityRunner};
use chrono::{Duration, Utc};

fn stats(handle: &str) -> AgentStats {
    AgentStats {
        handle: handle.into(),
        model: Some("sonnet".into()),
        profile: "trusted".into(),
        state: "idle".into(),
        ..Default::default()
    }
}

/// A week that reads like the first real team run: a lead that talks more
/// than it assigns, a developer blocked on the same approval, and a
/// guardrail that froze a ticket.
fn digest() -> Digest {
    let now = Utc::now();
    let mut lead = stats("lead");
    lead.runs = 14;
    lead.exits = BTreeMap::from([("Completed".into(), 12), ("Budget exceeded".into(), 2)]);
    lead.turns = 410;
    lead.cost_usd = 9.80;
    lead.input_tokens = 2_400_000;
    lead.output_tokens = 60_000;
    lead.tool_calls = 220;
    lead.board_calls = 205;
    lead.comments = 31;
    lead.messages = 26;
    lead.ticket_moves = 3;
    lead.mentions_sent = BTreeMap::from([("fullstack".into(), 19), ("quality".into(), 12)]);
    lead.mentions_received = 8;
    lead.idle_runs = 6;
    lead.skips = BTreeMap::from([("wake rate exceeded".into(), 5), ("cooling down".into(), 2)]);
    lead.trips = 1;
    lead.worst_runs = vec![RunBrief {
        started_at: now - Duration::hours(30),
        trigger: "3 wake item(s)".into(),
        exit: "Budget exceeded".into(),
        cost_usd: 1.95,
        turns: 100,
        error: Some("error_max_turns".into()),
        result: None,
    }];

    let mut dev = stats("fullstack");
    dev.runs = 9;
    dev.exits = BTreeMap::from([("Completed".into(), 8), ("Crashed".into(), 1)]);
    dev.turns = 260;
    dev.cost_usd = 7.10;
    dev.input_tokens = 1_500_000;
    dev.output_tokens = 90_000;
    dev.tool_calls = 310;
    dev.board_calls = 40;
    dev.tool_errors = 14;
    dev.commits = 11;
    dev.ticket_moves = 7;
    dev.comments = 9;
    dev.messages = 4;
    dev.mentions_received = 19;
    dev.approvals = 17;
    dev.approvals_denied = 1;
    dev.approval_wait_secs = 17 * 540;
    dev.approval_examples = vec![
        "Bash: cargo doc --no-deps".into(),
        "Bash: cargo doc --no-deps --open".into(),
        "Bash: cargo tree -d".into(),
        "Bash: git push origin ticket/WD-4".into(),
    ];
    dev.worst_runs = vec![RunBrief {
        started_at: now - Duration::hours(50),
        trigger: "1 wake item(s)".into(),
        exit: "Crashed".into(),
        cost_usd: 0.40,
        turns: 12,
        error: Some("process exited without a result: Failed to authenticate".into()),
        result: None,
    }];

    let mut quality = stats("quality");
    quality.runs = 6;
    quality.exits = BTreeMap::from([("Completed".into(), 6)]);
    quality.turns = 90;
    quality.cost_usd = 2.10;
    quality.tool_calls = 70;
    quality.board_calls = 30;
    quality.comments = 8;
    quality.mentions_received = 12;
    quality.idle_runs = 2;

    Digest {
        project: "Worktree Deck".into(),
        since: now - Duration::days(7),
        until: now,
        agents: vec![lead, dev, quality],
        trips: vec![
            TripLine {
                at: now - Duration::hours(40),
                rule: "comment storm".into(),
                action: "freeze ticket".into(),
                detail: "WD-7 had 15 comments in 30 minutes; frozen".into(),
            },
            TripLine {
                at: now - Duration::hours(20),
                rule: "wake storm".into(),
                action: "notify".into(),
                detail: "@lead reached 12 wakes in an hour; further wakes wait".into(),
            },
        ],
        frozen_tickets: vec!["WD-7".into()],
        thresholds: GuardrailThresholds::default(),
        profiles: PermissionProfile::defaults(),
        previous: vec![],
    }
}

fn definitions() -> Vec<AgentDefinition> {
    let mut lead = AgentDefinition::template("lead", "Team lead");
    lead.frontmatter.description =
        "Runs the team: triages, assigns, splits work, keeps the board true.".into();
    lead.body = "# @lead — team lead

You are **@lead**, who runs this project's agent team for @me.

**You own:** triage, assignment, the board's health.
**You review:** that approvals are in before a merge.
**You never:** write feature code or merge.
**Done means:** every ticket has an owner and a true column.

## Team

- **@fullstack** builds features. Give them scoped tickets.
- **@quality** owns the review gate.

## A standup

Run one whenever you are woken. Post a comment on the standup ticket and message every teammate about what they should do next.
"
    .into();
    let mut dev = AgentDefinition::template("fullstack", "Developer");
    dev.frontmatter.description = "Owns domain logic, persistence and the view wiring.".into();
    dev.body = "# @fullstack — developer

You are **@fullstack**, one of the agents on this project's team.

**You own:** `src/`, the tests for it.
**You review:** nothing.
**You never:** merge.
**Done means:** the ticket branch builds, tests pass, @quality is asked to review.

## Team

- **@lead** assigns your work.
- **@quality** reviews it.
"
    .into();
    let mut quality = AgentDefinition::template("quality", "Quality");
    quality.frontmatter.description = "Owns the test suites and the review gate.".into();
    quality.body = "# @quality — quality

You are **@quality**, one of the agents on this project's team.

**You own:** the review gate.
**You review:** every ticket in Review.
**You never:** merge.
**Done means:** an APPROVED comment naming the commit.

## Team

- **@fullstack** writes the code you review.
"
    .into();
    vec![lead, dev, quality]
}

#[tokio::test]
#[ignore = "spends a real Claude Code call"]
async fn analyst_round_trip_with_real_claude_code() {
    let tmp = tempfile::tempdir().unwrap();
    let runner =
        ClaudeCodeRunner::from_settings(&GlobalSettings::default(), &HomePaths::at(tmp.path()));
    println!("runner: {}", runner.describe());

    let d = digest();
    let markdown = analyst::render_digest(&d);
    println!("----- digest ({} chars) -----\n{markdown}", markdown.len());
    let defs = definitions();
    let handles: Vec<String> = defs.iter().map(|d| d.handle().to_string()).collect();
    let request = analyst::analyse_request(
        &markdown,
        &defs,
        "The human is tired of approving cargo doc. Keep recommendations few.",
        None,
    );
    let output = runner.call(request).await.expect("Claude Code call failed");
    println!(
        "cost: {:?}  duration: {:.1}s  dir: {:?}",
        output.cost_usd,
        output.duration.as_secs_f32(),
        output.call_dir
    );
    println!("----- reply -----\n{}\n-----------------", output.text);

    let analysis =
        analyst::apply_analysis(&output.text, &handles, &ProjectPolicy::new("Worktree Deck"))
            .expect("the reply was not the expected JSON");
    println!("summary: {}", analysis.summary);
    println!("warnings: {:?}", analysis.warnings);
    for r in &analysis.recommendations {
        println!(
            "- [{}] {} -> {}\n    evidence: {}\n    change: {}\n    expected: {}",
            r.confidence.label(),
            r.title,
            r.target.label(),
            r.evidence,
            r.change,
            r.expected_effect
        );
        if let Target::AgentSection { text, .. } = &r.target {
            println!("    text: {}", text.replace('\n', "\n          "));
        }
    }
    assert!(!analysis.summary.is_empty());
    assert!(
        !analysis.recommendations.is_empty(),
        "a week this noisy earns at least one recommendation"
    );
    assert!(analysis.recommendations.len() <= 12);
    assert!(
        analysis
            .recommendations
            .iter()
            .any(|r| !r.target.is_human()),
        "at least one recommendation the app can apply"
    );
}
