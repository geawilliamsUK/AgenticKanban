//! The two hooks Claude Code calls during a run.
//!
//! PreToolUse: the permission profile decides, and anything it does not cover
//! becomes an approval the human answers. The request is held open until then
//! or until the timeout, which denies. If the bridge is down, Claude Code
//! falls back to its own rules, which deny anything not pre-allowed.
//!
//! Stop: while the agent has wake notifications pending, the stop is blocked
//! with a reason listing them. Claude Code caps consecutive blocks itself.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use ak_core::approval::{Approval, ApprovalState};
use ak_core::permissions::{self, Decision};

use crate::tools::describe;
use crate::{Bridge, BridgeEvent, RunContext};

/// The prefix of this bridge's own tools as Claude Code names them.
const BOARD_TOOL_PREFIX: &str = "mcp__board__";

#[derive(Debug, Deserialize)]
pub struct PreToolUseInput {
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: Value,
}

#[derive(Debug, Deserialize, Default)]
pub struct StopInput {
    #[serde(default)]
    pub stop_hook_active: bool,
}

fn permission(decision: &str, reason: &str) -> Value {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    })
}

pub async fn pre_tool_use(
    State(bridge): State<Bridge>,
    Path(token): Path<String>,
    Json(input): Json<PreToolUseInput>,
) -> Response {
    let Some(ctx) = bridge.run(&token) else {
        return Json(permission("deny", "this run has ended")).into_response();
    };
    let outcome = decide(&bridge, &ctx, &input).await;
    Json(outcome).into_response()
}

/// The decision for one tool call. Separate from the handler so tests can
/// call it without HTTP.
pub async fn decide(bridge: &Bridge, ctx: &RunContext, input: &PreToolUseInput) -> Value {
    // Repeated tool call: the same call over and over is a loop, not work.
    let limit = ctx.project.policy().guardrails.repeated_tool_calls;
    let streak = bridge.note_tool_call(ctx.run_id, &input.tool_name, &input.tool_input);
    if limit > 0 && streak >= limit {
        let reason = format!(
            "@{} made the same {} call {streak} times in a row",
            ctx.agent.handle, input.tool_name
        );
        bridge.emit(BridgeEvent::RunGuardrail {
            run_id: ctx.run_id,
            agent_id: ctx.agent.actor_id,
            reason,
        });
        return permission(
            "deny",
            &format!(
                "guardrail: this exact call was made {streak} times in a row; the run is being stopped"
            ),
        );
    }
    if input.tool_name.starts_with(BOARD_TOOL_PREFIX) {
        return permission("allow", "board tools are always available");
    }
    match permissions::decide(&ctx.profile, &input.tool_name, &input.tool_input) {
        Decision::Allow => permission("allow", &format!("allowed by profile {}", ctx.profile.name)),
        Decision::Deny => permission("deny", &format!("denied by profile {}", ctx.profile.name)),
        Decision::Ask => ask_human(bridge, ctx, input).await,
    }
}

async fn ask_human(bridge: &Bridge, ctx: &RunContext, input: &PreToolUseInput) -> Value {
    let approval = Approval::new(
        ctx.run_id,
        ctx.agent.actor_id,
        input.tool_name.clone(),
        input.tool_input.clone(),
        Utc::now(),
    );
    let store = ctx.store();
    if let Err(e) = store.insert_approval(&approval).await {
        return permission(
            "deny",
            &format!("could not record the approval request: {e}"),
        );
    }
    let rx = bridge.register_waiter(approval.id);
    bridge.emit(BridgeEvent::ApprovalRequested(approval.clone()));

    let timeout = bridge.approval_timeout();
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(ApprovalState::Approved)) => permission("allow", "approved by the human"),
        Ok(Ok(_)) => permission("deny", "denied by the human"),
        Ok(Err(_)) => permission("deny", "the approval was dropped"),
        Err(_) => {
            bridge.forget_waiter(approval.id);
            let _ = store
                .decide_approval(approval.id, ApprovalState::Expired, None)
                .await;
            permission(
                "deny",
                &format!(
                    "no decision from the human within {} minutes; ask on the ticket and try later",
                    timeout.as_secs() / 60
                ),
            )
        }
    }
}

pub async fn stop(
    State(bridge): State<Bridge>,
    Path(token): Path<String>,
    body: Option<Json<StopInput>>,
) -> Response {
    let Some(ctx) = bridge.run(&token) else {
        return Json(json!({})).into_response();
    };
    let _ = body;
    Json(stop_decision(&ctx).await).into_response()
}

/// `{}` to let the agent stop, or a block with the pending wake items.
pub async fn stop_decision(ctx: &RunContext) -> Value {
    let store = ctx.store();
    let open = match store.notifications(ctx.agent.actor_id, true).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("stop hook could not read the inbox: {e}");
            return json!({});
        }
    };
    let wake: Vec<_> = open.iter().filter(|n| n.wake).collect();
    if wake.is_empty() {
        return json!({});
    }
    let events = store.events_since(0, 5000).await.unwrap_or_default();
    let snap = match crate::tools::snapshot(store.as_ref()).await {
        Ok(s) => s,
        Err(_) => return json!({}),
    };
    let mut lines = Vec::new();
    for n in wake.iter().take(10) {
        let what = events
            .iter()
            .find(|e| e.id == n.event_id)
            .map(|e| describe(e, &snap))
            .unwrap_or_else(|| n.kind.as_str().to_string());
        lines.push(format!("- {what}"));
    }
    json!({
        "decision": "block",
        "reason": format!(
            "{} notification(s) still need your attention before you finish:\n{}\nCall inbox_list, deal with them (a ticket mutation handles its notifications; inbox_handle handles the rest), then memory_update, then stop.",
            wake.len(),
            lines.join("\n")
        )
    })
}
