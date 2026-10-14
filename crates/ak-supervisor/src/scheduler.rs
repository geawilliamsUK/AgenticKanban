//! When an agent runs. One scheduler per open project, sharing a global pool
//! of run slots with the others.
//!
//! Wake notifications and direct activations put an agent on the list. A tick
//! starts runs for listed agents that are idle, not halted, not cooling down,
//! not over their wake rate, and within budget, as long as a slot is free.
//! When a run ends with the same wake items still pending, the agent cools
//! down instead of waking again straight away.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::{broadcast, mpsc, OwnedSemaphorePermit, Semaphore};

use ak_bridge::tools;
use ak_core::agent::{Agent, AgentState};
use ak_core::config::{GlobalSettings, PermissionProfile};
use ak_core::event::Subject;
use ak_core::guardrails::{GuardrailAction, GuardrailRule, Trip};
use ak_core::ids::{ActorId, NotificationId, RunId};
use ak_core::run::RunTrigger;
use ak_runtime::OpenProject;
use ak_store::Store;

use crate::supervisor::{RunHandle, RunNotice, RunSpec, StopReason, Supervisor};
use crate::workspace;
use crate::SupervisorError;

struct ActiveRun {
    handle: RunHandle,
    _permit: OwnedSemaphorePermit,
    /// The wake items the run was started for.
    trigger_ids: HashSet<NotificationId>,
}

struct State {
    active: HashMap<ActorId, ActiveRun>,
    paused: bool,
    wake_times: HashMap<ActorId, VecDeque<Instant>>,
    /// Direct activations waiting for the next tick.
    activations: HashMap<ActorId, (ActorId, String)>,
    /// Consecutive runs per agent that handled nothing they were woken for.
    churn: HashMap<ActorId, u32>,
    /// When the human was last told about a wake storm, per agent.
    storm_notified: HashMap<ActorId, Instant>,
}

struct Inner {
    project: Arc<OpenProject>,
    supervisor: Arc<Supervisor>,
    slots: Arc<Semaphore>,
    settings: Mutex<GlobalSettings>,
    state: Mutex<State>,
    nudge: mpsc::UnboundedSender<()>,
    /// Ticks never overlap, so an agent cannot be started twice.
    tick_lock: tokio::sync::Mutex<()>,
    stopping: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

impl Scheduler {
    /// Starts the scheduler's loop. It ticks on store events, on a timer, and
    /// whenever a run ends or the UI nudges it.
    pub fn spawn(
        project: Arc<OpenProject>,
        supervisor: Arc<Supervisor>,
        slots: Arc<Semaphore>,
        settings: GlobalSettings,
    ) -> Self {
        let (nudge, mut nudges) = mpsc::unbounded_channel();
        let scheduler = Self {
            inner: Arc::new(Inner {
                project: project.clone(),
                supervisor,
                slots,
                settings: Mutex::new(settings),
                state: Mutex::new(State {
                    active: HashMap::new(),
                    paused: false,
                    wake_times: HashMap::new(),
                    activations: HashMap::new(),
                    churn: HashMap::new(),
                    storm_notified: HashMap::new(),
                }),
                nudge,
                tick_lock: tokio::sync::Mutex::new(()),
                stopping: std::sync::atomic::AtomicBool::new(false),
            }),
        };
        let this = scheduler.clone();
        let mut bridge_events = this.inner.supervisor.bridge().subscribe();
        tokio::spawn(async move {
            let mut events = project.store.subscribe();
            let mut timer = tokio::time::interval(Duration::from_secs(30));
            loop {
                tokio::select! {
                    r = events.recv() => {
                        if matches!(r, Err(broadcast::error::RecvError::Closed)) {
                            return;
                        }
                        // Coalesce bursts: a mutation emits several events.
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        while events.try_recv().is_ok() {}
                    }
                    _ = timer.tick() => {}
                    n = nudges.recv() => {
                        if n.is_none() {
                            return;
                        }
                    }
                    ev = bridge_events.recv() => {
                        if let Ok(ak_bridge::BridgeEvent::RunGuardrail { run_id, agent_id, reason }) = ev {
                            this.guardrail_stop(agent_id, run_id, reason).await;
                        }
                        continue;
                    }
                }
                if this
                    .inner
                    .stopping
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    return;
                }
                if let Err(e) = this.tick().await {
                    tracing::warn!("scheduler tick failed: {e}");
                }
            }
        });
        scheduler
    }

    pub fn nudge(&self) {
        let _ = self.inner.nudge.send(());
    }

    /// Ends the loop. Active runs keep going until they finish or are stopped.
    pub fn shutdown(&self) {
        self.inner
            .stopping
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.nudge();
    }

    pub fn set_paused(&self, paused: bool) {
        self.inner.state.lock().expect("state").paused = paused;
        if !paused {
            self.nudge();
        }
    }

    pub fn paused(&self) -> bool {
        self.inner.state.lock().expect("state").paused
    }

    pub fn update_settings(&self, settings: GlobalSettings) {
        *self.inner.settings.lock().expect("settings") = settings;
    }

    pub fn is_running(&self, agent: ActorId) -> bool {
        self.inner
            .state
            .lock()
            .expect("state")
            .active
            .contains_key(&agent)
    }

    /// Kills the agent's run if any. The agent may be woken again.
    pub fn stop(&self, agent: ActorId) {
        if let Some(a) = self
            .inner
            .state
            .lock()
            .expect("state")
            .active
            .get_mut(&agent)
        {
            a.handle.stop(StopReason::Stopped);
        }
    }

    /// Kills every run in progress. Agents stay wakeable.
    pub fn stop_all(&self) {
        let mut state = self.inner.state.lock().expect("state");
        for a in state.active.values_mut() {
            a.handle.stop(StopReason::Stopped);
        }
    }

    /// Kills the run if any and keeps the agent from waking until resumed.
    pub async fn halt(&self, agent: ActorId) -> Result<(), SupervisorError> {
        let store = self.inner.project.store();
        store
            .set_agent_state(agent, AgentState::Halted, None)
            .await?;
        if let Some(a) = self
            .inner
            .state
            .lock()
            .expect("state")
            .active
            .get_mut(&agent)
        {
            a.handle.stop(StopReason::Halted);
        }
        Ok(())
    }

    pub async fn resume(&self, agent: ActorId) -> Result<(), SupervisorError> {
        let store = self.inner.project.store();
        store.set_agent_state(agent, AgentState::Idle, None).await?;
        store.set_agent_cooldown(agent, None).await?;
        self.nudge();
        Ok(())
    }

    /// Asks for a run regardless of pending notifications.
    pub fn activate(&self, agent: ActorId, by: ActorId, reason: String) {
        self.inner
            .state
            .lock()
            .expect("state")
            .activations
            .insert(agent, (by, reason));
        self.nudge();
    }

    /// One pass over the agents.
    pub async fn tick(&self) -> Result<(), SupervisorError> {
        let _guard = self.inner.tick_lock.lock().await;
        if self.paused() {
            return Ok(());
        }
        let project = &self.inner.project;
        let store = project.store();
        let policy = project.policy();
        let settings = self.inner.settings.lock().expect("settings").clone();
        let now = Utc::now();

        let agents = store.agents().await?;
        for agent in agents {
            if agent.is_retired() || self.is_running(agent.actor_id) {
                continue;
            }
            match agent.state {
                AgentState::Halted => continue,
                AgentState::Cooldown => match agent.cooldown_until {
                    Some(until) if until > now => continue,
                    _ => {
                        store
                            .set_agent_state(agent.actor_id, AgentState::Idle, None)
                            .await?;
                        store.set_agent_cooldown(agent.actor_id, None).await?;
                    }
                },
                AgentState::Idle | AgentState::Scheduled => {}
                // Starting/Running/Stopping/Errored without an active handle: a
                // leftover from a crash. Treat as idle.
                _ => {
                    store
                        .set_agent_state(agent.actor_id, AgentState::Idle, None)
                        .await?;
                }
            }

            let activation = self
                .inner
                .state
                .lock()
                .expect("state")
                .activations
                .remove(&agent.actor_id);
            let open = store.notifications(agent.actor_id, true).await?;
            let wake: Vec<_> = open.iter().filter(|n| n.wake).collect();
            if wake.is_empty() && activation.is_none() {
                continue;
            }

            // Wake rate: past the storm threshold, wakes wait and the human
            // hears about it once an hour.
            let storm = {
                let mut state = self.inner.state.lock().expect("state");
                let times = state.wake_times.entry(agent.actor_id).or_default();
                while times
                    .front()
                    .is_some_and(|t| t.elapsed() > Duration::from_secs(3600))
                {
                    times.pop_front();
                }
                let exceeded = times.len() as u32 >= policy.guardrails.wake_storm_per_hour;
                if exceeded {
                    let notify = !matches!(
                        state.storm_notified.get(&agent.actor_id),
                        Some(t) if t.elapsed() < Duration::from_secs(3600)
                    );
                    if notify {
                        state.storm_notified.insert(agent.actor_id, Instant::now());
                    }
                    Some(notify)
                } else {
                    None
                }
            };
            if let Some(notify) = storm {
                tracing::warn!(handle = %agent.handle, "wake rate exceeded; not starting a run");
                if notify {
                    let _ = store
                        .record_guardrail_trip(&Trip {
                            rule: GuardrailRule::WakeStorm,
                            action: GuardrailAction::Notify,
                            subject: Subject::Agent(agent.actor_id),
                            detail: format!(
                                "@{} reached {} wakes in an hour; further wakes wait",
                                agent.handle, policy.guardrails.wake_storm_per_hour
                            ),
                        })
                        .await;
                }
                continue;
            }

            // Budgets.
            let midnight = now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .map(|t| t.and_utc())
                .unwrap_or(now);
            let spent_today = store.run_cost_since(midnight, None).await?
                + store.utility_cost_since(midnight).await?;
            if spent_today >= settings.global_daily_budget_usd {
                tracing::warn!("daily budget reached; not starting runs");
                return Ok(());
            }
            let agent_today = store.run_cost_since(midnight, Some(agent.actor_id)).await?;
            let agent_cap = agent
                .budgets
                .as_ref()
                .and_then(|b| b.max_usd_per_day)
                .unwrap_or(policy.guardrails.max_usd_per_agent_per_day);
            if agent_today >= agent_cap {
                tracing::warn!(handle = %agent.handle, "agent daily budget reached");
                continue;
            }

            // A slot.
            let Ok(permit) = self.inner.slots.clone().try_acquire_owned() else {
                store
                    .set_agent_state(agent.actor_id, AgentState::Scheduled, None)
                    .await?;
                return Ok(());
            };

            // Describe the wake for the prompt.
            let trigger_ids: HashSet<NotificationId> = wake.iter().map(|n| n.id).collect();
            let mut trigger_lines = Vec::new();
            if !wake.is_empty() {
                let events = store.events_since(0, 5000).await?;
                if let Ok(snap) = tools::snapshot(store.as_ref()).await {
                    for n in &wake {
                        if let Some(e) = events.iter().find(|e| e.id == n.event_id) {
                            trigger_lines.push(tools::describe(e, &snap));
                        }
                    }
                }
            }
            let trigger = match &activation {
                Some((by, reason)) => {
                    trigger_lines.push(format!("direct activation: {reason}"));
                    RunTrigger::Activated {
                        by: *by,
                        reason: reason.clone(),
                    }
                }
                None => RunTrigger::Notifications {
                    ids: trigger_ids.iter().copied().collect(),
                },
            };

            let profile = policy
                .permission_profiles
                .iter()
                .find(|p| p.name == agent.permission_profile)
                .cloned()
                .unwrap_or_else(|| {
                    PermissionProfile::defaults()
                        .into_iter()
                        .next()
                        .expect("a default profile")
                });
            let base = project
                .entry
                .worktree_base
                .clone()
                .unwrap_or_else(|| workspace::default_base(&project.entry.path));
            let ws = match workspace::ensure(&project.entry.path, &base, &agent.handle).await {
                Ok(ws) => ws,
                Err(e) => {
                    tracing::error!(handle = %agent.handle, "workspace failed: {e}");
                    continue;
                }
            };
            let budgets = agent.budgets.clone().unwrap_or_default();
            let spec = RunSpec {
                project: project.clone(),
                model: agent.model.clone().or_else(|| policy.default_model.clone()),
                agent: agent.clone(),
                trigger,
                trigger_lines,
                profile,
                max_turns: budgets
                    .max_turns_per_run
                    .unwrap_or(policy.guardrails.max_turns_per_run),
                max_usd: budgets
                    .max_usd_per_run
                    .unwrap_or(policy.guardrails.max_usd_per_run),
                wall_clock: Duration::from_secs(
                    60 * budgets
                        .max_wall_clock_minutes
                        .unwrap_or(policy.guardrails.max_wall_clock_minutes)
                        as u64,
                ),
                workspace: ws,
            };
            match self.inner.supervisor.start_run(spec).await {
                Ok(handle) => {
                    let mut state = self.inner.state.lock().expect("state");
                    state
                        .wake_times
                        .entry(agent.actor_id)
                        .or_default()
                        .push_back(Instant::now());
                    state.active.insert(
                        agent.actor_id,
                        ActiveRun {
                            handle,
                            _permit: permit,
                            trigger_ids,
                        },
                    );
                    drop(state);
                    self.watch_run(agent);
                }
                Err(e) => {
                    tracing::error!(handle = %agent.handle, "could not start run: {e}");
                    let _ = store
                        .set_agent_state(agent.actor_id, AgentState::Errored, None)
                        .await;
                    drop(permit);
                }
            }
        }
        Ok(())
    }

    /// Waits for the agent's run to end, then decides what happens next.
    fn watch_run(&self, agent: Agent) {
        let this = self.clone();
        let mut notices = self.inner.supervisor.subscribe();
        let run_id = {
            let state = this.inner.state.lock().expect("state");
            state.active.get(&agent.actor_id).map(|a| a.handle.run_id)
        };
        let _ = run_id;
        tokio::spawn(async move {
            // Wait for this agent's run to end.
            loop {
                match notices.recv().await {
                    Ok(RunNotice::Ended { run, .. }) if run.agent_id == agent.actor_id => break,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            let removed = this
                .inner
                .state
                .lock()
                .expect("state")
                .active
                .remove(&agent.actor_id);
            let Some(active) = removed else { return };
            drop(active.handle);

            // Churn guard: the same wake items still pending means the run
            // made no progress on them. After enough such runs in a row, cool
            // down rather than loop.
            let store = this.inner.project.store();
            let mut no_progress = false;
            if !active.trigger_ids.is_empty() {
                if let Ok(open) = store.notifications(agent.actor_id, true).await {
                    let still: HashSet<NotificationId> =
                        open.iter().filter(|n| n.wake).map(|n| n.id).collect();
                    no_progress = still == active.trigger_ids;
                }
            }
            let is_halted = matches!(
                store.agent(agent.actor_id).await.map(|a| a.state),
                Ok(AgentState::Halted)
            );
            let guardrails = this.inner.project.policy().guardrails;
            let strikes = {
                let mut state = this.inner.state.lock().expect("state");
                if no_progress {
                    let n = state.churn.entry(agent.actor_id).or_insert(0);
                    *n += 1;
                    *n
                } else {
                    state.churn.remove(&agent.actor_id);
                    0
                }
            };
            if !is_halted
                && no_progress
                && guardrails.run_churn_runs > 0
                && strikes >= guardrails.run_churn_runs
            {
                let minutes = guardrails.run_churn_cooldown_minutes;
                let until = Utc::now() + chrono::Duration::minutes(minutes as i64);
                tracing::warn!(handle = %agent.handle, "{strikes} runs handled nothing they were woken for; cooling down for {minutes} min");
                let _ = store
                    .set_agent_state(agent.actor_id, AgentState::Cooldown, None)
                    .await;
                let _ = store.set_agent_cooldown(agent.actor_id, Some(until)).await;
                this.inner
                    .state
                    .lock()
                    .expect("state")
                    .churn
                    .remove(&agent.actor_id);
                let _ = store
                    .record_guardrail_trip(&Trip {
                        rule: GuardrailRule::RunChurn,
                        action: GuardrailAction::Cooldown,
                        subject: Subject::Agent(agent.actor_id),
                        detail: format!(
                            "@{}: {strikes} runs in a row handled nothing they were woken for; cooling down for {minutes} minutes",
                            agent.handle
                        ),
                    })
                    .await;
            }
            this.nudge();
        });
    }

    /// A bridge-side guardrail asked for the run to stop.
    async fn guardrail_stop(&self, agent: ActorId, run_id: RunId, reason: String) {
        let stopped = {
            let mut state = self.inner.state.lock().expect("state");
            match state.active.get_mut(&agent) {
                Some(a) if a.handle.run_id == run_id => {
                    a.handle.stop(StopReason::Guardrail);
                    true
                }
                _ => false,
            }
        };
        if stopped {
            let store = self.inner.project.store();
            let _ = store
                .record_guardrail_trip(&Trip {
                    rule: GuardrailRule::RepeatedToolCall,
                    action: GuardrailAction::StopRun,
                    subject: Subject::Agent(agent),
                    detail: reason,
                })
                .await;
        }
    }
}
