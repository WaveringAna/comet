//! Host-local, short-lived previews for file-edit tool calls.
//!
//! Tool input is deliberately stripped before transcript parts are synced. For
//! harnesses such as Codex that only report changed paths, this store snapshots
//! the file at tool start and materializes a bounded display diff when the tool
//! completes. The previews never enter the journal or a document: they live
//! under `{data_dir}/ephemeral-diffs` only for this engine lifetime, are read on
//! demand by `ToolDiff`, and are removed at both startup and graceful shutdown.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use comet_proto::{ToolCall, ToolDiffReply};

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_DIFF_BYTES: usize = 48 * 1024;

#[derive(Debug, Clone)]
enum Source {
    Missing,
    Text(String),
    Unavailable(String),
}

#[derive(Debug, Clone)]
struct PendingDiff {
    path: String,
    source_path: PathBuf,
    before: Source,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredDiff {
    path: String,
    diff: String,
}

/// Disk-backed preview cache. The pending source snapshot is process-local;
/// only the final rendered diff is written to disk, and only until shutdown.
pub struct EphemeralDiffStore {
    dir: PathBuf,
    pending: Mutex<HashMap<(String, String), PendingDiff>>,
}

impl EphemeralDiffStore {
    /// Begin a fresh lifetime, deliberately discarding anything a previous
    /// process left behind (including after a crash).
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        let dir = data_dir.join("ephemeral-diffs");
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// Capture a file before a tool modifies it. Calls carrying explicit old/new
    /// content can materialize immediately; Codex file-change events generally
    /// carry only paths, so they take the snapshot route.
    pub fn begin(&self, chat_id: &str, tool_id: &str, call: &ToolCall, cwd: &str) {
        // A protocol-provided patch may have arrived before its matching tool
        // lifecycle event. It is the authoritative preview; don't replace it
        // with a filesystem reconstruction.
        if self.path_for(chat_id, tool_id).exists() {
            return;
        }
        let Some((path, source_path)) = tool_path(call, cwd) else {
            return;
        };

        if let Some((old, new)) = explicit_change(call) {
            if let Err(err) = self.save(chat_id, tool_id, &path, presentation_diff(&path, old, new))
            {
                tracing::warn!(chat = %chat_id, tool = %tool_id, error = %err, "ephemeral diff save failed");
            }
            return;
        }

        let key = (chat_id.to_string(), tool_id.to_string());
        let mut pending = self.lock_pending();
        if pending.contains_key(&key) {
            return;
        }
        let before = read_source(&source_path).unwrap_or_else(|err| {
            tracing::debug!(path = %source_path.display(), error = %err, "ephemeral diff source unavailable");
            Source::Unavailable("couldn't read the file before this edit".into())
        });
        pending.insert(
            key,
            PendingDiff {
                path,
                source_path,
                before,
            },
        );
    }

    /// Materialize the preview only after a successful tool result, when the
    /// changed file reflects the result the transcript is about to show.
    pub fn finish(&self, chat_id: &str, tool_id: &str, is_error: bool) {
        let pending = self
            .lock_pending()
            .remove(&(chat_id.to_string(), tool_id.to_string()));
        if self.path_for(chat_id, tool_id).exists() {
            return;
        }
        let Some(pending) = pending else {
            return;
        };
        if is_error {
            return;
        }
        let after = read_source(&pending.source_path).unwrap_or_else(|err| {
            tracing::debug!(path = %pending.source_path.display(), error = %err, "ephemeral diff result unavailable");
            Source::Unavailable("couldn't read the file after this edit".into())
        });
        let diff = match (&pending.before, &after) {
            (Source::Text(before), Source::Text(after)) => {
                presentation_diff(&pending.path, before, after)
            }
            (Source::Missing, Source::Text(after)) => presentation_diff(&pending.path, "", after),
            (Source::Text(before), Source::Missing) => presentation_diff(&pending.path, before, ""),
            (Source::Missing, Source::Missing) => unavailable_diff(
                &pending.path,
                "the file was not present before or after this edit",
            ),
            (Source::Unavailable(reason), _) | (_, Source::Unavailable(reason)) => {
                unavailable_diff(&pending.path, reason)
            }
        };
        if let Err(err) = self.save(chat_id, tool_id, &pending.path, diff) {
            tracing::warn!(chat = %chat_id, tool = %tool_id, error = %err, "ephemeral diff save failed");
        }
    }

    /// Lazy read for the transcript. There is intentionally no journal fallback:
    /// a preview is private to this process lifetime.
    pub fn load(&self, chat_id: &str, tool_id: &str) -> io::Result<ToolDiffReply> {
        let path = self.path_for(chat_id, tool_id);
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(ToolDiffReply {
                    found: false,
                    path: None,
                    diff: None,
                });
            }
            Err(err) => return Err(err),
        };
        match serde_json::from_str::<StoredDiff>(&text) {
            Ok(stored) => Ok(ToolDiffReply {
                found: true,
                path: Some(stored.path),
                diff: Some(stored.diff),
            }),
            Err(err) => {
                tracing::warn!(chat = %chat_id, tool = %tool_id, error = %err, "discarding malformed ephemeral diff");
                let _ = fs::remove_file(self.path_for(chat_id, tool_id));
                Ok(ToolDiffReply {
                    found: false,
                    path: None,
                    diff: None,
                })
            }
        }
    }

    /// Store a harness-provided unified patch without ever putting it in the
    /// run journal. This is used by Codex's `patchUpdated` notification.
    pub fn save_rendered(&self, chat_id: &str, tool_id: &str, path: &str, diff: &str) {
        let diff = cap_diff(diff);
        if let Err(err) = self.save(chat_id, tool_id, path, diff) {
            tracing::warn!(chat = %chat_id, tool = %tool_id, error = %err, "ephemeral diff save failed");
        }
    }

    /// Best-effort cleanup for graceful app/daemon shutdown. Startup cleanup is
    /// the crash-safe backstop.
    pub fn clear(&self) {
        self.lock_pending().clear();
        match fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(path = %self.dir.display(), error = %err, "ephemeral diff cleanup failed")
            }
        }
    }

    fn lock_pending(&self) -> MutexGuard<'_, HashMap<(String, String), PendingDiff>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn path_for(&self, chat_id: &str, tool_id: &str) -> PathBuf {
        self.dir
            .join(sanitize_id(chat_id))
            .join(format!("{}.json", sanitize_id(tool_id)))
    }

    fn save(&self, chat_id: &str, tool_id: &str, path: &str, diff: String) -> io::Result<()> {
        let target = self.path_for(chat_id, tool_id);
        let parent = target.parent().expect("ephemeral diff path has parent");
        fs::create_dir_all(parent)?;
        let serialized = serde_json::to_vec(&StoredDiff {
            path: path.to_string(),
            diff,
        })
        .map_err(io::Error::other)?;
        let tmp = target.with_extension("json.tmp");
        fs::write(&tmp, serialized)?;
        fs::rename(tmp, target)
    }
}

fn tool_path(call: &ToolCall, cwd: &str) -> Option<(String, PathBuf)> {
    let path = match call {
        ToolCall::WriteFile { path, .. } | ToolCall::EditFile { path, .. } => path,
        ToolCall::ApplyPatch { path: Some(path) } => path,
        _ => return None,
    };
    let source_path = PathBuf::from(path);
    let source_path = if source_path.is_absolute() {
        source_path
    } else {
        Path::new(cwd).join(source_path)
    };
    Some((path.clone(), source_path))
}

fn explicit_change(call: &ToolCall) -> Option<(&str, &str)> {
    match call {
        ToolCall::WriteFile {
            content: Some(content),
            ..
        } => Some(("", content)),
        ToolCall::EditFile {
            old_string: Some(old),
            new_string: Some(new),
            ..
        } => Some((old, new)),
        _ => None,
    }
}

fn read_source(path: &Path) -> io::Result<Source> {
    match fs::read(path) {
        Ok(bytes) if bytes.len() > MAX_SOURCE_BYTES => Ok(Source::Unavailable(format!(
            "the file is larger than the {} KB preview limit",
            MAX_SOURCE_BYTES / 1024
        ))),
        Ok(bytes) if bytes.contains(&0) => Ok(Source::Unavailable("the file is binary".into())),
        Ok(bytes) => Ok(Source::Text(String::from_utf8_lossy(&bytes).into_owned())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Source::Missing),
        Err(err) => Err(err),
    }
}

fn presentation_diff(path: &str, old: &str, new: &str) -> String {
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    let truncated = if old.is_empty() {
        append_diff_lines(&mut out, "+ ", new, MAX_DIFF_BYTES)
    } else if new.is_empty() {
        append_diff_lines(&mut out, "- ", old, MAX_DIFF_BYTES)
    } else {
        let old_truncated = append_diff_lines(&mut out, "- ", old, MAX_DIFF_BYTES);
        old_truncated || append_diff_lines(&mut out, "+ ", new, MAX_DIFF_BYTES)
    };
    if truncated {
        out.push_str("… diff truncated …\n");
    }
    out
}

fn append_diff_lines(out: &mut String, prefix: &str, source: &str, max_bytes: usize) -> bool {
    for line in source.lines() {
        let needed = prefix.len() + line.len() + 1;
        if out.len() + needed > max_bytes {
            return true;
        }
        out.push_str(prefix);
        out.push_str(line);
        out.push('\n');
    }
    false
}

fn unavailable_diff(path: &str, reason: &str) -> String {
    format!("--- a/{path}\n+++ b/{path}\n# diff unavailable: {reason}\n")
}

fn cap_diff(diff: &str) -> String {
    if diff.len() <= MAX_DIFF_BYTES {
        return diff.to_string();
    }
    let mut end = MAX_DIFF_BYTES;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    let end = diff[..end].rfind('\n').unwrap_or(end);
    format!("{}\n… diff truncated …\n", &diff[..end])
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_completed_path_only_edit_and_cleans_on_reopen() {
        let root = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let file = worktree.path().join("src.rs");
        fs::write(&file, "before\n").unwrap();
        let store = EphemeralDiffStore::open(root.path()).unwrap();
        let call = ToolCall::EditFile {
            path: "src.rs".into(),
            old_string: None,
            new_string: None,
        };
        store.begin("chat", "tool", &call, &worktree.path().to_string_lossy());
        fs::write(&file, "after\n").unwrap();
        store.finish("chat", "tool", false);

        let reply = store.load("chat", "tool").unwrap();
        assert!(reply.found);
        assert!(reply.diff.unwrap().contains("- before\n+ after"));

        let reopened = EphemeralDiffStore::open(root.path()).unwrap();
        assert!(!reopened.load("chat", "tool").unwrap().found);
    }

    #[test]
    fn explicit_change_is_written_without_waiting_for_a_result() {
        let root = tempfile::tempdir().unwrap();
        let store = EphemeralDiffStore::open(root.path()).unwrap();
        let call = ToolCall::EditFile {
            path: "src.rs".into(),
            old_string: Some("old".into()),
            new_string: Some("new".into()),
        };
        store.begin("chat", "tool", &call, "");
        let reply = store.load("chat", "tool").unwrap();
        assert!(reply.diff.unwrap().contains("- old\n+ new"));
    }

    #[test]
    fn saves_a_harness_patch_without_a_filesystem_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let store = EphemeralDiffStore::open(root.path()).unwrap();
        let patch = "--- a/src.rs\n+++ b/src.rs\n@@\n-old\n+new\n";
        store.save_rendered("chat", "tool", "src.rs", patch);
        assert_eq!(
            store.load("chat", "tool").unwrap().diff.as_deref(),
            Some(patch)
        );
    }
}
