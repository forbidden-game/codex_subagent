# codex_subagent

MCP server that runs Codex subagents in isolated `codex app-server` processes.
It is compatible with `~/.pi/agent/agents` and `.pi/agents` (project scope), and
supports workflow presets from `~/.pi/agent/prompts` and `.pi/prompts`.

## Why

- Process-level isolation per subagent.
- Parallel and chain execution.
- Auto planning (planner agent decides tasks).
- CLI-only, works as an MCP server for Codex.

## Build

```bash
cargo build --release
```

Binary:

```
target/release/codex_subagent
```

## Install into Codex

Add to `~/.codex/config.toml`:

```toml
[mcp_servers.codex_subagent]
command = "/Users/fg/work/codex_subagent/target/release/codex_subagent"
```

Restart Codex, then the `subagent` tool will be available.

## Tool usage

### Auto (planner decides)

```json
{
  "task": "Refactor the auth flow to support OAuth",
  "mode": "auto",
  "workflow": "implement"
}
```

### Explicit parallel

```json
{
  "mode": "explicit",
  "tasks": [
    { "agent": "scout", "task": "Find auth entry points" },
    { "agent": "scout", "task": "List DB migrations" }
  ]
}
```

### Explicit chain

```json
{
  "mode": "explicit",
  "chain": [
    { "agent": "scout", "task": "Locate session store" },
    { "agent": "planner", "task": "Plan changes based on: {previous}" },
    { "agent": "worker", "task": "Implement plan: {previous}" }
  ]
}
```

## Notes

- Max parallel subagents defaults to 10 (`--max-workers`).
- `confirmProjectAgents=true` blocks `.pi/agents` by default (set to false to allow).
- `approvalPolicy` and `sandboxMode` can be set per tool call.
