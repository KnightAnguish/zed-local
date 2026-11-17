//! Per-thread queued follow-up prompts, persisted so they survive reloads.

use anyhow::Context as _;
use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Task};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

use crate::thread_metadata_store::ThreadId;

const NAMESPACE: &str = "agent_queued_prompts";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedPrompt {
    pub content: Vec<agent_client_protocol::v2::schema::ContentBlock>,
    pub steer: bool,
}

pub fn read(thread_id: ThreadId, cx: &App) -> Option<Vec<QueuedPrompt>> {
    let kvp = KeyValueStore::global(cx);
    let raw = kvp
        .scoped(NAMESPACE)
        .read(&thread_id.to_key_string())
        .log_err()
        .flatten()?;
    serde_json::from_str(&raw).log_err()
}

pub fn write(
    thread_id: ThreadId,
    prompts: &[QueuedPrompt],
    cx: &App,
) -> Task<anyhow::Result<()>> {
    let kvp = KeyValueStore::global(cx);
    let key = thread_id.to_key_string();
    let payload = match serde_json::to_string(prompts).context("serializing queued prompts") {
        Ok(payload) => payload,
        Err(error) => return Task::ready(Err(error)),
    };
    cx.background_spawn(async move { kvp.scoped(NAMESPACE).write(key, payload).await })
}

pub fn delete(thread_id: ThreadId, cx: &App) -> Task<anyhow::Result<()>> {
    let kvp = KeyValueStore::global(cx);
    let key = thread_id.to_key_string();
    cx.background_spawn(async move { kvp.scoped(NAMESPACE).delete(key).await })
}
