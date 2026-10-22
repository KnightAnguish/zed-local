use super::{Session, events, transport::Rpc};
use agent_client_protocol::schema::{v1 as acp, v2 as acp_v2};
use anyhow::{Result, bail};
use gpui::AsyncApp;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    time::Duration,
};

const CHECK_INTERVAL: Duration = Duration::from_secs(60);
const FIRST_CHECK: Duration = Duration::from_secs(180);
const RESPONSE_DEADLINE: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
pub(super) struct Progress {
    revision: u64,
    tools: HashSet<String>,
    requests: HashSet<String>,
}

impl Progress {
    pub(super) fn observe(&mut self, method: &str, params: &Value) {
        if method == "thread/status/changed" {
            return;
        }
        self.revision = self.revision.saturating_add(1);
        if matches!(method, "item/started" | "item/completed") {
            if let (Some(id), Some(kind)) = (
                params["item"]["id"].as_str(),
                params["item"]["type"].as_str(),
            ) {
                if method == "item/completed" {
                    self.tools.remove(id);
                } else if !matches!(kind, "userMessage" | "agentMessage" | "reasoning" | "plan") {
                    self.tools.insert(id.to_owned());
                }
            }
        }
    }

    pub(super) fn begin_request(&mut self, id: String) {
        self.requests.insert(id);
        self.revision = self.revision.saturating_add(1);
    }

    pub(super) fn resolve_request(&mut self, id: &str) {
        if self.requests.remove(id) {
            self.revision = self.revision.saturating_add(1);
        }
    }

    fn waiting_on_tool_or_user(&self) -> bool {
        !self.tools.is_empty() || !self.requests.is_empty()
    }
}

fn snapshot(
    sessions: &RefCell<HashMap<acp_v2::SessionId, Session>>,
    id: &acp_v2::SessionId,
    turn_id: &str,
) -> Option<(u64, bool, bool)> {
    let sessions = sessions.borrow();
    let active = sessions.get(id)?.active.as_ref()?;
    if active.id.as_deref() != Some(turn_id) {
        return None;
    }
    Some((
        active.recovery.revision,
        active.recovery.waiting_on_tool_or_user(),
        active.cancel_requested,
    ))
}

fn waiting_on_backend_user(status: &Value) -> bool {
    status["activeFlags"].as_array().is_some_and(|flags| {
        flags.iter().any(|flag| {
            matches!(
                flag.as_str(),
                Some("waitingOnApproval" | "waitingOnUserInput")
            )
        })
    })
}

fn release(
    sessions: &RefCell<HashMap<acp_v2::SessionId, Session>>,
    id: &acp_v2::SessionId,
    turn_id: &str,
    uncertain: bool,
) {
    if let Some(session) = sessions.borrow_mut().get_mut(id) {
        if session
            .active
            .as_ref()
            .is_some_and(|active| active.id.as_deref() == Some(turn_id))
        {
            session.uncertain = uncertain;
            session.active.take();
        }
    }
}

pub(super) async fn watch(
    rpc: Rpc,
    sessions: Rc<RefCell<HashMap<acp_v2::SessionId, Session>>>,
    id: acp_v2::SessionId,
    turn_id: String,
    cx: &mut AsyncApp,
) -> Result<acp::PromptResponse> {
    let mut revision = snapshot(&sessions, &id, &turn_id).map(|state| state.0);
    let mut silent = Duration::ZERO;
    let mut failed_checks = 0;
    loop {
        cx.background_executor().timer(CHECK_INTERVAL).await;
        let Some((current, waiting, cancelled)) = snapshot(&sessions, &id, &turn_id) else {
            // The event loop has already sent completion; let the original waiter consume it.
            return futures::future::pending().await;
        };
        if revision != Some(current) {
            revision = Some(current);
            silent = Duration::ZERO;
            failed_checks = 0;
        } else {
            silent += CHECK_INTERVAL;
        }
        if silent < FIRST_CHECK && !cancelled {
            continue;
        }
        let response = rpc
            .request("thread/read", json!({"threadId":id,"includeTurns":false}))
            .await;
        if snapshot(&sessions, &id, &turn_id).map(|state| state.0) != Some(current) {
            continue;
        }
        let response = match response {
            Ok(response) => {
                failed_checks = 0;
                response
            }
            Err(error) => {
                failed_checks += 1;
                if cancelled || failed_checks >= 3 {
                    release(&sessions, &id, &turn_id, true);
                    bail!(
                        "Codex stopped responding to status checks: {error}. This turn's outcome is unknown; reconnect before continuing. No message was replayed."
                    );
                }
                continue;
            }
        };
        if response["thread"]["id"].as_str() != Some(id.0.as_ref()) {
            release(&sessions, &id, &turn_id, true);
            bail!("Codex returned an unrelated thread status; no recovery action was taken.");
        }
        let status = &response["thread"]["status"];
        match status["type"].as_str() {
            Some("idle") => {
                let turns = rpc.request("thread/turns/list", json!({"threadId":id,"limit":20,"sortDirection":"desc","itemsView":"notLoaded"})).await;
                if snapshot(&sessions, &id, &turn_id).map(|state| state.0) != Some(current) {
                    continue;
                }
                let terminal = turns
                    .as_ref()
                    .ok()
                    .and_then(|value| value["data"].as_array())
                    .and_then(|turns| {
                        turns
                            .iter()
                            .find(|turn| turn["id"].as_str() == Some(&turn_id))
                    })
                    .filter(|turn| {
                        matches!(
                            turn["status"].as_str(),
                            Some("completed" | "failed" | "interrupted")
                        )
                    });
                if let Some(turn) = terminal {
                    if turn["status"] == "completed" {
                        // A terminal record alone cannot restore reply chunks lost from the stream.
                        release(&sessions, &id, &turn_id, true);
                        bail!(
                            "Codex finished, but its completion stream was lost. Reopen this chat to load the saved reply; no message was replayed."
                        );
                    }
                    let result = events::turn_result(&json!({"turn":turn}));
                    release(&sessions, &id, &turn_id, false);
                    return result;
                }
                release(&sessions, &id, &turn_id, true);
                bail!(
                    "Codex is idle, but this turn's final state could not be confirmed. Reopen this chat before continuing; no message was replayed."
                );
            }
            Some("active") if cancelled => {
                release(&sessions, &id, &turn_id, true);
                bail!(
                    "Codex has not confirmed cancellation. Reconnect this chat before continuing; other chats were not interrupted."
                );
            }
            Some("active") if waiting_on_backend_user(status) => {
                silent = Duration::ZERO;
            }
            Some("active")
                if silent >= RESPONSE_DEADLINE && !waiting && !waiting_on_backend_user(status) =>
            {
                let thread = sessions
                    .borrow()
                    .get(&id)
                    .map(|session| session.thread.clone());
                if let Some(thread) = thread {
                    let updates = events::TurnEvents::default().update("error", &json!({
                        "willRetry":true,"error":{"message":"Codex produced no events for 15 minutes. Interrupting only this turn, without replaying your message. Completed actions are retained."}
                    }))?;
                    for update in updates {
                        thread
                            .update(cx, |thread, cx| thread.handle_session_update(update, cx))??;
                    }
                }
                // Only generation silence has a deadline. Tool execution and human input do not.
                if let Some(active) = sessions
                    .borrow_mut()
                    .get_mut(&id)
                    .and_then(|session| session.active.as_mut())
                {
                    active.cancel_requested = true;
                }
                let interrupted = rpc
                    .request("turn/interrupt", json!({"threadId":id,"turnId":turn_id}))
                    .await;
                if snapshot(&sessions, &id, &turn_id).is_none() {
                    return futures::future::pending().await;
                }
                if let Err(error) = interrupted {
                    release(&sessions, &id, &turn_id, true);
                    bail!(
                        "Codex produced no events for 15 minutes and interruption could not be confirmed: {error}. Reconnect this chat; no message was replayed."
                    );
                }
                // The original receiver remains live while we wait for turn/completed.
            }
            Some("active") => {}
            _ => {
                release(&sessions, &id, &turn_id, true);
                bail!(
                    "Codex thread is unavailable or has an unknown status. Reconnect this chat before continuing; no message was replayed."
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_and_user_waits_are_not_generation_silence() {
        let mut progress = Progress::default();
        progress.observe(
            "item/started",
            &json!({"item":{"id":"command","type":"commandExecution"}}),
        );
        assert!(progress.waiting_on_tool_or_user());
        progress.observe(
            "item/completed",
            &json!({"item":{"id":"command","type":"commandExecution"}}),
        );
        assert!(!progress.waiting_on_tool_or_user());
        progress.begin_request("approval".into());
        assert!(progress.waiting_on_tool_or_user());
        progress.resolve_request("approval");
        assert!(!progress.waiting_on_tool_or_user());
        assert!(waiting_on_backend_user(
            &json!({"activeFlags":["waitingOnApproval"]})
        ));
        assert!(waiting_on_backend_user(
            &json!({"activeFlags":["waitingOnUserInput"]})
        ));
        assert!(!waiting_on_backend_user(&json!({"activeFlags":[]})));
    }

    #[test]
    fn streamed_progress_resets_deadline_but_status_does_not() {
        let mut progress = Progress::default();
        progress.observe("item/agentMessage/delta", &json!({}));
        let revision = progress.revision;
        progress.observe("thread/status/changed", &json!({}));
        assert_eq!(progress.revision, revision);
        progress.observe("item/commandExecution/outputDelta", &json!({}));
        assert!(progress.revision > revision);
    }
}
