use agent_client_protocol::schema::v2 as acp;
use anyhow::{Context as _, Result, anyhow, ensure};
use chrono::{DateTime, Local};
use collections::HashMap;
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Task};
use http_client::{AsyncBody, HttpClient, Method, Request};
use project::agent_server_store::AgentServerCommand;
use serde::Deserialize;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    process::Stdio,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use util::{ResultExt as _, process::Child};

const USAGE_ID: &str = "opencode-usage";
const USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

#[derive(Default)]
struct UsageState {
    usage: Option<GoUsage>,
    error: Option<String>,
    requested: Option<Instant>,
}

#[derive(Clone)]
pub(crate) struct OpenCodeUsage {
    executable: PathBuf,
    environment: Option<HashMap<String, String>>,
    directory: Option<PathBuf>,
    state: Rc<RefCell<UsageState>>,
    sender: Rc<RefCell<watch::Sender<()>>>,
    pub(crate) receiver: watch::Receiver<()>,
    task: Rc<RefCell<Option<Task<()>>>>,
}

impl OpenCodeUsage {
    pub(crate) fn from_command(
        command: &AgentServerCommand,
        directory: Option<PathBuf>,
        cx: &App,
    ) -> Option<Self> {
        let executable = command.path.clone();
        if !executable.is_absolute() || !executable.is_file() {
            return None;
        }
        // The registry ships a native binary. Npx/shell wrappers do not expose the
        // same auth store, so do not guess at their command line.
        if !executable
            .file_stem()
            .is_some_and(|stem| stem.to_string_lossy().eq_ignore_ascii_case("opencode"))
        {
            return None;
        }

        let (sender, receiver) = watch::channel(());
        let usage = Self {
            executable,
            environment: command.env.clone(),
            directory,
            state: Rc::new(RefCell::new(UsageState::default())),
            sender: Rc::new(RefCell::new(sender)),
            receiver,
            task: Rc::new(RefCell::new(None)),
        };
        usage.refresh(cx, false);
        Some(usage)
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
        let executable = self.executable.clone();
        let environment = self.environment.clone();
        let directory = self.directory.clone();
        let http_client = cx.http_client();
        let executor = cx.background_executor().clone();
        let request = cx.background_spawn(async move {
            match futures::future::select(
                Box::pin(query_usage(
                    &executable,
                    environment.as_ref(),
                    directory.as_deref(),
                    http_client,
                )),
                Box::pin(executor.timer(Duration::from_secs(30))),
            )
            .await
            {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right(_) => {
                    Err(anyhow!("OpenCode usage request timed out"))
                }
            }
        });
        let state = self.state.clone();
        let sender = self.sender.clone();
        *self.task.borrow_mut() = Some(cx.foreground_executor().spawn(async move {
            let result = request.await;
            let mut state = state.borrow_mut();
            match result {
                Ok(usage) => {
                    state.usage = Some(usage);
                    state.error = None;
                }
                Err(error) => {
                    state.usage = None;
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
            state.usage.as_ref(),
            state.error.as_deref(),
            chrono::Utc::now().timestamp_millis(),
        ));
        options
    }
}

#[derive(Clone, Debug, Deserialize)]
struct UsageResponse {
    usage: GoUsage,
}

#[derive(Clone, Debug, Deserialize)]
struct GoUsage {
    rolling: Meter,
    weekly: Meter,
    monthly: Meter,
}

#[derive(Clone, Debug, Deserialize)]
struct Meter {
    #[serde(default)]
    status: String,
    percent: f64,
    #[serde(rename = "resetsAt", default)]
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct ExportedCredential {
    #[serde(default)]
    active: bool,
    value: ExportedValue,
}

#[derive(Deserialize)]
struct ExportedValue {
    key: Option<String>,
    access: Option<String>,
}

async fn query_usage(
    executable: &Path,
    environment: Option<&HashMap<String, String>>,
    directory: Option<&Path>,
    http_client: Arc<dyn HttpClient>,
) -> Result<GoUsage> {
    let token = active_token(executable, environment, directory).await?;
    let request = Request::builder()
        .method(Method::GET)
        .uri(USAGE_URL)
        .header("Accept", "application/json")
        // OpenCode's own inference endpoint sits behind Cloudflare and accepts its
        // native client signature; a generic HTTP user agent is rejected.
        .header("User-Agent", "opencode")
        .header("Authorization", format!("Bearer {token}"))
        .body(AsyncBody::empty())?;
    let mut response = http_client.send(request).await?;
    ensure!(
        response.status().is_success(),
        "OpenCode Go usage returned HTTP {}",
        response.status()
    );
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    let response: UsageResponse = serde_json::from_str(&body)?;
    Ok(response.usage)
}

async fn active_token(
    executable: &Path,
    environment: Option<&HashMap<String, String>>,
    directory: Option<&Path>,
) -> Result<String> {
    let mut process = std::process::Command::new(executable);
    process
        .args(["--log-level", "none", "auth", "export", "opencode-go"])
        .stdin(Stdio::null());
    if let Some(environment) = environment {
        process.envs(environment);
    }
    if let Some(directory) = directory {
        process.current_dir(directory);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        process.creation_flags(0x08000000);
    }
    let output = Child::spawn(process, Stdio::null(), Stdio::piped(), Stdio::piped())?
        .output()
        .await
        .context("Failed to run OpenCode auth export")?;
    // stdout contains credentials. Never put it in an error or log message.
    ensure!(output.status.success(), "OpenCode auth export failed");
    parse_active_token(&output.stdout)
}

fn parse_active_token(stdout: &[u8]) -> Result<String> {
    let credentials: Vec<ExportedCredential> =
        serde_json::from_slice(stdout).context("OpenCode auth export returned invalid JSON")?;
    let credential = credentials
        .iter()
        .find(|credential| credential.active)
        .or_else(|| credentials.first())
        .context("OpenCode Go credential is not configured")?;
    let token = credential
        .value
        .key
        .as_ref()
        .or(credential.value.access.as_ref())
        .context("OpenCode Go credential has no token")?;
    ensure!(!token.is_empty(), "OpenCode Go credential is empty");
    Ok(token.clone())
}

fn usage_option(
    usage: Option<&GoUsage>,
    error: Option<&str>,
    now_ms: i64,
) -> acp::SessionConfigOption {
    let weekly = usage.map(|usage| &usage.weekly);
    let reset = weekly
        .and_then(|meter| meter.resets_at.as_deref())
        .and_then(parse_reset);
    let weekly_used = weekly
        .map(|meter| meter.percent)
        .filter(|percent| percent.is_finite() && (0.0..=100.0).contains(percent))
        .filter(|_| reset.is_none_or(|reset| reset > now_ms));
    let label = weekly_used
        .map(|used| format!("{}%", remaining_percent(used)))
        .unwrap_or_else(|| "—".into());
    let description = if let Some(usage) = usage {
        let reset = reset
            .map(format_reset)
            .unwrap_or_else(|| "неизвестно".into());
        format!(
            "OpenCode Go, осталось: 5ч — {}%, неделя — {}%, месяц — {}%. Сброс недели: {reset}.",
            remaining(&usage.rolling),
            remaining(&usage.weekly),
            remaining(&usage.monthly),
        )
    } else {
        format!(
            "Лимит OpenCode недоступен. {}",
            error.unwrap_or("Нет данных от OpenCode Go.")
        )
    };
    acp::SessionConfigOption::select(
        USAGE_ID,
        "Лимит OpenCode",
        "status",
        vec![
            acp::SessionConfigSelectOption::new("status", label),
            acp::SessionConfigSelectOption::new("refresh", "Обновить"),
        ],
    )
    .description(description)
}

fn remaining(meter: &Meter) -> u8 {
    if meter.status == "rate-limited" {
        0
    } else {
        remaining_percent(meter.percent)
    }
}

fn remaining_percent(used: f64) -> u8 {
    (100.0 - used).clamp(0.0, 100.0).round() as u8
}

fn parse_reset(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
}

fn format_reset(timestamp: i64) -> String {
    DateTime::from_timestamp_millis(timestamp)
        .map(|timestamp| {
            timestamp
                .with_timezone(&Local)
                .format("%d.%m %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "неизвестно".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exported_active_credential_is_used() {
        let exported = serde_json::to_vec(&json!([
            {"active": false, "value": {"type": "api", "key": "old"}},
            {"active": true, "value": {"type": "api", "key": "current"}}
        ]))
        .expect("json");
        assert_eq!(parse_active_token(&exported).expect("token"), "current");
    }

    #[test]
    fn exported_oauth_access_token_is_used() {
        let exported = serde_json::to_vec(&json!([
            {"active": true, "value": {"type": "oauth", "access": "access", "refresh": "refresh"}}
        ]))
        .expect("json");
        assert_eq!(parse_active_token(&exported).expect("token"), "access");
    }

    #[test]
    fn opencode_option_shows_remaining_weekly_allowance() {
        let usage: GoUsage = serde_json::from_value(json!({
            "rolling": {"status": "ok", "percent": 1, "resetsAt": "2030-01-01T03:00:00.000Z"},
            "weekly": {"status": "ok", "percent": 25, "resetsAt": "2030-01-09T00:00:00.000Z"},
            "monthly": {"status": "ok", "percent": 40, "resetsAt": "2030-02-05T19:00:00.000Z"}
        }))
        .expect("usage");
        let option = serde_json::to_value(usage_option(Some(&usage), None, 0)).expect("option");
        assert_eq!(option["options"][0]["name"], "75%");
        assert!(
            option["description"]
                .as_str()
                .expect("description")
                .contains("неделя — 75%")
        );
    }

    #[test]
    fn opencode_option_marks_a_limited_window() {
        let usage: GoUsage = serde_json::from_value(json!({
            "rolling": {"status": "rate-limited", "percent": 100, "resetsAt": "2030-01-01T03:00:00.000Z"},
            "weekly": {"status": "ok", "percent": 0, "resetsAt": "2030-01-09T00:00:00.000Z"},
            "monthly": {"status": "ok", "percent": 0, "resetsAt": "2030-02-05T19:00:00.000Z"}
        }))
        .expect("usage");
        let option = serde_json::to_value(usage_option(Some(&usage), None, 0)).expect("option");
        assert_eq!(option["options"][0]["name"], "100%");
        assert!(
            option["description"]
                .as_str()
                .expect("description")
                .contains("5ч — 0%")
        );
    }

    #[gpui::test]
    #[ignore]
    async fn live_opencode_go_usage(cx: &mut gpui::TestAppContext) {
        let Ok(executable) = std::env::var("OPENCODE_USAGE_TEST_EXE") else {
            return;
        };
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_tokio::init(cx);
            let http_client =
                reqwest_client::ReqwestClient::user_agent("opencode").expect("http client");
            cx.set_http_client(Arc::new(http_client));
        });
        let http_client = cx.update(|cx| cx.http_client());
        let usage = query_usage(Path::new(&executable), None, None, http_client)
            .await
            .expect("live OpenCode Go usage");
        eprintln!(
            "OpenCode Go usage: rolling={}% weekly={}% monthly={}%",
            usage.rolling.percent, usage.weekly.percent, usage.monthly.percent
        );
        assert!((0.0..=100.0).contains(&usage.weekly.percent));
    }

    #[test]
    fn opencode_option_without_data_is_explicit() {
        let option =
            serde_json::to_value(usage_option(None, Some("not signed in"), 0)).expect("option");
        assert_eq!(option["options"][0]["name"], "—");
        assert!(
            option["description"]
                .as_str()
                .expect("description")
                .contains("not signed in")
        );
    }
}
