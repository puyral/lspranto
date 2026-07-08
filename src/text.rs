//! Rendering of LSP results into text for MCP tool output.
//!
//! These are intentionally defensive: LSP response types drift across versions,
//! so where the shape is unstable we parse raw JSON and read fields by name.

use crate::lsp::conv;
use itertools::Itertools;
use lsp_types::{
    CompletionResponse, Diagnostic, DocumentSymbol, DocumentSymbolResponse, GotoDefinitionResponse,
    Hover, HoverContents, Location, MarkupContent, SymbolInformation, Uri, WorkspaceEdit,
};
use serde_json::Value;

pub fn uri_to_path(uri: &Uri) -> String {
    conv::uri_to_path_string(uri)
}

fn short_from_str(s: &str) -> String {
    s.rsplit('/').next().unwrap_or(s).to_string()
}

pub fn location_line(loc: &Location) -> String {
    let s = loc.range.start;
    format!("{}:{}:{}", uri_to_path(&loc.uri), s.line, s.character)
}

// ---- hover ----

pub fn format_hover(hover: Option<Hover>) -> String {
    match hover {
        None => "No hover information.".to_string(),
        Some(h) => match h.contents {
            HoverContents::Scalar(m) => marked_string(&m),
            HoverContents::Array(arr) => arr.iter().map(marked_string).join("\n\n"),
            HoverContents::Markup(m) => markup(&m),
        },
    }
}

fn marked_string(m: &lsp_types::MarkedString) -> String {
    match m {
        lsp_types::MarkedString::String(s) => s.clone(),
        lsp_types::MarkedString::LanguageString(ls) => {
            format!("```{}\n{}\n```", ls.language, ls.value)
        }
    }
}

fn markup(m: &MarkupContent) -> String {
    m.value.clone()
}

// ---- definition / references ----

pub fn format_definition(def: Option<GotoDefinitionResponse>) -> String {
    match def {
        None => "No definition found.".to_string(),
        Some(GotoDefinitionResponse::Scalar(loc)) => format!("- {}", location_line(&loc)),
        Some(GotoDefinitionResponse::Array(locs)) => format_locations(&locs),
        Some(GotoDefinitionResponse::Link(links)) => links
            .iter()
            .map(|l| {
                let s = l.target_selection_range.start;
                format!("- {}:{}:{}", uri_to_path(&l.target_uri), s.line, s.character)
            })
            .join("\n"),
    }
}

pub fn format_locations(locs: &[Location]) -> String {
    if locs.is_empty() {
        return "No locations found.".to_string();
    }
    locs.iter().map(|l| format!("- {}", location_line(l))).join("\n")
}

pub fn format_references(refs: Option<Vec<Location>>) -> String {
    match refs {
        None => "No references found.".to_string(),
        Some(r) => format_locations(&r),
    }
}

// ---- completion ----

pub fn format_completion(comp: Option<CompletionResponse>) -> String {
    let items = match comp {
        None => return "No completions.".to_string(),
        Some(CompletionResponse::Array(items)) => items,
        Some(CompletionResponse::List(list)) => list.items,
    };
    if items.is_empty() {
        return "No completions.".to_string();
    }
    items
        .iter()
        .map(|it| match it.kind {
            Some(k) => format!("- {} ({k:?})", it.label),
            None => format!("- {}", it.label),
        })
        .join("\n")
}

// ---- symbols ----

pub fn format_document_symbols(syms: Option<DocumentSymbolResponse>) -> String {
    match syms {
        None => "No document symbols.".to_string(),
        Some(DocumentSymbolResponse::Nested(list)) => {
            if list.is_empty() {
                return "No document symbols.".to_string();
            }
            let mut out = String::new();
            for s in &list {
                render_doc_symbol(s, 0, &mut out);
            }
            out
        }
        Some(DocumentSymbolResponse::Flat(list)) => format_symbol_information(&list),
    }
}

fn render_doc_symbol(s: &DocumentSymbol, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    let start = s.selection_range.start;
    out.push_str(&format!(
        "{indent}- {} ({:?}) [{}:{}]\n",
        s.name, s.kind, start.line, start.character
    ));
    if let Some(children) = &s.children {
        for c in children {
            render_doc_symbol(c, depth + 1, out);
        }
    }
}

fn format_symbol_information(list: &[SymbolInformation]) -> String {
    if list.is_empty() {
        return "No symbols found.".to_string();
    }
    list.iter()
        .map(|s| format!("- {} ({:?}) @ {}", s.name, s.kind, location_line(&s.location)))
        .join("\n")
}

pub fn format_workspace_symbols(v: Option<Value>) -> String {
    let arr = match v {
        None | Some(Value::Null) => return "No workspace symbols.".to_string(),
        Some(Value::Array(a)) => a,
        Some(_) => return "No workspace symbols.".to_string(),
    };
    if arr.is_empty() {
        return "No workspace symbols.".to_string();
    }
    arr.iter()
        .map(|it| {
            let name = it.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let kind = it
                .get("kind")
                .and_then(|v| v.as_i64())
                .map(|k| format!(" (kind {k})"))
                .unwrap_or_default();
            let loc = it.get("location");
            let uri_str = loc.and_then(|l| l.get("uri")).and_then(|v| v.as_str()).unwrap_or("");
            let pos = loc.and_then(|l| l.get("range")).and_then(|r| r.get("start"));
            let line = pos
                .and_then(|p| p.get("line"))
                .and_then(|v| v.as_i64())
                .map(|l| l.to_string())
                .unwrap_or_else(|| "?".to_string());
            let ch = pos
                .and_then(|p| p.get("character"))
                .and_then(|v| v.as_i64())
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".to_string());
            let file = if uri_str.is_empty() {
                String::new()
            } else {
                format!("{}:", short_from_str(uri_str))
            };
            format!("- {name}{kind} {file}{line}:{ch}")
        })
        .join("\n")
}

// ---- diagnostics ----

pub fn format_diagnostics(diags: &[Diagnostic]) -> String {
    if diags.is_empty() {
        return "No diagnostics.".to_string();
    }
    diags
        .iter()
        .map(|d| {
            let sev = d
                .severity
                .map(|s| format!("{s:?}").to_lowercase())
                .unwrap_or_else(|| "info".to_string());
            let s = d.range.start;
            format!("- {sev} at {}:{}: {}", s.line, s.character, d.message)
        })
        .join("\n")
}

// ---- rename ----

pub fn format_prepare_rename(v: Option<Value>) -> String {
    match v {
        None => "Rename is not supported at this position.".to_string(),
        Some(val) => {
            if let Some(p) = val.get("placeholder").and_then(|v| v.as_str()) {
                format!("Renamable here. Current placeholder: `{p}`")
            } else {
                "Renamable here.".to_string()
            }
        }
    }
}

pub fn format_workspace_edit(edit: Option<WorkspaceEdit>) -> String {
    let edit = match edit {
        None => return "No edits produced.".to_string(),
        Some(e) => e,
    };
    let mut out = String::new();
    if let Some(changes) = &edit.changes {
        for (uri, edits) in changes {
            out.push_str(&format!("{}:\n", uri_to_path(uri)));
            for e in edits {
                let s = e.range.start;
                let en = e.range.end;
                out.push_str(&format!(
                    "  {}:{}-{}:{} -> {:?}\n",
                    s.line, s.character, en.line, en.character, e.new_text
                ));
            }
        }
    }
    if out.trim().is_empty()
        && let Some(dc) = &edit.document_changes
    {
        out.push_str(&serde_json::to_string_pretty(dc).unwrap_or_default());
    }
    if out.trim().is_empty() {
        "No edits produced.".to_string()
    } else {
        out
    }
}
