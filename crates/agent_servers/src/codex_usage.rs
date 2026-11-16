use crate::codex_history::CodexHistory;
use agent_client_protocol::v2::schema as acp;
use gpui::{App, AppContext as _, Task};
use serde_json::Value;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
use util::ResultExt as _;

const USAGE_ID_PREFIX: &str = "usage-";
const USAGE_FALLBACK_ID: &str = "usage";

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
        owns_usage(config_id)
    }

    pub(crate) fn options(
        &self,
        mut options: Vec<acp::SessionConfigOption>,
    ) -> Vec<acp::SessionConfigOption> {
        options.retain(|option| !Self::owns(&option.config_id));
        let state = self.state.borrow();
        options.extend(usage_options(
            state.response.as_ref(),
            state.error.as_deref(),
            chrono::Utc::now().timestamp(),
        ));
        options
    }
}

/// One rate-limit window reported by the account.
struct UsageWindow {
    minutes: u64,
    remaining_percent: Option<u8>,
    resets_at: Option<i64>,
}

/// Recognizes every option produced by [`usage_options`].
pub(crate) fn owns_usage(config_id: &acp::SessionConfigId) -> bool {
    let id = config_id.0.as_ref();
    id == USAGE_FALLBACK_ID || id.starts_with(USAGE_ID_PREFIX)
}

fn usage_snapshots(response: Option<&Value>) -> Vec<&Value> {
    let Some(response) = response else {
        return Vec::new();
    };

    if let Some(limits) = response
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
    {
        let matching: Vec<&Value> = limits
            .iter()
            .filter(|(key, snapshot)| {
                snapshot
                    .get("limitId")
                    .and_then(Value::as_str)
                    .unwrap_or(key.as_str())
                    == "codex"
            })
            .map(|(_, snapshot)| snapshot)
            .collect();
        if !matching.is_empty() {
            return matching;
        }
    }

    response
        .get("rateLimits")
        .filter(|snapshot| {
            snapshot
                .get("limitId")
                .and_then(Value::as_str)
                .unwrap_or("codex")
                == "codex"
        })
        .into_iter()
        .collect()
}

fn usage_windows(response: Option<&Value>, now: i64) -> Vec<UsageWindow> {
    let mut windows: Vec<UsageWindow> = Vec::new();
    for snapshot in usage_snapshots(response) {
        for key in ["primary", "secondary"] {
            let Some(window) = snapshot.get(key) else {
                continue;
            };
            let Some(minutes) = window.get("windowDurationMins").and_then(Value::as_u64) else {
                continue;
            };
            if minutes == 0
                || windows
                    .iter()
                    .any(|existing| existing.minutes == minutes)
            {
                continue;
            }
            let resets_at = window.get("resetsAt").and_then(Value::as_i64);
            let used = window
                .get("usedPercent")
                .and_then(Value::as_f64)
                .filter(|used| used.is_finite() && resets_at.is_none_or(|reset| reset > now));
            windows.push(UsageWindow {
                minutes,
                remaining_percent: used
                    .map(|used| (100.0 - used).clamp(0.0, 100.0).round() as u8),
                resets_at,
            });
        }
    }
    windows.sort_by_key(|window| window.minutes);
    windows
}

fn window_label(minutes: u64) -> String {
    match minutes {
        300 => "5 ч".to_string(),
        1_440 => "сутки".to_string(),
        10_080 => "неделя".to_string(),
        43_200 => "месяц".to_string(),
        _ if minutes % 10_080 == 0 => format!("{} нед.", minutes / 10_080),
        _ if minutes % 1_440 == 0 => format!("{} дн.", minutes / 1_440),
        _ if minutes % 60 == 0 => format!("{} ч", minutes / 60),
        _ => format!("{minutes} мин"),
    }
}

fn reset_label(resets_at: Option<i64>) -> String {
    resets_at
        .and_then(|timestamp| chrono::DateTime::from_timestamp(timestamp, 0))
        .map(|timestamp| {
            (timestamp + chrono::Duration::hours(3))
                .format("%d.%m.%Y %H:%M МСК")
                .to_string()
        })
        .unwrap_or_else(|| "время не предоставлено".into())
}

fn window_option(window: UsageWindow, error: Option<&str>) -> acp::SessionConfigOption {
    let label = window
        .remaining_percent
        .map(|remaining| format!("{remaining}%"))
        .unwrap_or_else(|| "—".into());
    let description = if window.remaining_percent.is_some() {
        format!(
            "Осталось {label}. Сброс: {}.",
            reset_label(window.resets_at)
        )
    } else {
        format!(
            "Данные лимита недоступны. {} Нажмите «Обновить».",
            error.unwrap_or("")
        )
    };
    acp::SessionConfigOption::select(
        format!("{USAGE_ID_PREFIX}{}", window.minutes),
        format!("Лимит Codex · {}", window_label(window.minutes)),
        "status",
        vec![
            acp::SessionConfigSelectOption::new("status", label),
            acp::SessionConfigSelectOption::new("refresh", "Обновить"),
        ],
    )
    .description(description)
}

fn fallback_option(error: Option<&str>) -> acp::SessionConfigOption {
    acp::SessionConfigOption::select(
        USAGE_FALLBACK_ID,
        "Лимит Codex",
        "status",
        vec![
            acp::SessionConfigSelectOption::new("status", "—"),
            acp::SessionConfigSelectOption::new("refresh", "Обновить"),
        ],
    )
    .description(format!(
        "Данные лимитов недоступны. {} Нажмите «Обновить».",
        error.unwrap_or("")
    ))
}

/// Builds one panel option per reported rate-limit window, so a plan with a
/// 5-hour, a weekly and a monthly limit shows all three. The number of options
/// follows the response.
pub(crate) fn usage_options(
    response: Option<&Value>,
    error: Option<&str>,
    now: i64,
) -> Vec<acp::SessionConfigOption> {
    let windows = usage_windows(response, now);
    if windows.is_empty() {
        return vec![fallback_option(error)];
    }
    windows
        .into_iter()
        .map(|window| window_option(window, error))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ids(options: &[acp::SessionConfigOption]) -> Vec<String> {
        options
            .iter()
            .map(|option| option.config_id.0.to_string())
            .collect()
    }

    #[test]
    fn native_usage_uses_account_weekly_remaining_and_reset() {
        let value = json!({"rateLimitsByLimitId":{"codex":{"secondary":{"usedPercent":23.4,"windowDurationMins":10080,"resetsAt":2000000000}},"model":{"secondary":{"usedPercent":99,"windowDurationMins":10080}}}});
        let options = usage_options(Some(&value), None, 100);
        assert_eq!(ids(&options), vec!["usage-10080"]);
        let option = serde_json::to_value(&options[0]).expect("option");
        assert_eq!(option["options"][0]["name"], "77%");
        assert!(
            option["description"]
                .as_str()
                .expect("description")
                .contains("МСК")
        );
        let expired = usage_options(Some(&value), None, 2000000001);
        assert_eq!(
            serde_json::to_value(&expired[0]).expect("option")["options"][0]["name"],
            "—"
        );
    }

    #[test]
    fn native_usage_lists_every_window_in_shortest_first_order() {
        let value = json!({"rateLimitsByLimitId":{"codex":{
            "primary":{"usedPercent":10,"windowDurationMins":300,"resetsAt":2000000000},
            "secondary":{"usedPercent":20,"windowDurationMins":10080,"resetsAt":2000000000}
        },"codex_monthly":{
            "limitId":"codex",
            "primary":{"usedPercent":40,"windowDurationMins":43200,"resetsAt":2000000000}
        }}});
        let options = usage_options(Some(&value), None, 100);
        assert_eq!(
            ids(&options),
            vec!["usage-300", "usage-10080", "usage-43200"]
        );
        assert_eq!(
            serde_json::to_value(&options[0]).expect("option")["name"],
            "Лимит Codex · 5 ч"
        );
        assert_eq!(
            serde_json::to_value(&options[1]).expect("option")["name"],
            "Лимит Codex · неделя"
        );
        assert_eq!(
            serde_json::to_value(&options[2]).expect("option")["name"],
            "Лимит Codex · месяц"
        );
    }

    #[test]
    fn native_usage_does_not_substitute_model_limits() {
        for value in [
            json!({"rateLimitsByLimitId":{"model":{"secondary":{"usedPercent":5,"windowDurationMins":10080}}}}),
            json!({}),
        ] {
            let options = usage_options(Some(&value), None, 0);
            assert_eq!(ids(&options), vec!["usage"]);
        }
    }

    #[test]
    fn usage_action_ids_are_recognized() {
        for id in ["usage", "usage-300", "usage-10080", "usage-43200"] {
            assert!(owns_usage(&acp::SessionConfigId::new(id)));
        }
        assert!(!owns_usage(&acp::SessionConfigId::new("weekly-usage")));
        assert!(!owns_usage(&acp::SessionConfigId::new("mode")));
    }
}
