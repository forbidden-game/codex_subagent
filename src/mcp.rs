use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::subagent::{SubagentArgs, SubagentExecutor, summarize_results};

const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct McpServer {
    executor: SubagentExecutor,
}

impl McpServer {
    pub fn new(executor: SubagentExecutor) -> Self {
        Self { executor }
    }

    pub async fn run(&self) -> Result<()> {
        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        let mut stdout = tokio::io::stdout();

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let parsed: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let method = parsed.get("method").and_then(|v| v.as_str());
            let id = parsed.get("id").cloned();
            if method.is_none() {
                continue;
            }
            let method = method.unwrap();
            if let Some(id) = id {
                let result = self.handle_request(method, parsed.get("params")).await;
                let response = match result {
                    Ok(result) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result
                    }),
                    Err(err) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": err.to_string()
                        }
                    }),
                };
                let mut line = serde_json::to_string(&response)?;
                line.push('\n');
                stdout.write_all(line.as_bytes()).await?;
                stdout.flush().await?;
            } else {
                // Notification; ignore for now.
                continue;
            }
        }
        Ok(())
    }

    async fn handle_request(&self, method: &str, params: Option<&Value>) -> Result<Value> {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": {
                    "name": "codex_subagent",
                    "version": "0.1.0"
                },
                "capabilities": {
                    "tools": {}
                }
            })),
            "tools/list" => Ok(json!({
                "tools": [subagent_tool_definition()]
            })),
            "tools/call" => self.handle_tool_call(params).await,
            "ping" => Ok(json!({})),
            _ => Err(anyhow!("unsupported method: {method}")),
        }
    }

    async fn handle_tool_call(&self, params: Option<&Value>) -> Result<Value> {
        let params = params.ok_or_else(|| anyhow!("missing tool params"))?;
        let name = params
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow!("missing tool name"))?;
        if name != "subagent" {
            return Err(anyhow!("unknown tool: {name}"));
        }
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let args: SubagentArgs =
            serde_json::from_value(arguments).map_err(|err| anyhow!("invalid arguments: {err}"))?;
        let result = self.executor.execute(args).await?;
        let summary = summarize_results(&result);
        Ok(json!({
            "content": [
                { "type": "text", "text": summary }
            ],
            "structuredContent": serde_json::to_value(result).ok()
        }))
    }
}

fn subagent_tool_definition() -> Value {
    json!({
        "name": "subagent",
        "title": "Subagent",
        "description": "Run subagents in isolated Codex app-server processes. Supports auto-planning, parallel, and chain modes. Agents are loaded from ~/.pi/agent/agents and .pi/agents.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["auto", "explicit"],
                    "default": "auto",
                    "description": "auto = built-in workflow mapping (or single worker), explicit = provide tasks/chain/single."
                },
                "workflow": {
                    "type": "string",
                    "description": "Workflow preset name (from ~/.pi/agent/prompts or .pi/prompts)."
                },
                "agent": { "type": "string" },
                "task": { "type": "string" },
                "tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["agent", "task"],
                        "properties": {
                            "agent": { "type": "string" },
                            "task": { "type": "string" },
                            "cwd": { "type": "string" }
                        }
                    }
                },
                "chain": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["agent", "task"],
                        "properties": {
                            "agent": { "type": "string" },
                            "task": { "type": "string" },
                            "cwd": { "type": "string" }
                        }
                    }
                },
                "agentScope": {
                    "type": "string",
                    "enum": ["user", "project", "both"],
                    "default": "user"
                },
                "confirmProjectAgents": {
                    "type": "boolean",
                    "default": true,
                    "description": "If true, refuse project agents unless explicitly disabled."
                },
                "cwd": {
                    "type": "string",
                    "description": "Default working directory for subagents."
                },
                "approvalPolicy": {
                    "type": "string",
                    "enum": ["untrusted", "on-failure", "on-request", "never"],
                    "default": "on-request"
                },
                "sandboxMode": {
                    "type": "string",
                    "enum": ["read-only", "workspace-write", "danger-full-access"],
                    "default": "workspace-write"
                },
                "plannerAgent": {
                    "type": "string",
                    "description": "Deprecated; auto mode does not invoke a planner."
                }
            }
        }
    })
}
