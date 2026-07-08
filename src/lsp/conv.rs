//! Conversions between `lsp_types::Uri` and filesystem paths.
//!
//! `lsp-types` 0.97 introduced its own [`Uri`](lsp_types::Uri) type with no
//! direct file-path constructors, so we round-trip through the `url` crate.

use anyhow::Result;
use lsp_types::Uri;
use std::path::{Path, PathBuf};

pub fn uri_from_path(path: &Path) -> Result<Uri> {
    let url = url::Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("invalid file path {}", path.display()))?;
    url.as_str()
        .parse::<Uri>()
        .map_err(|e| anyhow::anyhow!("invalid uri `{}`: {e:?}", url.as_str()))
}

pub fn uri_to_path(uri: &Uri) -> Result<PathBuf> {
    let url =
        url::Url::parse(uri.as_str()).map_err(|e| anyhow::anyhow!("invalid uri: {e}"))?;
    url.to_file_path()
        .map_err(|_| anyhow::anyhow!("not a file uri: {}", uri.as_str()))
}

pub fn uri_to_path_string(uri: &Uri) -> String {
    uri_to_path(uri)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| uri.as_str().to_string())
}
