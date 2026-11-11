//! Translation at the UI boundary only; ACP is not used as a transport.
use agent_client_protocol::v1::schema as acp;
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

const MAX_TOOL_OUTPUT: usize = 64 * 1024;
const MAX_STREAMING_TOOLS: usize = 128;

#[derive(Default)]
pub(super) struct TurnEvents {
    streamed: HashSet<String>,
    tool_output: HashMap<String, String>,
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
                    _ => {
                        let mut final_item = item.clone();
                        if completed
                            && kind == "commandExecution"
                            && final_item["aggregatedOutput"].is_null()
                        {
                            if let Some(output) = self.tool_output.get(id) {
                                final_item["aggregatedOutput"] = json!(output);
                            }
                        }
                        updates.push(tool(&final_item, completed)?);
                        if completed {
                            self.tool_output.remove(id);
                        }
                    }
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
                let id = text(params, "itemId")?;
                anyhow::ensure!(
                    self.tool_output.contains_key(id)
                        || self.tool_output.len() < MAX_STREAMING_TOOLS,
                    "Too many concurrent Codex tool output streams"
                );
                let output = self.tool_output.entry(id.to_owned()).or_default();
                output.push_str(text(params, "delta")?);
                *output = bounded_output(output);
                updates.push(serde_json::from_value(json!({
                    "sessionUpdate":"tool_call_update", "toolCallId":id,
                    "content":[{"type":"content","content":{"type":"text","text":output}}]
                }))?);
            }
            "turn/plan/updated" => {
                let entries: Vec<Value> = params["plan"].as_array().context("Missing Codex plan")?
                    .iter().map(|step| json!({"content":step["step"],"priority":"medium","status":step["status"]})).collect();
                updates.push(serde_json::from_value(
                    json!({"sessionUpdate":"plan","entries":entries}),
                )?);
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
        content.push(
            json!({"type":"content","content":{"type":"text","text":bounded_output(output)}}),
        );
    }
    if let Some(changes) = item["changes"].as_array() {
        for change in changes {
            // Unified diffs are text, not a replacement file body. Never treat them as newText.
            if let Some(diff) = change["diff"].as_str() {
                content.push(json!({"type":"content","content":{"type":"text","text":bounded_output(&format!("{}\n{diff}",change["path"].as_str().unwrap_or("")))}}));
            }
        }
    }
    if kind == "imageGeneration" {
        if let Some(result) = item["result"].as_str().filter(|value| !value.is_empty()) {
            content.push(json!({"type":"content","content":{"type":"image","mimeType":"image/png","data":result}}));
        }
    }
    if let Some(output) = item["output"].as_str() {
        content.push(
            json!({"type":"content","content":{"type":"text","text":bounded_output(output)}}),
        );
    }
    if let Some(blocks) = item["contentItems"]
        .as_array()
        .or_else(|| item["output"].as_array())
    {
        for block in blocks {
            let block = output_block(block)?;
            content.push(json!({"type":"content","content":block}));
        }
    }
    if kind == "collabAgentToolCall" {
        content.push(json!({"type":"content","content":{"type":"text","text":bounded_output(&format!("Agents: {}\n{}", item["receiverThreadIds"], item["agentsStates"]))}}));
    }
    if let Some(result) = item["result"]["content"].as_array() {
        for block in result {
            // Validate MCP content against the actual UI schema before forwarding it.
            let mut block = block.clone();
            if let Some(value) = block["text"].as_str() {
                block["text"] = json!(bounded_output(value));
            }
            if let Ok(block) = serde_json::from_value::<acp::ContentBlock>(block.clone()) {
                content.push(json!({"type":"content","content":block}));
            } else {
                content.push(json!({"type":"content","content":{"type":"text","text":bounded_output(&block.to_string())}}));
            }
        }
    }
    if let Some(error) = item["error"]["message"].as_str() {
        content
            .push(json!({"type":"content","content":{"type":"text","text":bounded_output(error)}}));
    }
    let failed = matches!(
        item["status"].as_str(),
        Some("failed" | "declined" | "cancelled" | "canceled")
    ) || item["success"].as_bool() == Some(false)
        || !item["error"].is_null()
        || !item["failure"].is_null()
        || item["exitCode"].as_i64().is_some_and(|exit| exit != 0);
    Ok(serde_json::from_value(json!({
        "sessionUpdate":if completed {"tool_call_update"} else {"tool_call"},
        "toolCallId":text(item,"id")?,"title":title,"kind":tool_kind,
        "status":if !completed {"in_progress"} else if failed {"failed"} else {"completed"},
        "content":content
    }))?)
}

fn output_block(block: &Value) -> Result<Value> {
    match text(block, "type")? {
        "input_text" | "inputText" => {
            Ok(json!({"type":"text","text":bounded_output(text(block,"text")?)}))
        }
        "input_image" | "inputImage" | "input_audio" | "inputAudio" => {
            let image = matches!(text(block, "type")?, "input_image" | "inputImage");
            let url = block
                .get(if image { "image_url" } else { "audio_url" })
                .or_else(|| block.get(if image { "imageUrl" } else { "audioUrl" }))
                .and_then(Value::as_str)
                .context("Missing tool media URL")?;
            if let Some((mime, data)) = url
                .strip_prefix("data:")
                .and_then(|url| url.split_once(";base64,"))
            {
                return Ok(
                    json!({"type":if image {"image"} else {"audio"},"mimeType":mime,"data":data}),
                );
            }
            Ok(
                json!({"type":"resource_link","name":if image {"Tool image"} else {"Tool audio"},"uri":url}),
            )
        }
        "encrypted_content" => {
            Ok(json!({"type":"text","text":"[Encrypted tool output; retained by Codex]"}))
        }
        kind => Ok(json!({"type":"text","text":format!("[Unsupported tool output type: {kind}]")})),
    }
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
    blocks
        .iter()
        .map(|block| input_block(&serde_json::to_value(block)?))
        .collect()
}

fn input_block(block: &Value) -> Result<Value> {
    let text_input = |value: String| json!({"type":"text","text":value,"text_elements":[]});
    match text(block, "type")? {
        "text" => Ok(text_input(text(block, "text")?.to_owned())),
        "image" | "audio" => {
            let kind = text(block, "type")?;
            let url = block["uri"].as_str().filter(|uri| {
                uri.starts_with("https://")
                    || uri.starts_with("http://")
                    || uri.starts_with("data:")
            });
            Ok(json!({"type":kind,"url":match url {
                Some(url) => url.to_owned(),
                None => format!("data:{};base64,{}",text(block,"mimeType")?,text(block,"data")?),
            }}))
        }
        "resource_link" => Ok(text_input(format!(
            "{} ({})",
            text(block, "name")?,
            text(block, "uri")?
        ))),
        "resource" => {
            let resource = &block["resource"];
            let uri = text(resource, "uri")?;
            if let Some(value) = resource["text"].as_str() {
                return Ok(text_input(format!("Attached resource {uri}:\n{value}")));
            }
            let mime = resource["mimeType"]
                .as_str()
                .unwrap_or("application/octet-stream");
            let blob = text(resource, "blob")?;
            if mime.starts_with("image/") || mime.starts_with("audio/") {
                Ok(
                    json!({"type":if mime.starts_with("image/") {"image"} else {"audio"},"url":format!("data:{mime};base64,{blob}")}),
                )
            } else {
                Ok(text_input(format!(
                    "Attached resource {uri} ({mime}, base64):\n{blob}"
                )))
            }
        }
        kind => bail!("Unsupported native Codex attachment: {kind}"),
    }
}

fn bounded_output(value: &str) -> String {
    if value.len() <= MAX_TOOL_OUTPUT {
        return value.to_owned();
    }
    let mut start = value.len() - MAX_TOOL_OUTPUT;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "[Earlier output omitted from the editor]\n{}",
        &value[start..]
    )
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
    fn attachments_preserve_order_and_resource_contents() {
        let blocks: Vec<acp::ContentBlock> = serde_json::from_value(json!([
            {"type":"text","text":"inspect"},
            {"type":"resource","resource":{"uri":"file:///C:/my%20file.txt","text":"important contents"}},
            {"type":"resource_link","uri":"file:///C:/image.png","name":"image"},
            {"type":"audio","data":"YQ==","mimeType":"audio/wav"},
            {"type":"image","data":"YQ==","mimeType":"image/png"}
        ])).unwrap();
        let values = input(&blocks).unwrap();
        assert_eq!(values.len(), 5);
        assert!(
            values[1]["text"]
                .as_str()
                .unwrap()
                .contains("important contents")
        );
        assert!(
            values[2]["text"]
                .as_str()
                .unwrap()
                .contains("file:///C:/image.png")
        );
        assert_eq!(
            values[3],
            json!({"type":"audio","url":"data:audio/wav;base64,YQ=="})
        );
        assert_eq!(values[4]["type"], "image");
    }

    #[test]
    fn streaming_output_is_bounded_and_final_replaces_it() {
        let mut events = TurnEvents::default();
        let output = "Ж".repeat(MAX_TOOL_OUTPUT);
        let updates = events
            .update(
                "item/commandExecution/outputDelta",
                &json!({"itemId":"x","delta":output}),
            )
            .unwrap();
        let value = serde_json::to_value(&updates[0]).unwrap();
        let text = value["content"][0]["content"]["text"].as_str().unwrap();
        assert!(text.len() < MAX_TOOL_OUTPUT + 100);
        assert!(text.ends_with('Ж'));
        assert!(text.starts_with("[Earlier output omitted"));
        let final_updates = events.update("item/completed", &json!({"item":{"type":"commandExecution","id":"x","aggregatedOutput":"final","status":"completed"}})).unwrap();
        assert_eq!(
            serde_json::to_value(&final_updates[0]).unwrap()["content"][0]["content"]["text"],
            "final"
        );
        assert!(events.tool_output.is_empty());
    }

    #[test]
    fn plan_updates_and_mcp_results_reach_the_ui() {
        let mut events = TurnEvents::default();
        let plan = events
            .update(
                "turn/plan/updated",
                &json!({"plan":[{"step":"Verify","status":"in_progress"}]}),
            )
            .unwrap();
        assert_eq!(
            serde_json::to_value(&plan[0]).unwrap()["entries"][0]["content"],
            "Verify"
        );
        let output = events.update("item/completed", &json!({"item":{"type":"mcpToolCall","id":"m","tool":"fetch","status":"completed","result":{"content":[{"type":"text","text":"answer"}]}}})).unwrap();
        assert_eq!(
            serde_json::to_value(&output[0]).unwrap()["content"][0]["content"]["text"],
            "answer"
        );
    }

    #[test]
    fn generated_image_and_mcp_error_are_not_lost() {
        let output = tool(
            &json!({"type":"imageGeneration","id":"image","status":"completed","result":"YQ=="}),
            true,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(output).unwrap()["content"][0]["content"]["type"],
            "image"
        );
        let output = tool(&json!({"type":"mcpToolCall","id":"mcp","status":"completed","error":{"message":"denied"}}), true).unwrap();
        assert_eq!(serde_json::to_value(output).unwrap()["status"], "failed");
    }

    #[test]
    fn mixed_function_output_retains_media_without_exposing_encrypted_payloads() {
        let output = tool(
            &json!({"type":"functionCallOutput","id":"result","output":[
                {"type":"input_text","text":"caption"},
                {"type":"input_image","image_url":"data:image/png;base64,YQ=="},
                {"type":"encrypted_content","encrypted_content":"private-opaque-body"}
            ]}),
            true,
        )
        .unwrap();
        let value = serde_json::to_value(output).unwrap();
        assert_eq!(value["content"][1]["content"]["data"], "YQ==");
        assert!(!value.to_string().contains("private-opaque-body"));
    }
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
