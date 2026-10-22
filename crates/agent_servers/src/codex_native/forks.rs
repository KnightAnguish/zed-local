use super::{Session, events::text, transport::Rpc};
use acp_thread::SessionForkPoint;
#[cfg(test)]
use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::schema::v2 as acp_v2;
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::Path,
    rc::Rc,
};

pub(super) struct SessionGuard {
    sessions: Rc<RefCell<HashMap<acp_v2::SessionId, Session>>>,
    id: acp_v2::SessionId,
}

impl SessionGuard {
    pub(super) fn acquire(
        sessions: Rc<RefCell<HashMap<acp_v2::SessionId, Session>>>,
        id: acp_v2::SessionId,
    ) -> Result<Self> {
        {
            let mut sessions = sessions.borrow_mut();
            let session = sessions
                .get_mut(&id)
                .context("Open the source chat before branching")?;
            ensure!(
                session.active.is_none() && !session.uncertain && !session.forking,
                "Stop the turn or reconnect before branching"
            );
            session.forking = true;
        }
        Ok(Self { sessions, id })
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(session) = self.sessions.borrow_mut().get_mut(&self.id) {
            session.forking = false;
        }
    }
}

#[derive(Debug, PartialEq)]
struct Boundary {
    last_turn: String,
    clear_first: bool,
}

fn boundary(
    turn: &Value,
    previous: Option<&str>,
    point: &SessionForkPoint,
) -> Result<Option<Boundary>> {
    let items = turn["items"]
        .as_array()
        .context("Fork history has no items")?;
    let Some(item) = items
        .iter()
        .find(|item| item["id"].as_str() == Some(point.message_id.0.as_ref()))
    else {
        return Ok(None);
    };
    ensure!(
        matches!(
            turn["status"].as_str(),
            Some("completed" | "interrupted" | "failed")
        ),
        "Fork turn is not finalized"
    );
    let turn_id = text(turn, "id")?;
    if point.before {
        let first = items.iter().find(|item| item["type"] == "userMessage");
        ensure!(
            item["type"] == "userMessage" && first == Some(item),
            "Only the first user message in a turn can be edited"
        );
        Ok(Some(Boundary {
            last_turn: previous.unwrap_or(turn_id).to_owned(),
            clear_first: previous.is_none(),
        }))
    } else {
        let final_reply = items
            .iter()
            .rev()
            .find(|item| item["type"] == "agentMessage" && item["phase"] != "commentary");
        ensure!(
            item["type"] == "agentMessage" && final_reply == Some(item),
            "Branch after the final assistant reply, not an intermediate step"
        );
        Ok(Some(Boundary {
            last_turn: turn_id.to_owned(),
            clear_first: false,
        }))
    }
}

async fn resolve(rpc: &Rpc, id: &acp_v2::SessionId, point: &SessionForkPoint) -> Result<Boundary> {
    ensure!(
        !point.message_id.0.trim().is_empty(),
        "Missing fork message ID"
    );
    let mut cursor: Option<String> = None;
    let mut cursors = HashSet::new();
    let mut previous: Option<String> = None;
    // Keep one page, not the entire conversation, even for very old edit points.
    for _ in 0..10_000 {
        let page = rpc.request("thread/turns/list", json!({"threadId":id,"cursor":cursor,"limit":20,"sortDirection":"asc","itemsView":"summary"})).await?;
        let turns = page["data"]
            .as_array()
            .context("Missing fork history page")?;
        ensure!(turns.len() <= 20, "Oversized fork history page");
        for turn in turns {
            if let Some(boundary) = boundary(turn, previous.as_deref(), point)? {
                return Ok(boundary);
            }
            previous = Some(text(turn, "id")?.to_owned());
        }
        match page.get("nextCursor") {
            Some(Value::String(next)) => {
                ensure!(
                    !next.is_empty() && cursors.insert(next.clone()),
                    "Repeated fork history cursor"
                );
                cursor = Some(next.clone());
            }
            None | Some(Value::Null) => {
                bail!("Fork message no longer exists in the source history")
            }
            _ => bail!("Invalid fork history cursor"),
        }
    }
    bail!("Fork history scan exceeded its page limit")
}

pub(super) async fn fork(
    rpc: &Rpc,
    source: &acp_v2::SessionId,
    cwd: Option<&Path>,
    point: Option<&SessionForkPoint>,
) -> Result<acp_v2::SessionId> {
    let source_state = rpc
        .request(
            "thread/read",
            json!({"threadId":source,"includeTurns":false}),
        )
        .await?;
    ensure!(
        text(&source_state["thread"], "id")? == source.0.as_ref(),
        "Unexpected source thread"
    );
    ensure!(
        matches!(
            source_state["thread"]["status"]["type"].as_str(),
            Some("idle" | "notLoaded")
        ),
        "Source chat is not idle"
    );
    let boundary = match point {
        Some(point) => Some(resolve(rpc, source, point).await?),
        None => None,
    };
    let mut params = json!({"threadId":source,"excludeTurns":true});
    if let Some(cwd) = cwd {
        params["cwd"] = json!(cwd);
    }
    if let Some(boundary) = &boundary {
        params["lastTurnId"] = json!(boundary.last_turn);
    }
    let response = rpc.request("thread/fork", params).await?;
    let id = acp_v2::SessionId::new(text(&response["thread"], "id")?);
    ensure!(
        id != *source && !id.0.is_empty(),
        "Codex did not return a separate fork"
    );
    let result: Result<()> = async {
        if let Some(boundary) = boundary.filter(|boundary| boundary.clear_first) {
            // Revert only the new copy: editing the first message must leave no hidden context.
            rpc.request(
                "thread/revert",
                json!({"threadId":id,"beforeTurnId":boundary.last_turn}),
            )
            .await?;
            let page = rpc
                .request(
                    "thread/turns/list",
                    json!({"threadId":id,"limit":1,"itemsView":"notLoaded"}),
                )
                .await?;
            ensure!(
                page["data"].as_array().is_some_and(Vec::is_empty) && page["nextCursor"].is_null(),
                "First-message fork retained history"
            );
        }
        rpc.request("thread/unsubscribe", json!({"threadId":id}))
            .await?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        // A failed candidate must not appear as a successful variant. Never archive the source.
        if let Err(cleanup) = rpc.request("thread/archive", json!({"threadId":id})).await {
            bail!(
                "Fork preparation failed: {error}; candidate {id} cleanup also failed: {cleanup}"
            );
        }
        return Err(error);
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::super::transport::FakePeer;
    use super::*;
    #[gpui::test]
    #[ignore = "Explicit diagnostic source and runtime required; creates and archives copies, no model requests"]
    fn live_diagnostic_forks(cx: &mut gpui::TestAppContext) {
        use futures::FutureExt as _;
        cx.background_executor.allow_parking();
        let executable =
            std::path::PathBuf::from(std::env::var("ZED_CODEX_HISTORY_TEST_EXE").expect("runtime"));
        let source = acp_v2::SessionId::new(
            std::env::var("ZED_CODEX_HISTORY_TEST_THREAD").expect("disposable diagnostic source"),
        );
        let (rpc, events) = Rpc::start(
            executable,
            Default::default(),
            None,
            cx.background_executor.clone(),
        );
        let responder = rpc.clone();
        let drain = cx.background_executor.spawn(async move {
            while let Ok(event) = events.recv().await {
                if let super::super::transport::Event::Request { id, .. } = event {
                    responder
                        .respond(id, Err(anyhow::anyhow!("Diagnostic request rejected")))
                        .expect("reject unexpected request");
                }
            }
        });
        let operation = cx.background_executor.spawn(async move {
            rpc.initialize().await?;
            let restore_archive =
                std::env::var("ZED_CODEX_FORK_TEST_UNARCHIVE").as_deref() == Ok("1");
            if restore_archive {
                rpc.request("thread/unarchive", json!({"threadId":source}))
                    .await?;
            }
            let test_result: Result<()> = async {
                let before = rpc
                    .request(
                        "thread/read",
                        json!({"threadId":source,"includeTurns":true}),
                    )
                    .await?;
                let first = before["thread"]["turns"]
                    .as_array()
                    .and_then(|turns| turns.first())
                    .context("Diagnostic source is empty")?;
                let items = first["items"]
                    .as_array()
                    .context("Diagnostic turn has no items")?;
                let user = items
                    .iter()
                    .find(|item| item["type"] == "userMessage")
                    .context("No diagnostic user message")?;
                let reply = items
                    .iter()
                    .rev()
                    .find(|item| item["type"] == "agentMessage" && item["phase"] != "commentary")
                    .context("No diagnostic reply")?;
                let points = [
                    Some(point(text(user, "id")?, true)),
                    Some(point(text(reply, "id")?, false)),
                    None,
                ];
                let mut copies = Vec::new();
                let result: Result<()> = async {
                    for (index, point) in points.iter().enumerate() {
                        let copy = fork(&rpc, &source, None, point.as_ref()).await?;
                        copies.push(copy.clone());
                        let history = rpc
                            .request("thread/read", json!({"threadId":copy,"includeTurns":true}))
                            .await?;
                        let turns = history["thread"]["turns"]
                            .as_array()
                            .context("Missing copied turns")?;
                        match index {
                            0 => ensure!(turns.is_empty(), "First-message fork is not empty"),
                            1 => ensure!(
                                turns.len() == 1 && turns.first() == Some(first),
                                "Reply fork differs from source prefix"
                            ),
                            _ => ensure!(
                                history["thread"]["turns"] == before["thread"]["turns"],
                                "Full fork differs from source"
                            ),
                        }
                    }
                    let after = rpc
                        .request(
                            "thread/read",
                            json!({"threadId":source,"includeTurns":true}),
                        )
                        .await?;
                    ensure!(
                        after["thread"]["turns"] == before["thread"]["turns"],
                        "Source history changed"
                    );
                    Ok(())
                }
                .await;
                let mut cleanup_errors = Vec::new();
                for copy in copies {
                    if let Err(error) = rpc
                        .request("thread/archive", json!({"threadId":copy}))
                        .await
                    {
                        cleanup_errors.push(format!("{copy}: {error}"));
                    }
                }
                ensure!(
                    cleanup_errors.is_empty(),
                    "Diagnostic fork cleanup failed: {}",
                    cleanup_errors.join("; ")
                );
                result
            }
            .await;
            if restore_archive {
                rpc.request("thread/archive", json!({"threadId":source}))
                    .await
                    .context("Restore diagnostic source archive")?;
            }
            drop(drain);
            test_result
        });
        futures::pin_mut!(operation);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if let Some(result) = operation.as_mut().now_or_never() {
                result.expect("real native fork protocol");
                break;
            }
            while cx.background_executor.tick() {}
            assert!(
                std::time::Instant::now() < deadline,
                "Live fork probe timed out"
            );
            std::thread::yield_now();
        }
    }
    async fn reply(peer: &FakePeer, method: &str, result: Value) -> Value {
        let request = peer.next().await;
        assert_eq!(request["method"], method);
        peer.send(json!({"id":request["id"],"result":result})).await;
        request["params"].clone()
    }
    #[gpui::test]
    async fn first_message_reverts_only_copy_and_unsubscribes(cx: &mut gpui::TestAppContext) {
        let (rpc, peer, _events) = Rpc::fake(cx.background_executor.clone());
        let task = cx.background_executor.spawn(async move {
            fork(
                &rpc,
                &acp_v2::SessionId::new("source"),
                None,
                Some(&point("u", true)),
            )
            .await
        });
        reply(
            &peer,
            "thread/read",
            json!({"thread":{"id":"source","status":{"type":"idle"}}}),
        )
        .await;
        reply(&peer, "thread/turns/list", json!({"data":[{"id":"t","status":"completed","items":[{"id":"u","type":"userMessage"}]}],"nextCursor":null})).await;
        let params = reply(&peer, "thread/fork", json!({"thread":{"id":"copy"}})).await;
        assert_eq!(params["lastTurnId"], "t");
        assert_eq!(params["excludeTurns"], true);
        assert_eq!(
            reply(&peer, "thread/revert", json!({})).await,
            json!({"threadId":"copy","beforeTurnId":"t"})
        );
        assert_eq!(
            reply(
                &peer,
                "thread/turns/list",
                json!({"data":[],"nextCursor":null})
            )
            .await["threadId"],
            "copy"
        );
        assert_eq!(
            reply(&peer, "thread/unsubscribe", json!({})).await["threadId"],
            "copy"
        );
        assert_eq!(task.await.unwrap(), acp_v2::SessionId::new("copy"));
    }
    #[gpui::test]
    async fn source_id_returned_as_fork_is_never_modified(cx: &mut gpui::TestAppContext) {
        let (rpc, peer, _events) = Rpc::fake(cx.background_executor.clone());
        let task = cx
            .background_executor
            .spawn(async move { fork(&rpc, &acp_v2::SessionId::new("source"), None, None).await });
        reply(
            &peer,
            "thread/read",
            json!({"thread":{"id":"source","status":{"type":"idle"}}}),
        )
        .await;
        reply(&peer, "thread/fork", json!({"thread":{"id":"source"}})).await;
        assert!(task.await.is_err());
    }
    #[gpui::test]
    async fn failed_first_message_clear_archives_only_copy(cx: &mut gpui::TestAppContext) {
        let (rpc, peer, _events) = Rpc::fake(cx.background_executor.clone());
        let task = cx.background_executor.spawn(async move {
            fork(
                &rpc,
                &acp_v2::SessionId::new("source"),
                None,
                Some(&point("u", true)),
            )
            .await
        });
        reply(
            &peer,
            "thread/read",
            json!({"thread":{"id":"source","status":{"type":"idle"}}}),
        )
        .await;
        reply(&peer, "thread/turns/list", json!({"data":[{"id":"t","status":"completed","items":[{"id":"u","type":"userMessage"}]}]})).await;
        reply(&peer, "thread/fork", json!({"thread":{"id":"copy"}})).await;
        reply(&peer, "thread/revert", json!({})).await;
        reply(
            &peer,
            "thread/turns/list",
            json!({"data":[{"id":"unexpected"}]}),
        )
        .await;
        assert_eq!(
            reply(&peer, "thread/archive", json!({})).await["threadId"],
            "copy"
        );
        assert!(task.await.is_err());
    }
    #[gpui::test]
    async fn boundary_keeps_previous_turn_across_pages(cx: &mut gpui::TestAppContext) {
        let (rpc, peer, _events) = Rpc::fake(cx.background_executor.clone());
        let task = cx.background_executor.spawn(async move {
            resolve(&rpc, &acp_v2::SessionId::new("source"), &point("u", true)).await
        });
        let params = reply(
            &peer,
            "thread/turns/list",
            json!({"data":[{"id":"previous","items":[]}],"nextCursor":"page2"}),
        )
        .await;
        assert_eq!(params["sortDirection"], "asc");
        let params = reply(&peer, "thread/turns/list", json!({"data":[{"id":"t","status":"completed","items":[{"id":"u","type":"userMessage"}]}],"nextCursor":null})).await;
        assert_eq!(params["cursor"], "page2");
        assert_eq!(
            task.await.unwrap(),
            Boundary {
                last_turn: "previous".into(),
                clear_first: false
            }
        );
    }
    fn point(id: &str, before: bool) -> SessionForkPoint {
        SessionForkPoint {
            message_id: acp::MessageId::new(id),
            before,
        }
    }
    #[test]
    fn validates_edit_and_reply_boundaries() {
        let turn = json!({"id":"t","status":"completed","items":[
            {"id":"u","type":"userMessage"},{"id":"steer","type":"userMessage"},
            {"id":"c","type":"agentMessage","phase":"commentary"},
            {"id":"a","type":"agentMessage","phase":"final_answer"}]});
        assert_eq!(
            boundary(&turn, None, &point("u", true)).unwrap(),
            Some(Boundary {
                last_turn: "t".into(),
                clear_first: true
            })
        );
        assert_eq!(
            boundary(&turn, Some("previous"), &point("u", true)).unwrap(),
            Some(Boundary {
                last_turn: "previous".into(),
                clear_first: false
            })
        );
        assert_eq!(
            boundary(&turn, None, &point("a", false)).unwrap(),
            Some(Boundary {
                last_turn: "t".into(),
                clear_first: false
            })
        );
        assert!(boundary(&turn, None, &point("steer", true)).is_err());
        assert!(boundary(&turn, None, &point("c", false)).is_err());
        assert!(boundary(&turn, None, &point("a", true)).is_err());
        assert!(
            boundary(&turn, None, &point("missing", false))
                .unwrap()
                .is_none()
        );
        let mut active = turn;
        active["status"] = json!("inProgress");
        assert!(boundary(&active, None, &point("u", true)).is_err());
    }
}
