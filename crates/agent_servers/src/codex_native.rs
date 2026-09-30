//! Native Codex integration under construction. Not selected by production routing.
//! Keep the legacy connector until the parity gates in NATIVE_CODEX.md pass.
mod events;
mod profiles;
mod settings;
mod transport;

use acp_thread::{
    AcpThread, AgentConnection, AgentSessionInfo, AgentSessionList, AgentSessionListRequest,
    AgentSessionListResponse, AgentSessionSetTitle, AuthRequired, AuthorizationKind, HistoryPage,
    PermissionOptions, RequestPermissionOutcome,
};
use action_log::ActionLog;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, ensure};
use events::{TurnEvents, text};
use futures::channel::oneshot;
use gpui::{App, AppContext as _, AsyncApp, Entity, SharedString, Task, TaskExt as _, WeakEntity};
use project::{AgentId, Project, agent_server_store::AgentServerCommand};
use serde_json::{Value, json};
use std::{any::Any, cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc};
use transport::{Event, Rpc};
use util::{ResultExt as _, path_list::PathList};

struct ActiveTurn {
    id: Option<String>,
    completion: oneshot::Sender<Result<acp::PromptResponse>>,
    events: TurnEvents,
    cancel_requested: bool,
}

struct Session {
    thread: WeakEntity<AcpThread>,
    active: Option<ActiveTurn>,
    uncertain: bool,
    preferences: settings::Preferences,
    settings_sender: Rc<RefCell<watch::Sender<()>>>,
    settings_receiver: watch::Receiver<()>,
}

struct Approval {
    _task: Task<()>,
    thread: Option<WeakEntity<AcpThread>>,
    item_id: Option<acp::ToolCallId>,
}

impl Approval {
    fn cancel_ui(&self, cx: &mut AsyncApp) {
        if let (Some(thread), Some(item_id)) = (&self.thread, &self.item_id) {
            thread
                .update(cx, |thread, cx| {
                    thread.cancel_tool_call_authorization(item_id, cx)
                })
                .log_err();
        }
    }
}

pub(crate) struct NativeCodexConnection {
    rpc: Rpc,
    sessions: Rc<RefCell<HashMap<acp::SessionId, Session>>>,
    approvals: RefCell<HashMap<String, Approval>>,
    event_task: RefCell<Option<Task<()>>>,
    profile_home: RefCell<Option<PathBuf>>,
    profile_override: RefCell<Option<String>>,
    models: RefCell<Rc<Vec<settings::Model>>>,
    quota: Rc<settings::Quota>,
}

impl NativeCodexConnection {
    pub(crate) async fn connect(
        command: AgentServerCommand,
        directory: Option<PathBuf>,
        cx: &mut AsyncApp,
    ) -> Result<Rc<Self>> {
        let environment = command
            .env
            .context("Native Codex requires explicit CODEX_PATH")?;
        let profile_override = environment
            .get("CODEX_ACP_SKILL_PROFILES")
            .cloned()
            .or_else(|| std::env::var("CODEX_ACP_SKILL_PROFILES").ok());
        let executable = PathBuf::from(
            environment
                .get("CODEX_PATH")
                .context("CODEX_PATH is not configured")?,
        );
        let (rpc, events) = Rpc::start(
            executable,
            environment,
            directory,
            cx.background_executor().clone(),
        );
        let connection = Self::from_rpc(rpc, events, cx);
        let initialized = connection.rpc.initialize().await?;
        *connection.profile_home.borrow_mut() =
            Some(PathBuf::from(text(&initialized, "codexHome")?));
        *connection.profile_override.borrow_mut() = profile_override;
        *connection.models.borrow_mut() = settings::catalog(&connection.rpc).await?;
        cx.update(|cx| connection.quota.refresh(cx, false));
        Ok(connection)
    }

    fn from_rpc(rpc: Rpc, events: async_channel::Receiver<Event>, cx: &mut AsyncApp) -> Rc<Self> {
        let quota = settings::Quota::new(rpc.clone());
        let connection = Rc::new(Self {
            rpc,
            sessions: Rc::new(RefCell::new(HashMap::new())),
            approvals: RefCell::new(HashMap::new()),
            event_task: RefCell::new(None),
            profile_home: RefCell::new(None),
            profile_override: RefCell::new(None),
            models: RefCell::new(Rc::new(Vec::new())),
            quota,
        });
        let weak = Rc::downgrade(&connection);
        *connection.event_task.borrow_mut() = Some(cx.spawn(async move |cx| {
            while let Ok(event) = events.recv().await {
                let Some(connection) = weak.upgrade() else { return; };
                match event {
                    Event::Notification { method, params } => {
                        if let Err(error) = connection.notification(&method, params, cx) {
                            // Mapping errors are not successful turns. Never keep a spinner forever.
                            connection.fail_active(&format!("Native Codex event error: {error}"));
                        }
                    }
                    Event::Request { id, method, params } => connection.server_request(id, method, params, cx),
                }
            }
            if let Some(connection) = weak.upgrade() {
                connection.quota.invalidate("Codex disconnected");
                connection.fail_active("Codex app-server disconnected. Reconnect before continuing; no turn was replayed.");
                let approvals = std::mem::take(&mut *connection.approvals.borrow_mut());
                for approval in approvals.into_values() { approval.cancel_ui(cx); }
            }
        }));
        connection
    }

    fn fail_active(&self, reason: &str) {
        for session in self.sessions.borrow_mut().values_mut() {
            session.uncertain = true;
            if let Some(active) = session.active.take() {
                let _ = active
                    .completion
                    .send(Err(anyhow::anyhow!(reason.to_owned())));
            }
        }
    }

    fn notification(&self, method: &str, params: Value, cx: &mut AsyncApp) -> Result<()> {
        if method == "account/updated" {
            self.quota.invalidate("Account changed");
            cx.update(|cx| self.quota.refresh(cx, false));
            return Ok(());
        }
        if method == "account/rateLimits/updated" {
            // Rolling updates are sparse. Refetch the account snapshot instead of replacing
            // complete weekly metadata with a possibly model-specific/partial window.
            cx.update(|cx| self.quota.refresh(cx, false));
            return Ok(());
        }
        if method == "serverRequest/resolved" {
            if let Some(id) = params.get("requestId") {
                let approval = self.approvals.borrow_mut().remove(&id.to_string());
                if let Some(approval) = approval {
                    approval.cancel_ui(cx);
                }
            }
            return Ok(());
        }
        let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
            return Ok(());
        };
        let id = acp::SessionId::new(thread_id);
        // Extract UI updates before touching GPUI; no RefCell borrow across callbacks.
        let update = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(session) = sessions.get_mut(&id) else {
                return Ok(());
            };
            let Some(active) = &mut session.active else {
                return Ok(());
            };
            let turn_id = params
                .get("turnId")
                .and_then(Value::as_str)
                .or_else(|| params["turn"]["id"].as_str());
            if let (Some(expected), Some(actual)) = (&active.id, turn_id) {
                if expected != actual {
                    return Ok(());
                }
            }
            if method == "turn/started" {
                active.id = Some(text(&params["turn"], "id")?.to_owned());
                if active.cancel_requested {
                    let rpc = self.rpc.clone();
                    let turn_id = active.id.clone();
                    let id = id.clone();
                    cx.spawn(async move |_| {
                        rpc.request("turn/interrupt", json!({"threadId":id,"turnId":turn_id}))
                            .await
                            .log_err();
                    })
                    .detach();
                }
            }
            if method == "turn/completed" {
                cx.update(|cx| self.quota.refresh(cx, false));
                if let Some(active) = session.active.take() {
                    let _ = active.completion.send(events::turn_result(&params));
                }
                return Ok(());
            }
            (
                session.thread.clone(),
                active.events.update(method, &params)?,
            )
        };
        for value in update.1 {
            update
                .0
                .update(cx, |thread, cx| thread.handle_session_update(value, cx))??;
        }
        Ok(())
    }

    fn server_request(
        self: &Rc<Self>,
        id: Value,
        method: String,
        params: Value,
        cx: &mut AsyncApp,
    ) {
        let key = id.to_string();
        if self.approvals.borrow().len() >= 64 || self.approvals.borrow().contains_key(&key) {
            self.rpc
                .respond(
                    id,
                    Err(anyhow::anyhow!(
                        "Duplicate or excessive outstanding Codex server requests; nothing approved"
                    )),
                )
                .log_err();
            return;
        }
        let thread = params["threadId"].as_str().and_then(|id| {
            self.sessions
                .borrow()
                .get(&acp::SessionId::new(id))
                .map(|session| session.thread.clone())
        });
        let rpc = self.rpc.clone();
        let approval_thread = thread.clone();
        let approval_item = params["itemId"]
            .as_str()
            .map(|id| acp::ToolCallId::new(id.to_owned()));
        let weak = Rc::downgrade(self);
        let task_key = key.clone();
        let task = cx.spawn(async move |cx| {
            let result: Result<Value> = async {
                ensure!(
                    matches!(
                        method.as_str(),
                        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
                    ),
                    "Unsupported Codex server request {method}; nothing was approved"
                );
                let thread = thread.context("Approval requested for an unopened thread")?;
                let item_id = text(&params, "itemId")?.to_owned();
                let options = vec![
                    acp::PermissionOption::new(
                        "accept",
                        "Allow once",
                        acp::PermissionOptionKind::AllowOnce,
                    ),
                    acp::PermissionOption::new(
                        "decline",
                        "Reject",
                        acp::PermissionOptionKind::RejectOnce,
                    ),
                ];
                let title = params["command"]
                    .as_str()
                    .or_else(|| params["reason"].as_str())
                    .unwrap_or("Codex requests permission");
                let tool = acp::ToolCallUpdate::new(
                    item_id,
                    acp::ToolCallUpdateFields::new()
                        .title(title.to_owned())
                        .status(acp::ToolCallStatus::Pending),
                );
                let outcome = thread
                    .update(cx, |thread, cx| {
                        thread.request_tool_call_authorization(
                            tool,
                            PermissionOptions::Flat(options),
                            AuthorizationKind::PermissionGrant,
                            cx,
                        )
                    })??
                    .await;
                let decision = match outcome {
                    RequestPermissionOutcome::Selected(outcome)
                        if outcome.option_id.0.as_ref() == "accept" =>
                    {
                        "accept"
                    }
                    RequestPermissionOutcome::Selected(_) => "decline",
                    RequestPermissionOutcome::Cancelled
                    | RequestPermissionOutcome::InterruptedByFollowUp => "cancel",
                };
                Ok(json!({"decision":decision}))
            }
            .await;
            rpc.respond(id, result).log_err();
            if let Some(connection) = weak.upgrade() {
                connection.approvals.borrow_mut().remove(&task_key);
            }
        });
        self.approvals.borrow_mut().insert(
            key,
            Approval {
                _task: task,
                thread: approval_thread,
                item_id: approval_item,
            },
        );
    }

    async fn open(
        self: Rc<Self>,
        session_id: Option<acp::SessionId>,
        project: Entity<Project>,
        work_dirs: PathList,
        title: Option<SharedString>,
        cx: &mut AsyncApp,
    ) -> Result<Entity<AcpThread>> {
        ensure!(
            project.read_with(cx, |project, _| project.is_local()),
            "Native Codex remote projects are not implemented"
        );
        ensure!(
            work_dirs.paths().len() <= 1,
            "Native Codex multi-root configuration is not implemented"
        );
        let account = self
            .rpc
            .request("account/read", json!({"refreshToken":false}))
            .await?;
        if account["requiresOpenaiAuth"].as_bool() == Some(true) && account["account"].is_null() {
            return Err(AuthRequired::new()
                .with_description("Sign in using the configured Codex CLI, then reconnect.".into())
                .into());
        }
        let mut params = json!({"cwd":work_dirs.ordered_paths().next(),"sandbox":"read-only","approvalPolicy":"on-request"});
        let profile_home = self.profile_home.borrow().clone();
        if let (Some(home), Some(cwd)) = (profile_home, work_dirs.ordered_paths().next()) {
            let cwd = cwd.to_string_lossy().into_owned();
            let profile_cwd = cwd.clone();
            let profile_override = self.profile_override.borrow().clone();
            let profile = cx
                .background_spawn(async move {
                    profiles::load(&home, &profile_cwd, profile_override.as_deref())
                })
                .await?;
            if let Some(profile) = profile {
                let (catalog, effective) = futures::try_join!(
                    self.rpc
                        .request("skills/list", json!({"cwds":[cwd],"forceReload":true})),
                    self.rpc
                        .request("config/read", json!({"cwd":cwd,"includeLayers":false}))
                )?;
                params["config"] =
                    profile.apply(&catalog, &effective["config"]["skills"]["config"])?;
            }
        }
        let method = if let Some(id) = &session_id {
            ensure!(
                !self.sessions.borrow().contains_key(id),
                "Native Codex session is already open"
            );
            params["threadId"] = json!(id);
            params["excludeTurns"] = json!(true);
            "thread/resume"
        } else {
            "thread/start"
        };
        let response = self.rpc.request(method, params).await?;
        let preferences =
            settings::Preferences::from_response(self.models.borrow().clone(), &response)?;
        let id = acp::SessionId::new(text(&response["thread"], "id")?);
        if response["thread"]["status"]["type"].as_str() == Some("active") {
            self.rpc
                .request("thread/unsubscribe", json!({"threadId":id}))
                .await?;
            anyhow::bail!(
                "This Codex thread is still active; live reattachment is not implemented yet"
            );
        }
        let tail = if session_id.is_some() {
            let response = self
                .rpc
                .request(
                    "thread/turns/list",
                    json!({"threadId":id,"limit":20,"sortDirection":"desc","itemsView":"summary"}),
                )
                .await?;
            Some(crate::codex_history::decode_page(response, None)?)
        } else {
            None
        };
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let connection = self.clone();
        let thread = cx.new(|cx| {
            AcpThread::new(
                None,
                title,
                Some(work_dirs),
                connection,
                project,
                action_log,
                id.clone(),
                watch::Receiver::constant(acp::PromptCapabilities::new().image(true)),
                cx,
            )
        });
        if let Some(tail) = tail {
            for message in tail.messages {
                for content in message.content {
                    let chunk = acp::ContentChunk::new(content).message_id(message.id.clone());
                    let update = if message.is_user {
                        acp::SessionUpdate::UserMessageChunk(chunk)
                    } else {
                        acp::SessionUpdate::AgentMessageChunk(chunk)
                    };
                    thread.update(cx, |thread, cx| thread.handle_session_update(update, cx))?;
                }
            }
        }
        let (settings_sender, settings_receiver) = watch::channel(());
        self.sessions.borrow_mut().insert(
            id,
            Session {
                thread: thread.downgrade(),
                active: None,
                uncertain: false,
                preferences,
                settings_sender: Rc::new(RefCell::new(settings_sender)),
                settings_receiver,
            },
        );
        Ok(thread)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    async fn session(
        cx: &mut TestAppContext,
    ) -> (
        Rc<NativeCodexConnection>,
        transport::FakePeer,
        Entity<AcpThread>,
    ) {
        let fs = crate::e2e_tests::init_test(cx).await;
        let project = Project::test(fs, [], cx).await;
        let (rpc, peer, events) = Rpc::fake(cx.background_executor.clone());
        let connection = NativeCodexConnection::from_rpc(rpc, events, &mut cx.to_async());
        *connection.models.borrow_mut() = Rc::new(serde_json::from_value(json!([{"model":"a","displayName":"A","supportedReasoningEfforts":[{"reasoningEffort":"medium","description":"balanced"},{"reasoningEffort":"high","description":"deep"}],"defaultReasoningEffort":"medium","serviceTiers":[{"id":"fast"}]}])).unwrap());
        let open = cx.update(|cx| {
            connection
                .clone()
                .new_session(project, PathList::default(), cx)
        });
        let account = peer.next().await;
        assert_eq!(account["method"], "account/read");
        peer.send(json!({"id":account["id"],"result":{"requiresOpenaiAuth":false}}))
            .await;
        let start = peer.next().await;
        assert_eq!(start["method"], "thread/start");
        assert_eq!(start["params"]["sandbox"], "read-only");
        peer.send(json!({"id":start["id"],"result":{"thread":{"id":"session"},"model":"a","reasoningEffort":"medium","serviceTier":null}}))
            .await;
        (connection, peer, open.await.unwrap())
    }

    #[gpui::test]
    async fn settings_reach_only_next_turn_and_cannot_change_active_turn(cx: &mut TestAppContext) {
        let (connection, peer, _thread) = session(cx).await;
        let id = acp::SessionId::new("session");
        let options = cx
            .update(|cx| connection.session_config_options(&id, cx))
            .unwrap();
        let quota = peer.next().await;
        assert_eq!(quota["method"], "account/rateLimits/read");
        peer.send(json!({"id":quota["id"],"result":{}})).await;
        for (key, value) in [
            (
                "reasoning-effort",
                acp::SessionConfigOptionValue::from("high"),
            ),
            ("mode", "agent-full-access".into()),
            ("fast-mode", true.into()),
        ] {
            cx.update(|cx| options.set_config_option(acp::SessionConfigId::new(key), value, cx))
                .await
                .unwrap();
        }
        let prompt = cx.update(|cx| {
            connection.prompt(acp::PromptRequest::new("session", vec!["hello".into()]), cx)
        });
        let request = peer.next().await;
        assert_eq!(request["method"], "turn/start");
        assert_eq!(request["params"]["effort"], "high");
        assert_eq!(request["params"]["serviceTier"], "fast");
        assert_eq!(
            request["params"]["sandboxPolicy"]["type"],
            "dangerFullAccess"
        );
        assert!(
            cx.update(|cx| options.set_config_option(
                acp::SessionConfigId::new("mode"),
                "native-read-only".into(),
                cx
            ))
            .await
            .is_err()
        );
        peer.send(json!({"id":request["id"],"result":{"turn":{"id":"turn"}}}))
            .await;
        peer.send(json!({"method":"turn/completed","params":{"threadId":"session","turn":{"id":"turn","status":"completed"}}})).await;
        prompt.await.unwrap();
        assert!(
            cx.update(|cx| options.set_config_option(
                acp::SessionConfigId::new("mode"),
                "native-read-only".into(),
                cx
            ))
            .await
            .is_ok()
        );
    }

    #[gpui::test]
    async fn cancellation_before_turn_start_is_forwarded(cx: &mut TestAppContext) {
        let (connection, peer, _thread) = session(cx).await;
        let prompt = cx.update(|cx| {
            connection.prompt(acp::PromptRequest::new("session", vec!["hello".into()]), cx)
        });
        let request = peer.next().await;
        assert_eq!(request["method"], "turn/start");
        cx.update(|cx| connection.cancel(&acp::SessionId::new("session"), cx));
        peer.send(
            json!({"method":"turn/started","params":{"threadId":"session","turn":{"id":"turn"}}}),
        )
        .await;
        let interrupt = peer.next().await;
        assert_eq!(interrupt["method"], "turn/interrupt");
        assert_eq!(interrupt["params"]["turnId"], "turn");
        peer.send(json!({"id":interrupt["id"],"result":{}})).await;
        peer.send(json!({"id":request["id"],"result":{"turn":{"id":"turn"}}}))
            .await;
        peer.send(json!({"method":"turn/completed","params":{"threadId":"session","turn":{"id":"turn","status":"interrupted"}}})).await;
        assert_eq!(
            prompt.await.unwrap().stop_reason,
            acp::StopReason::Cancelled
        );
        assert!(
            connection.sessions.borrow()[&acp::SessionId::new("session")]
                .active
                .is_none()
        );
    }

    #[gpui::test]
    async fn disconnect_fails_prompt_without_replaying_it(cx: &mut TestAppContext) {
        let (connection, peer, _thread) = session(cx).await;
        let prompt = cx.update(|cx| {
            connection.prompt(acp::PromptRequest::new("session", vec!["hello".into()]), cx)
        });
        let request = peer.next().await;
        peer.send(json!({"id":request["id"],"result":{"turn":{"id":"turn"}}}))
            .await;
        peer.disconnect();
        assert!(
            prompt
                .await
                .unwrap_err()
                .to_string()
                .contains("disconnected")
        );
        let retry = cx.update(|cx| {
            connection.prompt(acp::PromptRequest::new("session", vec!["again".into()]), cx)
        });
        assert!(
            retry
                .await
                .unwrap_err()
                .to_string()
                .contains("unknown outcome")
        );
    }

    #[gpui::test]
    async fn completion_before_start_response_and_interleaved_thread_are_safe(
        cx: &mut TestAppContext,
    ) {
        let (connection, peer, thread) = session(cx).await;
        let prompt = cx.update(|cx| {
            connection.prompt(acp::PromptRequest::new("session", vec!["hello".into()]), cx)
        });
        let request = peer.next().await;
        peer.send(json!({"method":"item/agentMessage/delta","params":{"threadId":"other","turnId":"other-turn","itemId":"other-item","delta":"not our message"}})).await;
        peer.send(json!({"method":"item/agentMessage/delta","params":{"threadId":"session","turnId":"turn","itemId":"reply","delta":"hello"}})).await;
        peer.send(json!({"method":"turn/completed","params":{"threadId":"session","turn":{"id":"turn","status":"completed"}}})).await;
        peer.send(json!({"id":request["id"],"result":{"turn":{"id":"turn"}}}))
            .await;
        assert_eq!(prompt.await.unwrap().stop_reason, acp::StopReason::EndTurn);
        thread.read_with(cx, |thread, _| assert_eq!(thread.entries().len(), 1));
    }

    #[gpui::test]
    async fn unsupported_server_request_is_rejected_not_approved(cx: &mut TestAppContext) {
        let (_connection, peer, _thread) = session(cx).await;
        peer.send(json!({"id":"unsupported","method":"item/newPermission/requestApproval","params":{"threadId":"session","itemId":"tool"}})).await;
        let response = peer.next().await;
        assert_eq!(response["id"], "unsupported");
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("nothing was approved")
        );
        assert!(response.get("result").is_none());
    }

    #[gpui::test]
    async fn approval_is_not_sent_before_user_decision(cx: &mut TestAppContext) {
        let (connection, peer, thread) = session(cx).await;
        peer.send(json!({"id":"approval","method":"item/commandExecution/requestApproval","params":{"threadId":"session","turnId":"turn","itemId":"tool","command":"echo hello"}})).await;
        cx.run_until_parked();
        assert_eq!(connection.approvals.borrow().len(), 1);
        thread.update(cx, |thread, cx| {
            thread.authorize_tool_call(
                acp::ToolCallId::new("tool"),
                acp_thread::SelectedPermissionOutcome::new(
                    acp::PermissionOptionId::new("decline"),
                    acp::PermissionOptionKind::RejectOnce,
                ),
                cx,
            )
        });
        let response = peer.next().await;
        assert_eq!(response["id"], "approval");
        assert_eq!(response["result"]["decision"], "decline");
        cx.run_until_parked();
        assert!(connection.approvals.borrow().is_empty());
    }
}

impl AgentConnection for NativeCodexConnection {
    fn agent_id(&self) -> AgentId {
        AgentId::new(crate::CODEX_ID)
    }
    fn telemetry_id(&self) -> SharedString {
        "codex-native".into()
    }
    fn new_session(
        self: Rc<Self>,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        cx.spawn(async move |cx| self.open(None, project, work_dirs, None, cx).await)
    }
    fn supports_load_session(&self) -> bool {
        true
    }
    fn load_session(
        self: Rc<Self>,
        id: acp::SessionId,
        project: Entity<Project>,
        dirs: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        // UI uses history_page after opening; never hydrate the entire stored conversation here.
        cx.spawn(async move |cx| self.open(Some(id), project, dirs, title, cx).await)
    }
    fn supports_history_pages(&self) -> bool {
        true
    }
    fn history_page(
        &self,
        id: acp::SessionId,
        cursor: Option<String>,
        cx: &mut App,
    ) -> Task<Result<HistoryPage>> {
        let rpc = self.rpc.clone();
        cx.spawn(async move |_| {
            let response = rpc.request("thread/turns/list", json!({"threadId":id,"cursor":cursor,"limit":20,"sortDirection":"desc","itemsView":"summary"})).await?;
            crate::codex_history::decode_page(response, cursor.as_deref())
        })
    }
    fn auth_methods(&self) -> &[acp::AuthMethod] {
        &[]
    }
    fn authenticate(&self, _: acp::AuthMethodId, _: &mut App) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "Native login UI is not implemented; use the configured Codex CLI"
        )))
    }
    fn prompt(
        &self,
        params: acp::PromptRequest,
        cx: &mut App,
    ) -> Task<Result<acp::PromptResponse>> {
        let input = match events::input(&params.prompt) {
            Ok(input) => input,
            Err(error) => return Task::ready(Err(error)),
        };
        let (completion, receiver) = oneshot::channel();
        let mut sessions = self.sessions.borrow_mut();
        let Some(session) = sessions.get_mut(&params.session_id) else {
            return Task::ready(Err(anyhow::anyhow!("Native Codex session not open")));
        };
        if session.active.is_some() {
            return Task::ready(Err(anyhow::anyhow!("A Codex turn is already running")));
        }
        if session.uncertain {
            return Task::ready(Err(anyhow::anyhow!(
                "Previous Codex operation has an unknown outcome; reconnect before sending another turn"
            )));
        }
        let turn_params = session.preferences.turn_params(&params.session_id, input);
        session.active = Some(ActiveTurn {
            id: None,
            completion,
            events: TurnEvents::default(),
            cancel_requested: false,
        });
        let rpc = self.rpc.clone();
        let sessions = self.sessions.clone();
        cx.spawn(async move |_| {
            let response = match rpc.request("turn/start", turn_params).await {
                Ok(response) => response,
                Err(error) => {
                    if let Some(session) = sessions.borrow_mut().get_mut(&params.session_id) {
                        session.active.take();
                        session.uncertain = true;
                    }
                    return Err(error);
                }
            };
            // Completion can arrive before the response. The receiver was registered before send.
            let turn_id = text(&response["turn"], "id")?.to_owned();
            let cancel = {
                let mut sessions = sessions.borrow_mut();
                if let Some(active) = sessions
                    .get_mut(&params.session_id)
                    .and_then(|session| session.active.as_mut())
                {
                    let interrupt = active.id.is_none() && active.cancel_requested;
                    active.id = Some(turn_id.clone());
                    interrupt
                } else {
                    false
                }
            };
            if cancel {
                rpc.request(
                    "turn/interrupt",
                    json!({"threadId":params.session_id,"turnId":turn_id}),
                )
                .await?;
            }
            receiver.await.context("Codex turn completion was lost")?
        })
    }
    fn cancel(&self, id: &acp::SessionId, cx: &mut App) {
        let turn = self
            .sessions
            .borrow_mut()
            .get_mut(id)
            .and_then(|session| session.active.as_mut())
            .and_then(|active| {
                active.cancel_requested = true;
                active.id.clone()
            });
        if let Some(turn) = turn {
            let rpc = self.rpc.clone();
            let id = id.clone();
            cx.spawn(async move |_| {
                rpc.request("turn/interrupt", json!({"threadId":id,"turnId":turn}))
                    .await
                    .map(|_| ())
            })
            .detach_and_log_err(cx);
        }
    }
    fn supports_close_session(&self) -> bool {
        true
    }
    fn close_session(self: Rc<Self>, id: &acp::SessionId, cx: &mut App) -> Task<Result<()>> {
        let id = id.clone();
        cx.spawn(async move |_| {
            ensure!(
                !self
                    .sessions
                    .borrow()
                    .get(&id)
                    .is_some_and(|session| session.active.is_some()),
                "Stop the active turn before closing its session"
            );
            self.rpc
                .request("thread/unsubscribe", json!({"threadId":id}))
                .await?;
            self.sessions.borrow_mut().remove(&id);
            Ok(())
        })
    }
    fn set_title(&self, id: &acp::SessionId, _: &App) -> Option<Rc<dyn AgentSessionSetTitle>> {
        Some(Rc::new(Title {
            rpc: self.rpc.clone(),
            id: id.clone(),
        }))
    }
    fn session_config_options(
        &self,
        id: &acp::SessionId,
        cx: &App,
    ) -> Option<Rc<dyn acp_thread::AgentSessionConfigOptions>> {
        let sessions = self.sessions.borrow();
        let session = sessions.get(id)?;
        self.quota.refresh(cx, false);
        Some(Rc::new(ConfigOptions {
            sessions: Rc::downgrade(&self.sessions),
            id: id.clone(),
            quota: self.quota.clone(),
            sender: session.settings_sender.clone(),
            receiver: session.settings_receiver.clone(),
            quota_watch: RefCell::new(None),
        }))
    }
    fn session_list(&self, _: &mut App) -> Option<Rc<dyn AgentSessionList>> {
        Some(Rc::new(SessionList(self.rpc.clone())))
    }
    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

struct ConfigOptions {
    sessions: std::rc::Weak<RefCell<HashMap<acp::SessionId, Session>>>,
    id: acp::SessionId,
    quota: Rc<settings::Quota>,
    sender: Rc<RefCell<watch::Sender<()>>>,
    receiver: watch::Receiver<()>,
    quota_watch: RefCell<Option<Task<()>>>,
}
impl acp_thread::AgentSessionConfigOptions for ConfigOptions {
    fn config_options(&self) -> Vec<acp::SessionConfigOption> {
        let mut options = self
            .sessions
            .upgrade()
            .and_then(|sessions| {
                sessions
                    .borrow()
                    .get(&self.id)
                    .map(|session| session.preferences.options())
            })
            .unwrap_or_default();
        options.push(self.quota.option());
        options
    }
    fn set_config_option(
        &self,
        id: acp::SessionConfigId,
        value: acp::SessionConfigOptionValue,
        cx: &mut App,
    ) -> Task<Result<Vec<acp::SessionConfigOption>>> {
        let result = (|| {
            if id.0.as_ref() == "weekly-usage" {
                ensure!(
                    value
                        .as_value_id()
                        .is_some_and(|value| matches!(value.0.as_ref(), "refresh" | "status")),
                    "Invalid quota action"
                );
                self.quota.refresh(cx, true);
            } else {
                let sessions = self
                    .sessions
                    .upgrade()
                    .context("Codex connection was closed")?;
                let mut sessions = sessions.borrow_mut();
                let session = sessions
                    .get_mut(&self.id)
                    .context("Codex session was closed")?;
                ensure!(
                    session.active.is_none() && !session.uncertain,
                    "Stop the turn or reconnect before changing Codex settings"
                );
                session.preferences.change(id.0.as_ref(), &value)?;
            }
            self.sender.borrow_mut().send(()).log_err();
            Ok(self.config_options())
        })();
        Task::ready(result)
    }
    fn watch(&self, cx: &mut App) -> Option<watch::Receiver<()>> {
        let mut receiver = self.quota.receiver.clone();
        let sender = self.sender.clone();
        *self.quota_watch.borrow_mut() = Some(cx.foreground_executor().spawn(async move {
            while receiver.recv().await.is_ok() {
                sender.borrow_mut().send(()).log_err();
            }
        }));
        Some(self.receiver.clone())
    }
}

struct Title {
    rpc: Rpc,
    id: acp::SessionId,
}
impl AgentSessionSetTitle for Title {
    fn run(&self, title: SharedString, cx: &mut App) -> Task<Result<()>> {
        let rpc = self.rpc.clone();
        let id = self.id.clone();
        cx.spawn(async move |_| {
            rpc.request(
                "thread/name/set",
                json!({"threadId":id,"name":title.as_ref()}),
            )
            .await
            .map(|_| ())
        })
    }
}
struct SessionList(Rpc);
impl AgentSessionList for SessionList {
    fn list_sessions(
        &self,
        request: AgentSessionListRequest,
        cx: &mut App,
    ) -> Task<Result<AgentSessionListResponse>> {
        let rpc = self.0.clone();
        cx.spawn(async move |_| {
            let response = rpc.request("thread/list", json!({"cwd":request.cwd,"cursor":request.cursor,"limit":50,"sortKey":"updated_at","modelProviders":[],"archived":false})).await?;
            let mut sessions = Vec::new();
            for value in response["data"].as_array().context("Missing Codex thread list")? {
                let mut info = AgentSessionInfo::new(text(value, "id")?.to_owned());
                info.title = value["name"].as_str().or_else(|| value["preview"].as_str()).map(|value| value.to_owned().into());
                info.work_dirs = value["cwd"].as_str().map(|cwd| PathList::new(&[cwd]));
                info.updated_at = value["updatedAt"].as_i64().and_then(|time| chrono::DateTime::from_timestamp(time, 0));
                info.created_at = value["createdAt"].as_i64().and_then(|time| chrono::DateTime::from_timestamp(time, 0));
                sessions.push(info);
            }
            let mut result = AgentSessionListResponse::new(sessions);
            result.next_cursor = response["nextCursor"].as_str().map(str::to_owned);
            Ok(result)
        })
    }
    fn supports_delete(&self) -> bool {
        true
    }
    fn delete_session(&self, id: &acp::SessionId, cx: &mut App) -> Task<Result<()>> {
        let rpc = self.0.clone();
        let id = id.clone();
        cx.spawn(async move |_| {
            rpc.request("thread/archive", json!({"threadId":id}))
                .await
                .map(|_| ())
        })
    }
    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}
