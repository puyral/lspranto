//! The MCP server: exposes LSP features as `lsp_*` tools.

use crate::lsp::client::LspClient;
use crate::lsp::edit;
use crate::lsp::manager::Manager;
use crate::text;
use anyhow::Result;
use itertools::Itertools;
use lsp_types::{Position, Uri};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData, ServerInfo};
use rmcp::schemars;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

type McpError = ErrorData;

#[derive(Clone)]
pub struct LsprantoServer {
    manager: Arc<Manager>,
    #[allow(dead_code)]
    tool_router: ToolRouter<LsprantoServer>,
}

#[tool_router]
impl LsprantoServer {
    pub fn new(manager: Arc<Manager>) -> Self {
        Self {
            manager,
            tool_router: Self::tool_router(),
        }
    }

    // ---------------- workspace lifecycle ----------------

    #[tool(
        name = "lsp_activate_workspace",
        description = "Activate a workspace directory so files under it can be queried. Files are routed to a language server by extension. Returns the canonical path."
    )]
    async fn lsp_activate_workspace(
        &self,
        Parameters(p): Parameters<WorkspacePathParam>,
    ) -> Result<CallToolResult, McpError> {
        match self.manager.activate_workspace(p.workspace_path.into()).await {
            Ok(path) => Ok(ok_text(format!("activated: {}", path.display()))),
            Err(e) => Ok(err_text(format!("activate failed: {e:#}"))),
        }
    }

    #[tool(name = "lsp_list_workspaces", description = "List currently activated workspace directories.")]
    async fn lsp_list_workspaces(&self) -> Result<CallToolResult, McpError> {
        let ws = self.manager.list_workspaces().await;
        if ws.is_empty() {
            return Ok(ok_text("No active workspaces."));
        }
        Ok(ok_text(ws.iter().map(|p| format!("- {}", p.display())).join("\n")))
    }

    #[tool(
        name = "lsp_deactivate_workspace",
        description = "Deactivate a workspace and shut down its language server(s)."
    )]
    async fn lsp_deactivate_workspace(
        &self,
        Parameters(p): Parameters<WorkspacePathParam>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .manager
            .deactivate_workspace(std::path::Path::new(&p.workspace_path))
            .await
        {
            Ok(true) => Ok(ok_text("deactivated.")),
            Ok(false) => Ok(err_text("workspace was not active.")),
            Err(e) => Ok(err_text(format!("deactivate failed: {e:#}"))),
        }
    }

    // ---------------- queries ----------------

    #[tool(
        name = "lsp_hover",
        description = "Return hover (type/documentation) information at a 0-based position in a file."
    )]
    async fn lsp_hover(
        &self,
        Parameters(p): Parameters<PositionParam>,
    ) -> Result<CallToolResult, McpError> {
        let (client, uri, pos) = match self.route(&p).await {
            Ok(v) => v,
            Err(e) => return Ok(err_text(e)),
        };
        if !client.supports_hover() {
            return Ok(err_text("server does not support hover"));
        }
        match client.hover(&uri, pos).await {
            Ok(h) => Ok(ok_text(text::format_hover(h))),
            Err(e) => Ok(err_text(format!("hover failed: {e:#}"))),
        }
    }

    #[tool(
        name = "lsp_goto_definition",
        description = "Jump to the definition of the symbol at a 0-based position in a file. Locations are reported as path:line:character (0-based)."
    )]
    async fn lsp_goto_definition(
        &self,
        Parameters(p): Parameters<PositionParam>,
    ) -> Result<CallToolResult, McpError> {
        let (client, uri, pos) = match self.route(&p).await {
            Ok(v) => v,
            Err(e) => return Ok(err_text(e)),
        };
        if !client.supports_definition() {
            return Ok(err_text("server does not support goto definition"));
        }
        match client.definition(&uri, pos).await {
            Ok(d) => Ok(ok_text(text::format_definition(d))),
            Err(e) => Ok(err_text(format!("definition failed: {e:#}"))),
        }
    }

    #[tool(
        name = "lsp_find_references",
        description = "Find references to the symbol at a 0-based position in a file. Locations are reported as path:line:character (0-based)."
    )]
    async fn lsp_find_references(
        &self,
        Parameters(p): Parameters<ReferencesParam>,
    ) -> Result<CallToolResult, McpError> {
        let (client, uri, pos) = match self.route_raw(&p.file_path, p.line, p.character).await {
            Ok(v) => v,
            Err(e) => return Ok(err_text(e)),
        };
        if !client.supports_references() {
            return Ok(err_text("server does not support references"));
        }
        let include = p.include_declaration.unwrap_or(true);
        match client.references(&uri, pos, include).await {
            Ok(r) => Ok(ok_text(text::format_references(r))),
            Err(e) => Ok(err_text(format!("references failed: {e:#}"))),
        }
    }

    #[tool(
        name = "lsp_completion",
        description = "Return completion items at a 0-based position in a file."
    )]
    async fn lsp_completion(
        &self,
        Parameters(p): Parameters<PositionParam>,
    ) -> Result<CallToolResult, McpError> {
        let (client, uri, pos) = match self.route(&p).await {
            Ok(v) => v,
            Err(e) => return Ok(err_text(e)),
        };
        if !client.supports_completion() {
            return Ok(err_text("server does not support completion"));
        }
        match client.completion(&uri, pos).await {
            Ok(c) => Ok(ok_text(text::format_completion(c))),
            Err(e) => Ok(err_text(format!("completion failed: {e:#}"))),
        }
    }

    #[tool(
        name = "lsp_diagnostics",
        description = "Return published diagnostics for a file (errors/warnings reported by the language server)."
    )]
    async fn lsp_diagnostics(
        &self,
        Parameters(p): Parameters<FileParam>,
    ) -> Result<CallToolResult, McpError> {
        let (client, uri) = match self.route_file(&p.file_path).await {
            Ok(v) => v,
            Err(e) => return Ok(err_text(e)),
        };
        Ok(ok_text(text::format_diagnostics(&client.diagnostics(&uri))))
    }

    #[tool(
        name = "lsp_document_symbols",
        description = "Return the symbol tree of a file (functions, types, etc.)."
    )]
    async fn lsp_document_symbols(
        &self,
        Parameters(p): Parameters<FileParam>,
    ) -> Result<CallToolResult, McpError> {
        let (client, uri) = match self.route_file(&p.file_path).await {
            Ok(v) => v,
            Err(e) => return Ok(err_text(e)),
        };
        if !client.supports_document_symbol() {
            return Ok(err_text("server does not support document symbols"));
        }
        match client.document_symbols(&uri).await {
            Ok(s) => Ok(ok_text(text::format_document_symbols(s))),
            Err(e) => Ok(err_text(format!("document symbols failed: {e:#}"))),
        }
    }

    #[tool(
        name = "lsp_workspace_symbols",
        description = "Search workspace symbols by query. `file_path` anchors the workspace and language to search."
    )]
    async fn lsp_workspace_symbols(
        &self,
        Parameters(p): Parameters<WorkspaceSymbolParam>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _uri) = match self.route_file(&p.file_path).await {
            Ok(v) => v,
            Err(e) => return Ok(err_text(e)),
        };
        if !client.supports_workspace_symbol() {
            return Ok(err_text("server does not support workspace symbols"));
        }
        match client.workspace_symbols(&p.query).await {
            Ok(s) => Ok(ok_text(text::format_workspace_symbols(s))),
            Err(e) => Ok(err_text(format!("workspace symbols failed: {e:#}"))),
        }
    }

    #[tool(
        name = "lsp_rename_symbol",
        description = "Check whether the symbol at a 0-based position can be renamed, and if so compute the edits needed to rename it. By default returns the proposed edits for review and does NOT apply them. Pass `apply: true` to actually write the edits to disk and sync the affected documents with the language server. Fails cleanly if the position is not renamable."
    )]
    async fn lsp_rename_symbol(
        &self,
        Parameters(p): Parameters<RenameParam>,
    ) -> Result<CallToolResult, McpError> {
        let (client, uri, pos) = match self.route_raw(&p.file_path, p.line, p.character).await {
            Ok(v) => v,
            Err(e) => return Ok(err_text(e)),
        };
        if !client.supports_rename() {
            return Ok(err_text("server does not support rename"));
        }
        let edit = match client.rename(&uri, pos, &p.new_name).await {
            Ok(Some(e)) => e,
            Ok(None) => return Ok(err_text("no rename edits produced")),
            Err(e2) => return Ok(err_text(format!("rename failed: {e2:#}"))),
        };
        if p.apply.unwrap_or(false) {
            let applied = match edit::apply_to_disk(&edit) {
                Ok(a) => a,
                Err(e) => return Ok(err_text(format!("applying rename edits failed: {e:#}"))),
            };
            // Re-sync every open document the edit touched.
            for change in &applied {
                if let Ok(changed_uri) = crate::lsp::conv::uri_from_path(&change.path) {
                    let _ = client.sync_changed(&changed_uri).await;
                }
            }
            Ok(ok_text(format!(
                "Applied rename to {} file(s) ({}).\n\nProposed edits were:\n{}",
                applied.len(),
                applied
                    .iter()
                    .map(|c| c.path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                text::format_workspace_edit(Some(edit)),
            )))
        } else {
            Ok(ok_text(text::format_workspace_edit(Some(edit))))
        }
    }

    #[tool(
        name = "lsp_raw_request",
        description = "Escape hatch: send an arbitrary LSP request by method name and return the raw JSON result. Use this for unconventional or less-used LSP features that don't have a dedicated tool (e.g. textDocument/typeDefinition, textDocument/implementation, textDocument/callHierarchy, textDocument/semanticTokens, textDocument/codeAction, textDocument/foldingRange, textDocument/selectionRange, textDocument/codeLens). `file_path` anchors the workspace and language and is opened first; `params` is the raw LSP request params object (omit for requests that take none). Positions in params are 0-based. No capability gating is done — the server will return an error if it doesn't support the method."
    )]
    async fn lsp_raw_request(
        &self,
        Parameters(p): Parameters<RawRequestParam>,
    ) -> Result<CallToolResult, McpError> {
        let (client, _uri) = match self.route_file(&p.file_path).await {
            Ok(v) => v,
            Err(e) => return Ok(err_text(e)),
        };
        let params = p.params.unwrap_or(serde_json::Value::Null);
        match client.raw_request(&p.method, params).await {
            Ok(v) => Ok(ok_text(serde_json::to_string_pretty(&v).unwrap_or_else(|e| format!("<unserializable result: {e}>")))),
            Err(e) => Ok(err_text(format!("{} failed: {e:#}", p.method))),
        }
    }
}

/// Non-tool helpers.
impl LsprantoServer {
    async fn route(&self, p: &PositionParam) -> Result<(Arc<LspClient>, Uri, Position), String> {
        self.route_raw(&p.file_path, p.line, p.character).await
    }

    async fn route_raw(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<(Arc<LspClient>, Uri, Position), String> {
        let (client, uri) = self
            .manager
            .client_for(&PathBuf::from(file_path))
            .await
            .map_err(|e| format!("{e:#}"))?;
        Ok((client, uri, Position { line, character }))
    }

    async fn route_file(&self, file_path: &str) -> Result<(Arc<LspClient>, Uri), String> {
        self.manager
            .client_for(&PathBuf::from(file_path))
            .await
            .map_err(|e| format!("{e:#}"))
    }
}

#[tool_handler]
impl ServerHandler for LsprantoServer {
    fn get_info(&self) -> ServerInfo {
        use rmcp::model::ServerCapabilities;
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "LSP bridge. Activate a workspace with lsp_activate_workspace, then use \
             lsp_hover / lsp_goto_definition / lsp_find_references / lsp_completion / \
             lsp_diagnostics / lsp_document_symbols / lsp_workspace_symbols / \
             lsp_rename_symbol (apply=true to write) / lsp_raw_request. \
             All positions are 0-based line:character.",
        )
    }
}

// ---------------- tool parameter schemas ----------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkspacePathParam {
    #[schemars(description = "Absolute path to the workspace directory.")]
    pub workspace_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileParam {
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PositionParam {
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    #[schemars(description = "0-based line number.")]
    pub line: u32,
    #[schemars(description = "0-based character (UTF-16 code unit) offset on the line.")]
    pub character: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReferencesParam {
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    #[schemars(description = "0-based line number.")]
    pub line: u32,
    #[schemars(description = "0-based character offset on the line.")]
    pub character: u32,
    #[schemars(description = "Whether to include the declaration in the results. Defaults to true.")]
    pub include_declaration: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RenameParam {
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    #[schemars(description = "0-based line number.")]
    pub line: u32,
    #[schemars(description = "0-based character offset on the line.")]
    pub character: u32,
    #[schemars(description = "The new name for the symbol.")]
    pub new_name: String,
    #[schemars(description = "If true, write the rename edits to disk and sync the affected documents with the language server. Default false (dry run — return edits for review).")]
    pub apply: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceSymbolParam {
    #[schemars(description = "Absolute path to a file in the workspace; anchors the workspace and language.")]
    pub file_path: String,
    #[schemars(description = "Query string (symbol name / prefix).")]
    pub query: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RawRequestParam {
    #[schemars(description = "Absolute path to a file in the workspace; anchors the workspace and language and is opened first.")]
    pub file_path: String,
    #[schemars(description = "The LSP method name, e.g. `textDocument/typeDefinition`, `textDocument/implementation`, `textDocument/codeAction`.")]
    pub method: String,
    #[schemars(description = "Raw LSP request params as a JSON object. Omit for requests that take no params. Positions are 0-based line:character.")]
    pub params: Option<serde_json::Value>,
}

// ---------------- result helpers ----------------

fn ok_text(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s.into())])
}

fn err_text(s: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(s.into())])
}
