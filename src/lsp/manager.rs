//! Workspace + language routing.
//!
//! Given a file path we (1) find the longest activated workspace root that
//! contains it, (2) pick the language server by file extension, (3) get-or-spawn
//! a cached [`LspClient`] for that `(root, language_id)` pair, and (4) make sure
//! the document is open before returning it to the tool layer.

use crate::config::Config;
use crate::lsp::client::LspClient;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use crate::lsp::conv;
use lsp_types::Uri;
use std::sync::Arc;

pub struct Manager {
    config: Config,
    state: tokio::sync::Mutex<State>,
}

struct State {
    workspaces: Vec<PathBuf>,
    clients: HashMap<(PathBuf, String), Arc<LspClient>>,
}

impl Manager {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            state: tokio::sync::Mutex::new(State {
                workspaces: Vec::new(),
                clients: HashMap::new(),
            }),
        }
    }

    pub async fn activate_workspace(&self, path: PathBuf) -> Result<PathBuf> {
        let canon = path
            .canonicalize()
            .with_context(|| format!("resolving workspace {path:?}"))?;
        let mut st = self.state.lock().await;
        if !st.workspaces.contains(&canon) {
            st.workspaces.push(canon.clone());
        }
        Ok(canon)
    }

    pub async fn list_workspaces(&self) -> Vec<PathBuf> {
        self.state.lock().await.workspaces.clone()
    }

    pub async fn deactivate_workspace(&self, path: &Path) -> Result<bool> {
        let canon = path.canonicalize().ok();
        let to_remove: Vec<(PathBuf, String)> = {
            let st = self.state.lock().await;
            let before = st.workspaces.len();
            let retain_workspaces: Vec<PathBuf> = match &canon {
                Some(c) => st
                    .workspaces
                    .iter()
                    .filter(|w| *w != c)
                    .cloned()
                    .collect(),
                None => st.workspaces.clone(),
            };
            let removed = before != retain_workspaces.len();
            let _ = removed;
            st.clients
                .keys()
                .filter(|(root, _)| canon.as_ref() == Some(root))
                .cloned()
                .collect()
        };

        let mut st = self.state.lock().await;
        if let Some(canon) = &canon {
            st.workspaces.retain(|w| w != canon);
        }
        for key in &to_remove {
            if let Some(client) = st.clients.remove(key) {
                let client = client.clone();
                tokio::spawn(async move {
                    let _ = client.shutdown().await;
                });
            }
        }
        Ok(!to_remove.is_empty())
    }

    /// Resolve a file to its language client, opening the document first.
    pub async fn client_for(&self, file_path: &Path) -> Result<(Arc<LspClient>, Uri)> {
        let root = {
            let st = self.state.lock().await;
            longest_workspace(&st.workspaces, file_path)
                .context("no activated workspace contains this file; call `lsp_activate_workspace` first")?
                .clone()
        };
        let cfg = self
            .config
            .language_for_path(file_path)
            .context("no language server configured for this file extension")?
            .clone();

        let key = (root.clone(), cfg.language_id.clone());
        let client = {
            let mut st = self.state.lock().await;
            if let Some(c) = st.clients.get(&key) {
                c.clone()
            } else {
                let c = LspClient::spawn(cfg.clone(), root.clone()).await?;
                st.clients.insert(key.clone(), c.clone());
                c
            }
        };
        // `initialize` can be slow; do it outside the manager lock.
        client.initialize().await?;

        let uri = conv::uri_from_path(file_path)?;
        client.ensure_open(&uri).await?;
        Ok((client, uri))
    }

    /// Return the last `n` stderr lines for each language server running under
    /// the given workspace root. Used by the `lsp_get_server_logs` tool.
    pub async fn server_logs(
        &self,
        workspace: &Path,
        n: usize,
    ) -> Vec<(String, Vec<String>)> {
        let st = self.state.lock().await;
        st.clients
            .iter()
            .filter(|((root, _), _)| root == workspace)
            .map(|((_, lang), c)| (lang.clone(), c.stderr_tail(n)))
            .collect()
    }
}

fn longest_workspace<'a>(workspaces: &'a [PathBuf], file: &Path) -> Option<&'a PathBuf> {
    workspaces
        .iter()
        .filter(|w| file.starts_with(w))
        .max_by_key(|w| w.as_os_str().len())
}
