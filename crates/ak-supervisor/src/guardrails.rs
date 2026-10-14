//! Watches a project's events and applies the guardrails that need a
//! subject's history: ticket ping-pong, comment storms and mention loops.
//! The scheduler applies run churn and wake storms; the bridge applies
//! repeated tool calls. Every trip is recorded as a system event that
//! notifies the human.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::broadcast;

use ak_core::event::Subject;
use ak_core::guardrails::{self, GuardrailAction, Trip};
use ak_core::ids::ActorId;
use ak_runtime::OpenProject;
use ak_store::{Store, StoreError};

#[derive(Clone)]
pub struct GuardrailMonitor {
    stopping: Arc<AtomicBool>,
}

impl GuardrailMonitor {
    /// Subscribes to the project's events and evaluates every subject a
    /// burst touches.
    pub fn spawn(project: Arc<OpenProject>) -> Self {
        let stopping = Arc::new(AtomicBool::new(false));
        let flag = stopping.clone();
        tokio::spawn(async move {
            let mut events = project.store.subscribe();
            loop {
                let first = match events.recv().await {
                    Ok(e) => e,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                // Coalesce a burst: a mutation emits several events.
                tokio::time::sleep(Duration::from_millis(150)).await;
                let mut subjects: HashSet<Subject> = HashSet::new();
                subjects.insert(first.subject());
                while let Ok(e) = events.try_recv() {
                    subjects.insert(e.subject());
                }
                for subject in subjects {
                    if let Err(e) = evaluate(&project, subject).await {
                        tracing::warn!("guardrail evaluation failed: {e}");
                    }
                }
            }
        });
        Self { stopping }
    }

    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }
}

/// Evaluates every rule for one subject and applies the first trip. A
/// frozen ticket or a paused thread is left alone.
pub async fn evaluate(project: &OpenProject, subject: Subject) -> Result<Option<Trip>, StoreError> {
    let store = project.store();
    let thresholds = project.policy().guardrails;
    let now = Utc::now();
    let agents: HashSet<ActorId> = store
        .agents()
        .await?
        .into_iter()
        .map(|a| a.actor_id)
        .collect();
    let trip = match subject {
        Subject::Ticket(id) => {
            let ticket = store.ticket(id).await?;
            if ticket.frozen {
                return Ok(None);
            }
            let events = store.events_for_ticket(id).await?;
            guardrails::check_ticket(id, &events, &agents, &thresholds, now)
        }
        Subject::Thread(id) => {
            let thread = store.thread(id).await?;
            if thread.paused {
                return Ok(None);
            }
            let events = store.events_for_thread(id).await?;
            guardrails::check_thread(id, &events, &agents, &thresholds, now)
        }
        _ => None,
    };
    if let Some(trip) = &trip {
        apply(store.as_ref(), trip).await?;
    }
    Ok(trip)
}

/// Applies a trip's action as the system and records the trip.
pub async fn apply(store: &dyn Store, trip: &Trip) -> Result<(), StoreError> {
    match (trip.action, trip.subject) {
        (GuardrailAction::FreezeTicket, Subject::Ticket(id)) => {
            store
                .freeze_ticket(
                    id,
                    format!("{}: {}", trip.rule.label(), trip.detail),
                    ActorId::SYSTEM,
                )
                .await?;
        }
        (GuardrailAction::PauseThread, Subject::Thread(id)) => {
            store.set_thread_paused(id, true, ActorId::SYSTEM).await?;
        }
        _ => {}
    }
    store.record_guardrail_trip(trip).await?;
    tracing::warn!(
        rule = trip.rule.as_str(),
        "guardrail tripped: {}",
        trip.detail
    );
    Ok(())
}
