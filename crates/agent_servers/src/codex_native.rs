//! Native Codex integration under construction. Not selected by production routing.
//! Keep the legacy connector until the parity gates in NATIVE_CODEX.md pass.
mod events;
mod forks;
mod permissions;
mod profiles;
mod questions;
mod recovery;
mod settings;
mod transport;

use acp_thread::{
    AcpThread, AgentConnection, AgentSessionInfo, AgentSessionList, AgentSessionListRequest,
    AgentSessionListResponse, AgentSessionSetTitle, AuthRequired, AuthorizationKind, HistoryPage,
    PermissionOptions, RequestPermissionOutcome,
};
use action_log::ActionLog;
use agent_client_protocol::schema::{v1 as acp, v2 as acp_v2};
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
    optimistic_message: Option<acp_thread::ClientUserMessageId>,
    recovery: recovery::Progress,
}

struct Session {
    thread: WeakEntity<AcpThread>,
    active: Option<ActiveTurn>,
    uncertain: bool,
    forking: bool,
    preferences: settings::Preferences,
    attached: bool,
    work_dirs: PathList,
    history_notice: Option<usize>,
    settings_sender: Rc<RefCell<watch::Sender<()>>>,
    settings_receiver: watch::Receiver<()>,
}

struct Approval {
    _task: Task<()>,
    thread: Option<WeakEntity<AcpThread>>,
    permission: Rc<RefCell<Option<acp_thread::PermissionRequestId>>>,
    elicitation: Rc<RefCell<Option<acp_thread::ElicitationEntryId>>>,
}

impl Approval {
    fn cancel_ui(&self, cx: &mut AsyncApp) {
        let elicitation = self.elicitation.borrow_mut().take();
        if let (Some(thread), Some(id)) = (&self.thread, elicitation) {
            thread
                .update(cx, |thread, cx| thread.cancel_elicitation(&id, cx))
                .log_err();
        }
        let permission = self.permission.borrow_mut().take();
        if let (Some(thread), Some(id)) = (&self.thread, permission) {
            thread
                .update(cx, |thread, cx| thread.cancel_permission_request(id, cx))
                .log_err();
        }
    }
}

pub(crate) struct NativeCodexConnection {
    rpc: Rpc,
    sessions: Rc<RefCell<HashMap<acp_v2::SessionId, Session>>>,
    approvals: RefCell<HashMap<String, Approval>>,
    event_task: RefCell<Option<Task<()>>>,
    profile_home: RefCell<Option<PathBuf>>,
    profile_override: RefCell<Option<String>>,
    models: RefCell<Rc<Vec<settings::Model>>>,
    quota: Rc<settings::Quota>,
    fork_preferences: RefCell<HashMap<acp_v2::SessionId, settings::Preferences>>,
}

#[derive(Clone)]
struct SessionRuntime {
    rpc: Rpc,
    models: Rc<Vec<settings::Model>>,
    profile_home: Option<PathBuf>,
    profile_override: Option<String>,
}

impl SessionRuntime {
    async fn restore(
        &self,
        id: &acp_v2::SessionId,
        work_dirs: &PathList,
        cx: &mut AsyncApp,
    ) -> Result<settings::Preferences> {
        let models = if self.models.is_empty() {
            settings::catalog(&self.rpc).await?
        } else {
            self.models.clone()
        };
        let response = NativeCodexConnection::runtime_response(
            &self.rpc,
            self.profile_home.clone(),
            self.profile_override.clone(),
            work_dirs,
            Some(id),
            cx,
        )
        .await?;
        let result = (|| {
            ensure!(
                text(&response["thread"], "id")? == id.0.as_ref(),
                "Codex returned an unexpected session identity"
            );
            ensure!(
                response["thread"]["status"]["type"].as_str() != Some("active"),
                "This Codex thread is still active; no new turn was sent"
            );
            let mut preferences = settings::Preferences::from_response(models, &response)?;
            cx.update(|cx| preferences.apply_editor_defaults(cx))?;
            Ok(preferences)
        })();
        if result.is_err() {
            self.rpc
                .request("thread/unsubscribe", json!({"threadId":id}))
                .await
                .log_err();
        }
        result
    }
}

impl NativeCodexConnection {
    fn runtime(&self) -> SessionRuntime {
        SessionRuntime {
            rpc: self.rpc.clone(),
            models: self.models.borrow().clone(),
            profile_home: self.profile_home.borrow().clone(),
            profile_override: self.profile_override.borrow().clone(),
        }
    }
    fn open_task(
        self: Rc<Self>,
        id: Option<acp_v2::SessionId>,
        project: Entity<Project>,
        directories: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let (sender, receiver) = oneshot::channel();
        // A dropped UI waiter must not abandon a server-side thread/start or resume.
        cx.spawn(async move |cx| {
            let result = self.clone().open(id, project, directories, title, cx).await;
            if let Err(result) = sender.send(result) {
                match result {
                    Ok(thread) => {
                        let id = thread.read_with(cx, |thread, _| thread.session_id().clone());
                        let session = self.sessions.borrow_mut().remove(&id);
                        if session.is_some_and(|session| session.attached) {
                            self.rpc
                                .request("thread/unsubscribe", json!({"threadId":id}))
                                .await
                                .log_err();
                        }
                    }
                    Err(error) => {
                        log::warn!("Cancelled Codex open completed with an error: {error}")
                    }
                }
            }
        })
        .detach();
        cx.spawn(async move |_| {
            receiver
                .await
                .context("Codex open task ended without a result")?
        })
    }

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
        // Discover models when opening a session so a failed connection can fall back to history.
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
            fork_preferences: RefCell::new(HashMap::new()),
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
            if self
                .sessions
                .borrow()
                .values()
                .any(|session| session.attached)
            {
                cx.update(|cx| self.quota.refresh(cx, false));
            }
            return Ok(());
        }
        if method == "account/rateLimits/updated" {
            // Rolling updates are sparse. Refetch the account snapshot instead of replacing
            // complete weekly metadata with a possibly model-specific/partial window.
            if self
                .sessions
                .borrow()
                .values()
                .any(|session| session.attached)
            {
                cx.update(|cx| self.quota.refresh(cx, false));
            }
            return Ok(());
        }
        if method == "serverRequest/resolved" {
            if let Some(id) = params.get("requestId") {
                for session in self.sessions.borrow_mut().values_mut() {
                    if let Some(active) = session.active.as_mut() {
                        active.recovery.resolve_request(&id.to_string());
                    }
                }
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
        let id = acp_v2::SessionId::new(thread_id);
        if self
            .sessions
            .borrow()
            .get(&id)
            .is_some_and(|session| !session.attached)
        {
            return Ok(());
        }
        if matches!(method, "item/started" | "item/completed")
            && params["item"]["type"] == "userMessage"
        {
            let binding = {
                let mut sessions = self.sessions.borrow_mut();
                sessions.get_mut(&id).and_then(|session| {
                    let active = session.active.as_mut()?;
                    // A late event from the previous turn must not bind the new draft.
                    if active.id.as_deref().is_none()
                        || active.id.as_deref() != params["turnId"].as_str()
                    {
                        return None;
                    }
                    active
                        .optimistic_message
                        .take()
                        .map(|client_id| (session.thread.clone(), client_id))
                })
            };
            if let Some((thread, client_id)) = binding {
                let message_id = acp::MessageId::new(text(&params["item"], "id")?);
                thread.update(cx, |thread, cx| {
                    thread.bind_optimistic_message_id(&client_id, message_id, cx)
                })??;
            }
        }
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
            active.recovery.observe(method, &params);
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
                .get(&acp_v2::SessionId::new(id))
                .filter(|session| session.attached)
                .map(|session| session.thread.clone())
        });
        if (method != "item/tool/requestUserInput" || questions::is_blocking(&params))
            && let Some(id) = params["threadId"].as_str()
        {
            if let Some(active) = self
                .sessions
                .borrow_mut()
                .get_mut(&acp_v2::SessionId::new(id))
                .and_then(|session| session.active.as_mut())
            {
                active.recovery.begin_request(key.clone());
            }
        }
        let rpc = self.rpc.clone();
        let approval_thread = thread.clone();
        let permission = Rc::new(RefCell::new(None));
        let task_permission = permission.clone();
        let weak = Rc::downgrade(self);
        let task_key = key.clone();
        let elicitation = Rc::new(RefCell::new(None));
        let task_elicitation = elicitation.clone();
        let task = cx.spawn(async move |cx| {
            let result: Result<Value> = async {
                if matches!(method.as_str(), "item/tool/requestUserInput" | "mcpServer/elicitation/request") {
                    let thread = thread.context("Question requested for an unopened thread")?;
                    let mcp = method == "mcpServer/elicitation/request";
                    let request = if mcp { questions::mcp_request(&params)? } else { questions::request(&params)? };
                    let (entry, response) = thread.update(cx, |thread, cx| thread.request_elicitation_with_id(request, cx))??;
                    *task_elicitation.borrow_mut() = Some(entry);
                    let response = response.await;
                    return if mcp { questions::mcp_response(response) } else { questions::response(&params, response) };
                }
                let choices = permissions::Choices::new(&method, &params)?;
                let thread = thread.context("Approval requested for an unopened thread")?;
                let item_id = text(&params, "itemId")?.to_owned();
                let title = params["command"]
                    .as_str()
                    .or_else(|| params["reason"].as_str())
                    .unwrap_or("Codex requests permission");
                let tool: acp::ToolCallUpdate = serde_json::from_value(json!({
                    "toolCallId":item_id,"title":title,"status":"pending",
                    "content":[{"type":"content","content":{"type":"text","text":serde_json::to_string_pretty(&params)?}}]
                }))?;
                let (permission_id, response) = thread
                    .update(cx, |thread, cx| {
                        thread.request_tool_call_authorization_with_id(
                            tool,
                            PermissionOptions::Flat(choices.options.clone()),
                            AuthorizationKind::PermissionGrant,
                            cx,
                        )
                    })??;
                *task_permission.borrow_mut() = Some(permission_id);
                let outcome = response.await;
                match outcome {
                    RequestPermissionOutcome::Selected(outcome) => choices.reply(Some(outcome.option_id.0.as_ref())),
                    RequestPermissionOutcome::Cancelled
                    | RequestPermissionOutcome::InterruptedByFollowUp => choices.reply(None),
                }
            }
            .await;
            rpc.respond(id, result).log_err();
            if let Some(connection) = weak.upgrade() {
                for session in connection.sessions.borrow_mut().values_mut() {
                    if let Some(active) = session.active.as_mut() {
                        active.recovery.resolve_request(&task_key);
                    }
                }
                connection.approvals.borrow_mut().remove(&task_key);
            }
        });
        self.approvals.borrow_mut().insert(
            key,
            Approval {
                _task: task,
                thread: approval_thread,
                permission,
                elicitation,
            },
        );
    }

    async fn runtime_response(
        rpc: &Rpc,
        profile_home: Option<PathBuf>,
        profile_override: Option<String>,
        work_dirs: &PathList,
        session_id: Option<&acp_v2::SessionId>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let account = rpc
            .request("account/read", json!({"refreshToken":false}))
            .await?;
        if account["requiresOpenaiAuth"].as_bool() == Some(true) && account["account"].is_null() {
            return Err(AuthRequired::new()
                .with_description("Sign in using the configured Codex CLI, then reconnect.".into())
                .into());
        }
        let mut params = json!({"cwd":work_dirs.ordered_paths().next(),"sandbox":"read-only","approvalPolicy":"on-request"});
        if let (Some(home), Some(cwd)) = (profile_home, work_dirs.ordered_paths().next()) {
            let cwd = cwd.to_string_lossy().into_owned();
            let profile_cwd = cwd.clone();
            let profile = cx
                .background_spawn(async move {
                    profiles::load(&home, &profile_cwd, profile_override.as_deref())
                })
                .await?;
            if let Some(profile) = profile {
                let (catalog, effective) = futures::try_join!(
                    rpc.request("skills/list", json!({"cwds":[cwd],"forceReload":true})),
                    rpc.request("config/read", json!({"cwd":cwd,"includeLayers":false}))
                )?;
                params["config"] =
                    profile.apply(&catalog, &effective["config"]["skills"]["config"])?;
            }
        }
        let method = if let Some(id) = session_id {
            params["threadId"] = json!(id);
            params["excludeTurns"] = json!(true);
            "thread/resume"
        } else {
            "thread/start"
        };
        rpc.request(method, params).await
    }

    async fn open(
        self: Rc<Self>,
        session_id: Option<acp_v2::SessionId>,
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
        let mut attached = session_id.is_none();
        let response = if let Some(id) = &session_id {
            ensure!(
                !self.sessions.borrow().contains_key(id),
                "Native Codex session is already open"
            );
            self.rpc
                .request("thread/read", json!({"threadId":id,"includeTurns":false}))
                .await?
        } else {
            if self.models.borrow().is_empty() {
                let models = settings::catalog(&self.rpc).await?;
                *self.models.borrow_mut() = models;
            }
            Self::runtime_response(
                &self.rpc,
                self.profile_home.borrow().clone(),
                self.profile_override.borrow().clone(),
                &work_dirs,
                None,
                cx,
            )
            .await?
        };
        let id = acp_v2::SessionId::new(text(&response["thread"], "id")?);
        ensure!(
            !id.0.is_empty() && session_id.as_ref().is_none_or(|expected| expected == &id),
            "Codex returned an unexpected session identity"
        );
        let result: Result<Entity<AcpThread>> = async {
            let mut preferences = if attached {
                let mut preferences = settings::Preferences::from_response(self.models.borrow().clone(), &response)?;
                cx.update(|cx| preferences.apply_editor_defaults(cx))?;
                preferences
            } else {
                match self.runtime().restore(&id, &work_dirs, cx).await {
                    Ok(preferences) => {
                        attached = true;
                        preferences
                    }
                    Err(error) => {
                        log::warn!("Codex connection unavailable; opening stored history: {error}");
                        settings::Preferences::history()
                    }
                }
            };
            let tail = if session_id.is_some() {
                let response = self
                .rpc
                .request(
                    "thread/turns/list",
                    json!({"threadId":id,"limit":20,"sortDirection":"desc","itemsView":"summary"}),
                )
                .await?;
                Some(
                    cx.background_spawn(async move {
                        crate::codex_history::decode_page(response, None)
                    })
                    .await?,
                )
            } else {
                None
            };
            let action_log = cx.new(|_| ActionLog::new(project.clone()));
            let connection = self.clone();
            let thread = cx.new(|cx| {
                let weak_connection = Rc::downgrade(&connection);
                let released_id = id.clone();
                let entity_id = cx.entity().entity_id();
                cx.on_release(move |_, cx| {
                    let Some(connection) = weak_connection.upgrade() else {
                        return;
                    };
                    let removed = {
                        let mut sessions = connection.sessions.borrow_mut();
                        if sessions
                            .get(&released_id)
                            .is_some_and(|session| session.thread.entity_id() == entity_id)
                        {
                            sessions.remove(&released_id)
                        } else {
                            None
                        }
                    };
                    if let Some(session) = removed.filter(|session| session.attached) {
                        let rpc = connection.rpc.clone();
                        cx.spawn(async move |_| {
                            if let Some(turn) = session.active.and_then(|turn| turn.id) {
                                rpc.request(
                                    "turn/interrupt",
                                    json!({"threadId":released_id,"turnId":turn}),
                                )
                                .await
                                .log_err();
                            }
                            rpc.request("thread/unsubscribe", json!({"threadId":released_id}))
                                .await
                                .log_err();
                        })
                        .detach();
                    }
                })
                .detach();
                AcpThread::new(
                    None,
                    title,
                    Some(work_dirs.clone()),
                    connection,
                    project,
                    action_log,
                    id.clone(),
                    watch::Receiver::constant(acp_thread::prompt_capabilities::from_v1(
                        acp::PromptCapabilities::new()
                            .image(true)
                            .audio(true)
                            .embedded_context(true),
                    )),
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
            if let Some(inherited) = self.fork_preferences.borrow_mut().remove(&id) {
                preferences = inherited;
            }
            let history_notice = (!attached).then(|| {
                thread.update(cx, |thread, cx| {
                    thread.push_notice(acp_v2::Notice::new(acp_v2::NoticeSeverity::Warning, "Агент недоступен — локальная история")
                        .description("Не удалось подключить агента. История загружена с этого компьютера; повторить подключение можно через настройки чата или при отправке сообщения."), cx);
                    thread.notices().last().map(|(id, _)| *id)
                })
            }).flatten();
            self.sessions.borrow_mut().insert(
                id.clone(),
                Session {
                    thread: thread.downgrade(),
                    active: None,
                    uncertain: false,
                    forking: false,
                    preferences,
                    attached,
                    work_dirs,
                    history_notice,
                    settings_sender: Rc::new(RefCell::new(settings_sender)),
                    settings_receiver,
                },
            );
            Ok(thread)
        }
        .await;
        if let Err(error) = result {
            if !attached {
                return Err(error);
            }
            if let Err(cleanup) = self
                .rpc
                .request("thread/unsubscribe", json!({"threadId":id}))
                .await
            {
                anyhow::bail!(
                    "Opening Codex chat failed: {error}; unsubscribe also failed: {cleanup}"
                );
            }
            return Err(error);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt as _;
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
        let id = acp_v2::SessionId::new("session");
        let options = cx
            .update(|cx| connection.session_config_options(&id, cx))
            .unwrap();
        let quota = peer.next().await;
        assert_eq!(quota["method"], "account/rateLimits/read");
        peer.send(json!({"id":quota["id"],"result":{}})).await;
        for (key, value) in [
            (
                "reasoning-effort",
                acp_v2::SessionConfigOptionValue::from("high"),
            ),
            ("mode", "agent-full-access".into()),
            ("fast-mode", true.into()),
        ] {
            cx.update(|cx| options.set_config_option(acp_v2::SessionConfigId::new(key), value, cx))
                .await
                .unwrap();
        }
        let prompt = cx.update(|cx| {
            connection.prompt(
                acp_v2::PromptRequest::new("session", vec!["hello".into()]),
                cx,
            )
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
                acp_v2::SessionConfigId::new("mode"),
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
                acp_v2::SessionConfigId::new("mode"),
                "native-read-only".into(),
                cx
            ))
            .await
            .is_ok()
        );
    }

    async fn stored(
        connection: &Rc<NativeCodexConnection>,
        peer: &transport::FakePeer,
        project: Entity<Project>,
        cx: &mut TestAppContext,
    ) -> Entity<AcpThread> {
        let load = cx.update(|cx| {
            connection
                .clone()
                .load_session("stored".into(), project, PathList::default(), None, cx)
        });
        let read = peer.next().await;
        assert_eq!(read["method"], "thread/read");
        assert_eq!(read["params"]["includeTurns"], false);
        peer.send(json!({"id":read["id"],"result":{"thread":{"id":"stored","status":{"type":"notLoaded"}}}})).await;
        let connection_attempt = peer.next().await;
        assert_eq!(
            connection_attempt["method"],
            if connection.models.borrow().is_empty() {
                "model/list"
            } else {
                "account/read"
            }
        );
        peer.send(json!({"id":connection_attempt["id"],"error":{"code":-32603,"message":"network unavailable"}})).await;
        let page = peer.next().await;
        assert_eq!(page["method"], "thread/turns/list");
        assert_eq!(page["params"]["limit"], 20);
        assert_eq!(page["params"]["itemsView"], "summary");
        peer.send(json!({"id":page["id"],"result":{"data":[{"items":[{"id":"saved-reply","type":"agentMessage","text":"Saved locally"}]}],"nextCursor":"older"}})).await;
        load.await.expect("local history")
    }

    #[gpui::test]
    async fn connection_failure_opens_local_history_without_mutating_close(
        cx: &mut TestAppContext,
    ) {
        let fs = crate::e2e_tests::init_test(cx).await;
        let project = Project::test(fs, [], cx).await;
        let (rpc, peer, events) = Rpc::fake(cx.background_executor.clone());
        let connection = NativeCodexConnection::from_rpc(rpc, events, &mut cx.to_async());
        let thread = stored(&connection, &peer, project, cx).await;
        assert!(connection.models.borrow().is_empty());
        assert!(!connection.sessions.borrow()[&acp_v2::SessionId::new("stored")].attached);
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.entries().len(), 1);
            assert_eq!(thread.notices().len(), 1);
        });
        assert!(peer.next().now_or_never().is_none());
        cx.update(|cx| connection.clone().close_session(&"stored".into(), cx))
            .await
            .expect("close local view");
        assert!(peer.next().now_or_never().is_none());
        assert!(connection.sessions.borrow().is_empty());
    }

    #[gpui::test]
    async fn stored_chat_connects_by_default_without_starting_a_turn(cx: &mut TestAppContext) {
        let fs = crate::e2e_tests::init_test(cx).await;
        let project = Project::test(fs, [], cx).await;
        let (rpc, peer, events) = Rpc::fake(cx.background_executor.clone());
        let connection = NativeCodexConnection::from_rpc(rpc, events, &mut cx.to_async());
        let load = cx.update(|cx| {
            connection
                .clone()
                .load_session("stored".into(), project, PathList::default(), None, cx)
        });
        let read = peer.next().await;
        assert_eq!(read["method"], "thread/read");
        assert_eq!(read["params"]["includeTurns"], false);
        peer.send(json!({"id":read["id"],"result":{"thread":{"id":"stored"}}}))
            .await;
        let models = peer.next().await;
        assert_eq!(models["method"], "model/list");
        peer.send(json!({"id":models["id"],"result":{"data":[{"model":"a","displayName":"A"}]}}))
            .await;
        let account = peer.next().await;
        assert_eq!(account["method"], "account/read");
        peer.send(json!({"id":account["id"],"result":{"requiresOpenaiAuth":false}}))
            .await;
        let resume = peer.next().await;
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "stored");
        assert_eq!(resume["params"]["excludeTurns"], true);
        peer.send(json!({"id":resume["id"],"result":{"thread":{"id":"stored","status":{"type":"idle"}},"model":"a"}})).await;
        let page = peer.next().await;
        assert_eq!(page["method"], "thread/turns/list");
        assert_eq!(page["params"]["limit"], 20);
        assert_eq!(page["params"]["itemsView"], "summary");
        peer.send(json!({"id":page["id"],"result":{"data":[],"nextCursor":null}}))
            .await;
        let thread = load.await.expect("connected chat");
        assert!(connection.sessions.borrow()[&acp_v2::SessionId::new("stored")].attached);
        thread.read_with(cx, |thread, _| assert!(thread.notices().is_empty()));
        let options = cx
            .update(|cx| connection.session_config_options(&"stored".into(), cx))
            .unwrap();
        assert!(
            options
                .config_options()
                .iter()
                .any(|option| option.config_id.0.as_ref() == "model")
        );
        assert!(
            !options
                .config_options()
                .iter()
                .any(|option| option.config_id.0.as_ref() == "local-history")
        );
        let quota = peer.next().await;
        assert_eq!(quota["method"], "account/rateLimits/read");
        peer.send(json!({"id":quota["id"],"result":{}})).await;
        assert!(peer.next().now_or_never().is_none());
        let close = cx.update(|cx| connection.clone().close_session(&"stored".into(), cx));
        let unsubscribe = peer.next().await;
        assert_eq!(unsubscribe["method"], "thread/unsubscribe");
        peer.send(json!({"id":unsubscribe["id"],"result":{}})).await;
        close.await.unwrap();
    }

    #[gpui::test]
    async fn resume_failure_falls_back_to_paginated_history(cx: &mut TestAppContext) {
        let (connection, peer, source) = session(cx).await;
        let project = source.read_with(cx, |thread, _| thread.project().clone());
        let load = cx.update(|cx| {
            connection
                .clone()
                .load_session("stored".into(), project, PathList::default(), None, cx)
        });
        let read = peer.next().await;
        assert_eq!(read["method"], "thread/read");
        peer.send(json!({"id":read["id"],"result":{"thread":{"id":"stored"}}}))
            .await;
        let account = peer.next().await;
        assert_eq!(account["method"], "account/read");
        peer.send(json!({"id":account["id"],"result":{"requiresOpenaiAuth":false}}))
            .await;
        let resume = peer.next().await;
        assert_eq!(resume["method"], "thread/resume");
        peer.send(
            json!({"id":resume["id"],"error":{"code":-32603,"message":"network unavailable"}}),
        )
        .await;
        let page = peer.next().await;
        assert_eq!(page["method"], "thread/turns/list");
        peer.send(json!({"id":page["id"],"result":{"data":[{"items":[{"id":"reply","type":"agentMessage","text":"Saved locally"}]}]}})).await;
        let thread = load.await.expect("offline fallback");
        assert!(!connection.sessions.borrow()[&acp_v2::SessionId::new("stored")].attached);
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.entries().len(), 1);
            assert_eq!(thread.notices().len(), 1);
        });
        assert!(peer.next().now_or_never().is_none());
    }

    #[gpui::test]
    async fn connection_timeout_still_opens_local_history(cx: &mut TestAppContext) {
        let fs = crate::e2e_tests::init_test(cx).await;
        let project = Project::test(fs, [], cx).await;
        let (rpc, peer, events) = Rpc::fake(cx.background_executor.clone());
        let connection = NativeCodexConnection::from_rpc(rpc, events, &mut cx.to_async());
        let load = cx.update(|cx| {
            connection
                .clone()
                .load_session("stored".into(), project, PathList::default(), None, cx)
        });
        let read = peer.next().await;
        peer.send(json!({"id":read["id"],"result":{"thread":{"id":"stored"}}}))
            .await;
        let models = peer.next().await;
        assert_eq!(models["method"], "model/list");
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(std::time::Duration::from_secs(60));
        let page = peer.next().await;
        assert_eq!(page["method"], "thread/turns/list");
        peer.send(json!({"id":page["id"],"result":{"data":[]}}))
            .await;
        let thread = load.await.expect("history after timeout");
        peer.send(json!({"id":models["id"],"result":{"data":[{"model":"a","displayName":"A"}]}}))
            .await;
        cx.run_until_parked();
        assert!(!connection.sessions.borrow()[&acp_v2::SessionId::new("stored")].attached);
        thread.read_with(cx, |thread, _| assert_eq!(thread.notices().len(), 1));
        assert!(peer.next().now_or_never().is_none());
    }

    #[gpui::test]
    async fn offline_send_failure_keeps_history_and_retry_resumes_before_one_turn(
        cx: &mut TestAppContext,
    ) {
        let (connection, peer, source) = session(cx).await;
        let project = source.read_with(cx, |thread, _| thread.project().clone());
        let thread = stored(&connection, &peer, project, cx).await;
        let send = cx.update(|cx| {
            connection.prompt(
                acp_v2::PromptRequest::new("stored", vec!["continue".into()]),
                cx,
            )
        });
        let account = peer.next().await;
        assert_eq!(account["method"], "account/read");
        peer.send(json!({"id":account["id"],"result":{"requiresOpenaiAuth":false}}))
            .await;
        let resume = peer.next().await;
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "stored");
        assert_eq!(resume["params"]["excludeTurns"], true);
        peer.send(
            json!({"id":resume["id"],"error":{"code":-32603,"message":"network unavailable"}}),
        )
        .await;
        assert!(
            send.await
                .expect_err("offline")
                .to_string()
                .contains("network unavailable")
        );
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.entries().len(), 1);
            assert_eq!(thread.notices().len(), 1);
        });
        assert!(peer.next().now_or_never().is_none());
        let send = cx.update(|cx| {
            connection.prompt(
                acp_v2::PromptRequest::new("stored", vec!["continue".into()]),
                cx,
            )
        });
        let account = peer.next().await;
        assert_eq!(account["method"], "account/read");
        peer.send(json!({"id":account["id"],"result":{"requiresOpenaiAuth":false}}))
            .await;
        let resume = peer.next().await;
        assert_eq!(resume["method"], "thread/resume");
        peer.send(json!({"id":resume["id"],"result":{"thread":{"id":"stored","status":{"type":"idle"}},"model":"a","reasoningEffort":"medium"}})).await;
        let start = peer.next().await;
        assert_eq!(start["method"], "turn/start");
        assert_eq!(start["params"]["threadId"], "stored");
        peer.send(json!({"id":start["id"],"result":{"turn":{"id":"new-turn"}}}))
            .await;
        peer.send(json!({"method":"turn/completed","params":{"threadId":"stored","turn":{"id":"new-turn","status":"completed"}}})).await;
        send.await.expect("online retry");
        assert!(connection.sessions.borrow()[&acp_v2::SessionId::new("stored")].attached);
        thread.read_with(cx, |thread, _| assert!(thread.notices().is_empty()));
        assert!(peer.next().now_or_never().is_none());
    }

    #[gpui::test]
    async fn stopping_during_local_history_connection_does_not_send_a_turn(
        cx: &mut TestAppContext,
    ) {
        let (connection, peer, source) = session(cx).await;
        let project = source.read_with(cx, |thread, _| thread.project().clone());
        let _thread = stored(&connection, &peer, project, cx).await;
        let send = cx.update(|cx| {
            connection.prompt(
                acp_v2::PromptRequest::new("stored", vec!["continue".into()]),
                cx,
            )
        });
        let account = peer.next().await;
        assert_eq!(account["method"], "account/read");
        cx.update(|cx| connection.cancel(&"stored".into(), cx))
            .expect("cancellation request accepted");
        peer.send(json!({"id":account["id"],"result":{"requiresOpenaiAuth":false}}))
            .await;
        let resume = peer.next().await;
        assert_eq!(resume["method"], "thread/resume");
        peer.send(json!({"id":resume["id"],"result":{"thread":{"id":"stored","status":{"type":"idle"}},"model":"a"}})).await;
        assert!(
            send.await
                .expect_err("cancelled")
                .to_string()
                .contains("nothing was sent")
        );
        assert!(peer.next().now_or_never().is_none());
        assert!(
            connection.sessions.borrow()[&acp_v2::SessionId::new("stored")]
                .active
                .is_none()
        );
    }

    #[gpui::test]
    async fn explicit_connection_restores_model_controls_without_sending_a_message(
        cx: &mut TestAppContext,
    ) {
        let (connection, peer, source) = session(cx).await;
        let project = source.read_with(cx, |thread, _| thread.project().clone());
        let thread = stored(&connection, &peer, project, cx).await;
        let options = cx
            .update(|cx| connection.session_config_options(&"stored".into(), cx))
            .expect("options");
        assert!(peer.next().now_or_never().is_none());
        assert!(
            options
                .config_options()
                .iter()
                .any(|option| option.config_id.0.as_ref() == "local-history")
        );
        let connect = cx.update(|cx| {
            options.set_config_option(
                acp_v2::SessionConfigId::new("local-history"),
                "connect".into(),
                cx,
            )
        });
        let account = peer.next().await;
        assert_eq!(account["method"], "account/read");
        peer.send(json!({"id":account["id"],"result":{"requiresOpenaiAuth":false}}))
            .await;
        let resume = peer.next().await;
        assert_eq!(resume["method"], "thread/resume");
        peer.send(json!({"id":resume["id"],"result":{"thread":{"id":"stored","status":{"type":"idle"}},"model":"a"}})).await;
        let connected = connect.await.expect("connected");
        assert!(
            connected
                .iter()
                .any(|option| option.config_id.0.as_ref() == "model")
        );
        assert!(
            !connected
                .iter()
                .any(|option| option.config_id.0.as_ref() == "local-history")
        );
        let quota = peer.next().await;
        assert_eq!(quota["method"], "account/rateLimits/read");
        peer.send(json!({"id":quota["id"],"result":{}})).await;
        assert!(peer.next().now_or_never().is_none());
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.entries().len(), 1);
            assert!(thread.notices().is_empty());
        });
    }

    #[gpui::test]
    async fn invalid_local_history_does_not_resume_or_register_a_broken_session(
        cx: &mut TestAppContext,
    ) {
        let (connection, peer, source) = session(cx).await;
        let project = source.read_with(cx, |thread, _| thread.project().clone());
        let load = cx.update(|cx| {
            connection
                .clone()
                .load_session("broken".into(), project, PathList::default(), None, cx)
        });
        let read = peer.next().await;
        assert_eq!(read["method"], "thread/read");
        peer.send(json!({"id":read["id"],"result":{"thread":{"id":"different"}}}))
            .await;
        assert!(load.await.is_err());
        assert_eq!(connection.sessions.borrow().len(), 1);
        assert!(peer.next().now_or_never().is_none());
    }

    #[gpui::test]
    async fn fork_load_inherits_unsent_preferences_without_changing_source(
        cx: &mut TestAppContext,
    ) {
        let (connection, peer, source) = session(cx).await;
        let id = acp_v2::SessionId::new("session");
        let project = source.read_with(cx, |thread, _| thread.project().clone());
        connection
            .sessions
            .borrow_mut()
            .get_mut(&id)
            .unwrap()
            .preferences
            .change("reasoning-effort", &"high".into())
            .unwrap();
        async fn serve(peer: &transport::FakePeer, method: &str, result: Value) {
            let request = peer.next().await;
            assert_eq!(request["method"], method);
            peer.send(json!({"id":request["id"],"result":result})).await;
        }
        let fork = cx.update(|cx| {
            connection.clone().fork_session(
                id.clone(),
                project.clone(),
                PathList::default(),
                None,
                cx,
            )
        });
        serve(
            &peer,
            "thread/read",
            json!({"thread":{"id":"session","status":{"type":"idle"}}}),
        )
        .await;
        serve(&peer, "thread/fork", json!({"thread":{"id":"copy"}})).await;
        serve(&peer, "thread/unsubscribe", json!({})).await;
        let copy = fork.await.unwrap();
        assert_eq!(connection.sessions.borrow().len(), 1);
        let load = cx.update(|cx| {
            connection
                .clone()
                .load_session(copy.clone(), project, PathList::default(), None, cx)
        });
        serve(
            &peer,
            "thread/read",
            json!({"thread":{"id":"copy","status":{"type":"notLoaded"}}}),
        )
        .await;
        serve(&peer, "account/read", json!({"requiresOpenaiAuth":false})).await;
        serve(
            &peer,
            "thread/resume",
            json!({"thread":{"id":"copy","status":{"type":"idle"}},"model":"a","reasoningEffort":"medium"}),
        )
        .await;
        serve(
            &peer,
            "thread/turns/list",
            json!({"data":[],"nextCursor":null}),
        )
        .await;
        let _copy_thread = load.await.unwrap();
        assert!(connection.fork_preferences.borrow().is_empty());
        let sessions = connection.sessions.borrow();
        for id in [&id, &copy] {
            assert_eq!(
                sessions
                    .get(id)
                    .unwrap()
                    .preferences
                    .turn_params(id, vec![])["effort"],
                "high"
            );
        }
    }

    #[gpui::test]
    async fn send_through_thread_does_not_reenter_its_entity(cx: &mut TestAppContext) {
        let (_connection, peer, thread) = session(cx).await;
        let send = thread.update(cx, |thread, cx| thread.send_raw("hello", cx));
        let send = cx.background_executor.spawn(send);
        let start = peer.next().await;
        assert_eq!(start["method"], "turn/start");
        peer.send(json!({"id":start["id"],"result":{"turn":{"id":"turn"}}}))
            .await;
        peer.send(json!({"method":"item/started","params":{"threadId":"session","turnId":"turn","item":{"id":"user-id","type":"userMessage","content":[{"type":"text","text":"hello"}]}}})).await;
        peer.send(json!({"method":"turn/completed","params":{"threadId":"session","turn":{"id":"turn","status":"completed"}}})).await;
        assert!(send.await.unwrap().is_some());
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.entries().len(), 1);
            let acp_thread::AgentThreadEntry::UserMessage(message) = &thread.entries()[0] else {
                panic!("user message expected");
            };
            assert_eq!(message.identity.legacy_id().unwrap().0.as_ref(), "user-id");
        });
    }

    #[gpui::test]
    async fn live_user_identity_is_bound_once_without_echoing_text(cx: &mut TestAppContext) {
        let (connection, peer, thread) = session(cx).await;
        thread.update(cx, |thread, cx| {
            thread.push_user_content_block(
                Some(acp_thread::ClientUserMessageId::new()),
                "hello".into(),
                cx,
            )
        });
        let prompt = cx.update(|cx| {
            connection.prompt(
                acp_v2::PromptRequest::new("session", vec!["hello".into()]),
                cx,
            )
        });
        let start = peer.next().await;
        peer.send(json!({"id":start["id"],"result":{"turn":{"id":"turn"}}}))
            .await;
        peer.send(
            json!({"method":"turn/started","params":{"threadId":"session","turn":{"id":"turn"}}}),
        )
        .await;
        peer.send(json!({"method":"item/completed","params":{"threadId":"session","turnId":"old-turn","item":{"id":"wrong-id","type":"userMessage"}}})).await;
        for method in ["item/started", "item/completed"] {
            peer.send(json!({"method":method,"params":{"threadId":"session","turnId":"turn","item":{"id":"user-id","type":"userMessage","content":[{"type":"text","text":"hello"}]}}})).await;
        }
        peer.send(json!({"method":"turn/completed","params":{"threadId":"session","turn":{"id":"turn","status":"completed"}}})).await;
        prompt.await.unwrap();
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.entries().len(), 1);
            let acp_thread::AgentThreadEntry::UserMessage(message) = &thread.entries()[0] else {
                panic!("user message expected")
            };
            assert_eq!(message.identity.legacy_id().unwrap().0.as_ref(), "user-id");
            assert_eq!(message.content.source_blocks(), &["hello".into()]);
        });
    }

    #[gpui::test]
    async fn fork_guard_blocks_writes_and_is_released_on_drop(cx: &mut TestAppContext) {
        let (connection, _peer, _thread) = session(cx).await;
        let id = acp_v2::SessionId::new("session");
        let guard = forks::SessionGuard::acquire(connection.sessions.clone(), id.clone()).unwrap();
        assert!(forks::SessionGuard::acquire(connection.sessions.clone(), id.clone()).is_err());
        let prompt = cx.update(|cx| {
            connection.prompt(
                acp_v2::PromptRequest::new(
                    acp_v2::SessionId::new(id.0.clone()),
                    vec!["hello".into()],
                ),
                cx,
            )
        });
        assert!(prompt.await.is_err());
        let close = cx.update(|cx| connection.clone().close_session(&id, cx));
        assert!(close.await.is_err());
        drop(guard);
        assert!(!connection.sessions.borrow().get(&id).unwrap().forking);
        assert!(forks::SessionGuard::acquire(connection.sessions.clone(), id).is_ok());
    }

    #[gpui::test]
    async fn cancellation_before_turn_start_is_forwarded(cx: &mut TestAppContext) {
        let (connection, peer, _thread) = session(cx).await;
        let prompt = cx.update(|cx| {
            connection.prompt(
                acp_v2::PromptRequest::new("session", vec!["hello".into()]),
                cx,
            )
        });
        let request = peer.next().await;
        assert_eq!(request["method"], "turn/start");
        cx.update(|cx| connection.cancel(&acp_v2::SessionId::new("session"), cx))
            .expect("cancellation request accepted");
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
            connection.sessions.borrow()[&acp_v2::SessionId::new("session")]
                .active
                .is_none()
        );
    }

    #[gpui::test]
    async fn disconnect_fails_prompt_without_replaying_it(cx: &mut TestAppContext) {
        let (connection, peer, _thread) = session(cx).await;
        let prompt = cx.update(|cx| {
            connection.prompt(
                acp_v2::PromptRequest::new("session", vec!["hello".into()]),
                cx,
            )
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
            connection.prompt(
                acp_v2::PromptRequest::new("session", vec!["again".into()]),
                cx,
            )
        });
        assert!(
            retry
                .await
                .unwrap_err()
                .to_string()
                .contains("unknown outcome")
        );
    }

    async fn quiet_prompt(
        cx: &mut TestAppContext,
    ) -> (
        Rc<NativeCodexConnection>,
        transport::FakePeer,
        Entity<AcpThread>,
        Task<Result<acp::PromptResponse>>,
    ) {
        let (connection, peer, thread) = session(cx).await;
        let prompt = cx.update(|cx| {
            connection.prompt(
                acp_v2::PromptRequest::new("session", vec!["hello".into()]),
                cx,
            )
        });
        let request = peer.next().await;
        peer.send(json!({"id":request["id"],"result":{"turn":{"id":"turn"}}}))
            .await;
        cx.run_until_parked();
        (connection, peer, thread, prompt)
    }

    fn quiet_minutes(cx: &mut TestAppContext, minutes: usize) {
        for _ in 0..minutes {
            cx.background_executor
                .advance_clock(std::time::Duration::from_secs(60));
            cx.run_until_parked();
        }
    }

    #[gpui::test]
    async fn recovery_reads_terminal_state_without_loading_history_or_replay(
        cx: &mut TestAppContext,
    ) {
        let (connection, peer, _thread, prompt) = quiet_prompt(cx).await;
        quiet_minutes(cx, 3);
        let read = peer.next().await;
        assert_eq!(read["method"], "thread/read");
        assert_eq!(read["params"]["includeTurns"], false);
        peer.send(
            json!({"id":read["id"],"result":{"thread":{"id":"session","status":{"type":"idle"}}}}),
        )
        .await;
        let turns = peer.next().await;
        assert_eq!(turns["method"], "thread/turns/list");
        assert_eq!(turns["params"]["itemsView"], "notLoaded");
        peer.send(json!({"id":turns["id"],"result":{"data":[{"id":"turn","status":"failed","error":{"message":"connection failed"}}]}})).await;
        assert!(
            prompt
                .await
                .expect_err("failed turn")
                .to_string()
                .contains("connection failed")
        );
        let sessions = connection.sessions.borrow();
        let session = &sessions[&acp_v2::SessionId::new("session")];
        assert!(session.active.is_none());
        assert!(!session.uncertain);
    }

    #[gpui::test]
    async fn recovery_missing_reply_requires_reload_not_false_success(cx: &mut TestAppContext) {
        let (connection, peer, _thread, prompt) = quiet_prompt(cx).await;
        quiet_minutes(cx, 3);
        let read = peer.next().await;
        peer.send(
            json!({"id":read["id"],"result":{"thread":{"id":"session","status":{"type":"idle"}}}}),
        )
        .await;
        let turns = peer.next().await;
        peer.send(json!({"id":turns["id"],"result":{"data":[{"id":"turn","status":"completed"}]}}))
            .await;
        assert!(
            prompt
                .await
                .expect_err("missing reply")
                .to_string()
                .contains("load the saved reply")
        );
        assert!(connection.sessions.borrow()[&acp_v2::SessionId::new("session")].uncertain);
        let retry = cx.update(|cx| {
            connection.prompt(
                acp_v2::PromptRequest::new("session", vec!["retry".into()]),
                cx,
            )
        });
        assert!(
            retry
                .await
                .expect_err("no automatic replay")
                .to_string()
                .contains("unknown outcome")
        );
    }

    #[gpui::test]
    async fn recovery_silent_generation_interrupts_only_its_exact_turn(cx: &mut TestAppContext) {
        let (connection, peer, _thread, prompt) = quiet_prompt(cx).await;
        for minute in 1..=15 {
            quiet_minutes(cx, 1);
            if minute >= 3 {
                let read = peer.next().await;
                assert_eq!(read["method"], "thread/read");
                assert_eq!(read["params"]["threadId"], "session");
                peer.send(json!({"id":read["id"],"result":{"thread":{"id":"session","status":{"type":"active","activeFlags":[]}}}})).await;
                cx.run_until_parked();
            }
        }
        let interrupt = peer.next().await;
        assert_eq!(interrupt["method"], "turn/interrupt");
        assert_eq!(
            interrupt["params"],
            json!({"threadId":"session","turnId":"turn"})
        );
        peer.send(json!({"id":interrupt["id"],"result":{}})).await;
        peer.send(json!({"method":"turn/completed","params":{"threadId":"session","turn":{"id":"turn","status":"interrupted"}}})).await;
        assert_eq!(
            prompt.await.expect("cancelled").stop_reason,
            acp::StopReason::Cancelled
        );
        assert!(
            connection.sessions.borrow()[&acp_v2::SessionId::new("session")]
                .active
                .is_none()
        );
    }

    #[gpui::test]
    async fn recovery_does_not_interrupt_long_tools_or_permission_waits(cx: &mut TestAppContext) {
        let (connection, peer, _thread, prompt) = quiet_prompt(cx).await;
        {
            let mut sessions = connection.sessions.borrow_mut();
            let active = sessions
                .get_mut(&acp_v2::SessionId::new("session"))
                .expect("session")
                .active
                .as_mut()
                .expect("turn");
            active.recovery.observe(
                "item/started",
                &json!({"item":{"id":"long-command","type":"commandExecution"}}),
            );
        }
        for minute in 1..=18 {
            quiet_minutes(cx, 1);
            if minute >= 4 {
                let read = peer.next().await;
                assert_eq!(read["method"], "thread/read");
                peer.send(json!({"id":read["id"],"result":{"thread":{"id":"session","status":{"type":"active","activeFlags":[]}}}})).await;
                cx.run_until_parked();
            }
        }
        peer.send(json!({"method":"turn/completed","params":{"threadId":"session","turn":{"id":"turn","status":"completed"}}})).await;
        assert_eq!(
            prompt.await.expect("completed tool").stop_reason,
            acp::StopReason::EndTurn
        );
    }

    #[gpui::test]
    async fn recovery_rechecks_progress_before_interrupting(cx: &mut TestAppContext) {
        let (_connection, peer, _thread, prompt) = quiet_prompt(cx).await;
        quiet_minutes(cx, 3);
        let read = peer.next().await;
        peer.send(json!({"method":"item/agentMessage/delta","params":{"threadId":"session","turnId":"turn","itemId":"reply","delta":"resumed"}})).await;
        peer.send(
            json!({"id":read["id"],"result":{"thread":{"id":"session","status":{"type":"idle"}}}}),
        )
        .await;
        cx.run_until_parked();
        peer.send(json!({"method":"turn/completed","params":{"threadId":"session","turn":{"id":"turn","status":"completed"}}})).await;
        assert_eq!(
            prompt.await.expect("resumed").stop_reason,
            acp::StopReason::EndTurn
        );
    }

    #[gpui::test]
    async fn recovery_backend_approval_wait_does_not_consume_response_deadline(
        cx: &mut TestAppContext,
    ) {
        let (_connection, peer, _thread, prompt) = quiet_prompt(cx).await;
        for minute in 1..=19 {
            quiet_minutes(cx, 1);
            if minute % 3 == 0 {
                let read = peer.next().await;
                assert_eq!(read["method"], "thread/read");
                peer.send(json!({"id":read["id"],"result":{"thread":{"id":"session","status":{"type":"active","activeFlags":["waitingOnApproval"]}}}})).await;
                cx.run_until_parked();
            }
        }
        peer.send(json!({"method":"turn/completed","params":{"threadId":"session","turn":{"id":"turn","status":"completed"}}})).await;
        assert_eq!(
            prompt.await.expect("approval wait").stop_reason,
            acp::StopReason::EndTurn
        );
    }

    #[gpui::test]
    async fn recovery_manual_stop_does_not_wait_forever_for_completion(cx: &mut TestAppContext) {
        let (connection, peer, _thread, prompt) = quiet_prompt(cx).await;
        cx.update(|cx| connection.cancel(&acp_v2::SessionId::new("session"), cx))
            .expect("cancellation request accepted");
        let interrupt = peer.next().await;
        peer.send(json!({"id":interrupt["id"],"result":{}})).await;
        quiet_minutes(cx, 1);
        let read = peer.next().await;
        assert_eq!(read["method"], "thread/read");
        peer.send(json!({"id":read["id"],"result":{"thread":{"id":"session","status":{"type":"active","activeFlags":[]}}}})).await;
        assert!(
            prompt
                .await
                .expect_err("unconfirmed stop")
                .to_string()
                .contains("not confirmed cancellation")
        );
        assert!(connection.sessions.borrow()[&acp_v2::SessionId::new("session")].uncertain);
    }

    #[gpui::test]
    async fn recovery_status_failures_release_ui_but_block_unknown_replay(cx: &mut TestAppContext) {
        let (connection, peer, _thread, prompt) = quiet_prompt(cx).await;
        quiet_minutes(cx, 3);
        for attempt in 0..3 {
            if attempt > 0 {
                quiet_minutes(cx, 1);
            }
            let read = peer.next().await;
            assert_eq!(read["method"], "thread/read");
            peer.send(json!({"id":read["id"],"error":{"code":-32603,"message":"temporarily unavailable"}})).await;
            cx.run_until_parked();
        }
        assert!(
            prompt
                .await
                .expect_err("unreachable")
                .to_string()
                .contains("outcome is unknown")
        );
        let sessions = connection.sessions.borrow();
        let session = &sessions[&acp_v2::SessionId::new("session")];
        assert!(session.active.is_none());
        assert!(session.uncertain);
    }

    #[gpui::test]
    async fn completion_before_start_response_and_interleaved_thread_are_safe(
        cx: &mut TestAppContext,
    ) {
        let (connection, peer, thread) = session(cx).await;
        let prompt = cx.update(|cx| {
            connection.prompt(
                acp_v2::PromptRequest::new("session", vec!["hello".into()]),
                cx,
            )
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
                acp_v2::ToolCallId::new("tool"),
                acp_thread::SelectedPermissionOutcome::new(
                    acp_v2::PermissionOptionId::new("2"),
                    acp_v2::PermissionOptionKind::RejectOnce,
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

    #[gpui::test]
    async fn stale_approval_cleanup_does_not_cancel_a_replacement_request(cx: &mut TestAppContext) {
        let (connection, peer, thread) = session(cx).await;
        peer.send(json!({"id":"old-approval","method":"item/commandExecution/requestApproval","params":{"threadId":"session","turnId":"turn","itemId":"tool","command":"echo first"}})).await;
        cx.run_until_parked();
        let permission = connection
            .approvals
            .borrow()
            .get("\"old-approval\"")
            .expect("old approval")
            .permission
            .clone();
        let old_id = permission.borrow().expect("old request ID");
        let stale = Approval {
            _task: Task::ready(()),
            thread: Some(thread.downgrade()),
            permission,
            elicitation: Rc::new(RefCell::new(None)),
        };
        peer.send(json!({"id":"new-approval","method":"item/commandExecution/requestApproval","params":{"threadId":"session","turnId":"turn","itemId":"tool","command":"echo second"}})).await;
        cx.run_until_parked();
        let old_reply = peer.next().await;
        assert_eq!(old_reply["id"], "old-approval");
        let new_id = thread.read_with(cx, |thread, _| {
            thread
                .tool_call(&acp_v2::ToolCallId::new("tool"))
                .expect("tool")
                .1
                .authorization_id()
                .expect("replacement authorization")
        });
        assert_ne!(old_id, new_id);
        stale.cancel_ui(&mut cx.to_async());
        thread.read_with(cx, |thread, _| {
            assert_eq!(
                thread
                    .tool_call(&acp_v2::ToolCallId::new("tool"))
                    .expect("tool")
                    .1
                    .authorization_id(),
                Some(new_id)
            );
        });
        thread.update(cx, |thread, cx| {
            thread.authorize_tool_call(
                acp_v2::ToolCallId::new("tool"),
                acp_thread::SelectedPermissionOutcome::new(
                    acp_v2::PermissionOptionId::new("2"),
                    acp_v2::PermissionOptionKind::RejectOnce,
                ),
                cx,
            );
        });
        let reply = peer.next().await;
        assert_eq!(reply["id"], "new-approval");
        assert_eq!(reply["result"]["decision"], "decline");
    }

    #[gpui::test]
    async fn questions_wait_for_input_and_return_the_actual_answer(cx: &mut TestAppContext) {
        let (connection, peer, thread) = session(cx).await;
        peer.send(json!({"id":"question","method":"item/tool/requestUserInput","params":{"threadId":"session","itemId":"ask","isBlocking":false,"questions":[{"id":"answer","header":"Input","question":"What next?","isOther":true,"isSecret":false,"options":null}]}})).await;
        cx.run_until_parked();
        assert_eq!(connection.approvals.borrow().len(), 1);
        let entry = connection
            .approvals
            .borrow()
            .get("\"question\"")
            .unwrap()
            .elicitation
            .borrow()
            .clone()
            .unwrap();
        thread.read_with(cx, |thread, _| {
            assert!(!thread.is_waiting_for_confirmation());
            assert!(!thread.elicitation(&entry).unwrap().1.is_blocking());
        });
        thread.update(cx, |thread, cx| {
            thread.respond_to_elicitation(
                &entry,
                acp_v2::CreateElicitationResponse::new(acp_v2::ElicitationAction::Accept(
                    acp_v2::ElicitationAcceptAction::new().content(
                        std::collections::BTreeMap::from([(
                            "answer".to_owned(),
                            "Continue testing".into(),
                        )]),
                    ),
                )),
                cx,
            )
        });
        let reply = peer.next().await;
        assert_eq!(reply["id"], "question");
        assert_eq!(
            reply["result"]["answers"]["answer"]["answers"],
            json!(["Continue testing"])
        );
        cx.run_until_parked();
        assert!(connection.approvals.borrow().is_empty());
        peer.send(json!({"method":"serverRequest/resolved","params":{"threadId":"session","requestId":"question"}})).await;
        cx.run_until_parked();
        thread.read_with(cx, |thread, _| {
            assert!(matches!(
                thread.elicitation(&entry).unwrap().1.status,
                acp_thread::ElicitationStatus::Accepted
            ));
        });
    }

    #[gpui::test]
    async fn cancelled_open_unsubscribes_after_the_server_returns(cx: &mut TestAppContext) {
        let (connection, peer, thread) = session(cx).await;
        let project = thread.read_with(cx, |thread, _| thread.project().clone());
        let pending = cx.update(|cx| {
            connection
                .clone()
                .new_session(project, PathList::default(), cx)
        });
        let account = peer.next().await;
        peer.send(json!({"id":account["id"],"result":{"requiresOpenaiAuth":false}}))
            .await;
        let start = peer.next().await;
        assert_eq!(start["method"], "thread/start");
        drop(pending);
        cx.run_until_parked();
        peer.send(json!({"id":start["id"],"result":{"thread":{"id":"abandoned"},"model":"a"}}))
            .await;
        let unsubscribe = peer.next().await;
        assert_eq!(unsubscribe["method"], "thread/unsubscribe");
        assert_eq!(unsubscribe["params"]["threadId"], "abandoned");
        peer.send(json!({"id":unsubscribe["id"],"result":{}})).await;
        cx.run_until_parked();
        assert_eq!(connection.sessions.borrow().len(), 1);
        assert!(
            connection
                .sessions
                .borrow()
                .contains_key(&acp_v2::SessionId::new("session"))
        );
    }

    #[gpui::test]
    async fn cancelled_fork_archives_only_the_new_copy(cx: &mut TestAppContext) {
        let (connection, peer, thread) = session(cx).await;
        let project = thread.read_with(cx, |thread, _| thread.project().clone());
        let pending = cx.update(|cx| {
            connection.clone().fork_session(
                acp_v2::SessionId::new("session"),
                project,
                PathList::default(),
                None,
                cx,
            )
        });
        let read = peer.next().await;
        peer.send(
            json!({"id":read["id"],"result":{"thread":{"id":"session","status":{"type":"idle"}}}}),
        )
        .await;
        let fork = peer.next().await;
        assert_eq!(fork["method"], "thread/fork");
        drop(pending);
        cx.run_until_parked();
        peer.send(json!({"id":fork["id"],"result":{"thread":{"id":"abandoned-copy"}}}))
            .await;
        let unsubscribe = peer.next().await;
        assert_eq!(unsubscribe["method"], "thread/unsubscribe");
        peer.send(json!({"id":unsubscribe["id"],"result":{}})).await;
        let archive = peer.next().await;
        assert_eq!(archive["method"], "thread/archive");
        assert_eq!(archive["params"]["threadId"], "abandoned-copy");
        peer.send(json!({"id":archive["id"],"result":{}})).await;
        cx.run_until_parked();
        assert!(connection.fork_preferences.borrow().is_empty());
        assert!(!connection.sessions.borrow()[&acp_v2::SessionId::new("session")].forking);
    }

    #[gpui::test]
    async fn resolved_question_cancels_its_form_without_sending_an_answer(cx: &mut TestAppContext) {
        let (connection, peer, thread) = session(cx).await;
        peer.send(json!({"id":77,"method":"item/tool/requestUserInput","params":{"threadId":"session","itemId":"ask","questions":[{"id":"answer","header":"Input","question":"What next?","isOther":true,"isSecret":false,"options":null}]}})).await;
        cx.run_until_parked();
        let entry = connection
            .approvals
            .borrow()
            .get("77")
            .unwrap()
            .elicitation
            .borrow()
            .clone()
            .unwrap();
        peer.send(json!({"method":"serverRequest/resolved","params":{"threadId":"session","requestId":77}})).await;
        cx.run_until_parked();
        assert!(connection.approvals.borrow().is_empty());
        thread.read_with(cx, |thread, _| {
            assert!(matches!(
                thread.elicitation(&entry).unwrap().1.status,
                acp_thread::ElicitationStatus::Canceled
            ));
        });
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
        self.open_task(None, project, work_dirs, None, cx)
    }
    fn supports_load_session(&self) -> bool {
        true
    }
    fn supports_fork_session(&self) -> bool {
        true
    }
    fn supports_message_fork(&self) -> bool {
        true
    }
    fn fork_session(
        self: Rc<Self>,
        id: acp_v2::SessionId,
        project: Entity<Project>,
        work_dirs: PathList,
        point: Option<acp_thread::SessionForkPoint>,
        cx: &mut App,
    ) -> Task<Result<acp_v2::SessionId>> {
        let cleanup = self.clone();
        let operation = cx.spawn(async move |cx| {
            ensure!(
                project.read_with(cx, |project, _| project.is_local()),
                "Native Codex remote forks are not implemented"
            );
            ensure!(
                work_dirs.paths().len() <= 1,
                "Native Codex multi-root forks are not implemented"
            );
            let guard = forks::SessionGuard::acquire(self.sessions.clone(), id.clone())?;
            ensure!(
                self.fork_preferences.borrow().len() < 64,
                "Too many unopened native forks; reconnect before branching again"
            );
            let preferences = self
                .sessions
                .borrow()
                .get(&id)
                .context("Source session closed")?
                .preferences
                .clone();
            let result = forks::fork(
                &self.rpc,
                &id,
                work_dirs.ordered_paths().next().map(PathBuf::as_path),
                point.as_ref(),
            )
            .await;
            if let Ok(id) = &result {
                self.fork_preferences
                    .borrow_mut()
                    .insert(id.clone(), preferences);
            }
            drop(guard);
            result
        });
        let (sender, receiver) = oneshot::channel();
        cx.spawn(async move |_| {
            if let Err(result) = sender.send(operation.await) {
                match result {
                    Ok(copy) => {
                        cleanup.fork_preferences.borrow_mut().remove(&copy);
                        cleanup
                            .rpc
                            .request("thread/archive", json!({"threadId":copy}))
                            .await
                            .log_err();
                    }
                    Err(error) => {
                        log::warn!("Cancelled Codex fork completed with an error: {error}")
                    }
                }
            }
        })
        .detach();
        cx.spawn(async move |_| {
            receiver
                .await
                .context("Codex fork task ended without a result")?
        })
    }
    fn load_session(
        self: Rc<Self>,
        id: acp_v2::SessionId,
        project: Entity<Project>,
        dirs: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        // UI uses history_page after opening; never hydrate the entire stored conversation here.
        self.open_task(Some(id), project, dirs, title, cx)
    }
    fn supports_history_pages(&self) -> bool {
        true
    }
    fn history_page(
        &self,
        id: acp_v2::SessionId,
        cursor: Option<String>,
        cx: &mut App,
    ) -> Task<Result<HistoryPage>> {
        let rpc = self.rpc.clone();
        cx.spawn(async move |cx| {
            let response = rpc.request("thread/turns/list", json!({"threadId":id,"cursor":cursor,"limit":20,"sortDirection":"desc","itemsView":"summary"})).await?;
            cx.background_spawn(async move {
                crate::codex_history::decode_page(response, cursor.as_deref())
            }).await
        })
    }
    fn auth_methods(&self) -> &[acp_v2::AuthMethod] {
        &[]
    }
    fn authenticate(&self, _: acp_v2::AuthMethodId, _: &mut App) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "Native login UI is not implemented; use the configured Codex CLI"
        )))
    }
    fn validate_prompt_content(&self, content: &[acp_v2::ContentBlock]) -> Result<()> {
        acp_thread::content::validate_prompt_content_for_v1(content)
    }

    fn prompt(
        &self,
        params: acp_v2::PromptRequest,
        cx: &mut App,
    ) -> Task<Result<acp::PromptResponse>> {
        let session_id = params.session_id.clone();
        let params = match acp_thread::content::prompt_to_v1(params) {
            Ok(params) => params,
            Err(error) => return Task::ready(Err(error)),
        };
        let input = match events::input(&params.prompt) {
            Ok(input) => input,
            Err(error) => return Task::ready(Err(error)),
        };
        let (completion, receiver) = oneshot::channel();
        let mut sessions = self.sessions.borrow_mut();
        let Some(session) = sessions.get_mut(&session_id) else {
            return Task::ready(Err(anyhow::anyhow!("Native Codex session not open")));
        };
        if session.active.is_some() || session.forking {
            return Task::ready(Err(anyhow::anyhow!("A Codex turn is already running")));
        }
        if session.uncertain {
            return Task::ready(Err(anyhow::anyhow!(
                "Previous Codex operation has an unknown outcome; reconnect before sending another turn"
            )));
        }
        let attached = session.attached;
        let work_dirs = session.work_dirs.clone();
        let inherited = session
            .preferences
            .has_model()
            .then(|| session.preferences.clone());
        let runtime = self.runtime();
        let thread = session.thread.clone();
        session.active = Some(ActiveTurn {
            id: None,
            completion,
            events: TurnEvents::default(),
            cancel_requested: false,
            optimistic_message: None,
            recovery: recovery::Progress::default(),
        });
        let rpc = self.rpc.clone();
        let sessions = self.sessions.clone();
        cx.spawn(async move |cx| {
            let preparation: Result<Value> = async {
                if !attached {
                    let mut preferences = runtime.restore(&session_id, &work_dirs, cx).await?;
                    if let Some(inherited) = inherited { preferences = inherited; }
                    let (notice, sender) = {
                        let mut sessions = sessions.borrow_mut();
                        let session = sessions.get_mut(&session_id).context("Codex session closed during connection")?;
                        session.preferences = preferences;
                        session.attached = true;
                        (session.history_notice.take(), session.settings_sender.clone())
                    };
                    sender.borrow_mut().send(()).log_err();
                    if let Some(notice) = notice {
                        thread.update(cx, |thread, cx| thread.dismiss_notice(notice, cx))?;
                    }
                }
                let sessions = sessions.borrow();
                let session = sessions.get(&session_id).context("Codex session closed before sending")?;
                ensure!(!session.active.as_ref().is_none_or(|active| active.cancel_requested), "Codex send cancelled before starting a turn; nothing was sent");
                Ok(session.preferences.turn_params(&session_id, input))
            }.await;
            let turn_params = match preparation {
                Ok(params) => params,
                Err(error) => {
                    let (notice, cancelled) = {
                        let mut sessions = sessions.borrow_mut();
                        sessions.get_mut(&session_id).map(|session| {
                            let cancelled = session.active.take().is_some_and(|active| active.cancel_requested);
                            (session.history_notice.take(), cancelled)
                        }).unwrap_or_default()
                    };
                    thread.update(cx, |thread, cx| {
                        if let Some(notice) = notice { thread.dismiss_notice(notice, cx); }
                        let (title, message) = if cancelled {
                            ("Отправка отменена", "Сообщение агенту не отправлено. Сохранённая история доступна.")
                        } else {
                            ("Агент недоступен", "Не удалось подключить агента. Сохранённая история доступна; сообщение агенту не отправлено. Можно повторить отправку после восстановления связи.")
                        };
                        thread.push_notice(acp_v2::Notice::new(acp_v2::NoticeSeverity::Warning, title).description(message), cx);
                        let id = thread.notices().last().map(|(id, _)| *id);
                        if let Some(session) = sessions.borrow_mut().get_mut(&session_id) { session.history_notice = id; }
                    }).log_err();
                    return Err(error);
                }
            };
            // AcpThread calls prompt while updating itself. Defer access until that
            // update has returned, but bind the draft before sending any RPC.
            let optimistic_message = thread.upgrade().and_then(|thread| {
                thread.update(cx, |thread, _| thread.identify_pending_user_message())
            });
            if let Some(active) = sessions
                .borrow_mut()
                .get_mut(&session_id)
                .and_then(|session| session.active.as_mut())
            {
                active.optimistic_message = optimistic_message;
            }
            let response = match rpc.request("turn/start", turn_params).await {
                Ok(response) => response,
                Err(error) => {
                    if let Some(session) = sessions.borrow_mut().get_mut(&session_id) {
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
                    .get_mut(&session_id)
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
                    json!({"threadId":session_id,"turnId":turn_id}),
                )
                .await?;
            }
            match futures::future::select(
                Box::pin(receiver),
                Box::pin(recovery::watch(
                    rpc,
                    sessions,
                    session_id,
                    turn_id,
                    cx,
                )),
            )
            .await
            {
                futures::future::Either::Left((result, _)) => {
                    result.context("Codex turn completion was lost")?
                }
                futures::future::Either::Right((result, _)) => result,
            }
        })
    }
    fn cancel(&self, id: &acp_v2::SessionId, cx: &mut App) -> Result<()> {
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
        // This acknowledges the local cancellation request, not completion of
        // the asynchronous interrupt. Turn completion remains event-driven.
        Ok(())
    }
    fn supports_close_session(&self) -> bool {
        true
    }
    fn close_session(self: Rc<Self>, id: &acp_v2::SessionId, cx: &mut App) -> Task<Result<()>> {
        let id = id.clone();
        cx.spawn(async move |_| {
            ensure!(
                !self
                    .sessions
                    .borrow()
                    .get(&id)
                    .is_some_and(|session| session.active.is_some() || session.forking),
                "Stop the active turn before closing its session"
            );
            let attached = self
                .sessions
                .borrow()
                .get(&id)
                .is_some_and(|session| session.attached);
            if attached {
                self.rpc
                    .request("thread/unsubscribe", json!({"threadId":id}))
                    .await?;
            }
            self.sessions.borrow_mut().remove(&id);
            Ok(())
        })
    }
    fn set_title(&self, id: &acp_v2::SessionId, _: &App) -> Option<Rc<dyn AgentSessionSetTitle>> {
        Some(Rc::new(Title {
            rpc: self.rpc.clone(),
            id: id.clone(),
        }))
    }
    fn session_config_options(
        &self,
        id: &acp_v2::SessionId,
        cx: &App,
    ) -> Option<Rc<dyn acp_thread::AgentSessionConfigOptions>> {
        let sessions = self.sessions.borrow();
        let session = sessions.get(id)?;
        if session.attached {
            self.quota.refresh(cx, false);
        }
        Some(Rc::new(ConfigOptions {
            runtime: self.runtime(),
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
    runtime: SessionRuntime,
    sessions: std::rc::Weak<RefCell<HashMap<acp_v2::SessionId, Session>>>,
    id: acp_v2::SessionId,
    quota: Rc<settings::Quota>,
    sender: Rc<RefCell<watch::Sender<()>>>,
    receiver: watch::Receiver<()>,
    quota_watch: RefCell<Option<Task<()>>>,
}
impl acp_thread::AgentSessionConfigOptions for ConfigOptions {
    fn config_options(&self) -> Vec<acp_v2::SessionConfigOption> {
        let mut options = self
            .sessions
            .upgrade()
            .and_then(|sessions| {
                sessions
                    .borrow()
                    .get(&self.id)
                    .filter(|session| session.preferences.has_model())
                    .map(|session| session.preferences.options())
            })
            .unwrap_or_default();
        let attached = self.sessions.upgrade().is_some_and(|sessions| {
            sessions
                .borrow()
                .get(&self.id)
                .is_some_and(|session| session.attached)
        });
        if !attached {
            options.insert(0, acp_v2::SessionConfigOption::select("local-history", "Подключение", "local",
                vec![acp_v2::SessionConfigSelectOption::new("local", "Локальная история"), acp_v2::SessionConfigSelectOption::new("connect", "Подключить агента")])
                .description("Можно читать без связи. Подключите агента, чтобы выбрать модель и настройки до отправки сообщения."));
        }
        options.push(self.quota.option());
        options
    }
    fn set_config_option(
        &self,
        id: acp_v2::SessionConfigId,
        value: acp_v2::SessionConfigOptionValue,
        cx: &mut App,
    ) -> Task<Result<Vec<acp_v2::SessionConfigOption>>> {
        if id.0.as_ref() == "local-history" {
            if value
                .as_id()
                .is_some_and(|value| value.0.as_ref() == "local")
            {
                return Task::ready(Ok(self.config_options()));
            }
            if !value
                .as_id()
                .is_some_and(|value| value.0.as_ref() == "connect")
            {
                return Task::ready(Err(anyhow::anyhow!("Invalid connection action")));
            }
            let sessions = self.sessions.upgrade();
            let runtime = self.runtime.clone();
            let id = self.id.clone();
            let quota = self.quota.clone();
            return cx.spawn(async move |cx| {
                let sessions = sessions.context("Codex connection closed")?;
                let guard = forks::SessionGuard::acquire(sessions.clone(), id.clone())?;
                let (work_dirs, inherited) = {
                    let sessions = sessions.borrow();
                    let session = sessions.get(&id).context("Codex session closed")?;
                    ensure!(!session.attached, "Codex session is already connected");
                    (
                        session.work_dirs.clone(),
                        session
                            .preferences
                            .has_model()
                            .then(|| session.preferences.clone()),
                    )
                };
                let preferences = runtime.restore(&id, &work_dirs, cx).await?;
                let (thread, notice, sender, mut options) = {
                    let mut sessions = sessions.borrow_mut();
                    let session = sessions.get_mut(&id).context("Codex session closed")?;
                    session.preferences = inherited.unwrap_or(preferences);
                    session.attached = true;
                    (
                        session.thread.clone(),
                        session.history_notice.take(),
                        session.settings_sender.clone(),
                        session.preferences.options(),
                    )
                };
                if let Some(notice) = notice {
                    thread.update(cx, |thread, cx| thread.dismiss_notice(notice, cx))?;
                }
                drop(guard);
                sender.borrow_mut().send(()).log_err();
                cx.update(|cx| quota.refresh(cx, false));
                options.push(quota.option());
                Ok(options)
            });
        }
        let result = (|| {
            if id.0.as_ref() == "weekly-usage" {
                ensure!(
                    value
                        .as_id()
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
                    session.active.is_none()
                        && !session.uncertain
                        && !session.forking
                        && session.preferences.has_model(),
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
    id: acp_v2::SessionId,
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
    fn delete_session(&self, id: &acp_v2::SessionId, cx: &mut App) -> Task<Result<()>> {
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
