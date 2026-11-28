//! Read-side helpers for rebuildup.conversations (T3 / #88).
//!
//! Rust port of `plugins/conversations/src/read.ts` (retired; git
//! history preserves it), with the Issue #306 cursor fix:
//!
//! The normalized event log (`events/<key>.jsonl`) is append-only for
//! file-backed providers and atomically rewritten for opencode, so a
//! byte-offset cursor stays valid across index updates: appends only
//! extend the file, and a rewrite that shrinks it is detected by the
//! offset-beyond-size check (never silently re-read as if nothing
//! changed).
//!
//! `read_event_page` streams newline-terminated lines from `offset`,
//! applies an optional match predicate, and stops after `limit`
//! matching events or EOF. The opaque multi-session cursor is a
//! base64url JSON map of `{session_key: byte_offset}`.
//!
//! #306 fix: `scan_paged_sessions` records `next_offset` for EVERY
//! processed session — including fully-consumed ones. The TS version
//! dropped finished sessions from `next_cursor`, so page 2+ restarted
//! them at byte 0 and re-emitted every event (468/500 duplicates on
//! page 2; `usage` export never terminated). A recorded offset equal
//! to the file size costs one stat+seek per page and correctly
//! resumes after appends; a shrink is still caught by the
//! offset-beyond-size restart path.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use serde_json::Value;

use crate::state::events_file_path;

pub const READ_CHUNK_BYTES: usize = 256 * 1024;

/// One matched event plus the payload fields that matched (search).
#[derive(Debug, Clone)]
pub struct EventMatch {
    pub event: Value,
    /// Names of the searchable fields that contained the query.
    pub fields: Vec<String>,
}

/// Match predicate: returns matched field names, or None to skip.
pub type EventMatcher<'a> = dyn Fn(&Value) -> Option<Vec<String>> + 'a;

#[derive(Debug, Clone)]
pub struct EventPage {
    pub matches: Vec<EventMatch>,
    /// Byte offset of the first unprocessed byte; pass back as cursor.
    pub next_offset: u64,
    pub has_more: bool,
    pub malformed_lines: u64,
    pub duplicates_skipped: u64,
    pub present: bool,
    pub size: u64,
}

impl EventPage {
    /// Empty page for a missing events file (mirrors TS `present:false`).
    fn absent(offset: u64) -> Self {
        EventPage {
            matches: Vec::new(),
            next_offset: offset,
            has_more: false,
            malformed_lines: 0,
            duplicates_skipped: 0,
            present: false,
            size: 0,
        }
    }
}

/// Match every event; used by `history`.
pub fn match_all(_event: &Value) -> Option<Vec<String>> {
    Some(vec!["event".to_string()])
}

/// Read up to `limit` matching events starting at byte `offset`.
/// Only newline-terminated lines are consumed; a partial tail is left
/// for the next page. Malformed lines are skipped and counted; repeated
/// event_ids inside one page are dropped and counted — the index is
/// append-only, so a duplicate means a rewrite raced the read.
pub fn read_event_page(
    key: &str,
    offset: u64,
    limit: usize,
    match_: &EventMatcher,
) -> Result<EventPage, String> {
    let file = events_file_path(key);
    let mut handle = match File::open(&file) {
        Ok(f) => f,
        Err(_) => return Ok(EventPage::absent(offset)),
    };
    let size = handle.metadata().map_err(|e| e.to_string())?.len();
    if offset > size {
        return Err(format!(
            "cursor {} beyond events file size {} (rewritten? restart without --cursor)",
            offset, size
        ));
    }
    handle
        .seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;

    let mut matches: Vec<EventMatch> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut malformed = 0u64;
    let mut duplicates = 0u64;
    // Bytes of complete (newline-terminated) lines consumed so far.
    // `next_offset` only advances past consumed lines — a partial tail
    // stays for the next page, exactly like the TS lineBuf logic.
    let mut consumed = 0u64;
    let mut reader = BufReader::with_capacity(READ_CHUNK_BYTES, handle);
    let mut buf: Vec<u8> = Vec::with_capacity(8192);

    loop {
        if matches.len() >= limit {
            break;
        }
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(e) => return Err(e.to_string()),
        };
        if n == 0 || !buf.ends_with(b"\n") {
            // EOF, or a partial final line: not consumed this page.
            break;
        }
        consumed += n as u64;
        // Strip the newline, then a trailing \r (TS lineBuf logic).
        let mut line_end = buf.len() - 1;
        if line_end > 0 && buf[line_end - 1] == b'\r' {
            line_end -= 1;
        }
        let line = String::from_utf8_lossy(&buf[..line_end]);
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                malformed += 1;
                continue;
            }
        };
        if let Some(id) = parsed.get("event_id").and_then(|v| v.as_str()) {
            if !seen.insert(id.to_string()) {
                duplicates += 1;
                continue;
            }
        }
        if let Some(fields) = match_(&parsed) {
            matches.push(EventMatch {
                event: parsed,
                fields,
            });
        }
    }

    let next_offset = offset + consumed;
    Ok(EventPage {
        matches,
        next_offset,
        has_more: next_offset < size,
        malformed_lines: malformed,
        duplicates_skipped: duplicates,
        present: true,
        size,
    })
}

// --- Opaque multi-session cursor -------------------------------------------

/// Encode per-session byte offsets into an opaque cursor token.
///
/// Issue #306: ALL scanned sessions are recorded — including fully
/// consumed ones (offset == file size). Finished sessions MUST stay in
/// the cursor: omitting them made page 2+ restart at byte 0 and
/// re-emit every event. The token stays small because offsets are
/// integers; consumers treat it as opaque.
pub fn encode_offset_cursor(offsets: &BTreeMap<String, u64>) -> String {
    // TS `JSON.stringify({v:1,o:offsets})` emits keys in insertion
    // order (v first); serde_json::Map sorts, so build the bytes
    // directly for byte-identical cursors.
    let mut body = String::from("{\"v\":1,\"o\":{");
    for (i, (k, v)) in offsets.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str(&serde_json::to_string(k).unwrap_or_else(|_| "\"\"".to_string()));
        body.push(':');
        body.push_str(&v.to_string());
    }
    body.push_str("}}");
    URL_SAFE_NO_PAD.encode(body.as_bytes())
}

/// Decode an opaque cursor; None when malformed (caller → cursor_invalid).
pub fn decode_offset_cursor(cursor: &str) -> Option<BTreeMap<String, u64>> {
    // TS `Buffer.from(s, "base64url")` tolerates padding; try both.
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .or_else(|_| URL_SAFE.decode(cursor))
        .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let parsed: Value = serde_json::from_str(&text).ok()?;
    if parsed.get("v").and_then(|v| v.as_u64()) != Some(1) {
        return None;
    }
    let offsets = parsed.get("o")?.as_object()?;
    let mut out = BTreeMap::new();
    for (key, value) in offsets {
        // TS requires `typeof number && Number.isInteger && >= 0` —
        // `1.0` parses as a float in JSON but is integer-valued.
        let f = value.as_f64()?;
        if !f.is_finite() || f < 0.0 || f.fract() != 0.0 || f > u64::MAX as f64 {
            return None;
        }
        out.insert(key.clone(), f as u64);
    }
    Some(out)
}

#[derive(Debug, Clone)]
pub struct ScanTarget {
    pub key: String,
    /// Byte offset to resume from; 0 for a fresh scan.
    pub offset: u64,
}

#[derive(Debug, Clone)]
pub struct PagedMatch {
    pub key: String,
    pub event: Value,
    pub fields: Vec<String>,
}

#[derive(Debug, Default)]
pub struct PagedScanResult {
    pub matches: Vec<PagedMatch>,
    /// Offsets for sessions not fully consumed — encode as next cursor.
    /// (#306: fully-consumed sessions are recorded too — see module docs.)
    pub offsets: BTreeMap<String, u64>,
    pub has_more: bool,
    pub malformed_lines: u64,
    pub duplicates_skipped: u64,
    /// Sessions whose stored offset was beyond the file size (rewritten).
    pub restarted: Vec<String>,
    /// Per-session file presence/size observed during this page.
    pub sessions: BTreeMap<String, SessionFileInfo>,
}

#[derive(Debug, Clone, Copy)]
pub struct SessionFileInfo {
    pub present: bool,
    pub size: u64,
}

/// Page matches across several session event logs in the given order.
/// Consumes up to `limit` matches total; each file resumes at its
/// stored offset so index appends between pages surface exactly once.
/// A stored offset beyond the current file size means the log was
/// rewritten (opencode rescan): the file restarts at 0 and is reported
/// in `restarted` — duplicates are possible and flagged, never hidden.
pub fn scan_paged_sessions(
    targets: &[ScanTarget],
    limit: usize,
    match_: &EventMatcher,
) -> Result<PagedScanResult, String> {
    let mut result = PagedScanResult::default();

    for (i, target) in targets.iter().enumerate() {
        if result.matches.len() >= limit {
            // Untouched targets stay unfinished at their stored offset.
            for rest in &targets[i..] {
                result.offsets.insert(rest.key.clone(), rest.offset);
            }
            result.has_more = true;
            break;
        }
        let mut target = target.clone();
        // A stored offset beyond the current file size means the log was
        // rewritten (e.g. opencode rescan): restart at 0 and flag it —
        // duplicates are possible and surfaced, never hidden.
        let path = events_file_path(&target.key);
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.is_file() && target.offset > meta.len() {
                result.restarted.push(target.key.clone());
                target.offset = 0;
            }
        }
        let page = read_event_page(
            &target.key,
            target.offset,
            limit - result.matches.len(),
            match_,
        )?;
        result.sessions.insert(
            target.key.clone(),
            SessionFileInfo {
                present: page.present,
                size: page.size,
            },
        );
        result.malformed_lines += page.malformed_lines;
        result.duplicates_skipped += page.duplicates_skipped;
        for m in page.matches {
            result.matches.push(PagedMatch {
                key: target.key.clone(),
                event: m.event,
                fields: m.fields,
            });
        }
        // #306: record the resume offset for EVERY processed session.
        // A fully-consumed file records next_offset == size; the next
        // page resumes there (cheap EOF check) and picks up appends.
        // Omitting it restarted the file at 0 → duplicate events.
        result.offsets.insert(target.key.clone(), page.next_offset);
        if page.has_more {
            result.has_more = true;
        }
    }

    Ok(result)
}

/// Ensure `events_file_path`'s parent exists is handled by callers;
/// this helper exists for tests that need the raw path.
#[allow(dead_code)]
pub fn event_log_path_for_test(key: &str) -> std::path::PathBuf {
    let _ = Path::new("");
    events_file_path(key)
}
