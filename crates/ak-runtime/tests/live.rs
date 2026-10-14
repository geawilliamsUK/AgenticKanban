//! Live checks against the real Claude Code binary. Ignored by default because
//! each one spends a real model call. Run deliberately:
//!
//! ```bash
//! cargo test -p ak-runtime --test live -- --ignored --nocapture
//! ```

use ak_core::agent::AgentDefinition;
use ak_core::config::GlobalSettings;
use ak_runtime::authoring::{self, RosterEntry};
use ak_runtime::{ClaudeCodeRunner, HomePaths, UtilityRunner};

#[tokio::test]
#[ignore = "spends a real Claude Code call"]
async fn refine_round_trip_with_real_claude_code() {
    let tmp = tempfile::tempdir().unwrap();
    let runner =
        ClaudeCodeRunner::from_settings(&GlobalSettings::default(), &HomePaths::at(tmp.path()));
    println!("runner: {}", runner.describe());

    let mut draft = AgentDefinition::template("dba", "Database administrator");
    draft.frontmatter.description = "db person".into();
    draft.body =
        "# dba\n\nlooks after the postgres db, migrations and indexes. dont let it touch app \
                  code. hand stuff to @backend when done and tell them what changed\n"
            .into();
    let roster = vec![RosterEntry {
        handle: "backend".into(),
        description: "Owns the API and application services.".into(),
    }];

    let request = authoring::refine_request(&draft, &roster, None);
    let output = runner.call(request).await.expect("Claude Code call failed");
    println!(
        "cost: {:?}  duration: {:.1}s  dir: {:?}",
        output.cost_usd,
        output.duration.as_secs_f32(),
        output.call_dir
    );
    println!("----- reply -----\n{}\n-----------------", output.text);

    let proposal =
        authoring::apply_refine(&draft, &output.text).expect("reply was not a valid definition");
    println!("warnings: {:?}", proposal.warnings);
    println!("----- proposal -----\n{}", proposal.definition.render());

    assert_eq!(proposal.definition.handle(), "dba");
    assert!(proposal.definition.body.contains("**You own:**"));
    assert!(
        proposal.definition.body.contains("@backend"),
        "kept the draft's mention"
    );
}
