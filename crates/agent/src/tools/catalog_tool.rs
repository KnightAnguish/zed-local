use crate::{AgentTool, Thread, ToolCallEventStream, ToolInput};
use agent_client_protocol::v2::schema as acp;
use gpui::{App, SharedString, Task, WeakEntity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const MAX_CATALOG_CHARS: usize = 200_000;

/// Describe the tools available to `batch`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CatalogToolInput {
    /// Optional tool names to describe. Omit this to describe every tool.
    pub tools: Option<Vec<String>>,
}

/// List available tools and their input schemas.
///
/// Use this before `batch` to learn the exact tool names and arguments. The
/// catalog is read-only and does not execute anything.
pub struct CatalogTool {
    thread: WeakEntity<Thread>,
}

impl CatalogTool {
    pub fn new(thread: WeakEntity<Thread>) -> Self {
        Self { thread }
    }
}

impl AgentTool for CatalogTool {
    type Input = CatalogToolInput;
    type Output = String;

    const NAME: &'static str = "catalog";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn description() -> SharedString {
        "List the tools available to the batch tool with their input schemas. Call this first, then pass the exact tool name and input object to batch.".into()
    }

    fn allow_in_restricted_mode() -> bool {
        false
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) if input.tools.as_ref().is_some_and(|tools| !tools.is_empty()) => {
                format!("Describe {} tools", input.tools.as_ref().map_or(0, Vec::len)).into()
            }
            _ => "Show tool catalog".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            let thread = self
                .thread
                .upgrade()
                .ok_or_else(|| "The thread is no longer available.".to_string())?;
            let tools = thread.read_with(cx, |thread, cx| thread.executable_tools(cx));
            let requested = input
                .tools
                .as_ref()
                .map(|names| names.iter().map(String::as_str).collect::<std::collections::HashSet<_>>());

            let mut output = String::new();
            for (name, tool) in tools {
                if requested
                    .as_ref()
                    .is_some_and(|names| !names.contains(name.as_ref()))
                {
                    continue;
                }
                let schema = serde_json::to_string_pretty(&tool.input_schema())
                    .unwrap_or_else(|_| "{}".to_string());
                output.push_str(&format!(
                    "## {}\n{}\n\nInput schema:\n```json\n{}\n```\n\n",
                    name,
                    tool.description(),
                    schema
                ));
                if output.chars().count() >= MAX_CATALOG_CHARS {
                    output.push_str("... (catalog truncated)");
                    break;
                }
            }

            if output.is_empty() {
                Err("No tools matched the requested names.".to_string())
            } else {
                Ok(output)
            }
        })
    }
}
