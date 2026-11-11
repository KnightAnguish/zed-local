use crate::codex_framing;
use acp_thread::{HistoryMessage, HistoryPage};
use agent_client_protocol::v1::schema as acp;
use agent_client_protocol::v2::schema as acp_v2;
use anyhow::{Context as _, Result, bail, ensure};
use futures::{AsyncWriteExt as _, io::BufReader};
use project::agent_server_store::AgentServerCommand;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio};
use util::process::Child;

const PAGE_SIZE: usize = 20;
pub(crate) async fn request_turn_page<F, Fut>(
    session_id: &acp_v2::SessionId,
    cursor: Option<&str>,
    mut request: F,
) -> Result<Value>
where
    F: FnMut(Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    request(json!({
        "threadId":session_id.to_string(),"cursor":cursor,"limit":PAGE_SIZE,
        "sortDirection":"desc","itemsView":"summary"
    }))
    .await
}

struct ReaderProcess(Child);

impl Drop for ReaderProcess {
    fn drop(&mut self) {
        if let Err(error) = self.0.kill() {
            log::debug!("Codex history reader already stopped: {error}");
        }
    }
}

#[derive(Clone)]
pub(crate) struct CodexHistory {
    executable: PathBuf,
    environment: collections::HashMap<String, String>,
    directory: Option<PathBuf>,
}

impl CodexHistory {
    pub(crate) fn from_command(
        command: &AgentServerCommand,
        directory: Option<PathBuf>,
    ) -> Option<Self> {
        let environment = command.env.clone()?;
        let executable = PathBuf::from(environment.get("CODEX_PATH")?);
        // Never resolve another account/runtime via PATH or invoke a shell wrapper.
        if !executable.is_absolute() || !executable.is_file() {
            return None;
        }
        #[cfg(windows)]
        if !executable
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        {
            return None;
        }
        Some(Self {
            executable,
            environment,
            directory,
        })
    }

    pub(crate) async fn page(
        &self,
        session_id: acp_v2::SessionId,
        cursor: Option<String>,
    ) -> Result<HistoryPage> {
        let response = request_turn_page(&session_id, cursor.as_deref(), |params| {
            self.read("thread/turns/list", params)
        })
        .await?;
        decode_page(response, cursor.as_deref())
    }

    pub(crate) async fn rate_limits(&self) -> Result<Value> {
        self.read("account/rateLimits/read", json!({})).await
    }

    async fn read(&self, method: &str, params: Value) -> Result<Value> {
        ensure!(
            matches!(method, "thread/turns/list" | "account/rateLimits/read"),
            "Unsupported native read method"
        );
        let mut command = std::process::Command::new(&self.executable);
        command
            .args(["app-server", "--listen", "stdio://"])
            .envs(&self.environment);
        if let Some(directory) = &self.directory {
            command.current_dir(directory);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(0x08000000);
        }
        let mut process = ReaderProcess(Child::spawn(
            command,
            Stdio::piped(),
            Stdio::piped(),
            Stdio::null(),
        )?);
        let child = &mut process.0;
        let mut input = child
            .stdin
            .take()
            .context("Codex history stdin unavailable")?;
        let mut output = BufReader::new(
            child
                .stdout
                .take()
                .context("Codex history stdout unavailable")?,
        );
        let result = async {
            input
                .write_all(
                    format!(
                        "{}\n",
                        json!({"id":1,"method":"initialize","params":{
                            "clientInfo":{"name":"zed_local_history","version":"1"},
                            "capabilities":{"experimentalApi":true}
                        }})
                    )
                    .as_bytes(),
                )
                .await?;
            input.flush().await?;
            read_response(&mut output, 1).await?;
            input
                .write_all(
                    format!(
                        "{}\n{}\n",
                        json!({"method":"initialized","params":{}}),
                        json!({"id":2,"method":method,"params":params})
                    )
                    .as_bytes(),
                )
                .await?;
            input.flush().await?;
            read_response(&mut output, 2).await
        }
        .await;
        // This process only reads stored pages. It never resumes or prompts a session.
        if let Err(error) = child.kill() {
            log::warn!("Could not stop Codex history reader: {error}");
        }
        child
            .status()
            .await
            .context("Could not reap Codex history reader")?;
        result
    }
}

async fn read_response(
    reader: &mut (impl futures::AsyncBufRead + Unpin),
    id: u64,
) -> Result<Value> {
    for _ in 0..128 {
        let mut value = codex_framing::read(reader).await?;
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            ensure!(
                value.get("id").is_none(),
                "Unexpected request in read-only Codex history connection"
            );
            continue;
        }
        if let Some(error) = value.get("error") {
            bail!(
                "Codex history request failed: {}",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown RPC error")
            );
        }
        return value
            .get_mut("result")
            .map(Value::take)
            .context("Missing Codex history result");
    }
    bail!("Too many notifications from Codex history reader")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnPage {
    data: Vec<Turn>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct Turn {
    items: Vec<Value>,
}

pub(crate) fn decode_page(value: Value, previous_cursor: Option<&str>) -> Result<HistoryPage> {
    decode_page_in(
        value,
        previous_cursor,
        &acp_thread::attachment_store::root(),
    )
}

fn decode_page_in(
    value: Value,
    previous_cursor: Option<&str>,
    attachment_root: &std::path::Path,
) -> Result<HistoryPage> {
    let page: TurnPage = serde_json::from_value(value)?;
    ensure!(
        page.data.len() <= PAGE_SIZE,
        "Codex returned too many history turns"
    );
    ensure!(
        page.next_cursor.as_deref().is_none() || page.next_cursor.as_deref() != previous_cursor,
        "Codex returned a repeated history cursor"
    );
    let mut messages = Vec::new();
    let mut identifiers = std::collections::HashSet::new();
    for turn in page.data.into_iter().rev() {
        let turn_start = messages.len();
        let artifacts: Vec<_> = turn
            .items
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("imageGeneration"))
            .filter_map(|item| item.get("savedPath").and_then(Value::as_str))
            .filter_map(history_image_link)
            .collect();
        for item in turn.items {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            if kind != "userMessage" && kind != "agentMessage" {
                continue;
            }
            if item.get("phase").and_then(Value::as_str) == Some("commentary") {
                continue;
            }
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .context("History message has no ID")?;
            ensure!(
                identifiers.insert(id.to_owned()),
                "Duplicate history message ID"
            );
            let content = if kind == "agentMessage" {
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    item.get("text")
                        .and_then(Value::as_str)
                        .context("History reply has no text")?,
                ))]
            } else {
                let mut content = Vec::new();
                for input in item
                    .get("content")
                    .and_then(Value::as_array)
                    .context("History message has no content")?
                {
                    match input.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            content.push(acp::ContentBlock::Text(acp::TextContent::new(
                                input
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .context("History text is missing")?,
                            )))
                        }
                        // Preserve originals only in the requested page; do not fetch remote
                        // resources or decode full-resolution pixels into the UI.
                        Some("localImage") => {
                            let path = input.get("path").and_then(Value::as_str).unwrap_or("");
                            let original = std::path::Path::new(path);
                            let saved = if original.is_absolute()
                                && !path.starts_with("//")
                                && !path.starts_with(r"\\")
                                && original.is_file()
                            {
                                match acp_thread::attachment_store::import_file(
                                    attachment_root,
                                    original,
                                ) {
                                    Ok(path) => Some(path),
                                    Err(error) => {
                                        log::warn!(
                                            "Could not preserve local image attachment: {error}"
                                        );
                                        None
                                    }
                                }
                            } else {
                                None
                            };
                            let saved_path = saved.as_ref().map(|path| path.to_string_lossy());
                            content.push(
                                history_image_link(saved_path.as_deref().unwrap_or(path))
                                    .unwrap_or_else(|| {
                                        acp::ContentBlock::Text(acp::TextContent::new(
                                            "[Image attachment unavailable]",
                                        ))
                                    }),
                            );
                        }
                        Some("image") => {
                            let restored =
                                input.get("url").and_then(Value::as_str).and_then(|url| {
                                    match acp_thread::attachment_store::save_data_url(
                                        attachment_root,
                                        url,
                                    ) {
                                        Ok(path) => history_image_link(&path.to_string_lossy()),
                                        Err(error) => {
                                            log::warn!(
                                                "Could not restore image attachment: {error}"
                                            );
                                            None
                                        }
                                    }
                                });
                            content.push(restored.unwrap_or_else(|| {
                                acp::ContentBlock::Text(acp::TextContent::new(
                                    "[Image attachment unavailable]",
                                ))
                            }));
                        }
                        _ => content.push(acp::ContentBlock::Text(acp::TextContent::new(
                            "[Attachment]",
                        ))),
                    }
                }
                content
            };
            messages.push(HistoryMessage {
                id: acp::MessageId::new(id),
                is_user: kind == "userMessage",
                content,
            });
        }
        if let Some(reply) = messages[turn_start..]
            .iter_mut()
            .rev()
            .find(|message| !message.is_user)
        {
            reply.content.extend(artifacts);
        }
    }
    Ok(HistoryPage {
        messages,
        next_cursor: page.next_cursor,
    })
}

fn history_image_link(path: &str) -> Option<acp::ContentBlock> {
    let file = std::path::Path::new(path);
    if !file.is_absolute() || path.starts_with("\\\\") || path.starts_with("//") {
        return None;
    }
    let mime = match file.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    };
    Some(acp::ContentBlock::ResourceLink(
        acp::ResourceLink::new(
            file.file_name()?.to_string_lossy(),
            format!("file://{path}"),
        )
        .mime_type(mime)
        .meta(serde_json::Map::from_iter([(
            "zedHistoryImage".into(),
            json!(true),
        )])),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_pages_preserve_the_requested_cursor_and_pagination() {
        futures::executor::block_on(async {
            let mut limits = Vec::new();
            let page = request_turn_page(
                &acp_v2::SessionId::new("thread"),
                Some("same-cursor"),
                |params| {
                    assert_eq!(params["cursor"], "same-cursor");
                    assert_eq!(params["itemsView"], "summary");
                    limits.push(params["limit"].as_u64().unwrap());
                    futures::future::ready(Ok(json!({"data":[],"nextCursor":"older-turns"})))
                },
            )
            .await
            .unwrap();
            assert_eq!(limits, [20]);
            assert_eq!(page["nextCursor"], "older-turns");
        });
    }

    #[test]
    fn history_errors_are_not_retried() {
        futures::executor::block_on(async {
            let mut calls = 0;
            request_turn_page(&acp_v2::SessionId::new("thread"), None, |_| {
                calls += 1;
                futures::future::ready(Err(anyhow::anyhow!("permission denied")))
            })
            .await
            .expect_err("ordinary errors must propagate");
            assert_eq!(calls, 1);
        });
    }

    #[test]
    fn single_large_history_turn_is_loaded_without_truncation() {
        futures::executor::block_on(async {
            let length = 17 * 1024 * 1024;
            let frame = format!(
                "{{\"id\":2,\"result\":{{\"data\":[{{\"items\":[{{\"type\":\"agentMessage\",\"id\":\"large\",\"text\":\"{}\"}}]}}],\"nextCursor\":\"older\"}}}}\n",
                "x".repeat(length)
            );
            let mut reader = futures::io::Cursor::new(frame);
            let response = read_response(&mut reader, 2).await.unwrap();
            let page = decode_page(response, None).unwrap();
            assert_eq!(page.messages.len(), 1);
            let acp::ContentBlock::Text(text) = &page.messages[0].content[0] else {
                panic!("history text");
            };
            assert_eq!(text.text.len(), length);
            assert_eq!(page.next_cursor.as_deref(), Some("older"));
        });
    }

    #[test]
    fn restores_embedded_history_images_to_permanent_files() {
        let directory = tempfile::tempdir().expect("store");
        // A complete 1x1 PNG; the history decoder must not retain its base64 payload.
        let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aL1sAAAAASUVORK5CYII=";
        let page = decode_page_in(
            json!({"data":[{"items":[{
                "type":"userMessage","id":"attachment","content":[
                    {"type":"image","url":format!("data:image/png;base64,{data}")}
                ]
            }]}]}),
            None,
            directory.path(),
        )
        .expect("page");
        let acp::ContentBlock::ResourceLink(link) = &page.messages[0].content[0] else {
            panic!("durable link")
        };
        assert!(
            link.uri
                .contains(directory.path().to_string_lossy().as_ref())
        );
        assert_eq!(
            std::fs::read_dir(directory.path()).expect("files").count(),
            1
        );
        assert!(!format!("{:?}", page.messages).contains(data));
    }

    #[test]
    fn codex_history_rpc_errors_are_not_empty_pages() {
        futures::executor::block_on(async {
            let mut input =
                futures::io::Cursor::new(b"{\"id\":2,\"error\":{\"message\":\"not found\"}}\n");
            assert!(
                read_response(&mut input, 2)
                    .await
                    .expect_err("error")
                    .to_string()
                    .contains("not found")
            );
            let mut input = futures::io::Cursor::new(
                b"{\"method\":\"notice\"}\n{\"id\":2,\"result\":{\"data\":[]}}\n",
            );
            assert_eq!(
                read_response(&mut input, 2).await.expect("reply"),
                json!({"data":[]})
            );
        });
    }

    #[test]
    #[ignore = "Requires explicitly selected diagnostic thread and Codex executable; never prompts"]
    fn codex_history_live_read_only() {
        let executable = PathBuf::from(
            std::env::var("ZED_CODEX_HISTORY_TEST_EXE").expect("diagnostic executable"),
        );
        let session = std::env::var("ZED_CODEX_HISTORY_TEST_THREAD").expect("diagnostic thread");
        let history = CodexHistory {
            executable,
            environment: Default::default(),
            directory: None,
        };
        let page = futures::executor::block_on(history.page(acp_v2::SessionId::new(session), None))
            .expect("read-only page");
        assert!(!page.messages.is_empty());
        let usage =
            futures::executor::block_on(history.rate_limits()).expect("read-only account limits");
        assert!(usage.get("rateLimits").is_some() || usage.get("rateLimitsByLimitId").is_some());
    }

    #[test]
    fn codex_history_preserves_order_and_filters_actions() {
        let page = decode_page(
            json!({"data":[
            {"items":[{"type":"agentMessage","id":"new","text":"new"}]},
            {"items":[{"type":"userMessage","id":"old","content":[{"type":"text","text":"old"}]},
                {"type":"agentMessage","id":"hidden","phase":"commentary","text":"step"},
                {"type":"commandExecution","id":"tool"}]}
        ],"nextCursor":"older"}),
            None,
        )
        .expect("page");
        assert_eq!(
            page.messages
                .iter()
                .map(|message| message.id.to_string())
                .collect::<Vec<_>>(),
            ["old", "new"]
        );
        assert_eq!(page.next_cursor.as_deref(), Some("older"));
    }

    #[test]
    fn codex_history_rejects_invalid_pages() {
        assert!(decode_page(json!({"data":[],"nextCursor":"same"}), Some("same")).is_err());
        assert!(
            decode_page(
                json!({"data":[{"items":[{"type":"agentMessage","text":"missing ID"}]}]}),
                None
            )
            .is_err()
        );
        assert!(decode_page(json!({"data":vec![json!({"items":[]});21]}), None).is_err());
        assert!(
            decode_page(
                json!({"data":[{"items":[
            {"type":"agentMessage","id":"same","text":"a"},
            {"type":"agentMessage","id":"same","text":"b"}]}]}),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn codex_history_invalid_attachment_does_not_expose_data_url() {
        let page = decode_page(json!({"data":[{"items":[{"type":"userMessage","id":"image","content":[{"type":"image","url":"data:image/png;base64,secret"}]}]}],"nextCursor":null}), None).expect("page");
        assert!(!format!("{:?}", page.messages).contains("secret"));
    }

    #[test]
    fn codex_history_image_references_preserve_reply_identity_without_payloads() {
        let path = if cfg!(windows) {
            "C:/pictures/image.png"
        } else {
            "/pictures/image.png"
        };
        let page = decode_page(
            json!({"data":[{"items":[
                {"type":"userMessage","id":"user","content":[{"type":"localImage","path":path}]},
                {"type":"imageGeneration","id":"tool","savedPath":path,"result":"private-base64"},
                {"type":"agentMessage","id":"reply","text":"Done"}
            ]}]}),
            None,
        )
        .expect("page");
        assert_eq!(page.messages.len(), 2);
        assert_eq!(page.messages[1].id.to_string(), "reply");
        assert_eq!(page.messages[1].content.len(), 2);
        assert!(matches!(
            page.messages[0].content[0],
            acp::ContentBlock::ResourceLink(_)
        ));
        assert!(!format!("{:?}", page).contains("private-base64"));
        for invalid in [
            "https://example.com/p.png",
            "relative.png",
            "//server/share/p.png",
            "\\\\server\\share\\p.png",
            "C:/secret.txt",
        ] {
            assert!(history_image_link(invalid).is_none());
        }
    }
}
