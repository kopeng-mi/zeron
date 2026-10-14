//! Replies to a subagent that is blocked waiting on its orchestrator.
//!
//! A subagent's `send_message { wait: true }` long-polls its host engine
//! ([`AgentReplies::wait`]) for its chat. While it waits, the orchestrator's
//! `send_message` to that chat lands here ([`AgentReplies::deliver`]) and is
//! handed back as the subagent's tool result, instead of steering a turn that
//! is parked inside the tool call. Nothing is persisted: a wait that outlives
//! an engine restart simply times out and the subagent decides what to do.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a waiting mark outlives its last poll: covers the gap between
/// one long-poll returning empty and the next arriving.
const GRACE: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Slot {
    expires: Option<Instant>,
    replies: Vec<String>,
}

#[derive(Clone, Default)]
pub struct AgentReplies {
    slots: Arc<Mutex<HashMap<String, Slot>>>,
    notify: Arc<tokio::sync::Notify>,
}

impl AgentReplies {
    fn slots(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn waiting(slot: &Slot) -> bool {
        slot.expires.is_some_and(|at| at > Instant::now())
    }

    /// Mark `chat` waiting and block up to `max` for a reply. `None` on
    /// timeout; the mark lingers briefly so the next poll catches a reply
    /// that lands in between.
    pub async fn wait(&self, chat: &str, max: Duration) -> Option<String> {
        let deadline = Instant::now() + max;
        loop {
            let notified = self.notify.notified();
            {
                let mut slots = self.slots();
                let slot = slots.entry(chat.to_owned()).or_default();
                if !slot.replies.is_empty() {
                    let text = slot.replies.join("\n\n");
                    slots.remove(chat);
                    return Some(text);
                }
                slot.expires = Some(deadline + GRACE);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let _ = tokio::time::timeout(deadline - now, notified).await;
        }
    }

    /// Hand `text` to `chat`'s waiting subagent. `false` when nothing waits,
    /// so the caller delivers it as an ordinary message instead.
    pub fn deliver(&self, chat: &str, text: &str) -> bool {
        let mut slots = self.slots();
        match slots.get_mut(chat) {
            Some(slot) if Self::waiting(slot) => {
                slot.replies.push(text.to_owned());
                drop(slots);
                self.notify.notify_waiters();
                true
            }
            _ => false,
        }
    }

    /// The subagent gave up waiting: later messages go the ordinary way.
    pub fn cancel(&self, chat: &str) -> Option<String> {
        let slot = self.slots().remove(chat)?;
        (!slot.replies.is_empty()).then(|| slot.replies.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_reply_reaches_the_waiting_chat() {
        let replies = AgentReplies::default();
        let waiter = {
            let replies = replies.clone();
            tokio::spawn(async move { replies.wait("child", Duration::from_secs(5)).await })
        };
        // Let the wait mark itself first.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(replies.deliver("child", "use the v2 API"));
        assert_eq!(waiter.await.unwrap().as_deref(), Some("use the v2 API"));
    }

    #[tokio::test]
    async fn nothing_waiting_means_ordinary_delivery() {
        let replies = AgentReplies::default();
        assert!(!replies.deliver("child", "hi"));
    }

    #[tokio::test]
    async fn a_reply_between_polls_is_kept_for_the_next_one() {
        let replies = AgentReplies::default();
        assert_eq!(replies.wait("child", Duration::from_millis(20)).await, None);
        assert!(replies.deliver("child", "late answer"));
        assert_eq!(
            replies.wait("child", Duration::from_millis(20)).await.as_deref(),
            Some("late answer")
        );
    }

    #[tokio::test]
    async fn cancel_returns_an_unread_reply_and_clears_the_mark() {
        let replies = AgentReplies::default();
        replies.wait("child", Duration::from_millis(10)).await;
        assert!(replies.deliver("child", "x"));
        assert_eq!(replies.cancel("child").as_deref(), Some("x"));
        assert!(!replies.deliver("child", "y"));
    }
}
