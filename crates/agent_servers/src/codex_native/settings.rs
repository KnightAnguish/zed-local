use super::transport::Rpc;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, bail, ensure};
use gpui::{App, Task};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::HashSet,
    rc::Rc,
    time::{Duration, Instant},
};
use util::ResultExt as _;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Model {
    pub model: String,
    display_name: String,
    #[serde(default)]
    supported_reasoning_efforts: Vec<Effort>,
    #[serde(default)]
    default_reasoning_effort: Option<String>,
    #[serde(default)]
    service_tiers: Vec<Tier>,
    #[serde(default)]
    additional_speed_tiers: Vec<String>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Effort {
    reasoning_effort: String,
    description: String,
}
#[derive(Clone, Deserialize)]
struct Tier {
    id: String,
}

impl Model {
    fn supports_effort(&self, effort: &str) -> bool {
        self.supported_reasoning_efforts
            .iter()
            .any(|entry| entry.reasoning_effort == effort)
    }
    fn supports_fast(&self) -> bool {
        self.service_tiers.iter().any(|tier| tier.id == "fast")
            || self
                .additional_speed_tiers
                .iter()
                .any(|tier| tier == "fast")
    }
}

pub(super) async fn catalog(rpc: &Rpc) -> Result<Rc<Vec<Model>>> {
    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    let mut cursors = HashSet::new();
    let mut names = HashSet::new();
    for _ in 0..64 {
        let response = rpc
            .request(
                "model/list",
                json!({"limit":100,"includeHidden":false,"cursor":cursor}),
            )
            .await?;
        let page: Vec<Model> = serde_json::from_value(
            response
                .get("data")
                .cloned()
                .context("Missing Codex model catalog")?,
        )?;
        for model in page {
            ensure!(
                !model.model.is_empty() && names.insert(model.model.clone()),
                "Invalid or duplicate Codex model"
            );
            models.push(model);
            ensure!(models.len() <= 4096, "Codex model catalog exceeds limit");
        }
        cursor = response["nextCursor"].as_str().map(str::to_owned);
        let Some(next) = &cursor else {
            ensure!(!models.is_empty(), "Codex model catalog is empty");
            return Ok(Rc::new(models));
        };
        ensure!(
            cursors.insert(next.clone()),
            "Repeated Codex model catalog cursor"
        );
    }
    bail!("Too many Codex model catalog pages")
}

#[derive(Clone)]
pub(super) struct Preferences {
    models: Rc<Vec<Model>>,
    model: String,
    effort: Option<String>,
    tier: Option<String>,
    // None preserves arbitrary server permission profiles, rather than broadening them.
    mode: Option<String>,
}

impl Preferences {
    pub(super) fn from_response(models: Rc<Vec<Model>>, response: &Value) -> Result<Self> {
        Ok(Self {
            models,
            model: response["model"]
                .as_str()
                .context("Missing session model")?
                .to_owned(),
            effort: response["reasoningEffort"].as_str().map(str::to_owned),
            tier: response["serviceTier"].as_str().map(str::to_owned),
            mode: None,
        })
    }

    fn current(&self) -> Option<&Model> {
        self.models.iter().find(|model| model.model == self.model)
    }

    pub(super) fn options(&self) -> Vec<acp::SessionConfigOption> {
        let mut model_options: Vec<_> = self
            .models
            .iter()
            .map(|model| {
                acp::SessionConfigSelectOption::new(model.model.clone(), model.display_name.clone())
            })
            .collect();
        if self.current().is_none() {
            model_options.push(acp::SessionConfigSelectOption::new(
                self.model.clone(),
                format!("{} (saved session)", self.model),
            ));
        }
        let mut result = vec![acp::SessionConfigOption::select("model", "Model", self.model.clone(), model_options).category(acp::SessionConfigOptionCategory::Model).description("For the next turn in this chat; does not change other chats or global defaults")];
        if let Some(model) = self.current() {
            let mut efforts: Vec<_> = model
                .supported_reasoning_efforts
                .iter()
                .map(|effort| {
                    acp::SessionConfigSelectOption::new(
                        effort.reasoning_effort.clone(),
                        effort.reasoning_effort.clone(),
                    )
                    .description(effort.description.clone())
                })
                .collect();
            let current_effort = self.effort.as_deref().unwrap_or("inherit");
            if !model.supports_effort(current_effort) {
                efforts.push(acp::SessionConfigSelectOption::new(
                    current_effort.to_owned(),
                    if self.effort.is_none() {
                        "Server default".to_owned()
                    } else {
                        format!("{current_effort} (saved session)")
                    },
                ));
            }
            result.push(
                acp::SessionConfigOption::select(
                    "reasoning-effort",
                    "Reasoning effort",
                    current_effort.to_owned(),
                    efforts,
                )
                .category(acp::SessionConfigOptionCategory::ThoughtLevel),
            );
            if model.supports_fast() {
                result.push(acp::SessionConfigOption::boolean(
                    "fast-mode",
                    "Fast mode",
                    self.tier.as_deref() == Some("fast"),
                ));
            }
        }
        result.push(
            acp::SessionConfigOption::select(
                "mode",
                "Permissions",
                self.mode.clone().unwrap_or_else(|| "server".into()),
                vec![
                    acp::SessionConfigSelectOption::new("server", "Keep server permissions"),
                    acp::SessionConfigSelectOption::new("native-read-only", "Read only"),
                    acp::SessionConfigSelectOption::new("read-only", "Ask for approval"),
                    acp::SessionConfigSelectOption::new("agent", "Approve for me"),
                    acp::SessionConfigSelectOption::new("agent-full-access", "Full access"),
                ],
            )
            .description(
                "Applies to the next turn; files and network access are controlled by Codex",
            )
            .category(acp::SessionConfigOptionCategory::Mode),
        );
        result
    }

    pub(super) fn change(&mut self, id: &str, value: &acp::SessionConfigOptionValue) -> Result<()> {
        let selected = || {
            value
                .as_value_id()
                .map(|value| value.0.as_ref())
                .context("Expected a selection value")
        };
        match id {
            "model" => {
                let selected = selected()?;
                if selected == self.model {
                    return Ok(());
                }
                let model = self
                    .models
                    .iter()
                    .find(|model| model.model == selected)
                    .context("Model is not in the current Codex catalog")?;
                let effort = self
                    .effort
                    .clone()
                    .filter(|effort| model.supports_effort(effort))
                    .or_else(|| {
                        model
                            .default_reasoning_effort
                            .clone()
                            .filter(|effort| model.supports_effort(effort))
                    });
                // Never silently keep Fast when the target model does not support it.
                let tier = if self.tier.as_deref() == Some("fast") && !model.supports_fast() {
                    None
                } else {
                    self.tier.clone()
                };
                self.model = selected.to_owned();
                self.effort = effort;
                self.tier = tier;
            }
            "reasoning-effort" => {
                let selected = selected()?;
                if selected == self.effort.as_deref().unwrap_or("inherit") {
                    return Ok(());
                }
                ensure!(
                    self.current()
                        .is_some_and(|model| model.supports_effort(selected)),
                    "Unsupported reasoning effort for this model"
                );
                self.effort = Some(selected.to_owned());
            }
            "fast-mode" => {
                let enabled = value
                    .as_bool()
                    .context("Expected a boolean Fast mode value")?;
                ensure!(
                    !enabled || self.current().is_some_and(Model::supports_fast),
                    "Fast mode is not supported by this model"
                );
                self.tier = enabled.then(|| "fast".into());
            }
            "mode" => {
                let selected = selected()?;
                ensure!(
                    matches!(
                        selected,
                        "server" | "native-read-only" | "read-only" | "agent" | "agent-full-access"
                    ),
                    "Unknown permission mode"
                );
                self.mode = (selected != "server").then(|| selected.to_owned());
            }
            _ => bail!("Unknown native Codex setting {id}"),
        }
        Ok(())
    }

    pub(super) fn turn_params(&self, thread_id: &acp::SessionId, input: Vec<Value>) -> Value {
        let mut params = json!({"threadId":thread_id,"input":input,"model":self.model});
        if let Some(effort) = &self.effort {
            params["effort"] = json!(effort);
        }
        // Explicit null clears a previously persisted service tier (omission would inherit it).
        params["serviceTier"] = json!(self.tier);
        match self.mode.as_deref() {
            Some("agent-full-access") => {
                params["sandboxPolicy"] = json!({"type":"dangerFullAccess"});
                params["approvalPolicy"] = json!("never");
                params["approvalsReviewer"] = json!("user");
            }
            Some(mode) => {
                params["sandboxPolicy"] = if mode == "native-read-only" {
                    json!({"type":"readOnly"})
                } else {
                    json!({"type":"workspaceWrite","writableRoots":[],"networkAccess":false,"excludeTmpdirEnvVar":false,"excludeSlashTmp":false})
                };
                params["approvalPolicy"] = json!("on-request");
                params["approvalsReviewer"] = json!(if mode == "agent" {
                    "auto_review"
                } else {
                    "user"
                });
            }
            None => {}
        }
        params
    }
}

#[derive(Default)]
struct QuotaState {
    response: Option<Value>,
    error: Option<String>,
    requested: Option<Instant>,
    generation: u64,
    in_flight: bool,
}
pub(super) struct Quota {
    rpc: Rpc,
    state: RefCell<QuotaState>,
    sender: RefCell<watch::Sender<()>>,
    pub receiver: watch::Receiver<()>,
    task: RefCell<Option<Task<()>>>,
}
impl Quota {
    pub(super) fn new(rpc: Rpc) -> Rc<Self> {
        let (sender, receiver) = watch::channel(());
        Rc::new(Self {
            rpc,
            state: RefCell::new(QuotaState::default()),
            sender: RefCell::new(sender),
            receiver,
            task: RefCell::new(None),
        })
    }
    pub(super) fn option(&self) -> acp::SessionConfigOption {
        let state = self.state.borrow();
        crate::codex_usage::usage_option(
            state.response.as_ref(),
            state.error.as_deref(),
            chrono::Utc::now().timestamp(),
        )
    }
    pub(super) fn invalidate(&self, reason: &str) {
        self.task.borrow_mut().take();
        let mut state = self.state.borrow_mut();
        state.response = None;
        state.error = Some(reason.to_owned());
        state.requested = None;
        state.in_flight = false;
        state.generation = state.generation.wrapping_add(1);
        drop(state);
        self.sender.borrow_mut().send(()).log_err();
    }
    pub(super) fn refresh(self: &Rc<Self>, cx: &App, force: bool) {
        let now = cx.background_executor().now();
        let generation = {
            let mut state = self.state.borrow_mut();
            if state.in_flight
                || state.requested.is_some_and(|previous| {
                    now.saturating_duration_since(previous)
                        < Duration::from_secs(if force { 2 } else { 60 })
                })
            {
                return;
            }
            state.requested = Some(now);
            state.in_flight = true;
            state.generation
        };
        let rpc = self.rpc.clone();
        let weak = Rc::downgrade(self);
        *self.task.borrow_mut() = Some(cx.foreground_executor().spawn(async move {
            let response = rpc.request("account/rateLimits/read", json!({})).await;
            if let Some(quota) = weak.upgrade() {
                let mut state = quota.state.borrow_mut();
                if state.generation != generation {
                    return;
                }
                state.in_flight = false;
                match response {
                    Ok(response) => {
                        state.response = Some(response);
                        state.error = None;
                    }
                    Err(error) => {
                        state.response = None;
                        state.error = Some(error.to_string());
                    }
                }
                drop(state);
                quota.sender.borrow_mut().send(()).log_err();
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn models() -> Rc<Vec<Model>> {
        Rc::new(serde_json::from_value(json!([
            {"model":"a","displayName":"A","supportedReasoningEfforts":[{"reasoningEffort":"medium","description":"balanced"},{"reasoningEffort":"high","description":"deep"}],"defaultReasoningEffort":"medium","serviceTiers":[{"id":"fast"}]},
            {"model":"b","displayName":"B","supportedReasoningEfforts":[{"reasoningEffort":"low","description":"quick"}],"defaultReasoningEffort":"low"}
        ])).unwrap())
    }
    fn preferences() -> Preferences {
        Preferences::from_response(
            models(),
            &json!({"model":"a","reasoningEffort":"high","serviceTier":"fast"}),
        )
        .unwrap()
    }
    #[test]
    fn model_change_reconciles_effort_and_fast_without_touching_other_session() {
        let original = preferences();
        let mut changed = original.clone();
        changed.change("model", &"b".into()).unwrap();
        assert_eq!(changed.effort.as_deref(), Some("low"));
        assert!(changed.tier.is_none());
        assert_eq!(original.model, "a");
        assert_eq!(original.tier.as_deref(), Some("fast"));
        assert!(changed.change("reasoning-effort", &"high".into()).is_err());
        assert!(changed.change("fast-mode", &true.into()).is_err());
    }
    #[test]
    fn permissions_are_only_overridden_when_explicitly_selected() {
        let mut preferences = preferences();
        let id = acp::SessionId::new("t");
        assert!(
            preferences
                .turn_params(&id, vec![])
                .get("sandboxPolicy")
                .is_none()
        );
        preferences
            .change("mode", &"agent-full-access".into())
            .unwrap();
        assert_eq!(
            preferences.turn_params(&id, vec![])["approvalPolicy"],
            "never"
        );
        preferences
            .change("mode", &"native-read-only".into())
            .unwrap();
        let params = preferences.turn_params(&id, vec![]);
        assert_eq!(params["sandboxPolicy"]["type"], "readOnly");
        assert_eq!(params["approvalPolicy"], "on-request");
    }
    #[test]
    fn ui_options_are_valid_and_fast_off_is_explicit_null() {
        let mut preferences = preferences();
        assert_eq!(preferences.options().len(), 4);
        preferences.change("fast-mode", &false.into()).unwrap();
        let params = preferences.turn_params(&acp::SessionId::new("t"), vec![]);
        assert!(params.get("serviceTier").unwrap().is_null());
    }
    #[gpui::test]
    async fn catalog_pages_and_repeated_cursor(cx: &mut gpui::TestAppContext) {
        let (rpc, peer, _events) = Rpc::fake(cx.background_executor.clone());
        let request = cx.update(|cx| cx.spawn(async move |_| catalog(&rpc).await));
        for _ in 0..2 {
            let frame = peer.next().await;
            peer.send(json!({"id":frame["id"],"result":{"data":[],"nextCursor":"same"}}))
                .await;
        }
        assert!(
            request
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("Repeated")
        );
    }
    #[gpui::test]
    async fn catalog_collects_all_pages(cx: &mut gpui::TestAppContext) {
        let (rpc, peer, _events) = Rpc::fake(cx.background_executor.clone());
        let request = cx.update(|cx| cx.spawn(async move |_| catalog(&rpc).await));
        let first = peer.next().await;
        peer.send(json!({"id":first["id"],"result":{"data":[{"model":"first","displayName":"First"}],"nextCursor":"page2"}})).await;
        let second = peer.next().await;
        assert_eq!(second["params"]["cursor"], "page2");
        peer.send(json!({"id":second["id"],"result":{"data":[{"model":"second","displayName":"Second"}],"nextCursor":null}})).await;
        assert_eq!(request.await.unwrap().len(), 2);
    }

    #[gpui::test]
    async fn quota_refresh_is_coalesced_and_reports_remaining(cx: &mut gpui::TestAppContext) {
        let (rpc, peer, _events) = Rpc::fake(cx.background_executor.clone());
        let quota = Quota::new(rpc);
        cx.update(|cx| {
            quota.refresh(cx, false);
            quota.refresh(cx, true);
        });
        let request = peer.next().await;
        assert_eq!(quota.state.borrow().generation, 0);
        let resets = chrono::Utc::now().timestamp() + 3600;
        peer.send(json!({"id":request["id"],"result":{"rateLimits":{"secondary":{"usedPercent":23,"windowDurationMins":10080,"resetsAt":resets}}}})).await;
        cx.run_until_parked();
        assert_eq!(
            serde_json::to_value(quota.option()).unwrap()["options"][0]["name"],
            "77%"
        );
        let previous = quota.state.borrow().requested;
        cx.update(|cx| quota.refresh(cx, true));
        assert_eq!(quota.state.borrow().requested, previous);
        assert!(!quota.state.borrow().in_flight);
        cx.background_executor.advance_clock(Duration::from_secs(3));
        cx.update(|cx| quota.refresh(cx, true));
        let refresh = peer.next().await;
        assert_ne!(request["id"], refresh["id"]);
        peer.send(json!({"id":refresh["id"],"result":{}})).await;
        cx.run_until_parked();
        assert_eq!(
            serde_json::to_value(quota.option()).unwrap()["options"][0]["name"],
            "—"
        );
    }

    #[gpui::test]
    async fn quota_invalidation_rejects_late_old_account_response(cx: &mut gpui::TestAppContext) {
        let (rpc, peer, _events) = Rpc::fake(cx.background_executor.clone());
        let quota = Quota::new(rpc);
        cx.update(|cx| quota.refresh(cx, false));
        let old = peer.next().await;
        quota.invalidate("Account changed");
        cx.update(|cx| quota.refresh(cx, false));
        let new = peer.next().await;
        peer.send(json!({"id":old["id"],"result":{"rateLimits":{"secondary":{"usedPercent":1,"windowDurationMins":10080}}}})).await;
        peer.send(json!({"id":new["id"],"error":{"code":-1,"message":"offline"}}))
            .await;
        cx.run_until_parked();
        let option = serde_json::to_value(quota.option()).unwrap();
        assert_eq!(option["options"][0]["name"], "—");
        assert!(option["description"].as_str().unwrap().contains("offline"));
    }
}
