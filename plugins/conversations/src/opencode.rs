//! OpenCode transcript adapter (provider id `opencode`).
//!
//! Rust port of `plugins/conversations/src/opencode.ts` (retired; git
//! history preserves it).
//!
//! Storage (verified against opencode 2.x `opencode.db`, schema mirrored
//! from T3 `provider-opencode` usage reader + a live 2.x database):
//!
//!   Data root: $OPENCODE_DATA_DIR (comma-separated) else
//!   $XDG_DATA_HOME/opencode or ~/.local/share/opencode.
//!   Database:  <root>/opencode.db (preferred) or opencode-<name>.db —
//!   SQLite, WAL mode. Read-only access; the plugin never writes it.
//!
//!   v2 schema: `session_v2` (id, directory, time_created) +
//!   `session_message` (id, session_id, type, seq, time_created,
//!   time_updated, data). `data` is a JSON document; assistant rows
//!   embed their parts in `content[]` (text / reasoning / tool).
//!   v1 schema (same file, older sessions): `session` + `message`
//!   (data JSON with `role`) + `part` (per-part `data` JSON).
//!   Pre-SQLite file storage (`<root>/storage/**.json`) is NOT
//!   supported — reported as `unsupported_layout`, never guessed.
//!
//! Because SQLite rows are mutable (time_updated) and ordered by `seq`
//! rather than append-only bytes, this adapter does a FULL rescan of
//! the session on every index run and the caller rewrites the session's
//! event log. Dedup is structural: event ids derive from provider row
//! ids, and a rewrite replaces the whole file atomically.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OpenFlags};
use serde_json::{Map, Value};

use crate::claude::bounded;
use crate::model::{ConversationEvent, UsageFields};

/// Bound stored text/payload so the index stays navigational, not archival.
pub const OPENCODE_MAX_TEXT_CHARS: usize = 16_384;
pub const OPENCODE_MAX_INPUT_CHARS: usize = 8_192;

#[derive(Debug)]
pub enum OpenCodeResolution {
    Missing,
    UnsupportedLayout {
        data_dir: String,
    },
    SessionNotFound {
        db_path: String,
    },
    Ok {
        db_path: String,
        session_id: String,
        schema: OpenCodeSchema,
        directory: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenCodeSchema {
    V2,
    V1,
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|v| {
        let t = v.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    })
}

fn is_absolute(p: &str) -> bool {
    Path::new(p).is_absolute()
}

/// Candidate opencode data roots, env-driven (never ambient HOME guesses beyond default).
pub fn opencode_data_dirs() -> Vec<String> {
    if let Some(configured) = env_var("OPENCODE_DATA_DIR") {
        let dirs: Vec<String> = configured
            .split(',')
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
        if !dirs.is_empty() {
            return dirs;
        }
    }
    let base = match env_var("XDG_DATA_HOME") {
        Some(d) if is_absolute(&d) => PathBuf::from(d),
        _ => {
            let home = env_var("HOME").unwrap_or_else(|| "/".to_string());
            Path::new(&home).join(".local").join("share")
        }
    };
    vec![base.join("opencode").to_string_lossy().into_owned()]
}

fn file_exists(file: &Path) -> bool {
    std::fs::metadata(file)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

fn dir_exists(dir: &Path) -> bool {
    std::fs::metadata(dir).map(|m| m.is_dir()).unwrap_or(false)
}

fn is_db_name(name: &str) -> bool {
    // /^opencode(?:-[A-Za-z0-9_-]+)?\.db$/
    let Some(rest) = name.strip_prefix("opencode") else {
        return false;
    };
    let Some(stem) = rest.strip_suffix(".db") else {
        return false;
    };
    stem.is_empty()
        || (stem
            .strip_prefix('-')
            .map(|s| {
                !s.is_empty()
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            })
            .unwrap_or(false))
}

/// Approximation of JS `String.prototype.localeCompare` (ICU root
/// collation) for the `is_db_name` charset `[A-Za-z0-9._-]`: punctuation
/// (`-`, `_`, `.`) is "variable" and ignored at the primary level, and
/// case is a tertiary difference — so the primary key is the
/// lowercase-alphanumeric skeleton, with byte order as the tiebreak.
/// Exact quaternary punctuation weighting is not replicated; ordering
/// only matters when multiple opencode DBs coexist, and each scan
/// filters rows by `session_id` regardless.
fn db_name_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn primary(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect()
    }
    primary(a).cmp(&primary(b)).then_with(|| a.cmp(b))
}

/// All opencode databases under a data root, opencode.db first.
fn find_databases(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut candidates: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| is_db_name(n))
        .collect();
    candidates.sort_by(|a, b| {
        let rank = |n: &str| if n == "opencode.db" { 0 } else { 1 };
        rank(a).cmp(&rank(b)).then_with(|| db_name_cmp(a, b))
    });
    candidates
        .into_iter()
        .map(|n| root.join(n).to_string_lossy().into_owned())
        .filter(|p| file_exists(Path::new(p)))
        .collect()
}

struct SessionRow {
    id: String,
    directory: Option<String>,
}

fn open_db(db_path: &str) -> Option<Connection> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.busy_timeout(std::time::Duration::from_millis(250))
        .ok()?;
    Some(conn)
}

fn table_names(conn: &Connection) -> std::collections::HashSet<String> {
    let mut stmt = match conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table'") {
        Ok(s) => s,
        Err(_) => return Default::default(),
    };
    stmt.query_map([], |r| r.get::<_, String>(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

fn find_session(
    conn: &Connection,
    tables: &std::collections::HashSet<String>,
    session_id: &str,
) -> Option<SessionRow> {
    for table in ["session_v2", "session"] {
        if !tables.contains(table) {
            continue;
        }
        let sql = format!("SELECT id, directory FROM {} WHERE id = ?1 LIMIT 1", table);
        let row = conn
            .query_row(&sql, params![session_id], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                ))
            })
            .ok();
        if let Some((Some(id), directory)) = row {
            return Some(SessionRow { id, directory });
        }
    }
    None
}

/// Resolve the opencode transcript for a session id. `cwd` is accepted
/// for signature symmetry; opencode sessions are found by id in the
/// database, and `directory` is reported for consumers.
pub fn resolve_opencode_transcript(session_id: &str, _cwd: Option<&str>) -> OpenCodeResolution {
    // Defensive: a value that is already a readable SQLite file.
    if (session_id.ends_with(".db") || session_id.contains('/'))
        && file_exists(Path::new(session_id))
    {
        let Some(db) = open_db(session_id) else {
            return OpenCodeResolution::Missing;
        };
        let tables = table_names(&db);
        let Some(session) = find_session(&db, &tables, session_id) else {
            return OpenCodeResolution::SessionNotFound {
                db_path: session_id.to_string(),
            };
        };
        let schema = if tables.contains("session_v2") && tables.contains("session_message") {
            OpenCodeSchema::V2
        } else {
            OpenCodeSchema::V1
        };
        return OpenCodeResolution::Ok {
            db_path: session_id.to_string(),
            session_id: session.id,
            schema,
            directory: session.directory,
        };
    }

    let mut first_db_path: Option<String> = None;
    let mut legacy_dir: Option<String> = None;
    for root in opencode_data_dirs() {
        let root_path = PathBuf::from(&root);
        for db_path in find_databases(&root_path) {
            if first_db_path.is_none() {
                first_db_path = Some(db_path.clone());
            }
            let Some(db) = open_db(&db_path) else {
                continue;
            };
            let tables = table_names(&db);
            if let Some(session) = find_session(&db, &tables, session_id) {
                let schema = if tables.contains("session_v2") && tables.contains("session_message")
                {
                    OpenCodeSchema::V2
                } else {
                    OpenCodeSchema::V1
                };
                return OpenCodeResolution::Ok {
                    db_path,
                    session_id: session.id,
                    schema,
                    directory: session.directory,
                };
            }
        }
        // No usable database in this root: legacy file storage gets an
        // explicit layout verdict instead of a silent miss (checked only
        // after every root's databases came up empty for the session).
        if legacy_dir.is_none() && dir_exists(&root_path.join("storage")) {
            legacy_dir = Some(root);
        }
    }
    if let Some(db_path) = first_db_path {
        return OpenCodeResolution::SessionNotFound { db_path };
    }
    if let Some(data_dir) = legacy_dir {
        return OpenCodeResolution::UnsupportedLayout { data_dir };
    }
    OpenCodeResolution::Missing
}

// --- Normalization -----------------------------------------------------------

fn as_record(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn token_count(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f.is_finite() && f > 0.0 {
                f.floor() as i64
            } else {
                0
            }
        }
        _ => 0,
    }
}

fn epoch_iso(ms: Option<&Value>, fallback: Option<&Value>) -> Option<String> {
    if let Some(Value::Number(n)) = ms {
        let f = n.as_f64().unwrap_or(0.0);
        if f.is_finite() && f > 0.0 {
            return chrono::DateTime::from_timestamp_millis(f as i64)
                .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        }
    }
    let created = as_record(as_record(fallback?)?.get("time")?).and_then(|t| t.get("created"));
    if let Some(Value::Number(n)) = created {
        let f = n.as_f64().unwrap_or(0.0);
        if f.is_finite() && f > 0.0 {
            return chrono::DateTime::from_timestamp_millis(f as i64)
                .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        }
    }
    None
}

/// opencode tokens {input,output,reasoning,cache:{read,write}} → UsageFields.
fn usage_from_tokens(raw: Option<&Value>) -> Option<UsageFields> {
    let tokens = as_record(raw?)?;
    let cache = tokens.get("cache").and_then(as_record);
    let input = token_count(tokens.get("input"));
    let output = token_count(tokens.get("output"));
    let reasoning = token_count(tokens.get("reasoning"));
    let cache_read = token_count(cache.and_then(|c| c.get("read")));
    let cache_write = token_count(cache.and_then(|c| c.get("write")));
    if input + output + reasoning + cache_read + cache_write == 0 {
        return None;
    }
    let mut result = UsageFields {
        // output excludes reasoning in opencode; surface it inside output
        // (consistent with claude/codex where reasoning rides output) and
        // separately as thinking_tokens.
        input_tokens: input,
        output_tokens: output + reasoning,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_write,
        thinking_tokens: None,
    };
    if reasoning > 0 {
        result.thinking_tokens = Some(reasoning);
    }
    Some(result)
}

fn model_from(raw: Option<&Value>) -> Option<String> {
    let model = as_record(raw?)?;
    let id = model.get("id").and_then(|v| v.as_str())?;
    if id.is_empty() {
        return None;
    }
    let provider = model.get("providerID").and_then(|v| v.as_str());
    if let Some(p) = provider {
        if !p.is_empty() && !id.contains('/') {
            return Some(format!("{}/{}", p, id));
        }
    }
    Some(id.to_string())
}

fn cost_from(raw: Option<&Value>) -> Option<f64> {
    match raw {
        Some(Value::Number(n)) => {
            let f = n.as_f64()?;
            if f.is_finite() {
                Some(f)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn content_text(content: Option<&Value>) -> (String, bool) {
    match content {
        Some(Value::String(s)) => bounded(s, OPENCODE_MAX_TEXT_CHARS),
        _ => {
            let blocks = content.and_then(|c| c.as_array());
            let mut parts: Vec<String> = Vec::new();
            let mut truncated = false;
            if let Some(blocks) = blocks {
                for block in blocks {
                    let Some(b) = as_record(block) else { continue };
                    let ty = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if matches!(ty, "text" | "Text" | "input_text" | "output_text") {
                        if let Some(Value::String(text)) = b.get("text") {
                            let (part, t) = bounded(text, OPENCODE_MAX_TEXT_CHARS);
                            parts.push(part);
                            truncated = truncated || t;
                        }
                    }
                }
            }
            let joined = parts.join("\n");
            let (cap, t2) = bounded(&joined, OPENCODE_MAX_TEXT_CHARS);
            (cap, truncated || t2)
        }
    }
}

fn tool_input_from(raw: Option<&Value>) -> (Value, bool) {
    let json = match raw {
        Some(Value::String(s)) => s.clone(),
        Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
        None => "null".to_string(),
    };
    let (cap, truncated) = bounded(&json, OPENCODE_MAX_INPUT_CHARS);
    match serde_json::from_str::<Value>(&cap) {
        Ok(v) => (v, truncated),
        Err(_) => (Value::String(cap), truncated),
    }
}

/// opencode tool state → result text + error flag.
struct ToolResult {
    text: String,
    truncated: bool,
    is_error: bool,
    has_result: bool,
}

fn tool_result_from(state: Option<&Map<String, Value>>) -> ToolResult {
    let empty = ToolResult {
        text: String::new(),
        truncated: false,
        is_error: false,
        has_result: false,
    };
    let Some(state) = state else {
        return empty;
    };
    let status = state.get("status").and_then(|s| s.as_str()).unwrap_or("");
    let is_error = status == "error" || state.get("error").map(|e| !e.is_null()).unwrap_or(false);
    if status != "completed" && status != "error" {
        return ToolResult { is_error, ..empty };
    }
    // TS `state.output ?? state.content`: `??` falls through on
    // null/undefined — an explicit `output: null` must not shadow
    // `content`.
    let output = state
        .get("output")
        .filter(|v| !v.is_null())
        .or_else(|| state.get("content").filter(|v| !v.is_null()));
    let (text, truncated) = content_text(output);
    ToolResult {
        text,
        truncated,
        is_error,
        has_result: true,
    }
}

pub struct OpenCodeScanResult {
    pub events: Vec<ConversationEvent>,
    pub malformed_lines: u64,
    pub error: Option<String>,
    pub turn_count: i64,
}

struct RowCursor {
    identity: Value,
    turn_index: i64,
    seen_turn: bool,
}

impl RowCursor {
    fn current_turn(&self) -> Option<i64> {
        if self.seen_turn {
            Some(self.turn_index)
        } else {
            None
        }
    }

    fn base_event(
        &self,
        kind: &str,
        timestamp: Option<String>,
        event_id: String,
    ) -> ConversationEvent {
        ConversationEvent::base(
            kind,
            timestamp,
            self.identity.clone(),
            self.current_turn(),
            event_id,
        )
    }
}

#[derive(Clone)]
#[allow(dead_code)] // `seq` kept for parity/debugging; ordering uses ORDER BY.
struct MessageRow {
    rowid: i64,
    id: Option<String>,
    r#type: Option<String>,
    seq: Option<i64>,
    time_created: Option<f64>,
    data: String,
}

fn row_id(row: &MessageRow, prefix: &str) -> String {
    match &row.id {
        Some(id) if !id.is_empty() => id.clone(),
        _ => format!("{}-row-{}", prefix, row.rowid),
    }
}

fn parse_data(raw: &str, malformed: &mut u64) -> Option<Value> {
    // TS `JSON.parse` + `asRecord`: only a parse *throw* counts as
    // malformed; valid JSON that is not an object (array/scalar/null)
    // returns null silently — callers skip it via `as_object()`.
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => Some(v),
        Err(_) => {
            *malformed += 1;
            None
        }
    }
}

/// Full rescan of one opencode session → normalized events. Returns
/// `error` set when the database cannot be read; per-row JSON failures
/// increment `malformed_lines` and are skipped, never fatal.
pub fn scan_opencode_session(
    resolution: &OpenCodeResolution,
    identity: Value,
) -> OpenCodeScanResult {
    let empty = |error: &str| OpenCodeScanResult {
        events: Vec::new(),
        malformed_lines: 0,
        error: Some(error.to_string()),
        turn_count: 0,
    };

    let OpenCodeResolution::Ok {
        db_path,
        session_id,
        schema,
        ..
    } = resolution
    else {
        return empty("not_ok_resolution");
    };

    let Some(db) = open_db(db_path) else {
        return empty("sqlite_open_failed");
    };

    let mut events: Vec<ConversationEvent> = Vec::new();
    let mut malformed: u64 = 0;
    let mut ctx = RowCursor {
        identity,
        turn_index: 0,
        seen_turn: false,
    };

    let tables = table_names(&db);
    let scan_result: Result<(), String> = (|| {
        if *schema == OpenCodeSchema::V2 && tables.contains("session_message") {
            scan_v2(&db, session_id, &mut ctx, &mut events, &mut malformed)?;
        } else if tables.contains("message") {
            scan_v1(&db, session_id, &mut ctx, &mut events, &mut malformed)?;
        } else {
            return Err("sqlite_schema_unsupported".to_string());
        }
        Ok(())
    })();

    if let Err(err) = scan_result {
        return OpenCodeScanResult {
            events: Vec::new(),
            malformed_lines: malformed,
            error: Some(err),
            turn_count: 0,
        };
    }
    OpenCodeScanResult {
        events,
        malformed_lines: malformed,
        error: None,
        turn_count: ctx.turn_index,
    }
}

fn user_events(
    row: &MessageRow,
    data: &Map<String, Value>,
    ctx: &mut RowCursor,
    timestamp: Option<String>,
    events: &mut Vec<ConversationEvent>,
) {
    let id = row_id(row, "msg");
    ctx.turn_index += 1;
    ctx.seen_turn = true;
    events.push(ctx.base_event("turn", timestamp.clone(), format!("{}:turn", id)));
    let mut message = ctx.base_event("user_message", timestamp, id);
    // v2 user rows carry `text`; v1 rows carry text `part`s handled by
    // the caller — here `content`/`text` fields are the fallback.
    // TS `data.text ?? data.content`: `??` falls through on null too.
    let fallback = data
        .get("text")
        .filter(|v| !v.is_null())
        .or_else(|| data.get("content").filter(|v| !v.is_null()));
    let (text, truncated) = content_text(fallback);
    message.text = Some(text);
    if truncated {
        message.text_truncated = Some(true);
    }
    events.push(message);
}

#[allow(clippy::too_many_arguments)]
fn assistant_events(
    row: &MessageRow,
    data: &Map<String, Value>,
    ctx: &RowCursor,
    timestamp: Option<String>,
    events: &mut Vec<ConversationEvent>,
    part_text: Option<(String, bool)>,
) {
    let id = row_id(row, "msg");
    let mut event = ctx.base_event("assistant_message", timestamp.clone(), id.clone());
    let content = data.get("content").and_then(|c| c.as_array());
    let mut text_parts: Vec<String> = Vec::new();
    let mut text_truncated = false;
    let mut has_text = false;

    let mut tool_blocks: Vec<(usize, &Map<String, Value>)> = Vec::new();
    if let Some(content) = content {
        for (index, block) in content.iter().enumerate() {
            let Some(b) = as_record(block) else { continue };
            let ty = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if ty == "text" {
                if let Some(Value::String(text)) = b.get("text") {
                    has_text = true;
                    let (part, t) = bounded(text, OPENCODE_MAX_TEXT_CHARS);
                    text_parts.push(part);
                    text_truncated = text_truncated || t;
                }
            } else if ty == "tool" {
                tool_blocks.push((index, b));
            }
            // reasoning / step-* / file / snapshot / patch parts: provider
            // internals or bookkeeping — skipped, never surfaced as text.
        }
    }

    // v1 path: text arrives via `part` rows, not embedded content.
    if let Some((text, truncated)) = &part_text {
        if !text.is_empty() || *truncated {
            has_text = true;
            text_parts.push(text.clone());
            text_truncated = text_truncated || *truncated;
        }
    }

    let model = model_from(data.get("model"));
    let fallback_model = if model.is_none() {
        data.get("modelID")
            .and_then(|v| v.as_str())
            .map(|model_id| {
                let provider = data.get("providerID").and_then(|v| v.as_str());
                match provider {
                    Some(p) if !p.is_empty() && !model_id.contains('/') => {
                        format!("{}/{}", p, model_id)
                    }
                    _ => model_id.to_string(),
                }
            })
    } else {
        None
    };
    let usage = usage_from_tokens(data.get("tokens"));
    let cost = cost_from(data.get("cost"));
    let content_len = content.map(|c| c.len()).unwrap_or(0);

    if !has_text && usage.is_none() && cost.is_none() && content_len == 0 {
        return;
    }

    let joined = text_parts.join("\n");
    let (joined, joined_truncated) = bounded(&joined, OPENCODE_MAX_TEXT_CHARS);
    if !joined.is_empty() {
        event.text = Some(joined);
        if joined_truncated || text_truncated {
            event.text_truncated = Some(true);
        }
    }
    // TS `if (finalModel)` — falsy ("") is omitted; `model_from` never
    // yields "", but a bare empty `modelID` can reach here.
    if let Some(m) = model.or(fallback_model).filter(|m| !m.is_empty()) {
        event.model = Some(m);
    }
    if let Some(u) = usage {
        event.usage = Some(u);
    }
    if let Some(c) = cost {
        event.cost_usd = Some(c);
    }
    events.push(event);
    // Message first, then its tool children — same order as claude.
    for (index, tool) in tool_blocks {
        emit_tool(tool, index, ctx, &timestamp, events, &id);
    }
}

fn emit_tool(
    tool: &Map<String, Value>,
    index: usize,
    ctx: &RowCursor,
    timestamp: &Option<String>,
    events: &mut Vec<ConversationEvent>,
    message_id: &str,
) {
    let call_id = tool
        .get("callID")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            tool.get("callId")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            tool.get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{}:tool:{}", message_id, index));
    let state = tool.get("state").and_then(as_record);

    let mut call = ctx.base_event("tool_call", timestamp.clone(), format!("{}:call", call_id));
    call.tool_use_id = Some(call_id.clone());
    let name = tool
        .get("name")
        .and_then(|v| v.as_str())
        .or_else(|| tool.get("tool").and_then(|v| v.as_str()));
    if let Some(n) = name {
        if !n.is_empty() {
            call.tool_name = Some(n.to_string());
        }
    }
    let input_raw = state
        .and_then(|s| s.get("input"))
        .filter(|v| !v.is_null())
        .or_else(|| tool.get("input").filter(|v| !v.is_null()));
    let (input, truncated) = tool_input_from(input_raw);
    call.tool_input = Some(input);
    if truncated {
        call.input_truncated = Some(true);
    }
    events.push(call);

    let result = tool_result_from(state);
    if result.has_result {
        let mut res = ctx.base_event(
            "tool_result",
            timestamp.clone(),
            format!("{}:result", call_id),
        );
        res.tool_use_id = Some(call_id);
        res.text = Some(result.text);
        if result.truncated {
            res.text_truncated = Some(true);
        }
        if result.is_error {
            res.is_error = Some(true);
        }
        events.push(res);
    }
}

fn scan_v2(
    db: &Connection,
    session_id: &str,
    ctx: &mut RowCursor,
    events: &mut Vec<ConversationEvent>,
    malformed: &mut u64,
) -> Result<(), String> {
    let mut stmt = db
        .prepare(
            "SELECT rowid AS rowid, id, type, seq, time_created, data
             FROM session_message WHERE session_id = ?1 ORDER BY seq ASC, rowid ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![session_id], |r| {
            Ok(MessageRow {
                rowid: r.get(0)?,
                id: r.get(1)?,
                r#type: r.get(2)?,
                seq: r.get(3)?,
                time_created: r.get::<_, Option<f64>>(4).ok().flatten(),
                data: r.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?;

    for row in rows {
        let row = row.map_err(|e| e.to_string())?;
        let Some(data_value) = parse_data(&row.data, malformed) else {
            continue;
        };
        let Some(data) = data_value.as_object() else {
            continue;
        };
        let timestamp = epoch_iso(
            row.time_created.map(Value::from).as_ref(),
            Some(&data_value),
        );
        match row.r#type.as_deref() {
            Some("user") => user_events(&row, data, ctx, timestamp, events),
            Some("assistant") => assistant_events(&row, data, ctx, timestamp, events, None),
            Some("compaction") => {
                let id = row_id(&row, "msg");
                let mut event = ctx.base_event("system", timestamp, id);
                event.subtype = Some("compaction".to_string());
                // TS `if (summary)` — empty string is falsy and emits
                // no text at all.
                if let Some(Value::String(summary)) = data.get("summary") {
                    if !summary.is_empty() {
                        let (text, truncated) = bounded(summary, OPENCODE_MAX_TEXT_CHARS);
                        event.text = Some(text);
                        if truncated {
                            event.text_truncated = Some(true);
                        }
                    }
                }
                events.push(event);
            }
            Some("system") => {
                let id = row_id(&row, "msg");
                let mut event = ctx.base_event("system", timestamp, id);
                event.subtype = Some(
                    data.get("subtype")
                        .and_then(|s| s.as_str())
                        .unwrap_or("system")
                        .to_string(),
                );
                events.push(event);
            }
            // idle / location-switched / model-switched / synthetic / …:
            // lifecycle bookkeeping, not conversation — skipped.
            _ => {}
        }
    }
    Ok(())
}

fn scan_v1(
    db: &Connection,
    session_id: &str,
    ctx: &mut RowCursor,
    events: &mut Vec<ConversationEvent>,
    malformed: &mut u64,
) -> Result<(), String> {
    let mut stmt = db
        .prepare(
            "SELECT rowid AS rowid, id, NULL AS type, NULL AS seq, time_created, data
             FROM message WHERE session_id = ?1 ORDER BY time_created ASC, rowid ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![session_id], |r| {
            Ok(MessageRow {
                rowid: r.get(0)?,
                id: r.get(1)?,
                r#type: r.get(2)?,
                seq: r.get(3)?,
                time_created: r.get::<_, Option<f64>>(4).ok().flatten(),
                data: r.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let rows: Vec<MessageRow> = rows.collect::<Result<_, _>>().map_err(|e| e.to_string())?;

    let mut part_stmt = db
        .prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY time_created ASC, rowid ASC")
        .map_err(|e| e.to_string())?;

    for row in rows {
        let Some(data_value) = parse_data(&row.data, malformed) else {
            continue;
        };
        let Some(data) = data_value.as_object() else {
            continue;
        };
        let timestamp = epoch_iso(
            row.time_created.map(Value::from).as_ref(),
            Some(&data_value),
        );
        let role = data.get("role").and_then(|r| r.as_str());

        // Collect this message's parts once (text + tool), in order.
        let message_key = row.id.clone().unwrap_or_default();
        let part_rows = part_stmt
            .query_map(params![message_key], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut texts: Vec<String> = Vec::new();
        let mut text_truncated = false;
        let mut tool_parts: Vec<Value> = Vec::new();
        for part_row in part_rows {
            let part_data = part_row.map_err(|e| e.to_string())?;
            let Some(part) = parse_data(&part_data, malformed).and_then(|v| v.as_object().cloned())
            else {
                continue;
            };
            let ty = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if ty == "text" {
                if let Some(Value::String(text)) = part.get("text") {
                    let (t, tr) = bounded(text, OPENCODE_MAX_TEXT_CHARS);
                    texts.push(t);
                    text_truncated = text_truncated || tr;
                }
            } else if ty == "tool" {
                tool_parts.push(Value::Object(part));
            }
            // reasoning / step-start / step-finish / patch / … skipped.
        }
        let part_text = (texts.join("\n"), text_truncated);

        match role {
            Some("user") => {
                let id = row_id(&row, "msg");
                ctx.turn_index += 1;
                ctx.seen_turn = true;
                events.push(ctx.base_event("turn", timestamp.clone(), format!("{}:turn", id)));
                let mut message = ctx.base_event("user_message", timestamp, id);
                // TS: contentText(data.text ?? partText.text) — `??`
                // falls through on null/undefined too.
                let fallback = data
                    .get("text")
                    .filter(|v| !v.is_null())
                    .cloned()
                    .unwrap_or_else(|| Value::String(part_text.0.clone()));
                let (text, truncated) = content_text(Some(&fallback));
                message.text = Some(text);
                if truncated || part_text.1 {
                    message.text_truncated = Some(true);
                }
                events.push(message);
            }
            Some("assistant") => {
                // v1 assistant rows embed no content[]; synthesize one so
                // the shared assistant path handles text + tool parts
                // uniformly.
                let mut content: Vec<Value> = Vec::new();
                if !part_text.0.is_empty() {
                    content.push(serde_json::json!({ "type": "text", "text": part_text.0 }));
                }
                content.extend(tool_parts);
                let mut merged = data.clone();
                merged.insert("content".to_string(), Value::Array(content));
                assistant_events(&row, &merged, ctx, timestamp, events, None);
            }
            // other roles: provider bookkeeping — skipped.
            _ => {}
        }
    }
    Ok(())
}
