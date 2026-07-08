//! An async LSP *client* that drives a single language server process.
//!
//! This owns the spawned server's stdio, matches responses back to requests by
//! id via a `oneshot` channel table, collects `publishDiagnostics` notifications
//! into a per-URI store, and caches advertised `ServerCapabilities` so tools can
//! gate calls (`supports_*`).

use crate::config::ServerConfig;
use crate::lsp::conv;
use crate::lsp::transport::{self, Incoming, RpcError};
use anyhow::{Context, Result};
use lsp_types::{
    ClientCapabilities, CompletionParams, CompletionResponse, ConfigurationParams,
    DidOpenTextDocumentParams, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionResponse,
    Hover, HoverParams, InitializeParams, InitializeResult, OneOf, Position,
    PublishDiagnosticsParams, ReferenceParams, RenameParams, ServerCapabilities, Uri,
    TextDocumentClientCapabilities, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, WorkspaceClientCapabilities, WorkspaceEdit, WorkspaceFolder,
    WorkspaceSymbolParams,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex as AsyncMutex, oneshot};

type OutResp = Result<Value, RpcError>;

/// A live language server session for one `(workspace_root, language_id)` pair.
pub struct LspClient {
    inner: Arc<Inner>,
}

struct Inner {
    writer: AsyncMutex<ChildStdin>,
    pending: Mutex<HashMap<i64, oneshot::Sender<OutResp>>>,
    diagnostics: Mutex<HashMap<Uri, Vec<lsp_types::Diagnostic>>>,
    caps: OnceLock<ServerCapabilities>,
    open_docs: Mutex<HashMap<Uri, i32>>,
    next_id: AtomicI64,
    root: Uri,
    root_path: std::path::PathBuf,
    cfg: ServerConfig,
    child: Mutex<Option<Child>>,
}

impl LspClient {
    /// Spawn the configured server and start its reader task.
    pub async fn spawn(cfg: ServerConfig, root_path: std::path::PathBuf) -> Result<Arc<Self>> {
        let root = conv::uri_from_path(&root_path)?;

        let mut command = tokio::process::Command::new(&cfg.command);
        command
            .args(&cfg.args)
            .envs(cfg.env.iter())
            .current_dir(&root_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .with_context(|| format!("spawning language server `{}`", cfg.command))?;
        let stdin = child.stdin.take().context("server has no stdin")?;
        let stdout = child.stdout.take().context("server has no stdout")?;

        let inner = Arc::new(Inner {
            writer: AsyncMutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
            caps: OnceLock::new(),
            open_docs: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            root,
            root_path,
            cfg,
            child: Mutex::new(Some(child)),
        });

        let inner_reader = inner.clone();
        tokio::spawn(async move {
            reader_loop(inner_reader, BufReader::new(stdout)).await;
        });

        Ok(Arc::new(Self { inner }))
    }

    #[allow(dead_code)]
    pub fn root(&self) -> &Uri {
        &self.inner.root
    }
    #[allow(dead_code)]
    pub fn root_path(&self) -> &Path {
        &self.inner.root_path
    }
    #[allow(dead_code)]
    pub fn language_id(&self) -> &str {
        &self.inner.cfg.language_id
    }
    #[allow(dead_code)]
    pub fn capabilities(&self) -> Option<&ServerCapabilities> {
        self.inner.caps.get()
    }

    /// Run `initialize` + `initialized` once; cache the server's capabilities.
    #[allow(deprecated)]
    pub async fn initialize(&self) -> Result<&ServerCapabilities> {
        if let Some(c) = self.inner.caps.get() {
            return Ok(c);
        }
        let name = self
            .inner
            .root_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".to_string());
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(self.inner.root.clone()),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: self.inner.root.clone(),
                name,
            }]),
            capabilities: client_capabilities(),
            initialization_options: self.inner.cfg.initialization_options.clone(),
            ..Default::default()
        };
        let result: InitializeResult =
            self.request("initialize", serde_json::to_value(&params)?).await?;
        let _ = self.inner.caps.set(result.capabilities);
        self.notify("initialized", serde_json::json!({})).await?;
        Ok(self.inner.caps.get().expect("capabilities just set"))
    }

    // ---- capability gates ----

    pub fn supports_hover(&self) -> bool {
        self.inner.caps.get().map(|c| c.hover_provider.is_some()).unwrap_or(true)
    }
    pub fn supports_definition(&self) -> bool {
        self.inner
            .caps
            .get()
            .map(|c| c.definition_provider.is_some())
            .unwrap_or(true)
    }
    pub fn supports_references(&self) -> bool {
        self.inner
            .caps
            .get()
            .map(|c| c.references_provider.is_some())
            .unwrap_or(true)
    }
    pub fn supports_completion(&self) -> bool {
        self.inner
            .caps
            .get()
            .map(|c| c.completion_provider.is_some())
            .unwrap_or(true)
    }
    pub fn supports_document_symbol(&self) -> bool {
        self.inner
            .caps
            .get()
            .map(|c| c.document_symbol_provider.is_some())
            .unwrap_or(true)
    }
    pub fn supports_workspace_symbol(&self) -> bool {
        self.inner
            .caps
            .get()
            .map(|c| c.workspace_symbol_provider.is_some())
            .unwrap_or(true)
    }
    pub fn supports_rename(&self) -> bool {
        self.inner
            .caps
            .get()
            .map(|c| c.rename_provider.is_some())
            .unwrap_or(true)
    }
    pub fn supports_prepare_rename(&self) -> bool {
        match self.inner.caps.get().and_then(|c| c.rename_provider.as_ref()) {
            Some(OneOf::Left(true)) => true,
            Some(OneOf::Right(opts)) => opts.prepare_provider.unwrap_or(false),
            _ => false,
        }
    }

    // ---- typed LSP requests ----

    pub async fn hover(&self, uri: &Uri, pos: Position) -> Result<Option<Hover>> {
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: pos,
            },
            work_done_progress_params: Default::default(),
        };
        self.request("textDocument/hover", serde_json::to_value(&params)?).await
    }

    pub async fn definition(&self, uri: &Uri, pos: Position) -> Result<Option<GotoDefinitionResponse>> {
        self.request("textDocument/definition", pos_params(uri, pos)).await
    }

    pub async fn references(
        &self,
        uri: &Uri,
        pos: Position,
        include_declaration: bool,
    ) -> Result<Option<Vec<lsp_types::Location>>> {
        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: pos,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: lsp_types::ReferenceContext { include_declaration },
        };
        self.request("textDocument/references", serde_json::to_value(&params)?).await
    }

    pub async fn completion(&self, uri: &Uri, pos: Position) -> Result<Option<CompletionResponse>> {
        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: pos,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        };
        self.request("textDocument/completion", serde_json::to_value(&params)?).await
    }

    pub async fn document_symbols(&self, uri: &Uri) -> Result<Option<DocumentSymbolResponse>> {
        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self.request("textDocument/documentSymbol", serde_json::to_value(&params)?).await
    }

    /// Returned as raw JSON because the `workspace/symbol` response type changed
    /// across LSP versions; we format it defensively in [`crate::text`].
    pub async fn workspace_symbols(&self, query: &str) -> Result<Option<Value>> {
        let params = WorkspaceSymbolParams {
            query: query.to_string(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self.request_raw("workspace/symbol", serde_json::to_value(&params)?).await.map(Some)
    }

    /// `prepareRename` response shape varies; returned as raw JSON.
    pub async fn prepare_rename(&self, uri: &Uri, pos: Position) -> Result<Option<Value>> {
        self.request_raw("textDocument/prepareRename", pos_params(uri, pos)).await.map(Some)
    }

    pub async fn rename(&self, uri: &Uri, pos: Position, new_name: &str) -> Result<Option<WorkspaceEdit>> {
        let params = RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: pos,
            },
            new_name: new_name.to_string(),
            work_done_progress_params: Default::default(),
        };
        self.request("textDocument/rename", serde_json::to_value(&params)?).await
    }

    /// Currently-published diagnostics for a document (from `publishDiagnostics`).
    pub fn diagnostics(&self, uri: &Uri) -> Vec<lsp_types::Diagnostic> {
        self.inner.diagnostics.lock().unwrap().get(uri).cloned().unwrap_or_default()
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _ = self.request_raw("shutdown", Value::Null).await;
        let _ = self.notify("exit", Value::Null).await;
        if let Some(mut child) = self.inner.child.lock().unwrap().take() {
            let _ = child.start_kill();
        }
        Ok(())
    }

    // ---- plumbing ----

    async fn request<R: serde::de::DeserializeOwned>(&self, method: &str, params: Value) -> Result<R> {
        let v = self.request_raw(method, params).await?;
        serde_json::from_value(v).map_err(Into::into)
    }

    async fn request_raw(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);

        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        {
            let mut w = self.inner.writer.lock().await;
            transport::write_message(&mut *w, &msg).await?;
        }

        let timeout = self.inner.cfg.timeout();
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(v)) => v.map_err(|e| anyhow::anyhow!("{e}")),
            Ok(Err(_)) => Err(anyhow::anyhow!("request `{method}` dropped without a response")),
            Err(_) => {
                self.inner.pending.lock().unwrap().remove(&id);
                Err(anyhow::anyhow!("request `{method}` timed out after {timeout:?}"))
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut w = self.inner.writer.lock().await;
        transport::write_message(&mut *w, &msg).await
    }

    /// Open a document (`didOpen`) with its on-disk content, once per session.
    pub async fn ensure_open(&self, uri: &Uri) -> Result<()> {
        if self.inner.open_docs.lock().unwrap().contains_key(uri) {
            return Ok(());
        }
        let path = conv::uri_to_path(uri)?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let item = TextDocumentItem {
            uri: uri.clone(),
            language_id: self.inner.cfg.language_id.clone(),
            version: 1,
            text,
        };
        self.notify(
            "textDocument/didOpen",
            serde_json::to_value(DidOpenTextDocumentParams { text_document: item })?,
        )
        .await?;
        self.inner.open_docs.lock().unwrap().insert(uri.clone(), 1);
        Ok(())
    }
}

fn pos_params(uri: &Uri, pos: Position) -> Value {
    serde_json::to_value(&TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position: pos,
    })
    .expect("TextDocumentPositionParams serializes")
}

fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        workspace: Some(WorkspaceClientCapabilities {
            configuration: Some(true),
            ..Default::default()
        }),
        text_document: Some(TextDocumentClientCapabilities::default()),
        ..Default::default()
    }
}

async fn reader_loop(inner: Arc<Inner>, mut reader: BufReader<ChildStdout>) {
    loop {
        match transport::read_message(&mut reader).await {
            Ok(Some(msg)) => inner.dispatch(msg).await,
            Ok(None) => {
                tracing::info!("language server `{}` closed its stdout", inner.cfg.command);
                break;
            }
            Err(e) => {
                tracing::warn!("language server read error: {e}");
                break;
            }
        }
    }
    inner.fail_all("language server disconnected");
}

impl Inner {
    async fn dispatch(&self, msg: Incoming) {
        match msg {
            Incoming::Response { id, result, error } => {
                if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                    let resp = match (result, error) {
                        (_, Some(e)) => Err(e),
                        (Some(v), None) => Ok(v),
                        (None, None) => Ok(Value::Null),
                    };
                    let _ = tx.send(resp);
                } else {
                    tracing::warn!("response for unknown request id {id}");
                }
            }
            Incoming::Notification { method, params } => {
                if method == "textDocument/publishDiagnostics" {
                    match serde_json::from_value::<PublishDiagnosticsParams>(params) {
                        Ok(p) => {
                            self.diagnostics.lock().unwrap().insert(p.uri, p.diagnostics);
                        }
                        Err(e) => tracing::warn!("malformed publishDiagnostics: {e}"),
                    }
                } else {
                    tracing::debug!("notification {method}");
                }
            }
            Incoming::Request { id, method, params } => {
                self.handle_server_request(id, method, params).await;
            }
        }
    }

    async fn handle_server_request(&self, id: Value, method: String, params: Value) {
        let result: Value = match method.as_str() {
            "workspace/configuration" => {
                let n = serde_json::from_value::<ConfigurationParams>(params)
                    .map(|p| p.items.len())
                    .unwrap_or(0);
                Value::Array(vec![Value::Null; n])
            }
            "client/registerCapability" | "client/unregisterCapability" | "window/workDoneProgress/create" => {
                Value::Null
            }
            _ => {
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("lspranto does not handle server request `{method}`"),
                    },
                });
                let mut w = self.writer.lock().await;
                let _ = transport::write_message(&mut *w, &resp).await;
                return;
            }
        };
        let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let mut w = self.writer.lock().await;
        let _ = transport::write_message(&mut *w, &resp).await;
    }

    fn fail_all(&self, reason: &str) {
        let mut pending = self.pending.lock().unwrap();
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(RpcError {
                code: -32000,
                message: reason.to_string(),
                data: None,
            }));
        }
    }
}
