use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScope {
    User,
    Project,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    User,
    Project,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AgentConfig {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub approval_policy: Option<String>,
    pub sandbox: Option<String>,
    pub system_prompt: String,
    pub source: AgentSource,
    pub file_path: PathBuf,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AgentDiscoveryResult {
    pub agents: Vec<AgentConfig>,
    pub project_agents_dir: Option<PathBuf>,
}

fn parse_frontmatter(content: &str) -> (HashMap<String, String>, String) {
    let mut frontmatter = HashMap::new();
    let normalized = content.replace("\r\n", "\n");
    if !normalized.starts_with("---\n") {
        return (frontmatter, normalized);
    }
    let end_index = normalized[4..].find("\n---");
    let Some(end_index) = end_index else {
        return (frontmatter, normalized);
    };
    let frontmatter_block = &normalized[4..4 + end_index];
    let body = normalized[(4 + end_index + 4)..].trim().to_string();
    for line in frontmatter_block.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let mut value = value.trim().to_string();
            if (value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\''))
            {
                value = value[1..value.len() - 1].to_string();
            }
            frontmatter.insert(key.trim().to_string(), value);
        }
    }
    (frontmatter, body)
}

fn load_agents_from_dir(dir: &Path, source: AgentSource) -> Vec<AgentConfig> {
    let mut agents = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return agents,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let (frontmatter, body) = parse_frontmatter(&content);
        let name = match frontmatter.get("name") {
            Some(value) if !value.trim().is_empty() => value.trim().to_string(),
            _ => continue,
        };
        let description = match frontmatter.get("description") {
            Some(value) if !value.trim().is_empty() => value.trim().to_string(),
            _ => continue,
        };
        let tools = frontmatter
            .get("tools")
            .map(|value| {
                value
                    .split(',')
                    .map(|item| item.trim().to_string())
                    .filter(|item| !item.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let model = frontmatter.get("model").cloned();
        let effort = frontmatter.get("effort").cloned();
        let approval_policy = frontmatter.get("approvalPolicy").cloned();
        let sandbox = frontmatter.get("sandbox").cloned();
        agents.push(AgentConfig {
            name,
            description,
            tools,
            model,
            effort,
            approval_policy,
            sandbox,
            system_prompt: body,
            source,
            file_path: path,
        });
    }
    agents
}

fn is_dir(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

fn find_nearest_project_agents_dir(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        let candidate = current.join(".pi").join("agents");
        if is_dir(&candidate) {
            return Some(candidate);
        }
        let parent = current.parent()?.to_path_buf();
        if parent == current {
            return None;
        }
        current = parent;
    }
}

pub fn discover_agents(cwd: &Path, scope: AgentScope) -> AgentDiscoveryResult {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let user_dir = home.join(".pi").join("agent").join("agents");
    let project_agents_dir = find_nearest_project_agents_dir(cwd);

    let user_agents = match scope {
        AgentScope::Project => Vec::new(),
        _ => load_agents_from_dir(&user_dir, AgentSource::User),
    };
    let project_agents = match scope {
        AgentScope::User => Vec::new(),
        _ => project_agents_dir
            .as_ref()
            .map(|dir| load_agents_from_dir(dir, AgentSource::Project))
            .unwrap_or_default(),
    };

    let mut agent_map: HashMap<String, AgentConfig> = HashMap::new();
    match scope {
        AgentScope::Both => {
            for agent in user_agents {
                agent_map.insert(agent.name.clone(), agent);
            }
            for agent in project_agents {
                agent_map.insert(agent.name.clone(), agent);
            }
        }
        AgentScope::User => {
            for agent in user_agents {
                agent_map.insert(agent.name.clone(), agent);
            }
        }
        AgentScope::Project => {
            for agent in project_agents {
                agent_map.insert(agent.name.clone(), agent);
            }
        }
    }

    AgentDiscoveryResult {
        agents: agent_map.into_values().collect(),
        project_agents_dir,
    }
}

pub fn parse_agent_scope(raw: Option<&str>) -> AgentScope {
    match raw.unwrap_or("user") {
        "project" => AgentScope::Project,
        "both" => AgentScope::Both,
        _ => AgentScope::User,
    }
}

pub fn format_agent_list(agents: &[AgentConfig]) -> String {
    let mut list = agents
        .iter()
        .map(|agent| format!("{} ({:?}): {}", agent.name, agent.source, agent.description))
        .collect::<Vec<_>>();
    list.sort();
    if list.is_empty() {
        "none".to_string()
    } else {
        list.join(", ")
    }
}
