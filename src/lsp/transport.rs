//! Async JSON-RPC 2.0 framing over a language server's stdio.
//!
//! LSP transports messages as `Content-Length: <n>\r\n\r\n<json>` frames.  This
//! module only does framing + dispatch of the decoded JSON into a small
//! [`Incoming`] enum; the request/response matching lives in [`crate::lsp::client`].

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}
impl std::error::Error for RpcError {}

/// A decoded incoming JSON-RPC 2.0 message.
#[derive(Debug)]
pub enum Incoming {
    /// Response to one of our (integer-id) requests.
    Response {
        id: i64,
        result: Option<Value>,
        error: Option<RpcError>,
    },
    /// Server -> client notification.
    Notification { method: String, params: Value },
    /// Server -> client request (we must reply with the same id).
    Request { id: Value, method: String, params: Value },
}

pub async fn write_message<W: AsyncWriteExt + Unpin>(w: &mut W, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    w.write_all(header.as_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_message<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Result<Option<Incoming>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(rest.trim().parse().context("invalid Content-Length")?);
        }
        // Other headers (e.g. Content-Type) are intentionally ignored.
    }
    let len = content_length.context("message missing Content-Length")?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    let value: Value = serde_json::from_slice(&buf)?;
    Ok(Some(parse_incoming(value)?))
}

fn parse_incoming(value: Value) -> Result<Incoming> {
    let obj = value
        .as_object()
        .context("JSON-RPC message is not an object")?;
    let id = obj.get("id");
    let method = obj.get("method").and_then(|m| m.as_str());
    match (id, method) {
        (Some(id_val), Some(method)) => Ok(Incoming::Request {
            id: id_val.clone(),
            method: method.to_string(),
            params: obj.get("params").cloned().unwrap_or(Value::Null),
        }),
        (Some(id_val), None) => {
            // A response to one of our requests.
            let id = id_val.as_i64().context("response id is not an integer")?;
            if let Some(err) = obj.get("error") {
                let error: RpcError = serde_json::from_value(err.clone())?;
                Ok(Incoming::Response {
                    id,
                    result: None,
                    error: Some(error),
                })
            } else {
                Ok(Incoming::Response {
                    id,
                    result: obj.get("result").cloned(),
                    error: None,
                })
            }
        }
        (None, Some(method)) => Ok(Incoming::Notification {
            method: method.to_string(),
            params: obj.get("params").cloned().unwrap_or(Value::Null),
        }),
        (None, None) => anyhow::bail!("unrecognized JSON-RPC message: {value}"),
    }
}
