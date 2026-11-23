# MCP clients and agent instructions

## stdio (default)

Claude Code:

```sh
claude mcp add virtmanager -- /path/to/virtmanager-mcp
```

Generic JSON config (Claude Desktop and others):

```json
{
  "mcpServers": {
    "virtmanager": {
      "command": "/path/to/virtmanager-mcp",
      "args": []
    }
  }
}
```

## Streamable HTTP

```sh
VIRTMANAGER_MCP_TOKEN=change-me virtmanager-mcp --transport http   # http://127.0.0.1:8765/mcp
claude mcp add --transport http virtmanager http://127.0.0.1:8765/mcp \
  --header "Authorization: Bearer change-me"
```

HTTP mode refuses to start without a token. Requests without a matching `Authorization: Bearer` header get `401`.

## Instructions for AI agents

Two templates tell an agent to install and test software in a development VM through this server
instead of on the host, keep an inventory of what the VM contains, and use snapshots and
disposable VMs when the management tools are enabled:

- `example-global-CLAUDE.md` for Claude Code: copy it into `~/.claude/CLAUDE.md`.
- `example-global-AGENTS.md` for agents that read `AGENTS.md` (e.g. Codex: `~/.codex/AGENTS.md`).

Adjust the VM name in them to yours. Both use the same inventory directory, so agents share it.
