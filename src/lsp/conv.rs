//! Conversions between `lsp_types::Uri` and filesystem paths, and LSP position
//! normalization.
//!
//! `lsp-types` 0.97 introduced its own [`Uri`](lsp_types::Uri) type with no
//! direct file-path constructors, so we round-trip through the `url` crate.
//!
//! Position normalization ([`normalize_position`]) snaps a 0-based cursor to
//! an interior point of the identifier token under (or nearest to) it on the
//! line. This makes position-based queries tolerant of imprecise cursor
//! placement: pointing at the first character of an identifier, somewhere in its
//! middle, or in the whitespace just before it all resolve to the same interior
//! point, so `find_references`, `rename`, `goto_definition`, `hover`, and
//! `completion` behave consistently. Some servers (e.g. rust-analyzer) treat a
//! cursor exactly at a token's left edge as "not on" the token and return empty
//! results; snapping one character into the token avoids that.

use anyhow::Result;
use lsp_types::{Position, Uri};
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

/// Normalize a 0-based LSP `(line, character)` position to an interior point of
/// the identifier token at (or nearest to) that character on the line.
///
/// `character` is a UTF-16 code unit offset, per the LSP spec. Returns `None`
/// (so callers can fall back to the raw position) if the file cannot be read, the
/// line is out of range, or no identifier can be found on the line.
///
/// The snapped position is `token_start + 1` (one character into the token) for
/// tokens of length ≥ 2, or `token_start` for single-character tokens. This
/// keeps the cursor strictly inside the token, avoiding servers that treat a
/// cursor exactly at a token boundary as "not on" it.
pub fn normalize_position(path: &str, line: u32, character: u32) -> Option<Position> {
    let text = std::fs::read_to_string(path).ok()?;
    let line_str = text.lines().nth(line as usize)?;
    let char_idx = utf16_to_char_index(line_str, character)?;
    let (start, end) = find_identifier_around(line_str, char_idx)?;
    let snap_char = if end - start > 1 { start + 1 } else { start };
    let snap_utf16 = char_index_to_utf16(line_str, snap_char)?;
    Some(Position {
        line,
        character: snap_utf16,
    })
}

/// Map a UTF-16 code-unit offset on a line to a char index. Returns `None` if the
/// offset is not on a UTF-16 code-unit boundary or is past the line end.
fn utf16_to_char_index(line: &str, utf16: u32) -> Option<usize> {
    let mut units = 0u32;
    let mut char_idx = 0usize;
    for ch in line.chars() {
        if units >= utf16 {
            return Some(char_idx);
        }
        units += ch.len_utf16() as u32;
        char_idx += 1;
    }
    // Offset at the very end of the line.
    (units == utf16).then_some(char_idx)
}

/// Map a char index on a line to a UTF-16 code-unit offset.
fn char_index_to_utf16(line: &str, char_idx: usize) -> Option<u32> {
    let mut utf16 = 0u32;
    for (i, ch) in line.chars().enumerate() {
        if i == char_idx {
            return Some(utf16);
        }
        utf16 += ch.len_utf16() as u32;
    }
    // Index at the very end of the line.
    (line.chars().count() == char_idx).then_some(utf16)
}

/// `(start, end)` char-index spans of identifier runs (`[A-Za-z0-9_]` plus any
/// Unicode alphanumeric) on a line.
fn identifier_spans(line: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, ch) in line.chars().enumerate() {
        let is_id = ch.is_alphanumeric() || ch == '_';
        match (start, is_id) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                spans.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        spans.push((s, line.chars().count()));
    }
    spans
}

/// Find the identifier span that contains `char_idx`, is right-adjacent to it
/// (cursor immediately after a token), or is the nearest token across whitespace.
/// Tie-breaks left/right ties toward the right (the token after the cursor),
/// which is usually what an imprecise cursor was aiming at.
fn find_identifier_around(line: &str, char_idx: usize) -> Option<(usize, usize)> {
    let spans = identifier_spans(line);
    // Direct hit: cursor inside the token, or right at its end boundary.
    for &(s, e) in &spans {
        if char_idx >= s && char_idx <= e {
            return Some((s, e));
        }
    }
    // Otherwise pick the nearest token by gap, preferring the right on ties.
    spans
        .into_iter()
        .min_by_key(|&(s, e)| {
            if char_idx <= s {
                s - char_idx
            } else if char_idx >= e {
                // Penalize the left side so a tie goes to the right.
                (char_idx - e) * 2
            } else {
                0
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write a temp file and return its path string, so `normalize_position` can
    /// read it back from disk (as it does in production).
    fn tmp_file(lines: &[&str]) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "lspranto_conv_test_{}_{id}.rs",
            std::process::id()
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        path.to_string_lossy().to_string()
    }

    #[test]
    fn first_char_and_middle_char_snap_same() {
        // Line 0: `fn err_text()` — `err_text` spans char indices 3..11.
        let path = tmp_file(&["fn err_text() ;"]);
        // char 3 = 'e' (first char, was 0 results without normalization)
        let p3 = normalize_position(&path, 0, 3).unwrap();
        // char 6 = '_' (middle, was 30 results)
        let p6 = normalize_position(&path, 0, 6).unwrap();
        // Both should snap to char 4 (one into the token).
        assert_eq!(p3, p6);
        assert_eq!(p3.character, 4);
    }

    #[test]
    fn cursor_in_whitespace_snaps_to_nearest() {
        // `foo   bar` — cursor in the middle of the whitespace gap.
        let path = tmp_file(&["foo   bar"]);
        // Cursor at char 5 (middle of 3-space gap): closer to `bar` (right).
        let p = normalize_position(&path, 0, 5).unwrap();
        // `bar` starts at char 6; snap to 7.
        assert_eq!(p.character, 7);
    }

    #[test]
    fn cursor_before_first_token_snaps_to_it() {
        let path = tmp_file(&["    hello"]);
        // Cursor at char 0 (leading whitespace).
        let p = normalize_position(&path, 0, 0).unwrap();
        // `hello` starts at char 4; snap to 5.
        assert_eq!(p.character, 5);
    }

    #[test]
    fn cursor_after_last_token_snaps_to_it() {
        let path = tmp_file(&["hello    "]);
        // Cursor at char 8 (trailing whitespace).
        let p = normalize_position(&path, 0, 8).unwrap();
        // `hello` starts at char 0; snap to 1.
        assert_eq!(p.character, 1);
    }

    #[test]
    fn line_out_of_range_returns_none() {
        let path = tmp_file(&["only one line"]);
        assert!(normalize_position(&path, 5, 0).is_none());
    }

    #[test]
    fn no_identifier_on_line_returns_none() {
        let path = tmp_file(&["   ; ()  "]);
        assert!(normalize_position(&path, 0, 2).is_none());
    }

    #[test]
    fn non_ascii_utf16_handling() {
        // Line: `let x = "X";  hello` where X is a 2-code-unit emoji.
        let path = tmp_file(&["let x = \"\u{1F600}\";  hello"]);
        // Cursor pointing into the whitespace before `hello`.
        // `let x = "X";` = l(0)e(1)t(2) (3)x(4) (5)=(6) (7)"(8)emoji(9-10)"(11);(12)
        // So `hello` starts at char 15 (after 2 spaces at 13,14).
        let p = normalize_position(&path, 0, 14).unwrap();
        // Should snap into `hello` at char 16.
        assert_eq!(p.character, 16);
    }

    #[test]
    fn single_char_token_snaps_to_itself() {
        let path = tmp_file(&["  x  "]);
        // `x` is a single-char token at char index 2; snap = start (not start+1).
        let p = normalize_position(&path, 0, 1).unwrap();
        assert_eq!(p.character, 2);
    }

    #[test]
    fn serde_aliases_workspace_path() {
        #[derive(serde::Deserialize)]
        struct S {
            #[serde(alias = "workspace_dir", alias = "path", alias = "file_path")]
            workspace_path: String,
        }
        let a: S = serde_json::from_str("{\"workspace_path\":\"/x\"}").unwrap();
        assert_eq!(a.workspace_path, "/x");
        let b: S = serde_json::from_str("{\"workspace_dir\":\"/x\"}").unwrap();
        assert_eq!(b.workspace_path, "/x");
        let c: S = serde_json::from_str("{\"path\":\"/x\"}").unwrap();
        assert_eq!(c.workspace_path, "/x");
        let d: S = serde_json::from_str("{\"file_path\":\"/x\"}").unwrap();
        assert_eq!(d.workspace_path, "/x");
    }
}
