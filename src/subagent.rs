use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::agent::{
    AgentConfig, AgentScope, AgentSource, discover_agents, format_agent_list, parse_agent_scope,
};
use crate::app_server::AppServerClient;
use crate::prompts::find_prompt;

const DEFAULT_MAX_WORKERS: usize = 10;

#[derive(Debug, Deserialize, Clone)]
pub struct TaskItem {
    pub agent: String,
    pub task: String,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SubagentArgs {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub tasks: Option<Vec<TaskItem>>,
    #[serde(default)]
    pub chain: Option<Vec<TaskItem>>,
    #[serde(rename = "agentScope", default)]
    pub agent_scope: Option<String>,
    #[serde(rename = "confirmProjectAgents", default)]
    pub confirm_project_agents: Option<bool>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(rename = "approvalPolicy", default)]
    pub approval_policy: Option<String>,
    #[serde(rename = "sandboxMode", default)]
    pub sandbox_mode: Option<String>,
    #[serde(rename = "plannerAgent", default)]
    pub planner_agent: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct AgentExecutionResult {
    pub agent: String,
    pub task: String,
    pub cwd: String,
    pub status: String,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct SubagentResult {
    pub mode: String,
    pub results: Vec<AgentExecutionResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<Value>,
}

pub struct SubagentExecutor {
    pub codex_bin: String,
    pub max_workers: usize,
}

impl SubagentExecutor {
    pub fn new(codex_bin: String, max_workers: usize) -> Self {
        Self {
            codex_bin,
            max_workers: if max_workers == 0 {
                DEFAULT_MAX_WORKERS
            } else {
                max_workers
            },
        }
    }

    pub async fn execute(&self, args: SubagentArgs) -> Result<SubagentResult> {
        let mode = args.mode.clone().unwrap_or_else(|| "auto".to_string());
        let base_cwd = args
            .cwd
            .clone()
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?);
        let scope = parse_agent_scope(args.agent_scope.as_deref());
        let confirm_project_agents = args.confirm_project_agents.unwrap_or(true);
        let discovery = discover_agents(&base_cwd, scope);

        if discovery.agents.is_empty() {
            return Err(anyhow!(
                "no agents found. Check ~/.pi/agent/agents or .pi/agents"
            ));
        }

        let agent_map = discovery
            .agents
            .iter()
            .map(|agent| (agent.name.clone(), agent.clone()))
            .collect::<std::collections::HashMap<_, _>>();

        let approval_policy = args.approval_policy.as_deref().unwrap_or("on-request");
        let sandbox_mode = args.sandbox_mode.as_deref().unwrap_or("workspace-write");

        let explicit = args.tasks.is_some()
            || args.chain.is_some()
            || (args.agent.is_some() && args.task.is_some());
        let use_auto = mode == "auto" || !explicit;

        if use_auto {
            let overall_task = args
                .task
                .clone()
                .ok_or_else(|| anyhow!("auto mode requires `task`"))?;
            let plan = self
                .plan_tasks(
                    &base_cwd,
                    &agent_map,
                    approval_policy,
                    sandbox_mode,
                    args.workflow.as_deref(),
                    args.planner_agent.as_deref(),
                    overall_task,
                    scope,
                )
                .await?;
            return self
                .execute_plan(
                    &base_cwd,
                    &agent_map,
                    approval_policy,
                    sandbox_mode,
                    confirm_project_agents,
                    Some(plan.clone()),
                    plan,
                )
                .await;
        }

        if let Some(chain) = args.chain {
            let results = self
                .run_chain(
                    &base_cwd,
                    &agent_map,
                    approval_policy,
                    sandbox_mode,
                    confirm_project_agents,
                    &chain,
                )
                .await?;
            return Ok(SubagentResult {
                mode: "chain".to_string(),
                results,
                plan: None,
            });
        }

        if let Some(tasks) = args.tasks {
            let results = self
                .run_parallel(
                    &base_cwd,
                    &agent_map,
                    approval_policy,
                    sandbox_mode,
                    confirm_project_agents,
                    &tasks,
                )
                .await?;
            return Ok(SubagentResult {
                mode: "parallel".to_string(),
                results,
                plan: None,
            });
        }

        if let (Some(agent), Some(task)) = (args.agent, args.task) {
            let cwd = args
                .cwd
                .clone()
                .unwrap_or_else(|| base_cwd.display().to_string());
            let result = self
                .run_single(
                    &base_cwd,
                    &agent_map,
                    approval_policy,
                    sandbox_mode,
                    confirm_project_agents,
                    &TaskItem {
                        agent,
                        task,
                        cwd: Some(cwd),
                    },
                )
                .await?;
            return Ok(SubagentResult {
                mode: "single".to_string(),
                results: vec![result],
                plan: None,
            });
        }

        Err(anyhow!(
            "explicit mode requires one of: agent+task, tasks[], or chain[]"
        ))
    }

    async fn plan_tasks(
        &self,
        cwd: &Path,
        agent_map: &std::collections::HashMap<String, AgentConfig>,
        approval_policy: &str,
        sandbox_mode: &str,
        workflow: Option<&str>,
        planner_agent: Option<&str>,
        task: String,
        scope: AgentScope,
    ) -> Result<Value> {
        let planner_name = planner_agent.unwrap_or("planner");
        let planner = agent_map
            .get(planner_name)
            .ok_or_else(|| anyhow!("planner agent not found: {planner_name}"))?;
        let prompt = workflow
            .and_then(|name| find_prompt(cwd, scope, name))
            .map(|prompt| prompt.body);

        let mut instruction = String::new();
        instruction.push_str("You are a planning agent.\n");
        instruction.push_str("Create a subagent execution plan in JSON.\n");
        instruction.push_str("Output ONLY valid JSON with no extra text.\n");
        instruction.push_str("Max tasks: 10. Use only available agents.\n");
        instruction.push_str("Format options:\n");
        instruction.push_str(
            "{\"mode\":\"single\",\"agent\":\"name\",\"task\":\"...\",\"cwd\":\"...\"}\n",
        );
        instruction.push_str("{\"mode\":\"parallel\",\"tasks\":[{\"agent\":\"...\",\"task\":\"...\",\"cwd\":\"...\"}]}\n");
        instruction.push_str("{\"mode\":\"chain\",\"chain\":[{\"agent\":\"...\",\"task\":\"...\",\"cwd\":\"...\"}]}\n");
        if let Some(prompt) = prompt.as_deref() {
            instruction.push_str("\nWorkflow preset instructions:\n");
            instruction.push_str(prompt);
            instruction.push('\n');
        }

        let available = agent_map
            .values()
            .map(|agent| format!("- {}: {}", agent.name, agent.description))
            .collect::<Vec<_>>()
            .join("\n");

        let planner_task =
            format!("{instruction}\nAvailable agents:\n{available}\n\nUser goal:\n{task}");

        let result = self
            .run_agent_task(
                cwd,
                planner,
                &planner_task,
                approval_policy,
                sandbox_mode,
                None,
            )
            .await?;

        let json_value = parse_json_payload(&result.output)?;
        validate_plan(&json_value)?;
        Ok(json_value)
    }

    async fn execute_plan(
        &self,
        base_cwd: &Path,
        agent_map: &std::collections::HashMap<String, AgentConfig>,
        approval_policy: &str,
        sandbox_mode: &str,
        confirm_project_agents: bool,
        plan_json: Option<Value>,
        plan: Value,
    ) -> Result<SubagentResult> {
        let mode = plan
            .get("mode")
            .and_then(|value| value.as_str())
            .unwrap_or("parallel");
        match mode {
            "single" => {
                let agent = plan
                    .get("agent")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| anyhow!("plan missing agent"))?;
                let task = plan
                    .get("task")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| anyhow!("plan missing task"))?;
                let cwd = plan
                    .get("cwd")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string());
                let result = self
                    .run_single(
                        base_cwd,
                        agent_map,
                        approval_policy,
                        sandbox_mode,
                        confirm_project_agents,
                        &TaskItem {
                            agent: agent.to_string(),
                            task: task.to_string(),
                            cwd,
                        },
                    )
                    .await?;
                Ok(SubagentResult {
                    mode: "single".to_string(),
                    results: vec![result],
                    plan: plan_json,
                })
            }
            "chain" => {
                let chain = plan
                    .get("chain")
                    .and_then(|value| value.as_array())
                    .ok_or_else(|| anyhow!("plan missing chain"))?;
                let chain_items = chain
                    .iter()
                    .map(parse_task_item)
                    .collect::<Result<Vec<_>>>()?;
                let results = self
                    .run_chain(
                        base_cwd,
                        agent_map,
                        approval_policy,
                        sandbox_mode,
                        confirm_project_agents,
                        &chain_items,
                    )
                    .await?;
                Ok(SubagentResult {
                    mode: "chain".to_string(),
                    results,
                    plan: plan_json,
                })
            }
            _ => {
                let tasks = plan
                    .get("tasks")
                    .and_then(|value| value.as_array())
                    .ok_or_else(|| anyhow!("plan missing tasks"))?;
                let task_items = tasks
                    .iter()
                    .map(parse_task_item)
                    .collect::<Result<Vec<_>>>()?;
                let results = self
                    .run_parallel(
                        base_cwd,
                        agent_map,
                        approval_policy,
                        sandbox_mode,
                        confirm_project_agents,
                        &task_items,
                    )
                    .await?;
                Ok(SubagentResult {
                    mode: "parallel".to_string(),
                    results,
                    plan: plan_json,
                })
            }
        }
    }

    async fn run_parallel(
        &self,
        base_cwd: &Path,
        agent_map: &std::collections::HashMap<String, AgentConfig>,
        approval_policy: &str,
        sandbox_mode: &str,
        confirm_project_agents: bool,
        tasks: &[TaskItem],
    ) -> Result<Vec<AgentExecutionResult>> {
        if tasks.len() > self.max_workers {
            return Err(anyhow!(
                "too many tasks ({}). max is {}",
                tasks.len(),
                self.max_workers
            ));
        }
        let semaphore = Arc::new(Semaphore::new(self.max_workers));
        let mut handles = Vec::new();
        for task in tasks.iter().cloned() {
            let agent_map = agent_map.clone();
            let semaphore = Arc::clone(&semaphore);
            let base_cwd = base_cwd.to_path_buf();
            let approval_policy = approval_policy.to_string();
            let sandbox_mode = sandbox_mode.to_string();
            let codex_bin = self.codex_bin.clone();
            let confirm_project_agents = confirm_project_agents;
            handles.push(tokio::spawn(async move {
                let _permit = semaphore.acquire_owned().await;
                run_task(
                    &codex_bin,
                    &base_cwd,
                    &agent_map,
                    &approval_policy,
                    &sandbox_mode,
                    confirm_project_agents,
                    &task,
                )
                .await
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await??);
        }
        Ok(results)
    }

    async fn run_chain(
        &self,
        base_cwd: &Path,
        agent_map: &std::collections::HashMap<String, AgentConfig>,
        approval_policy: &str,
        sandbox_mode: &str,
        confirm_project_agents: bool,
        chain: &[TaskItem],
    ) -> Result<Vec<AgentExecutionResult>> {
        if chain.len() > self.max_workers {
            return Err(anyhow!(
                "too many chain steps ({}). max is {}",
                chain.len(),
                self.max_workers
            ));
        }
        let mut results = Vec::new();
        let mut previous_output = String::new();
        for item in chain {
            let mut task = item.task.clone();
            if task.contains("{previous}") {
                task = task.replace("{previous}", &previous_output);
            }
            let mut next = item.clone();
            next.task = task;
            let result = self
                .run_single(
                    base_cwd,
                    agent_map,
                    approval_policy,
                    sandbox_mode,
                    confirm_project_agents,
                    &next,
                )
                .await?;
            if result.status != "ok" {
                let message = result
                    .error
                    .clone()
                    .unwrap_or_else(|| "chain step failed".to_string());
                return Err(anyhow!(
                    "chain stopped at agent {}: {}",
                    result.agent,
                    message
                ));
            }
            previous_output = result.output.clone();
            results.push(result);
        }
        Ok(results)
    }

    async fn run_single(
        &self,
        base_cwd: &Path,
        agent_map: &std::collections::HashMap<String, AgentConfig>,
        approval_policy: &str,
        sandbox_mode: &str,
        confirm_project_agents: bool,
        task: &TaskItem,
    ) -> Result<AgentExecutionResult> {
        run_task(
            &self.codex_bin,
            base_cwd,
            agent_map,
            approval_policy,
            sandbox_mode,
            confirm_project_agents,
            task,
        )
        .await
    }

    async fn run_agent_task(
        &self,
        base_cwd: &Path,
        agent: &AgentConfig,
        task: &str,
        approval_policy: &str,
        sandbox_mode: &str,
        cwd_override: Option<&str>,
    ) -> Result<AgentExecutionResult> {
        let cwd = cwd_override
            .map(PathBuf::from)
            .unwrap_or_else(|| base_cwd.to_path_buf());
        run_task_internal(
            &self.codex_bin,
            &cwd,
            agent,
            task,
            approval_policy,
            sandbox_mode,
        )
        .await
    }
}

fn parse_task_item(value: &Value) -> Result<TaskItem> {
    let agent = value
        .get("agent")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("task missing agent"))?;
    let task = value
        .get("task")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("task missing task"))?;
    let cwd = value
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|v| v.to_string());
    Ok(TaskItem {
        agent: agent.to_string(),
        task: task.to_string(),
        cwd,
    })
}

fn parse_json_payload(output: &str) -> Result<Value> {
    if let Ok(value) = serde_json::from_str(output) {
        return Ok(value);
    }
    let start = output
        .find('{')
        .ok_or_else(|| anyhow!("planner did not return JSON"))?;
    let end = output
        .rfind('}')
        .ok_or_else(|| anyhow!("planner did not return JSON"))?;
    let slice = &output[start..=end];
    serde_json::from_str(slice).map_err(|err| anyhow!("failed to parse JSON: {err}"))
}

fn validate_plan(value: &Value) -> Result<()> {
    let mode = value
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("parallel");
    match mode {
        "single" => {
            if value.get("agent").is_none() || value.get("task").is_none() {
                return Err(anyhow!("single plan requires agent + task"));
            }
        }
        "chain" => {
            if value.get("chain").and_then(|v| v.as_array()).is_none() {
                return Err(anyhow!("chain plan requires chain[]"));
            }
        }
        _ => {
            if value.get("tasks").and_then(|v| v.as_array()).is_none() {
                return Err(anyhow!("parallel plan requires tasks[]"));
            }
        }
    }
    Ok(())
}

async fn run_task(
    codex_bin: &str,
    base_cwd: &Path,
    agent_map: &std::collections::HashMap<String, AgentConfig>,
    approval_policy: &str,
    sandbox_mode: &str,
    confirm_project_agents: bool,
    task: &TaskItem,
) -> Result<AgentExecutionResult> {
    let agent = agent_map.get(&task.agent).ok_or_else(|| {
        anyhow!(
            "unknown agent: {}. available: {}",
            task.agent,
            format_agent_list(&agent_map.values().cloned().collect::<Vec<_>>())
        )
    })?;
    if confirm_project_agents && agent.source == AgentSource::Project {
        return Err(anyhow!(
            "project agent '{}' requested. Set confirmProjectAgents=false to allow project agents.",
            agent.name
        ));
    }
    let cwd = task
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| base_cwd.to_path_buf());

    run_task_internal(
        codex_bin,
        &cwd,
        agent,
        &task.task,
        approval_policy,
        sandbox_mode,
    )
    .await
}

async fn run_task_internal(
    codex_bin: &str,
    cwd: &Path,
    agent: &AgentConfig,
    task: &str,
    approval_policy: &str,
    sandbox_mode: &str,
) -> Result<AgentExecutionResult> {
    let sandbox = agent
        .sandbox
        .clone()
        .unwrap_or_else(|| sandbox_mode.to_string());
    let sandbox_policy = match sandbox.as_str() {
        "read-only" => Some(json!({ "type": "readOnly" })),
        "danger-full-access" => Some(json!({ "type": "dangerFullAccess" })),
        _ => Some(json!({
            "type": "workspaceWrite",
            "writableRoots": [cwd],
            "networkAccess": true
        })),
    };

    let client = AppServerClient::spawn(codex_bin).await?;
    if let Err(err) = client.initialize().await {
        client.shutdown().await;
        return Err(err);
    }
    let thread_id = match client
        .start_thread(
            cwd,
            Some(agent.system_prompt.as_str()),
            agent.model.as_deref(),
            agent.approval_policy.as_deref().or(Some(approval_policy)),
            Some(sandbox.as_str()),
        )
        .await
    {
        Ok(thread_id) => thread_id,
        Err(err) => {
            client.shutdown().await;
            return Err(err);
        }
    };

    let turn_output = client
        .run_turn(
            &thread_id,
            task,
            cwd,
            agent.approval_policy.as_deref().or(Some(approval_policy)),
            sandbox_policy,
            agent.model.as_deref(),
            agent.effort.as_deref(),
        )
        .await;

    client.shutdown().await;

    match turn_output {
        Ok(result) => Ok(AgentExecutionResult {
            agent: agent.name.clone(),
            task: task.to_string(),
            cwd: cwd.display().to_string(),
            status: "ok".to_string(),
            output: result.output,
            error: None,
        }),
        Err(err) => Ok(AgentExecutionResult {
            agent: agent.name.clone(),
            task: task.to_string(),
            cwd: cwd.display().to_string(),
            status: "error".to_string(),
            output: String::new(),
            error: Some(err.to_string()),
        }),
    }
}

pub fn summarize_results(result: &SubagentResult) -> String {
    let mut lines = Vec::new();
    for item in &result.results {
        let status = if item.status == "ok" { "OK" } else { "ERR" };
        let preview = if !item.output.is_empty() {
            item.output
                .lines()
                .next()
                .unwrap_or("(no output)")
                .to_string()
        } else if let Some(err) = &item.error {
            err.clone()
        } else {
            "(no output)".to_string()
        };
        lines.push(format!("{status} [{}] {preview}", item.agent));
    }
    let mut text = format!("mode: {}\n", result.mode);
    if let Some(plan) = &result.plan {
        text.push_str("plan: ");
        text.push_str(&plan.to_string());
        text.push('\n');
    }
    text.push_str(&lines.join("\n"));
    text
}
