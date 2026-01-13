use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::agent::{AgentScope, AgentSource};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PromptDefinition {
    pub name: String,
    pub description: Option<String>,
    pub body: String,
    pub source: AgentSource,
    pub file_path: PathBuf,
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

fn is_dir(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

fn find_nearest_project_prompts_dir(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        let candidate = current.join(".pi").join("prompts");
        if is_dir(&candidate) {
            return Some(candidate);
        }
        let candidate_agent = current.join(".pi").join("agent").join("prompts");
        if is_dir(&candidate_agent) {
            return Some(candidate_agent);
        }
        let parent = current.parent()?.to_path_buf();
        if parent == current {
            return None;
        }
        current = parent;
    }
}

fn load_prompts_from_dir(dir: &Path, source: AgentSource) -> Vec<PromptDefinition> {
    let mut prompts = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return prompts,
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
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let description = frontmatter.get("description").cloned();
        prompts.push(PromptDefinition {
            name,
            description,
            body,
            source,
            file_path: path,
        });
    }
    prompts
}

pub fn discover_prompts(cwd: &Path, scope: AgentScope) -> Vec<PromptDefinition> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let user_dir = home.join(".pi").join("agent").join("prompts");
    let project_dir = find_nearest_project_prompts_dir(cwd);

    let user_prompts = match scope {
        AgentScope::Project => Vec::new(),
        _ => load_prompts_from_dir(&user_dir, AgentSource::User),
    };
    let project_prompts = match scope {
        AgentScope::User => Vec::new(),
        _ => project_dir
            .as_ref()
            .map(|dir| load_prompts_from_dir(dir, AgentSource::Project))
            .unwrap_or_default(),
    };

    let mut prompt_map: HashMap<String, PromptDefinition> = HashMap::new();
    match scope {
        AgentScope::Both => {
            for prompt in user_prompts {
                prompt_map.insert(prompt.name.clone(), prompt);
            }
            for prompt in project_prompts {
                prompt_map.insert(prompt.name.clone(), prompt);
            }
        }
        AgentScope::User => {
            for prompt in user_prompts {
                prompt_map.insert(prompt.name.clone(), prompt);
            }
        }
        AgentScope::Project => {
            for prompt in project_prompts {
                prompt_map.insert(prompt.name.clone(), prompt);
            }
        }
    }

    prompt_map.into_values().collect()
}

pub fn find_prompt(cwd: &Path, scope: AgentScope, name: &str) -> Option<PromptDefinition> {
    let normalized = name.trim_end_matches(".md");
    discover_prompts(cwd, scope)
        .into_iter()
        .find(|prompt| prompt.name == normalized)
}
