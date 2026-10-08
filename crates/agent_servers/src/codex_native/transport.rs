//! Persistent Codex app-server JSONL transport. No Node process, shell, or auth-file reads.
use anyhow::{Context as _, Result, bail, ensure};
use futures::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, channel::oneshot, io::BufReader,
};
use gpui::{BackgroundExecutor, Task};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use util::process::Child;

const MAX_FRAME: u64 = 16 * 1024 * 1024;
const MAX_PENDING: usize = 128;
const QUEUE_SIZE: usize = 8;

pub(super) enum Event {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

#[derive(Default)]
struct Routing {
    next_id: u64,
    pending: HashMap<u64, oneshot::Sender<Result<Value>>>,
    closed: Option<String>,
}

impl Routing {
    fn register(&mut self) -> Result<(u64, oneshot::Receiver<Result<Value>>)> {
        if let Some(reason) = &self.closed {
            bail!("Codex disconnected: {reason}");
        }
        ensure!(
            self.pending.len() < MAX_PENDING,
            "Too many pending Codex requests"
        );
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("Codex request ID exhausted")?;
        let (sender, receiver) = oneshot::channel();
        self.pending.insert(self.next_id, sender);
        Ok((self.next_id, receiver))
    }

    fn close(&mut self, reason: &str) {
        self.closed.get_or_insert_with(|| reason.to_owned());
        for (_, sender) in self.pending.drain() {
            let _ = sender.send(Err(anyhow::anyhow!("Codex disconnected: {reason}")));
        }
    }

    fn route(&mut self, frame: Value) -> Result<Option<Event>> {
        if let Some(method) = frame.get("method").and_then(Value::as_str) {
            let params = frame.get("params").cloned().unwrap_or(Value::Null);
            return Ok(Some(match frame.get("id") {
                Some(id) if id.is_string() || id.is_number() => Event::Request {
                    id: id.clone(),
                    method: method.to_owned(),
                    params,
                },
                Some(_) => bail!("Invalid Codex server request ID"),
                None => Event::Notification {
                    method: method.to_owned(),
                    params,
                },
            }));
        }
        let id = frame
            .get("id")
            .and_then(Value::as_u64)
            .context("Invalid Codex response ID")?;
        let result = match (frame.get("result"), frame.get("error")) {
            (Some(result), None) => Ok(result.clone()),
            (None, Some(error)) => Err(anyhow::anyhow!(
                "Codex RPC error {}: {}",
                error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown error")
            )),
            _ => bail!("Codex response must have exactly one result or error"),
        };
        // A timed-out/cancelled request can still reply. Never assign it to another request.
        if let Some(sender) = self.pending.remove(&id) {
            let _ = sender.send(result);
        }
        Ok(None)
    }
}

struct Pending {
    routing: Arc<Mutex<Routing>>,
    id: u64,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Ok(mut routing) = self.routing.lock() {
            routing.pending.remove(&self.id);
        }
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if let Err(error) = self.0.kill() {
            log::debug!("Codex process stop: {error}");
        }
    }
}

struct Inner {
    outgoing: async_channel::Sender<Vec<u8>>,
    routing: Arc<Mutex<Routing>>,
    executor: BackgroundExecutor,
    _pump: Task<()>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.outgoing.close();
        if let Ok(mut routing) = self.routing.lock() {
            routing.close("Connection closed");
        }
    }
}

#[derive(Clone)]
pub(super) struct Rpc(Arc<Inner>);

impl Rpc {
    #[cfg(test)]
    pub(super) fn fake(
        executor: BackgroundExecutor,
    ) -> (Self, FakePeer, async_channel::Receiver<Event>) {
        let (outgoing, requests) = async_channel::bounded(QUEUE_SIZE);
        let (events, receiver) = async_channel::bounded(QUEUE_SIZE);
        let routing = Arc::new(Mutex::new(Routing::default()));
        let peer = FakePeer {
            requests,
            events,
            routing: routing.clone(),
        };
        (
            Self(Arc::new(Inner {
                outgoing,
                routing,
                executor,
                _pump: Task::ready(()),
            })),
            peer,
            receiver,
        )
    }

    pub(super) fn start(
        executable: PathBuf,
        environment: collections::HashMap<String, String>,
        directory: Option<PathBuf>,
        executor: BackgroundExecutor,
    ) -> (Self, async_channel::Receiver<Event>) {
        let (outgoing, incoming) = async_channel::bounded::<Vec<u8>>(QUEUE_SIZE);
        let (events, receiver) = async_channel::bounded(QUEUE_SIZE);
        let routing = Arc::new(Mutex::new(Routing::default()));
        let task_routing = routing.clone();
        let pump = executor.spawn(async move {
            let result: Result<()> = async {
                ensure!(
                    executable.is_absolute() && executable.is_file(),
                    "CODEX_PATH must point to the configured executable"
                );
                let mut command = std::process::Command::new(executable);
                command
                    .args(["app-server", "--listen", "stdio://"])
                    .envs(environment);
                if let Some(directory) = directory {
                    command.current_dir(directory);
                }
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt as _;
                    command.creation_flags(0x08000000);
                }
                // Do not retain raw stderr: it may contain prompts, tool output or credentials.
                let mut process = Process(Child::spawn(
                    command,
                    Stdio::piped(),
                    Stdio::piped(),
                    Stdio::null(),
                )?);
                let mut input = process.0.stdin.take().context("Codex stdin unavailable")?;
                let mut output = BufReader::new(
                    process
                        .0
                        .stdout
                        .take()
                        .context("Codex stdout unavailable")?,
                );
                let read = async {
                    loop {
                        let frame = read_frame(&mut output).await?;
                        let event = task_routing
                            .lock()
                            .map_err(|_| anyhow::anyhow!("Codex routing lock poisoned"))?
                            .route(frame)?;
                        if let Some(event) = event {
                            events
                                .send(event)
                                .await
                                .context("Codex event consumer closed")?;
                        }
                    }
                    #[allow(unreachable_code)]
                    anyhow::Ok(())
                };
                let write = async {
                    while let Ok(frame) = incoming.recv().await {
                        input.write_all(&frame).await?;
                        input.flush().await?;
                    }
                    bail!("Codex command queue closed")
                };
                // Keep the reader alive while writing. Cancelling read_until mid-frame loses bytes.
                let outcome = match futures::future::select(Box::pin(read), Box::pin(write)).await {
                    futures::future::Either::Left((result, _)) => result,
                    futures::future::Either::Right((result, _)) => result,
                };
                if let Err(error) = process.0.kill() {
                    log::debug!("Codex process stop: {error}");
                }
                process
                    .0
                    .status()
                    .await
                    .context("Could not reap Codex app-server")?;
                outcome
            }
            .await;
            let reason = result
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "Codex exited".into());
            if let Ok(mut routing) = task_routing.lock() {
                routing.close(&reason);
            }
            // Dropping the sender wakes the foreground consumer even if the queue was full.
            drop(events);
        });
        (
            Self(Arc::new(Inner {
                outgoing,
                routing,
                executor,
                _pump: pump,
            })),
            receiver,
        )
    }

    pub(super) async fn initialize(&self) -> Result<Value> {
        let result = self
            .request(
                "initialize",
                json!({
                    "clientInfo":{"name":"zed_local_native","version":env!("CARGO_PKG_VERSION")},
                    "capabilities":{"experimentalApi":true}
                }),
            )
            .await?;
        self.send(json!({"method":"initialized","params":{}}))?;
        Ok(result)
    }

    pub(super) async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let (id, receiver) = self
            .0
            .routing
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex routing lock poisoned"))?
            .register()?;
        let _pending = Pending {
            routing: self.0.routing.clone(),
            id,
        };
        self.send(json!({"id":id,"method":method,"params":params}))?;
        match futures::future::select(
            Box::pin(receiver),
            Box::pin(self.0.executor.timer(Duration::from_secs(60))),
        )
        .await
        {
            futures::future::Either::Left((result, _)) => {
                result.context("Codex response channel closed")?
            }
            // Never retry writes automatically: a timeout does not prove the server rejected them.
            futures::future::Either::Right(_) => bail!(
                "Codex {method} timed out; server outcome is unknown. Request was not replayed."
            ),
        }
    }

    pub(super) fn respond(&self, id: Value, result: Result<Value>) -> Result<()> {
        self.send(match result {
            Ok(result) => json!({"id":id,"result":result}),
            Err(error) => json!({"id":id,"error":{"code":-32603,"message":error.to_string()}}),
        })
    }

    fn send(&self, frame: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&frame)?;
        ensure!(
            (bytes.len() as u64) < MAX_FRAME,
            "Codex request exceeds 16 MiB"
        );
        bytes.push(b'\n');
        self.0
            .outgoing
            .try_send(bytes)
            .context("Codex command queue is full or closed")
    }
}

#[cfg(test)]
pub(super) struct FakePeer {
    requests: async_channel::Receiver<Vec<u8>>,
    events: async_channel::Sender<Event>,
    routing: Arc<Mutex<Routing>>,
}

#[cfg(test)]
impl FakePeer {
    pub(super) async fn next(&self) -> Value {
        serde_json::from_slice(&self.requests.recv().await.unwrap()).unwrap()
    }
    pub(super) async fn send(&self, value: Value) {
        let event = self.routing.lock().unwrap().route(value).unwrap();
        if let Some(event) = event {
            self.events.send(event).await.unwrap();
        }
    }
    pub(super) fn disconnect(&self) {
        self.routing.lock().unwrap().close("Test disconnect");
        self.events.close();
    }
}

async fn read_frame(reader: &mut (impl futures::AsyncBufRead + Unpin)) -> Result<Value> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_FRAME + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    ensure!(!bytes.is_empty(), "Codex closed stdout");
    ensure!(
        bytes.len() as u64 <= MAX_FRAME,
        "Codex frame exceeds 16 MiB"
    );
    ensure!(bytes.last() == Some(&b'\n'), "Truncated Codex frame");
    serde_json::from_slice(&bytes).context("Invalid Codex JSONL frame")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    #[ignore = "Explicit runtime and empty diagnostic cwd required; sends one short model request"]
    fn live_ephemeral_turn(cx: &mut gpui::TestAppContext) {
        use futures::FutureExt as _;
        cx.background_executor.allow_parking();
        let executable =
            PathBuf::from(std::env::var("ZED_CODEX_HISTORY_TEST_EXE").expect("runtime"));
        let directory = PathBuf::from(
            std::env::var("ZED_CODEX_SMOKE_CWD").expect("empty diagnostic directory"),
        );
        let (rpc, events) = Rpc::start(
            executable,
            Default::default(),
            Some(directory.clone()),
            cx.background_executor.clone(),
        );
        let operation = cx.background_executor.spawn(async move {
            rpc.initialize().await.unwrap();
            let session = rpc.request("thread/start", json!({"cwd":directory,"ephemeral":true,"sandbox":"read-only","approvalPolicy":"on-request","baseInstructions":"This is a transport test. Follow the user instruction without using tools."})).await.unwrap();
            let id = session["thread"]["id"].as_str().unwrap().to_owned();
            let blocks: Vec<agent_client_protocol::schema::v1::ContentBlock> = serde_json::from_value(json!([
                {"type":"text","text":"Reply with the attached marker only. Do not use tools or access files."},
                {"type":"resource","resource":{"uri":"diagnostic://marker","text":"NATIVE_SMOKE_OK"}}
            ])).unwrap();
            let input = super::super::events::input(&blocks).unwrap();
            rpc.request("turn/start", json!({"threadId":id,"input":input})).await.unwrap();
            let mut translated = super::super::events::TurnEvents::default();
            let mut answer = String::new();
            let result: Result<()> = async {
                while let Ok(event) = events.recv().await {
                    match event {
                        Event::Request { id, .. } => rpc.respond(id, Err(anyhow::anyhow!("Diagnostic request rejected")))?,
                        Event::Notification { method, params } if params["threadId"] == id => {
                            if method == "turn/completed" {
                                ensure!(params["turn"]["status"] == "completed", "Diagnostic turn did not complete successfully");
                                ensure!(answer.contains("NATIVE_SMOKE_OK"), "Marker missing from native streamed answer");
                                return Ok(());
                            }
                            for update in translated.update(&method, &params)? {
                                let value = serde_json::to_value(update)?;
                                if value["sessionUpdate"] == "agent_message_chunk" {
                                    if let Some(chunk) = value["content"]["text"].as_str() { answer.push_str(chunk); }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                bail!("Native diagnostic disconnected before completion")
            }.await;
            rpc.request("thread/unsubscribe", json!({"threadId":id})).await.unwrap();
            result.unwrap();
        });
        futures::pin_mut!(operation);
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while operation.as_mut().now_or_never().is_none() {
            while cx.background_executor.tick() {}
            assert!(
                std::time::Instant::now() < deadline,
                "Live native turn exceeded 120 seconds"
            );
            std::thread::yield_now();
        }
    }
    #[gpui::test]
    #[ignore = "Explicit local runtime and diagnostic thread required; no model request"]
    fn live_read_only(cx: &mut gpui::TestAppContext) {
        use futures::FutureExt as _;
        cx.background_executor.allow_parking();
        let executable = PathBuf::from(
            std::env::var("ZED_CODEX_HISTORY_TEST_EXE").expect("diagnostic executable"),
        );
        let thread = std::env::var("ZED_CODEX_HISTORY_TEST_THREAD").expect("diagnostic thread");
        let (rpc, events) = Rpc::start(
            executable,
            Default::default(),
            None,
            cx.background_executor.clone(),
        );
        let responder = rpc.clone();
        let drain = cx.background_executor.spawn(async move {
            while let Ok(event) = events.recv().await {
                if let Event::Request { id, .. } = event {
                    responder
                        .respond(
                            id,
                            Err(anyhow::anyhow!("Read-only diagnostic; request rejected")),
                        )
                        .unwrap();
                }
            }
        });
        let operation = cx.background_executor.spawn(async move {
        let initialized = rpc.initialize().await.expect("initialize installed runtime");
        assert!(initialized["codexHome"].is_string());
        let (history, models, usage) = futures::join!(
            rpc.request("thread/turns/list", json!({"threadId":thread,"limit":20,"sortDirection":"desc","itemsView":"summary"})),
            rpc.request("model/list", json!({"limit":100})),
            rpc.request("account/rateLimits/read", json!({}))
        );
        assert!(!crate::codex_history::decode_page(history.unwrap(), None).unwrap().messages.is_empty());
        let models = models.unwrap();
        let catalog: Vec<crate::codex_native::settings::Model> = serde_json::from_value(models["data"].clone()).expect("installed model catalog schema");
        assert!(!catalog.is_empty());
        let usage = usage.unwrap();
        assert!(usage.get("rateLimits").is_some() || usage.get("rateLimitsByLimitId").is_some());
        drop(drain);
        });
        futures::pin_mut!(operation);
        // Real child IO must not auto-advance the simulated GPUI clock to the RPC deadline.
        // tick() executes runnable tasks without advancing time; the external probe has its
        // own wall-clock deadline. Normal tests use virtual timers, without this loop.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while operation.as_mut().now_or_never().is_none() {
            while cx.background_executor.tick() {}
            assert!(
                std::time::Instant::now() < deadline,
                "Live Codex probe exceeded 30 seconds"
            );
            std::thread::yield_now();
        }
    }
    #[test]
    fn routes_out_of_order_responses_and_server_requests() {
        futures::executor::block_on(async {
            let mut routing = Routing::default();
            let (one, first) = routing.register().unwrap();
            let (two, second) = routing.register().unwrap();
            assert!(matches!(routing.route(json!({"id":"approval-1","method":"item/commandExecution/requestApproval","params":{}})).unwrap(), Some(Event::Request { .. })));
            routing.route(json!({"id":two,"result":2})).unwrap();
            routing.route(json!({"id":one,"result":1})).unwrap();
            assert_eq!(first.await.unwrap().unwrap(), 1);
            assert_eq!(second.await.unwrap().unwrap(), 2);
            assert!(routing.pending.is_empty());
        });
    }
    #[test]
    fn disconnect_fails_every_pending_request_and_rejects_new_requests() {
        futures::executor::block_on(async {
            let mut routing = Routing::default();
            let (_, first) = routing.register().unwrap();
            let (_, second) = routing.register().unwrap();
            routing.close("test exit");
            assert!(first.await.unwrap().is_err());
            assert!(second.await.unwrap().is_err());
            assert!(routing.register().is_err());
        });
    }
    #[test]
    fn cancellation_removes_pending_slot_and_late_response_is_safe() {
        let routing = Arc::new(Mutex::new(Routing::default()));
        let (id, _receiver) = routing.lock().unwrap().register().unwrap();
        drop(Pending {
            routing: routing.clone(),
            id,
        });
        let mut routing = routing.lock().unwrap();
        assert!(routing.pending.is_empty());
        assert!(
            routing
                .route(json!({"id":id,"result":null}))
                .unwrap()
                .is_none()
        );
        assert!(
            routing
                .route(json!({"id":id,"result":null,"error":{}}))
                .is_err()
        );
    }
    #[test]
    fn pending_requests_are_bounded() {
        let mut routing = Routing::default();
        let mut receivers = Vec::new();
        for _ in 0..MAX_PENDING {
            receivers.push(routing.register().unwrap());
        }
        assert!(routing.register().is_err());
        routing.close("done");
        assert!(routing.pending.is_empty());
    }
    #[test]
    fn validates_framing_without_logging_payloads() {
        futures::executor::block_on(async {
            for frame in [b"{\"id\":1}".as_slice(), b"not-json\n", b""] {
                assert!(
                    read_frame(&mut futures::io::Cursor::new(frame))
                        .await
                        .is_err()
                );
            }
            assert_eq!(
                read_frame(&mut futures::io::Cursor::new(b"{\"id\":1}\n"))
                    .await
                    .unwrap()["id"],
                1
            );
            let huge = vec![b'x'; MAX_FRAME as usize + 1];
            assert!(
                read_frame(&mut futures::io::Cursor::new(huge))
                    .await
                    .is_err()
            );
        });
    }
}
