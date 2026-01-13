use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Duration, timeout};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TurnOutput {
    pub output: String,
    pub turn_id: String,
}

pub struct AppServerClient {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    events: Mutex<mpsc::UnboundedReceiver<Value>>,
    next_id: AtomicU64,
}

impl AppServerClient {
    pub async fn spawn(codex_bin: &str) -> Result<Self> {
        let mut command = Command::new(codex_bin);
        command.arg("app-server");
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());

        let mut child = command.spawn().map_err(|e| anyhow!(e.to_string()))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("missing stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("missing stderr"))?;

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = Arc::clone(&pending);

        let event_tx_out = event_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let parsed: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(_) => {
                        let _ = event_tx_out.send(json!({
                            "method": "codex/parseError",
                            "params": { "raw": line }
                        }));
                        continue;
                    }
                };
                let maybe_id = parsed.get("id").and_then(|id| id.as_u64());
                let has_method = parsed.get("method").is_some();
                let has_result_or_error =
                    parsed.get("result").is_some() || parsed.get("error").is_some();
                if let Some(id) = maybe_id {
                    if has_result_or_error {
                        if let Some(tx) = pending_clone.lock().await.remove(&id) {
                            let _ = tx.send(parsed);
                        }
                    } else if has_method {
                        let _ = event_tx_out.send(parsed);
                    } else if let Some(tx) = pending_clone.lock().await.remove(&id) {
                        let _ = tx.send(parsed);
                    }
                } else if has_method {
                    let _ = event_tx_out.send(parsed);
                }
            }
        });

        let event_tx_err = event_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = event_tx_err.send(json!({
                    "method": "codex/stderr",
                    "params": { "message": line }
                }));
            }
        });

        Ok(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending,
            events: Mutex::new(event_rx),
            next_id: AtomicU64::new(1),
        })
    }

    pub async fn initialize(&self) -> Result<()> {
        let init_params = json!({
            "clientInfo": {
                "name": "codex_subagent",
                "title": "codex_subagent",
                "version": "0.1.0"
            }
        });
        let response = timeout(
            Duration::from_secs(15),
            self.send_request("initialize", init_params),
        )
        .await
        .map_err(|_| anyhow!("codex app-server did not respond to initialize"))??;
        if response.get("error").is_some() {
            return Err(anyhow!("initialize failed: {}", response));
        }
        self.send_notification("initialized", None).await?;
        Ok(())
    }

    pub async fn start_thread(
        &self,
        cwd: &Path,
        base_instructions: Option<&str>,
        model: Option<&str>,
        approval_policy: Option<&str>,
        sandbox: Option<&str>,
    ) -> Result<String> {
        let mut params = serde_json::Map::new();
        params.insert("cwd".to_string(), json!(cwd));
        if let Some(instructions) = base_instructions {
            if !instructions.trim().is_empty() {
                params.insert("baseInstructions".to_string(), json!(instructions));
            }
        }
        if let Some(model) = model {
            params.insert("model".to_string(), json!(model));
        }
        if let Some(policy) = approval_policy {
            params.insert("approvalPolicy".to_string(), json!(policy));
        }
        if let Some(sandbox) = sandbox {
            params.insert("sandbox".to_string(), json!(sandbox));
        }
        let response = self
            .send_request("thread/start", Value::Object(params))
            .await?;
        if let Some(error) = response.get("error") {
            return Err(anyhow!("thread/start failed: {}", error));
        }
        let thread_id = response
            .get("result")
            .and_then(|result| result.get("thread"))
            .and_then(|thread| thread.get("id"))
            .and_then(|id| id.as_str())
            .ok_or_else(|| anyhow!("thread/start missing thread id"))?;
        Ok(thread_id.to_string())
    }

    pub async fn run_turn(
        &self,
        thread_id: &str,
        task: &str,
        cwd: &Path,
        approval_policy: Option<&str>,
        sandbox_policy: Option<Value>,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<TurnOutput> {
        let mut params = serde_json::Map::new();
        params.insert("threadId".to_string(), json!(thread_id));
        params.insert(
            "input".to_string(),
            json!([{ "type": "text", "text": task }]),
        );
        params.insert("cwd".to_string(), json!(cwd));
        if let Some(policy) = approval_policy {
            params.insert("approvalPolicy".to_string(), json!(policy));
        }
        if let Some(policy) = sandbox_policy {
            params.insert("sandboxPolicy".to_string(), policy);
        }
        if let Some(model) = model {
            params.insert("model".to_string(), json!(model));
        }
        if let Some(effort) = effort {
            params.insert("effort".to_string(), json!(effort));
        }
        let response = self
            .send_request("turn/start", Value::Object(params))
            .await?;
        if let Some(error) = response.get("error") {
            return Err(anyhow!("turn/start failed: {}", error));
        }
        let turn_id = response
            .get("result")
            .and_then(|result| result.get("turn"))
            .and_then(|turn| turn.get("id"))
            .and_then(|id| id.as_str())
            .ok_or_else(|| anyhow!("turn/start missing turn id"))?
            .to_string();

        let mut last_message = String::new();
        let mut buffer = String::new();
        loop {
            let next = self
                .next_event()
                .await
                .ok_or_else(|| anyhow!("event stream ended"))?;
            let method = next
                .get("method")
                .and_then(|value| value.as_str())
                .unwrap_or("");

            match method {
                "item/agentMessage/delta" => {
                    if let Some(delta) = next
                        .get("params")
                        .and_then(|params| params.get("delta"))
                        .and_then(|delta| delta.as_str())
                    {
                        buffer.push_str(delta);
                    }
                }
                "item/completed" => {
                    let item = next.get("params").and_then(|params| params.get("item"));
                    if let Some(item) = item {
                        if item.get("type").and_then(|v| v.as_str()) == Some("agentMessage") {
                            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                last_message = text.to_string();
                            }
                        }
                    }
                }
                "turn/completed" => {
                    let event_turn_id = next
                        .get("params")
                        .and_then(|params| params.get("turn"))
                        .and_then(|turn| turn.get("id"))
                        .and_then(|id| id.as_str())
                        .unwrap_or("");
                    if event_turn_id == turn_id {
                        break;
                    }
                }
                _ => {}
            }
        }

        let output = if !last_message.trim().is_empty() {
            last_message
        } else {
            buffer
        };
        Ok(TurnOutput { output, turn_id })
    }

    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.write_message(json!({ "id": id, "method": method, "params": params }))
            .await?;
        rx.await.map_err(|_| anyhow!("request canceled"))
    }

    async fn send_notification(&self, method: &str, params: Option<Value>) -> Result<()> {
        let payload = if let Some(params) = params {
            json!({ "method": method, "params": params })
        } else {
            json!({ "method": method })
        };
        self.write_message(payload).await
    }

    async fn write_message(&self, value: Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        let mut line = serde_json::to_string(&value)?;
        line.push('\n');
        stdin.write_all(line.as_bytes()).await?;
        Ok(())
    }

    async fn next_event(&self) -> Option<Value> {
        let mut events = self.events.lock().await;
        events.recv().await
    }
}
