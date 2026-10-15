use crate::codex_history::CodexHistory;
use agent_client_protocol::schema::v2 as acp;
use gpui::{App, AppContext as _, Task};
use serde_json::Value;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
use util::ResultExt as _;

const USAGE_ID: &str = "weekly-usage";

#[derive(Default)]
struct UsageState {
    response: Option<Value>,
    error: Option<String>,
    requested: Option<Instant>,
}

#[derive(Clone)]
pub(crate) struct NativeUsage {
    reader: CodexHistory,
    state: Rc<RefCell<UsageState>>,
    sender: Rc<RefCell<watch::Sender<()>>>,
    pub(crate) receiver: watch::Receiver<()>,
    task: Rc<RefCell<Option<Task<()>>>>,
}

impl NativeUsage {
    pub(crate) fn new(reader: CodexHistory, cx: &App) -> Self {
        let (sender, receiver) = watch::channel(());
        let usage = Self {
            reader,
            state: Rc::new(RefCell::new(UsageState::default())),
            sender: Rc::new(RefCell::new(sender)),
            receiver,
            task: Rc::new(RefCell::new(None)),
        };
        usage.refresh(cx, false);
        usage
    }

    pub(crate) fn refresh(&self, cx: &App, force: bool) {
        if self
            .state
            .borrow()
            .requested
            .is_some_and(|time| time.elapsed() < Duration::from_secs(if force { 2 } else { 60 }))
        {
            return;
        }
        self.state.borrow_mut().requested = Some(Instant::now());
        let reader = self.reader.clone();
        let executor = cx.background_executor().clone();
        let request = cx.background_spawn(async move {
            match futures::future::select(
                Box::pin(reader.rate_limits()),
                Box::pin(executor.timer(Duration::from_secs(30))),
            )
            .await
            {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right(_) => {
                    Err(anyhow::anyhow!("Account usage request timed out"))
                }
            }
        });
        let state = self.state.clone();
        let sender = self.sender.clone();
        *self.task.borrow_mut() = Some(cx.foreground_executor().spawn(async move {
            let result = request.await;
            let mut state = state.borrow_mut();
            match result {
                Ok(response) => {
                    state.response = Some(response);
                    state.error = None;
                }
                Err(error) => {
                    state.response = None;
                    state.error = Some(error.to_string());
                }
            }
            sender.borrow_mut().send(()).log_err();
        }));
    }

    pub(crate) fn owns(config_id: &acp::SessionConfigId) -> bool {
        config_id.0.as_ref() == USAGE_ID
    }

    pub(crate) fn options(
        &self,
        mut options: Vec<acp::SessionConfigOption>,
    ) -> Vec<acp::SessionConfigOption> {
        options.retain(|option| !Self::owns(&option.config_id));
        let state = self.state.borrow();
        options.push(usage_option(
            state.response.as_ref(),
            state.error.as_deref(),
            chrono::Utc::now().timestamp(),
        ));
        options
    }
}

pub(crate) fn usage_option(
    response: Option<&Value>,
    error: Option<&str>,
    now: i64,
) -> acp::SessionConfigOption {
    let snapshot = response.and_then(|response| {
        let map = response
            .get("rateLimitsByLimitId")
            .and_then(Value::as_object);
        if let Some(map) = map.filter(|map| !map.is_empty()) {
            map.iter().find_map(|(key, snapshot)| {
                (snapshot
                    .get("limitId")
                    .and_then(Value::as_str)
                    .unwrap_or(key)
                    == "codex")
                    .then_some(snapshot)
            })
        } else {
            response.get("rateLimits").filter(|snapshot| {
                snapshot
                    .get("limitId")
                    .and_then(Value::as_str)
                    .unwrap_or("codex")
                    == "codex"
            })
        }
    });
    let weekly = snapshot.and_then(|snapshot| {
        ["primary", "secondary"]
            .into_iter()
            .filter_map(|key| snapshot.get(key))
            .find(|window| window.get("windowDurationMins").and_then(Value::as_u64) == Some(10080))
    });
    let reset = weekly
        .and_then(|window| window.get("resetsAt"))
        .and_then(Value::as_i64);
    let used = weekly
        .and_then(|window| window.get("usedPercent"))
        .and_then(Value::as_f64)
        .filter(|used| used.is_finite() && reset.is_none_or(|reset| reset > now));
    let label = used
        .map(|used| format!("{}%", (100.0 - used).clamp(0.0, 100.0).round() as u8))
        .unwrap_or_else(|| "—".into());
    let reset_label = reset
        .and_then(|timestamp| chrono::DateTime::from_timestamp(timestamp, 0))
        .map(|timestamp| {
            (timestamp + chrono::Duration::hours(3))
                .format("%d.%m.%Y %H:%M МСК")
                .to_string()
        })
        .unwrap_or_else(|| "время не предоставлено".into());
    let description = if used.is_some() {
        format!("Осталось {label}. Сброс: {reset_label}.")
    } else {
        format!(
            "Данные лимита недоступны. {} Нажмите «Обновить».",
            error.unwrap_or("")
        )
    };
    acp::SessionConfigOption::select(
        USAGE_ID,
        "Недельный лимит Codex",
        "status",
        vec![
            acp::SessionConfigSelectOption::new("status", label),
            acp::SessionConfigSelectOption::new("refresh", "Обновить"),
        ],
    )
    .description(description)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn native_usage_uses_account_weekly_remaining_and_reset() {
        let value = json!({"rateLimitsByLimitId":{"codex":{"secondary":{"usedPercent":23.4,"windowDurationMins":10080,"resetsAt":2000000000}},"model":{"secondary":{"usedPercent":99,"windowDurationMins":10080}}}});
        let option = serde_json::to_value(usage_option(Some(&value), None, 100)).expect("option");
        assert_eq!(option["options"][0]["name"], "77%");
        assert!(
            option["description"]
                .as_str()
                .expect("description")
                .contains("МСК")
        );
        assert_eq!(
            serde_json::to_value(usage_option(Some(&value), None, 2000000001)).expect("option")["options"]
                [0]["name"],
            "—"
        );
    }
    #[test]
    fn native_usage_does_not_substitute_model_or_short_window() {
        for value in [
            json!({"rateLimitsByLimitId":{"model":{"secondary":{"usedPercent":5,"windowDurationMins":10080}}}}),
            json!({"rateLimits":{"secondary":{"usedPercent":5,"windowDurationMins":300}}}),
            json!({}),
        ] {
            let option = serde_json::to_value(usage_option(Some(&value), None, 0)).expect("option");
            assert_eq!(option["options"][0]["name"], "—");
        }
    }
}
