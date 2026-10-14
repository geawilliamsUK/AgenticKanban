//! The bridge: a localhost HTTP server that gives an agent run its view of
//! the board over MCP, and answers the hooks Claude Code calls during a run.
//!
//! Every URL carries a run token. The token resolves to a project, an agent
//! and a run, so identity never comes from the request body and an agent
//! cannot act as someone else.
//!
//! ```text
//! POST /mcp/{token}                  JSON-RPC: initialize, tools/list, tools/call
//! POST /hooks/{token}/pre-tool-use   permission decision, possibly after a human
//! POST /hooks/{token}/stop           block the stop while wake items are pending
//! ```

pub mod hooks;
pub mod mcp;
pub mod runs;
pub mod tools;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::routing::{any, post};
use axum::Router;
use tokio::sync::{broadcast, oneshot};

use ak_core::approval::{Approval, ApprovalState};
use ak_core::ids::RunId;
use ak_core::ids::{ActorId, ApprovalId};

pub use runs::RunContext;

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("could not bind the bridge: {0}")]
    Bind(std::io::Error),
    #[error(transparent)]
    Store(#[from] ak_store::StoreError),
    #[error("{0}")]
    Other(String),
}

/// What the bridge tells the rest of the app about.
#[derive(Debug, Clone)]
pub enum BridgeEvent {
    ApprovalRequested(Approval),
    ApprovalDecided(Approval),
    /// A run-level guardrail fired inside a hook; the scheduler stops the run.
    RunGuardrail {
        run_id: RunId,
        agent_id: ActorId,
        reason: String,
    },
}

struct Inner {
    addr: SocketAddr,
    runs: RwLock<HashMap<String, Arc<RunContext>>>,
    /// Hook requests waiting on a human decision.
    waiting: Mutex<HashMap<ApprovalId, oneshot::Sender<ApprovalState>>>,
    events: broadcast::Sender<BridgeEvent>,
    approval_timeout: Duration,
    /// The last tool call per run and how many times in a row it was made.
    repeats: Mutex<HashMap<RunId, (String, u32)>>,
}

/// A cheap handle to the running bridge.
#[derive(Clone)]
pub struct Bridge {
    inner: Arc<Inner>,
}

impl Bridge {
    /// Binds an ephemeral loopback port and serves until dropped.
    pub async fn start() -> Result<Self, BridgeError> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(BridgeError::Bind)?;
        let addr = listener.local_addr().map_err(BridgeError::Bind)?;
        let bridge = Self::with_addr(addr, Duration::from_secs(30 * 60));
        let router = bridge.router();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!("bridge server stopped: {e}");
            }
        });
        tracing::info!(%addr, "bridge listening");
        Ok(bridge)
    }

    /// A bridge that is not listening, for tests that drive the router directly.
    pub fn with_addr(addr: SocketAddr, approval_timeout: Duration) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                addr,
                runs: RwLock::new(HashMap::new()),
                waiting: Mutex::new(HashMap::new()),
                events,
                approval_timeout,
                repeats: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/mcp/{token}", any(mcp::handle))
            .route("/hooks/{token}/pre-tool-use", post(hooks::pre_tool_use))
            .route("/hooks/{token}/stop", post(hooks::stop))
            .with_state(self.clone())
    }

    pub fn addr(&self) -> SocketAddr {
        self.inner.addr
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BridgeEvent> {
        self.inner.events.subscribe()
    }

    pub fn approval_timeout(&self) -> Duration {
        self.inner.approval_timeout
    }

    /// Registers a run and returns its token.
    pub fn register_run(&self, ctx: RunContext) -> String {
        let token = uuid::Uuid::new_v4().simple().to_string();
        self.inner
            .runs
            .write()
            .expect("runs lock")
            .insert(token.clone(), Arc::new(ctx));
        token
    }

    pub fn unregister_run(&self, token: &str) {
        let removed = self.inner.runs.write().expect("runs lock").remove(token);
        if let Some(ctx) = removed {
            self.inner
                .repeats
                .lock()
                .expect("repeats lock")
                .remove(&ctx.run_id);
        }
    }

    /// Counts identical consecutive tool calls within a run and returns the
    /// current streak length.
    pub(crate) fn note_tool_call(&self, run: RunId, tool: &str, input: &serde_json::Value) -> u32 {
        let fingerprint = format!("{tool}:{input}");
        let mut repeats = self.inner.repeats.lock().expect("repeats lock");
        let entry = repeats.entry(run).or_insert_with(|| (String::new(), 0));
        if entry.0 == fingerprint {
            entry.1 += 1;
        } else {
            *entry = (fingerprint, 1);
        }
        entry.1
    }

    pub fn run(&self, token: &str) -> Option<Arc<RunContext>> {
        self.inner
            .runs
            .read()
            .expect("runs lock")
            .get(token)
            .cloned()
    }

    pub fn mcp_url(&self, token: &str) -> String {
        format!("http://{}/mcp/{token}", self.inner.addr)
    }

    pub fn hook_url(&self, token: &str, hook: &str) -> String {
        format!("http://{}/hooks/{token}/{hook}", self.inner.addr)
    }

    /// The `--mcp-config` document for a run.
    pub fn mcp_config(&self, token: &str) -> serde_json::Value {
        serde_json::json!({
            "mcpServers": {
                "board": { "type": "http", "url": self.mcp_url(token) }
            }
        })
    }

    /// The hooks section of the `--settings` document for a run.
    pub fn hook_settings(&self, token: &str) -> serde_json::Value {
        serde_json::json!({
            "hooks": {
                "PreToolUse": [{ "hooks": [{
                    "type": "http",
                    "url": self.hook_url(token, "pre-tool-use"),
                    "timeout": self.inner.approval_timeout.as_secs() + 30
                }]}],
                "Stop": [{ "hooks": [{
                    "type": "http",
                    "url": self.hook_url(token, "stop"),
                    "timeout": 30
                }]}]
            }
        })
    }

    /// Records the human's decision and releases the waiting hook, if any.
    pub async fn decide(
        &self,
        store: &dyn ak_store::Store,
        id: ApprovalId,
        approved: bool,
        by: ActorId,
    ) -> Result<Approval, BridgeError> {
        let state = if approved {
            ApprovalState::Approved
        } else {
            ApprovalState::Denied
        };
        let approval = store.decide_approval(id, state, Some(by)).await?;
        if let Some(tx) = self.inner.waiting.lock().expect("waiting lock").remove(&id) {
            let _ = tx.send(state);
        }
        let _ = self
            .inner
            .events
            .send(BridgeEvent::ApprovalDecided(approval.clone()));
        Ok(approval)
    }

    pub(crate) fn emit(&self, event: BridgeEvent) {
        let _ = self.inner.events.send(event);
    }

    pub(crate) fn register_waiter(&self, id: ApprovalId) -> oneshot::Receiver<ApprovalState> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .waiting
            .lock()
            .expect("waiting lock")
            .insert(id, tx);
        rx
    }

    pub(crate) fn forget_waiter(&self, id: ApprovalId) {
        self.inner.waiting.lock().expect("waiting lock").remove(&id);
    }
}
