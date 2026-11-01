//! Telling the human about a question when the app is not in front: a
//! Windows notification and a flash of the taskbar button.

use std::collections::HashSet;

use ak_core::ids::{NotificationId, ProjectId};

use crate::model::InboxItem;

/// Which questions the human has already been told about, per project.
#[derive(Default)]
pub struct Notifier {
    seen: HashSet<NotificationId>,
    /// Projects whose first inbox has arrived; questions already waiting at
    /// startup are not announced.
    primed: HashSet<ProjectId>,
}

impl Notifier {
    /// The questions in `items` the human has not been told about yet.
    pub fn fresh<'a>(&mut self, project: ProjectId, items: &'a [InboxItem]) -> Vec<&'a InboxItem> {
        let first = self.primed.insert(project);
        let fresh: Vec<&InboxItem> = items
            .iter()
            .filter(|i| i.needs_you && self.seen.insert(i.notification.id))
            .collect();
        if first {
            Vec::new()
        } else {
            fresh
        }
    }
}

/// Shows a desktop notification for a question, off the UI thread.
pub fn question(project: &str, text: &str) {
    let summary = format!("{project}: a question for you");
    let body: String = text
        .replace("**", "")
        .lines()
        .take(4)
        .collect::<Vec<_>>()
        .join("\n");
    std::thread::spawn(move || {
        if let Err(e) = notify_rust::Notification::new()
            .appname("AgenticKanban")
            .summary(&summary)
            .body(&body)
            .show()
        {
            tracing::warn!("could not show a desktop notification: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ak_core::ids::{ActorId, EventId};
    use ak_core::{Notification, NotificationKind, NotificationState};

    fn item(needs_you: bool) -> InboxItem {
        InboxItem {
            notification: Notification {
                id: NotificationId::new(),
                recipient_id: ActorId::new(),
                event_id: EventId::new(),
                kind: NotificationKind::Mentioned,
                wake: false,
                state: NotificationState::Pending,
                created_at: chrono::Utc::now(),
                handled_at: None,
                handled_in_run_id: None,
            },
            event: None,
            excerpt: Some("A or B?".into()),
            needs_you,
        }
    }

    #[test]
    fn only_new_questions_after_the_first_inbox_are_announced() {
        let mut n = Notifier::default();
        let project = ProjectId::new();
        let waiting = vec![item(true), item(false)];
        assert!(
            n.fresh(project, &waiting).is_empty(),
            "already waiting at startup"
        );
        let mut later = waiting.clone();
        later.push(item(true));
        later.push(item(false));
        let fresh = n.fresh(project, &later);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].notification.id, later[2].notification.id);
        assert!(n.fresh(project, &later).is_empty(), "told once");
    }
}

#[cfg(test)]
mod live {
    /// Shows a real notification. Ignored by default; run deliberately:
    /// `cargo test -p agentic-kanban notify::live -- --ignored`
    #[test]
    #[ignore = "shows a real desktop notification"]
    fn a_desktop_notification_shows() {
        notify_rust::Notification::new()
            .appname("AgenticKanban")
            .summary("AgenticKanban: a question for you")
            .body("Question for @me: print flags every line, or only on change?")
            .show()
            .expect("the notification was shown");
    }
}
