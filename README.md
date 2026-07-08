# lspranto

**NB**: as all LLM-tools should be, this is nearly 100% vibe-coded.

A config-driven [MCP](https://modelcontextprotocol.io/) server that bridges any
stdio-based [LSP](https://microsoft.github.io/language-server-protocol/) language
server to MCP clients. One process can serve many languages: files are routed to
the right server by extension, and a language server is spawned per
`(workspace, language)` pair on demand.

Inspired by [`isaacphi/mcp-language-server`](https://github.com/isaacphi/mcp-language-server)
(Go) and [`johnhnguyen97/lsp-mcp`](https://github.com/johnhnguyen97/lsp-mcp) (Rust),
but async end-to-end and config-driven rather than hard-coding any language.

## Stack

- [`rmcp`](https://crates.io/crates/rmcp) — official Rust MCP SDK (stdio transport, `#[tool]` macros)
- [`lsp-types`](https://crates.io/crates/lsp-types) — typed LSP messages
- A small hand-rolled async LSP *client* (`src/lsp/`) over `tokio::process` with
  `Content-Length` framing, an id→oneshot response table, and a diagnostics store.
  (`tower-lsp` was intentionally **not** used: it is an LSP *server* framework, and
  we need the *client* side.)
- `tokio` async runtime.

## Tools

All positions are **0-based** `line:character` (LSP convention).

| Tool | Description |
|------|-------------|
| `lsp_activate_workspace` | Activate a workspace directory. |
| `lsp_list_workspaces` | List active workspaces. |
| `lsp_deactivate_workspace` | Deactivate a workspace and shut down its server(s). |
| `lsp_get_server_logs` | Recent stderr output from the language server(s) — diagnose empty results (e.g. rust-analyzer indexing errors). Defaults to the last 100 lines. |
| `lsp_hover` | Type/documentation at a position. |
| `lsp_goto_definition` | Jump to a symbol's definition. |
| `lsp_find_references` | Find all references to a symbol. |
| `lsp_completion` | Completion items at a position. |
| `lsp_diagnostics` | Published diagnostics for a file. |
| `lsp_document_symbols` | The symbol tree of a file. |
| `lsp_workspace_symbols` | Search workspace symbols by query. |
| `lsp_raw_request` | Escape hatch: send any LSP request by method name, return raw JSON. |
| `lsp_rename_symbol` | Check whether a symbol can be renamed, and if so compute the rename edits. Pass `apply: true` to write them to disk (**off by default — dry run for review**). |

Each tool is capability-gated: if the server didn't advertise the feature in
`initialize`, the tool returns a clean error instead of calling it.

## Build

```bash
cargo build            # or:
nix build              # via the flake (crane + rust-flake)
nix develop            # devshell with cargo, rustc, clippy, rustfmt, rust-analyzer
```

The devshell includes `rust-analyzer` so lspranto can debug itself.

## Run

```bash
lspranto --workspace /path/to/project
# Activate more workspaces at startup (repeatable):
lspranto --workspace /a --workspace /b
# Override the built-in language-server registry:
lspranto --config my-config.toml
```

MCP runs over stdio; logs go to stderr (set `RUST_LOG=debug` for verbose output,
including a trace of LSP traffic).

## Configure in an MCP client

Claude Desktop / Cursor / etc. — point the client at the binary:

```json
{
  "mcpServers": {
    "lspranto": {
      "command": "lspranto",
      "args": ["--workspace", "/abs/path/to/your/project"]
    }
  }
}
```

## Configuration

The built-in registry (`config/default.toml`, embedded at build time) knows
rust-analyzer, gopls, pyright, typescript-language-server, clangd, nil, lua-language-server
and solargraph. Override or extend it with `--config <path>` using the same
`[[servers]]` schema (see `examples/user-config.toml`).

Each language server entry is launched from its workspace root over stdio;
`args`, `env`, `initialization_options` and a per-request `timeout_secs` are all
configurable.

## Architecture

```
src/
├── main.rs              # CLI: load config, activate workspaces, serve MCP over stdio
├── config.rs            # ServerConfig + registry (TOML)
├── lsp/
│   ├── transport.rs     # async Content-Length JSON-RPC framing
│   ├── client.rs        # spawn/initialize, id→oneshot matching, diagnostics store, caps cache
│   ├── manager.rs       # route file → (workspace, language) → cached client
│   └── conv.rs          # lsp_types::Uri ↔ filesystem path
├── mcp/server.rs        # rmcp ServerHandler + #[tool] definitions
└── text.rs              # render LSP results to text
```

## Status

Working but early. Tested against rust-analyzer (dogfooded on this repo). Other
servers should work but are not yet exercised in CI.
