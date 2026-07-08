//! Apply a `lsp_types::WorkspaceEdit` to files on disk.
//!
//! Pure logic — no LSP server, no network.

use anyhow::{anyhow, Result};
use lsp_types::{
    AnnotatedTextEdit, DocumentChangeOperation, DocumentChanges, OneOf, ResourceOp, TextEdit,
    WorkspaceEdit,
};
use std::fs;
use std::path::PathBuf;
use crate::lsp::conv::uri_to_path;

/// Represents a single change that was applied to disk.
pub struct AppliedChange {
    pub path: PathBuf,
    /// Description of the change; kept for callers that want to report it.
    #[allow(dead_code)]
    pub kind: AppliedKind,
}

/// The kind of change applied to a file.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum AppliedKind {
    Modified,
    Created,
    Renamed { from: PathBuf },
    Deleted,
}

/// Convert a 0-based LSP position (line + UTF-16 character) to a byte offset in the text.
///
/// LSP `character` is in UTF-16 code units. Lines are separated by `\n`.
/// Out-of-range positions are clamped to the end of the line / end of file.
fn pos_to_byte(text: &str, line: u32, character: u32) -> usize {
    let mut byte_offset = 0usize;
    let mut current_line = 0u32;
    // Advance to the start of the target line.
    for ch in text.chars() {
        if current_line == line {
            break;
        }
        byte_offset += ch.len_utf8();
        if ch == '\n' {
            current_line += 1;
        }
    }
    if current_line < line {
        return text.len(); // line out of range -> clamp to EOF
    }
    // Now byte_offset is the start of the target line. Walk chars on this line
    // counting UTF-16 code units. A position that falls *inside* a multi-unit
    // character (i.e. between its two UTF-16 code units) snaps to the START of
    // that character — this is the editor-friendly behavior and prevents edits
    // from splitting a surrogate pair.
    let mut utf16_seen = 0u32;
    for ch in text[byte_offset..].chars() {
        if ch == '\n' {
            break; // don't cross into the next line
        }
        if utf16_seen >= character {
            break;
        }
        let len16 = ch.len_utf16() as u32;
        // If the target lands strictly inside this character, snap to its start
        // (don't advance past it).
        if utf16_seen < character && character < utf16_seen + len16 {
            break;
        }
        utf16_seen += len16;
        byte_offset += ch.len_utf8();
    }
    byte_offset
}

/// Apply TextEdits to file content.
/// Edits are sorted by start position descending so earlier offsets stay valid.
fn apply_text_edits(content: String, edits: &[TextEdit]) -> Result<String> {
    if edits.is_empty() {
        return Ok(content);
    }

    // Compute byte offsets for start positions and sort descending
    let mut indexed: Vec<(usize, usize, &TextEdit)> = edits
        .iter()
        .map(|e| {
            let start = pos_to_byte(&content, e.range.start.line, e.range.start.character);
            let end = pos_to_byte(&content, e.range.end.line, e.range.end.character);
            (start, end, e)
        })
        .collect();

    // Sort descending by start byte offset
    indexed.sort_by_key(|a| std::cmp::Reverse(a.0));

    let mut result = content;
    for (start, end, edit) in indexed {
        result.replace_range(start..end, &edit.new_text);
    }
    Ok(result)
}

/// Extract a plain TextEdit from a OneOf<TextEdit, AnnotatedTextEdit>.
fn one_of_to_text_edit(one: &OneOf<TextEdit, AnnotatedTextEdit>) -> TextEdit {
    match one {
        OneOf::Left(t) => t.clone(),
        OneOf::Right(a) => TextEdit {
            range: a.text_edit.range,
            new_text: a.text_edit.new_text.clone(),
        },
    }
}

/// Apply a `WorkspaceEdit` to files on disk.
///
/// Handles both:
/// - `changes` (legacy form): `HashMap<Uri, Vec<TextEdit>>`
/// - `document_changes` (new form): `DocumentChanges`
pub fn apply_to_disk(edit: &WorkspaceEdit) -> Result<Vec<AppliedChange>> {
    let mut changes = Vec::new();

    // Handle legacy `changes` form
    if let Some(changes_map) = &edit.changes {
        for (uri, text_edits) in changes_map {
            let path = uri_to_path(uri)?;
            let content =
                fs::read_to_string(&path).map_err(|e| anyhow!("failed to read {}: {e}", path.display()))?;
            let new_content = apply_text_edits(content, text_edits)?;
            fs::write(&path, &new_content)
                .map_err(|e| anyhow!("failed to write {}: {e}", path.display()))?;
            changes.push(AppliedChange {
                path,
                kind: AppliedKind::Modified,
            });
        }
    }

    // Handle `document_changes` form
    if let Some(doc_changes) = &edit.document_changes {
        match doc_changes {
            DocumentChanges::Edits(edits_list) => {
                for text_doc_edit in edits_list {
                    let path = uri_to_path(&text_doc_edit.text_document.uri)?;
                    let content =
                        fs::read_to_string(&path).map_err(|e| anyhow!("failed to read {}: {e}", path.display()))?;

                    let plain_edits: Vec<TextEdit> = text_doc_edit
                        .edits
                        .iter()
                        .map(one_of_to_text_edit)
                        .collect();

                    let new_content = apply_text_edits(content, &plain_edits)?;
                    fs::write(&path, &new_content)
                        .map_err(|e| anyhow!("failed to write {}: {e}", path.display()))?;
                    changes.push(AppliedChange {
                        path,
                        kind: AppliedKind::Modified,
                    });
                }
            }
            DocumentChanges::Operations(operations) => {
                for op in operations {
                    match op {
                        // TextDocumentEdit (edit operation)
                        DocumentChangeOperation::Edit(text_doc_edit) => {
                            let path = uri_to_path(&text_doc_edit.text_document.uri)?;
                            let content =
                                fs::read_to_string(&path).map_err(|e| anyhow!("failed to read {}: {e}", path.display()))?;

                            let plain_edits: Vec<TextEdit> = text_doc_edit
                                .edits
                                .iter()
                                .map(one_of_to_text_edit)
                                .collect();

                            let new_content = apply_text_edits(content, &plain_edits)?;
                            fs::write(&path, &new_content)
                                .map_err(|e| anyhow!("failed to write {}: {e}", path.display()))?;
                            changes.push(AppliedChange {
                                path,
                                kind: AppliedKind::Modified,
                            });
                        }
                        // Resource operations (create / rename / delete)
                        DocumentChangeOperation::Op(resource_op) => match resource_op {
                            ResourceOp::Create(create) => {
                                let path = uri_to_path(&create.uri)?;

                                let should_create = if path.exists() {
                                    // File exists: check options
                                    let overwrite = create
                                        .options
                                        .as_ref()
                                        .and_then(|o| o.overwrite)
                                        .unwrap_or(false);
                                    let ignore_if_exists = create
                                        .options
                                        .as_ref()
                                        .and_then(|o| o.ignore_if_exists)
                                        .unwrap_or(false);

                                    // Overwrite wins over ignoreIfExists
                                    if overwrite {
                                        true // overwrite existing
                                    } else if ignore_if_exists {
                                        false // skip
                                    } else {
                                        return Err(anyhow!(
                                            "file {} already exists; set overwrite to replace",
                                            path.display()
                                        ));
                                    }
                                } else {
                                    true // file doesn't exist, create it
                                };

                                if !should_create {
                                    continue;
                                }

                                // Create parent directories
                                if let Some(parent) = path.parent()
                                    && !parent.as_os_str().is_empty()
                                {
                                    fs::create_dir_all(parent)
                                        .map_err(|e| anyhow!("failed to create directory {}: {e}", parent.display()))?;
                                }

                                fs::write(&path, [])
                                    .map_err(|e| anyhow!("failed to create {}: {e}", path.display()))?;
                                changes.push(AppliedChange {
                                    path,
                                    kind: AppliedKind::Created,
                                });
                            }
                            ResourceOp::Rename(rename) => {
                                let old_path = uri_to_path(&rename.old_uri)?;
                                let new_path = uri_to_path(&rename.new_uri)?;

                                if new_path.exists() {
                                    let overwrite = rename
                                        .options
                                        .as_ref()
                                        .and_then(|o| o.overwrite)
                                        .unwrap_or(false);
                                    let ignore_if_exists = rename
                                        .options
                                        .as_ref()
                                        .and_then(|o| o.ignore_if_exists)
                                        .unwrap_or(false);

                                    if !overwrite && !ignore_if_exists {
                                        return Err(anyhow!(
                                            "rename target {} already exists and overwrite is disabled",
                                            new_path.display()
                                        ));
                                    }
                                    if overwrite {
                                        // proceed, will overwrite
                                    } else if ignore_if_exists {
                                        continue;
                                    }
                                }

                                fs::rename(&old_path, &new_path)
                                    .map_err(|e| anyhow!("failed to rename {} to {}: {e}", old_path.display(), new_path.display()))?;
                                changes.push(AppliedChange {
                                    path: new_path,
                                    kind: AppliedKind::Renamed { from: old_path },
                                });
                            }
                            ResourceOp::Delete(delete) => {
                                let path = uri_to_path(&delete.uri)?;

                                if !path.exists() {
                                    let ignore = delete
                                        .options
                                        .as_ref()
                                        .and_then(|o| o.ignore_if_not_exists)
                                        .unwrap_or(false);
                                    if ignore {
                                        continue;
                                    }
                                    return Err(anyhow!(
                                        "delete target {} does not exist and ignore_if_not_exists is disabled",
                                        path.display()
                                    ));
                                }

                                fs::remove_file(&path)
                                    .map_err(|e| anyhow!("failed to delete {}: {e}", path.display()))?;
                                changes.push(AppliedChange {
                                    path,
                                    kind: AppliedKind::Deleted,
                                });
                            }
                        },
                    }
                }
            }
        }
    }

    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{
        CreateFile, CreateFileOptions, DeleteFile, DeleteFileOptions, Position, Range,
        RenameFile, RenameFileOptions, TextDocumentEdit,
    };
    use std::collections::HashMap;
    use std::fs;

    /// Helper: create a temp file and return its path.
    fn make_temp_file(name: &str, content: &str) -> PathBuf {
        let tmp_dir = std::env::temp_dir();
        let path = tmp_dir.join(format!("lspranto_edit_test_{}", name));
        fs::write(&path, content).unwrap();
        path
    }

    /// Build a Uri string from a path.
    fn uri_from_path(path: &PathBuf) -> lsp_types::Uri {
        url::Url::from_file_path(path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap()
    }

    // ----------------------------------------------------------------
    // pos_to_byte tests
    // ----------------------------------------------------------------

    #[test]
    fn test_pos_to_byte_ascii() {
        let text = "hello\nworld\n";
        // (0,0) -> byte 0
        assert_eq!(pos_to_byte(text, 0, 0), 0);
        // (0,5) -> byte 5 (end of "hello")
        assert_eq!(pos_to_byte(text, 0, 5), 5);
        // (1,0) -> byte 6 (start of "world")
        assert_eq!(pos_to_byte(text, 1, 0), 6);
        // (1,5) -> byte 11 (end of "world")
        assert_eq!(pos_to_byte(text, 1, 5), 11);
    }

    #[test]
    fn test_pos_to_byte_non_bmp() {
        // "😀" is 1 char but 2 UTF-16 code units and 4 bytes
        let text = "a😀b";
        // 'a' is 1 byte
        // '😀' is 4 bytes
        // 'b' is 1 byte
        // Total = 6 bytes
        assert_eq!(text.len(), 6);
        assert_eq!(text.chars().count(), 3);
        assert_eq!(text.encode_utf16().count(), 4); // a + 😀 + b

        // (0, 0) -> 'a' at byte 0
        assert_eq!(pos_to_byte(text, 0, 0), 0);
        // (0, 1) -> after 'a', at byte 1
        assert_eq!(pos_to_byte(text, 0, 1), 1);
        // (0, 2) -> start of '😀' at byte 1
        assert_eq!(pos_to_byte(text, 0, 2), 1);
        // (0, 3) -> after '😀' at byte 5
        assert_eq!(pos_to_byte(text, 0, 3), 5);
        // (0, 4) -> after 'b' at byte 6
        assert_eq!(pos_to_byte(text, 0, 4), 6);
    }

    #[test]
    fn test_pos_to_byte_utf16_multibyte_chars() {
        // "é" is U+00E9, 1 char, 1 UTF-16 unit, 2 bytes
        let text = "aébc";
        // a = 1 byte, é = 2 bytes, b = 1 byte, c = 1 byte = 5 bytes total
        assert_eq!(text.len(), 5);
        // (0, 0) -> byte 0
        assert_eq!(pos_to_byte(text, 0, 0), 0);
        // (0, 1) -> after 'a', byte 1
        assert_eq!(pos_to_byte(text, 0, 1), 1);
        // (0, 2) -> after 'é' (é is 1 UTF-16 unit), byte 3
        assert_eq!(pos_to_byte(text, 0, 2), 3);
        // (0, 3) -> after 'b', byte 4
        assert_eq!(pos_to_byte(text, 0, 3), 4);
    }

    #[test]
    fn test_pos_to_byte_clamp_to_end_of_line() {
        let text = "abc\n";
        // Requesting character 100 on line 0 should clamp to end of line (byte 3)
        assert_eq!(pos_to_byte(text, 0, 100), 3);
    }

    #[test]
    fn test_pos_to_byte_clamp_to_end_of_file() {
        let text = "abc\ndef\n";
        // "abc\ndef\n" is 8 bytes. Requesting line 100 clamps to EOF (byte 8).
        assert_eq!(pos_to_byte(text, 100, 0), 8);
    }

    // ----------------------------------------------------------------
    // Integration tests with actual file edits
    // ----------------------------------------------------------------

    #[test]
    fn test_apply_text_edits_ascii_multi_edit() {
        let content = "hello\nworld\n";
        let edits = vec![
            TextEdit {
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 5 },
                },
                new_text: "hi".to_string(),
            },
            TextEdit {
                range: Range {
                    start: Position { line: 1, character: 0 },
                    end: Position { line: 1, character: 5 },
                },
                new_text: "earth".to_string(),
            },
        ];
        let result = apply_text_edits(content.to_string(), &edits).unwrap();
        assert_eq!(result, "hi\nearth\n");
    }

    #[test]
    fn test_apply_text_edits_non_bmp_regression() {
        // Content with a non-BMP character (emoji)
        let content = "a😀world\n";
        // Insert "XYZ" after the emoji: emoji is at (0, 2) in UTF-16 units
        let edits = vec![TextEdit {
            range: Range {
                start: Position { line: 0, character: 3 }, // right after 😀
                end: Position { line: 0, character: 3 },   // zero-width insertion
            },
            new_text: "XYZ".to_string(),
        }];
        let result = apply_text_edits(content.to_string(), &edits).unwrap();
        assert_eq!(result, "a😀XYZworld\n");
    }

    #[test]
    fn test_apply_changes_legacy_form() {
        let tmp_path = make_temp_file("legacy_changes.txt", "hello world\n");
        let tmp_uri = uri_from_path(&tmp_path);

        let mut changes = HashMap::new();
        changes.insert(
            tmp_uri.clone(),
            vec![TextEdit {
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 5 },
                },
                new_text: "hi".to_string(),
            }],
        );

        let edit = WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        };

        let applied = apply_to_disk(&edit).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].path, tmp_path);
        assert!(matches!(applied[0].kind, AppliedKind::Modified));

        // Verify file content
        let content = fs::read_to_string(&tmp_path).unwrap();
        assert_eq!(content, "hi world\n");
    }

    #[test]
    fn test_apply_document_changes_edits_form() {
        let tmp_path = make_temp_file("doc_edits.txt", "foo bar baz\n");
        let tmp_uri = uri_from_path(&tmp_path);

        let text_doc_id = lsp_types::OptionalVersionedTextDocumentIdentifier {
            uri: tmp_uri.clone(),
            version: None,
        };

        let edits = vec![
            OneOf::Left(TextEdit {
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 3 },
                },
                new_text: "FOO".to_string(),
            }),
            OneOf::Right(AnnotatedTextEdit {
                text_edit: TextEdit {
                    range: Range {
                        start: Position { line: 0, character: 4 },
                        end: Position { line: 0, character: 7 },
                    },
                    new_text: "BAR".to_string(),
                },
                annotation_id: "ann1".to_string(),
            }),
        ];

        let text_doc_edit = TextDocumentEdit {
            text_document: text_doc_id,
            edits,
        };

        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Edits(vec![text_doc_edit])),
            change_annotations: None,
        };

        let applied = apply_to_disk(&edit).unwrap();
        assert_eq!(applied.len(), 1);
        assert!(matches!(applied[0].kind, AppliedKind::Modified));

        let content = fs::read_to_string(&tmp_path).unwrap();
        assert_eq!(content, "FOO BAR baz\n");
    }

    #[test]
    fn test_apply_create_file() {
        let tmp_dir = std::env::temp_dir();
        let path = tmp_dir.join("lspranto_edit_test_create_new.txt");
        let uri = uri_from_path(&path);

        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri,
                    options: Some(CreateFileOptions {
                        overwrite: None,
                        ignore_if_exists: None,
                    }),
                    annotation_id: None,
                })),
            ])),
            change_annotations: None,
        };

        let applied = apply_to_disk(&edit).unwrap();
        assert_eq!(applied.len(), 1);
        assert!(matches!(applied[0].kind, AppliedKind::Created));
        assert!(path.exists());

        // Clean up
        fs::remove_file(&path).ok();
    }

    #[test]
    fn test_apply_rename_file() {
        let tmp_dir = std::env::temp_dir();
        let old_name = "lspranto_edit_test_rename_old.txt";
        let new_name = "lspranto_edit_test_rename_new.txt";
        let old_path = tmp_dir.join(old_name);
        let new_path = tmp_dir.join(new_name);

        // Create old file
        fs::write(&old_path, "rename me").unwrap();
        let old_uri = uri_from_path(&old_path);
        let new_uri = uri_from_path(&new_path);

        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
                    old_uri,
                    new_uri,
                    options: Some(RenameFileOptions {
                        overwrite: None,
                        ignore_if_exists: None,
                    }),
                    annotation_id: None,
                })),
            ])),
            change_annotations: None,
        };

        let applied = apply_to_disk(&edit).unwrap();
        assert_eq!(applied.len(), 1);
        assert!(matches!(&applied[0].kind, AppliedKind::Renamed { from } if from == &old_path));
        assert!(new_path.exists());
        assert!(!old_path.exists());
        assert_eq!(fs::read_to_string(&new_path).unwrap(), "rename me");

        // Clean up
        fs::remove_file(&new_path).ok();
    }

    #[test]
    fn test_apply_delete_file() {
        let tmp_path = make_temp_file("delete_me.txt", "delete content\n");
        let tmp_uri = uri_from_path(&tmp_path);

        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Delete(DeleteFile {
                    uri: tmp_uri,
                    options: Some(DeleteFileOptions {
                        recursive: None,
                        ignore_if_not_exists: None,
                        annotation_id: None,
                    }),
                })),
            ])),
            change_annotations: None,
        };

        let applied = apply_to_disk(&edit).unwrap();
        assert_eq!(applied.len(), 1);
        assert!(matches!(applied[0].kind, AppliedKind::Deleted));
        assert!(!tmp_path.exists());
    }

    #[test]
    fn test_apply_create_file_ignore_if_exists() {
        let tmp_path = make_temp_file("skip_create.txt", "existing\n");
        let tmp_uri = uri_from_path(&tmp_path);

        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: tmp_uri,
                    options: Some(CreateFileOptions {
                        overwrite: Some(false),
                        ignore_if_exists: Some(true),
                    }),
                    annotation_id: None,
                })),
            ])),
            change_annotations: None,
        };

        let applied = apply_to_disk(&edit).unwrap();
        assert_eq!(applied.len(), 0); // skipped

        // Content unchanged
        assert_eq!(fs::read_to_string(&tmp_path).unwrap(), "existing\n");

        // Clean up
        fs::remove_file(&tmp_path).ok();
    }

    #[test]
    fn test_apply_create_file_overwrite() {
        let tmp_path = make_temp_file("overwrite_create.txt", "old content\n");
        let tmp_uri = uri_from_path(&tmp_path);

        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: tmp_uri,
                    options: Some(CreateFileOptions {
                        overwrite: Some(true),
                        ignore_if_exists: None,
                    }),
                    annotation_id: None,
                })),
            ])),
            change_annotations: None,
        };

        let applied = apply_to_disk(&edit).unwrap();
        assert_eq!(applied.len(), 1);
        assert!(matches!(applied[0].kind, AppliedKind::Created));
        assert!(tmp_path.exists());

        // Clean up
        fs::remove_file(&tmp_path).ok();
    }
}
