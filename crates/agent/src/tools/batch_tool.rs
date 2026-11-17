use crate::{AgentTool, AgentToolOutput, Thread, ToolCallEventStream, ToolInput};
use agent_client_protocol::v2::schema as acp;
use gpui::{App, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const MAX_BATCH_OUTPUT_CHARS: usize = 200_000;

/// One tool call inside a batch.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BatchCall {
    /// The name of a tool available to the model, for example `read_file`.
    pub tool: String,
    /// The input object for that tool, matching its schema.
    pub input: serde_json::Value,
}

/// Run several tool calls in one step.
///
/// Calls execute in order, and each result is returned in the same order. Use
/// this when several independent reads/searches are needed: it avoids a full
/// model round trip between every tool. Do not nest `batch` inside `batch`, and
/// do not put `ask_user` in a batch because it needs a synchronous answer.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BatchToolInput {
    /// Tool calls to execute, in order.
    pub calls: Vec<BatchCall>,
}

pub struct BatchTool {
    thread: WeakEntity<Thread>,
}

impl BatchTool {
    pub fn new(thread: WeakEntity<Thread>) -> Self {
        Self { thread }
    }
}

impl AgentTool for BatchTool {
    type Input = BatchToolInput;
    type Output = String;

    const NAME: &'static str = "batch";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn description() -> SharedString {
        "Run several tool calls in one step. Calls execute in order. Use this for independent reads, searches, and inspections instead of making the model wait for a separate round trip after each tool. Do not nest batch calls or put ask_user in a batch.".into()
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
            Ok(input) if !input.calls.is_empty() => {
                format!("Run {} tool calls", input.calls.len()).into()
            }
            _ => "Run tool calls".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let mut input = input.recv().await.map_err(|error| error.to_string())?;
            if input.calls.is_empty() {
                return Err("A batch must contain at least one tool call.".to_string());
            }

            let thread = self
                .thread
                .upgrade()
                .ok_or_else(|| "The thread is no longer available.".to_string())?;
            // Only run tools exposed to the model for this turn. Reading the
            // registry directly would bypass profile choices, restricted
            // workspace rules, provider compatibility, and feature flags.
            let tools = thread.read_with(cx, |thread, cx| thread.executable_tools(cx));
            let mut output = String::new();

            for (index, call) in input.calls.iter_mut().enumerate() {
                if call.tool == Self::NAME {
                    output.push_str(&format!(
                        "### {}. {}\nerror: batch cannot call itself\n\n",
                        index + 1,
                        call.tool
                    ));
                    continue;
                }
                if call.tool == crate::AskUserTool::NAME {
                    output.push_str(&format!(
                        "### {}. {}\nerror: ask_user needs a synchronous answer and cannot run in a batch\n\n",
                        index + 1,
                        call.tool
                    ));
                    continue;
                }

                let Some(tool) = tools.get(call.tool.as_str()).cloned() else {
                    output.push_str(&format!(
                        "### {}. {}\nerror: tool is not available in this turn (unknown, disabled by the profile, or blocked in a restricted workspace)\n\n",
                        index + 1,
                        call.tool
                    ));
                    continue;
                };

                let child_stream = event_stream.child(index);
                let task = cx.update(|cx| {
                    tool.clone()
                        .run(ToolInput::ready(std::mem::take(&mut call.input)), child_stream, cx)
                });
                let result = task.await;
                output.push_str(&format!("### {}. {}\n", index + 1, call.tool));
                match result {
                    Ok(result) => output.push_str(&batch_output_text(&result)),
                    Err(error) => {
                        output.push_str("error:\n");
                        output.push_str(&batch_output_text(&error));
                    }
                }
                output.push_str("\n\n");

                if output.chars().count() >= MAX_BATCH_OUTPUT_CHARS {
                    output.push_str("... (batch output truncated)");
                    break;
                }
            }

            Ok(output)
        })
    }
}

fn batch_output_text(output: &AgentToolOutput) -> String {
    output
        .llm_output
        .iter()
        .map(|content| match content {
            LanguageModelToolResultContent::Text(text) => text.to_string(),
            LanguageModelToolResultContent::Image(_) => "[image]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
