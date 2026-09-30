//! Translation at the UI boundary only; ACP is not used as a transport.
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct TurnEvents {
    streamed: HashSet<String>,
}

impl TurnEvents {
    pub(super) fn update(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Result<Vec<acp::SessionUpdate>> {
        let mut updates = Vec::new();
        match method {
            "item/agentMessage/delta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/textDelta"
            | "item/plan/delta" => {
                let id = text(params, "itemId")?;
                self.streamed.insert(id.to_owned());
                let thought = method.contains("reasoning/");
                updates.push(chunk(id, text(params, "delta")?, thought)?);
            }
            "item/started" | "item/completed" => {
                let item = params.get("item").context("Missing Codex item")?;
                let id = text(item, "id")?;
                let kind = text(item, "type")?;
                let completed = method == "item/completed";
                match kind {
                    "agentMessage" | "plan" if completed => {
                        if !self.streamed.contains(id) {
                            updates.push(chunk(id, text(item, "text")?, false)?);
                        }
                    }
                    "userMessage" => {} // Zed has already inserted the optimistic user message.
                    "reasoning" if completed && !self.streamed.contains(id) => {
                        if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                            for part in summary {
                                if let Some(part) = part.as_str() {
                                    updates.push(chunk(id, part, true)?);
                                }
                            }
                        }
                    }
                    "agentMessage" | "plan" | "reasoning" => {}
                    _ => updates.push(tool(item, completed)?),
                }
            }
            "thread/tokenUsage/updated" => {
                let usage = &params["tokenUsage"];
                if let (Some(used), Some(size)) = (
                    usage["last"]["totalTokens"].as_u64(),
                    usage["modelContextWindow"].as_u64(),
                ) {
                    updates.push(serde_json::from_value(
                        json!({"sessionUpdate":"usage_update","used":used,"size":size}),
                    )?);
                }
            }
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                // The completed item supplies the authoritative output. Do not accumulate it twice.
            }
            "error" => {
                // Zed's current ACP schema has no notice variant. Keep diagnostics visible
                // as a separate thought entry, not part of the final assistant reply.
                let message = params["error"]["message"]
                    .as_str()
                    .unwrap_or("Codex reported an error");
                let severity = if params["willRetry"].as_bool() == Some(true) {
                    "Warning"
                } else {
                    "Error"
                };
                updates.push(chunk(
                    "codex-transport-notice",
                    &format!("{severity}: {message}\n"),
                    true,
                )?);
            }
            _ => {}
        }
        Ok(updates)
    }
}

fn chunk(id: &str, value: &str, thought: bool) -> Result<acp::SessionUpdate> {
    Ok(serde_json::from_value(json!({
        "sessionUpdate":if thought { "agent_thought_chunk" } else { "agent_message_chunk" },
        "messageId":id,"content":{"type":"text","text":value}
    }))?)
}

fn tool(item: &Value, completed: bool) -> Result<acp::SessionUpdate> {
    let kind = text(item, "type")?;
    let (title, tool_kind) = match kind {
        "commandExecution" => (item["command"].as_str().unwrap_or("Run command"), "execute"),
        "fileChange" => ("Edit files", "edit"),
        "webSearch" => ("Search the web", "search"),
        "mcpToolCall" => (item["tool"].as_str().unwrap_or("MCP tool"), "other"),
        "imageGeneration" => ("Generate image", "other"),
        "contextCompaction" => ("Compact context", "other"),
        _ => (kind, "other"),
    };
    let mut content = Vec::new();
    if let Some(output) = item["aggregatedOutput"].as_str() {
        content.push(json!({"type":"content","content":{"type":"text","text":output}}));
    }
    if let Some(changes) = item["changes"].as_array() {
        for change in changes {
            // Unified diffs are text, not a replacement file body. Never treat them as newText.
            if let Some(diff) = change["diff"].as_str() {
                content.push(json!({"type":"content","content":{"type":"text","text":format!("{}\n{diff}",change["path"].as_str().unwrap_or(""))}}));
            }
        }
    }
    let failed = matches!(item["status"].as_str(), Some("failed" | "declined"))
        || item["exitCode"].as_i64().is_some_and(|exit| exit != 0);
    Ok(serde_json::from_value(json!({
        "sessionUpdate":if completed {"tool_call_update"} else {"tool_call"},
        "toolCallId":text(item,"id")?,"title":title,"kind":tool_kind,
        "status":if !completed {"in_progress"} else if failed {"failed"} else {"completed"},
        "content":content
    }))?)
}

pub(super) fn turn_result(params: &Value) -> Result<acp::PromptResponse> {
    match params["turn"]["status"].as_str() {
        Some("completed") => Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
        Some("interrupted") => Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)),
        Some("failed") => bail!(
            "Codex turn failed: {}",
            params["turn"]["error"]["message"]
                .as_str()
                .unwrap_or("Unknown error")
        ),
        _ => bail!("Unexpected Codex terminal turn status"),
    }
}

pub(super) fn input(blocks: &[acp::ContentBlock]) -> Result<Vec<Value>> {
    blocks.iter().map(|block| match block {
        acp::ContentBlock::Text(text) => Ok(json!({"type":"text","text":text.text,"text_elements":[]})),
        acp::ContentBlock::Image(image) => Ok(json!({"type":"image","url":format!("data:{};base64,{}",image.mime_type,image.data)})),
        // Unsupported input must fail visibly, never disappear from a sent prompt.
        _ => bail!("This attachment type is not implemented in native Codex yet"),
    }).collect()
}

pub(super) fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Missing Codex field {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stream_is_not_duplicated_by_completed_item() {
        let mut events = TurnEvents::default();
        assert_eq!(
            events
                .update(
                    "item/agentMessage/delta",
                    &json!({"itemId":"a","delta":"hello"})
                )
                .unwrap()
                .len(),
            1
        );
        assert!(
            events
                .update(
                    "item/completed",
                    &json!({"item":{"id":"a","type":"agentMessage","text":"hello"}})
                )
                .unwrap()
                .is_empty()
        );
        let updates = events
            .update(
                "item/completed",
                &json!({"item":{"id":"b","type":"agentMessage","text":"new"}}),
            )
            .unwrap();
        assert_eq!(serde_json::to_value(&updates[0]).unwrap()["messageId"], "b");
    }
    #[test]
    fn tools_and_context_are_valid_ui_updates() {
        let mut events = TurnEvents::default();
        for method in ["item/started", "item/completed"] {
            let updates = events.update(method, &json!({"item":{"id":"tool","type":"commandExecution","command":"echo hi","exitCode":1,"aggregatedOutput":"hi"}})).unwrap();
            assert_eq!(updates.len(), 1);
        }
        let updates = events
            .update(
                "thread/tokenUsage/updated",
                &json!({"tokenUsage":{"last":{"totalTokens":50},"modelContextWindow":1000}}),
            )
            .unwrap();
        assert_eq!(serde_json::to_value(&updates[0]).unwrap()["used"], 50);
    }
    #[test]
    fn failed_and_interrupted_turns_are_not_success() {
        assert!(
            turn_result(&json!({"turn":{"status":"failed","error":{"message":"offline"}}}))
                .is_err()
        );
        assert_eq!(
            turn_result(&json!({"turn":{"status":"interrupted"}}))
                .unwrap()
                .stop_reason,
            acp::StopReason::Cancelled
        );
    }
    #[test]
    fn retry_warning_remains_visible() {
        let updates = TurnEvents::default().update("error", &json!({"willRetry":true,"error":{"message":"Falling back from WebSockets to HTTPS transport"}})).unwrap();
        assert_eq!(
            serde_json::to_value(&updates[0]).unwrap()["sessionUpdate"],
            "agent_thought_chunk"
        );
        assert!(
            serde_json::to_value(&updates[0]).unwrap()["content"]["text"]
                .as_str()
                .unwrap()
                .starts_with("Warning:")
        );
    }
}
