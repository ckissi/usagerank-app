//! Reads Claude Code and Codex logs into daily totals. A port of the CLI's `src/scan.js`: same
//! sources, same de-duplication, same `state.json` rollups. Only dates, tool and model names,
//! hashed session ids and token counts leave this module — never prompts, code or paths.

use crate::config::Env;
use chrono::{DateTime, Local, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// One file's usage of one model on one day. `hours` (local hours with any call) is this app's
/// addition for the "per active hour" figure; the CLI ignores it and never sends it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayRollup {
    pub date: String,
    pub tool: String,
    pub model: String,
    pub calls: u64,
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub reasoning: u64,
    pub sessions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub tool: String,
    pub size: u64,
    #[serde(rename = "mtimeMs")]
    pub mtime_ms: f64,
    #[serde(default)]
    pub days: BTreeMap<String, DayRollup>,
}

/// `state.json`: per-file `{ size, mtimeMs, days }` keyed by a hash of the path.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub files: HashMap<String, FileEntry>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl State {
    pub fn load(file: &Path) -> Result<Self, String> {
        Ok(crate::config::read_json(file)?.unwrap_or_default())
    }

    pub fn save(&self, file: &Path) -> Result<(), String> {
        crate::config::write_json(file, self)
    }
}

/// A daily row as the server receives it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DayRow {
    pub date: String,
    pub tool: String,
    pub model: String,
    pub calls: u64,
    pub sessions: u64,
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub reasoning: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    pub key: &'static str,
    pub label: &'static str,
    pub dirs: Vec<String>,
}

pub struct ScanResult {
    pub files: HashMap<String, FileEntry>,
    pub touched: BTreeSet<String>,
    pub sources: Vec<SourceInfo>,
}

struct Record {
    tool: &'static str,
    session: String,
    date: String,
    hour: u8,
    model: String,
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
    reasoning: u64,
}

pub const SOURCES: [(&str, &str); 2] = [("claude", "Claude Code"), ("codex", "Codex")];

/// sha256 of the parts joined by NUL, first 32 hex chars — the CLI's `hash()`.
fn hash(parts: &[&str]) -> String {
    let digest = Sha256::digest(parts.join("\0").as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

pub fn source_dirs(env: &Env, key: &str) -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    match key {
        "claude" => match env.var("CLAUDE_CONFIG_DIR") {
            Some(v) => v.split(',').map(|r| PathBuf::from(r.trim()).join("projects")).collect(),
            None => vec![home.join(".claude").join("projects"), home.join(".config").join("claude").join("projects")],
        },
        _ => vec![env.var("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex")).join("sessions")],
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
        let kind = entry.file_type().map_err(|e| format!("Cannot read {}: {e}", entry.path().display()))?;
        let path = entry.path();
        if kind.is_dir() {
            walk(&path, out)?;
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
    Ok(())
}

fn num(v: &Value) -> u64 {
    v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)).unwrap_or(0)
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Calendar day and hour in the user's timezone, so late-night sessions land on the right day.
fn local_time(ts: &str) -> Option<(String, u8)> {
    let local = DateTime::parse_from_rfc3339(ts).ok()?.with_timezone(&Local);
    Some((local.format("%Y-%m-%d").to_string(), local.hour() as u8))
}

fn json_lines(file: &Path, mut keep: impl FnMut(&str) -> bool, mut each: impl FnMut(Value)) -> Result<(), String> {
    let f = fs::File::open(file).map_err(|e| format!("Cannot open {}: {e}", file.display()))?;
    for line in BufReader::new(f).lines() {
        let line = line.map_err(|e| format!("Cannot read {}: {e}", file.display()))?;
        if line.is_empty() || !keep(&line) {
            continue;
        }
        // A partially written line from a live session simply fails to parse.
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            each(v);
        }
    }
    Ok(())
}

/// Claude Code: one `assistant` line per content block, all sharing message.id + requestId and the
/// same usage, so only the first one per message counts.
fn parse_claude(file: &Path, emit: &mut Vec<Record>) -> Result<(), String> {
    let file_str = file.to_string_lossy().to_string();
    let mut seen = HashSet::new();
    json_lines(
        file,
        |line| line.contains("\"usage\""),
        |l| {
            if str_of(&l, "type") != Some("assistant") {
                return;
            }
            let Some(msg) = l.get("message") else { return };
            let Some(u) = msg.get("usage").filter(|u| u.is_object()) else { return };
            let Some(ts) = str_of(&l, "timestamp") else { return };
            let Some(model) = str_of(msg, "model").filter(|m| *m != "<synthetic>") else { return };
            let id = str_of(msg, "id").or_else(|| str_of(&l, "uuid")).unwrap_or("undefined");
            if !seen.insert(format!("{id}:{}", str_of(&l, "requestId").unwrap_or(""))) {
                return;
            }
            let Some((date, hour)) = local_time(ts) else { return };
            emit.push(Record {
                tool: "claude",
                session: hash(&["claude", str_of(&l, "sessionId").unwrap_or(&file_str)]),
                date,
                hour,
                model: model.to_string(),
                input: num(&u["input_tokens"]),
                output: num(&u["output_tokens"]),
                cache_write: num(&u["cache_creation_input_tokens"]),
                cache_read: num(&u["cache_read_input_tokens"]),
                reasoning: u.get("output_tokens_details").map(|d| num(&d["thinking_tokens"])).unwrap_or(0),
            });
        },
    )
}

/// Codex: newer versions write a `token_usage_record` per response; older ones only `token_count`
/// events (sometimes re-emitted with an unchanged total). Prefer the former when the file has any.
/// Codex counts cached tokens inside input_tokens; they're split out to match Claude's fields.
fn parse_codex(file: &Path, emit: &mut Vec<Record>) -> Result<(), String> {
    let mut session = file.to_string_lossy().to_string();
    let mut model = "unknown".to_string();
    let mut last_total: i64 = -1;
    let mut records = Vec::new();
    let mut counts = Vec::new();

    let row = |u: &Value, ts: &str, session: &str, model: &str| -> Option<Record> {
        let (date, hour) = local_time(ts)?;
        let cached = num(&u["cached_input_tokens"]);
        Some(Record {
            tool: "codex",
            session: hash(&["codex", session]),
            date,
            hour,
            model: model.to_string(),
            input: num(&u["input_tokens"]).saturating_sub(cached),
            output: num(&u["output_tokens"]),
            cache_write: num(&u["cache_write_input_tokens"]),
            cache_read: cached,
            reasoning: num(&u["reasoning_output_tokens"]),
        })
    };

    json_lines(
        file,
        |line| {
            line.contains("session_meta") || line.contains("turn_context") || line.contains("token_usage_record") || line.contains("token_count")
        },
        |l| {
            let empty = Value::Object(Map::new());
            let p = l.get("payload").unwrap_or(&empty);
            let ts = str_of(&l, "timestamp").unwrap_or("");
            match str_of(&l, "type") {
                Some("session_meta") => {
                    if let Some(id) = str_of(p, "id").or_else(|| str_of(p, "session_id")) {
                        session = id.to_string();
                    }
                }
                Some("turn_context") => {
                    if let Some(m) = str_of(p, "model") {
                        model = m.to_string();
                    }
                }
                Some("token_usage_record") => {
                    if let Some(u) = p.get("usage").filter(|u| u.is_object()) {
                        records.extend(row(u, ts, &session, &model));
                    }
                }
                Some("event_msg") if str_of(p, "type") == Some("token_count") => {
                    let Some(info) = p.get("info") else { return };
                    let Some(last) = info.get("last_token_usage").filter(|u| u.is_object()) else { return };
                    let total = info
                        .get("total_token_usage")
                        .and_then(|t| t.get("total_tokens"))
                        .and_then(Value::as_i64)
                        .unwrap_or(-1);
                    if total == last_total {
                        return;
                    }
                    last_total = total;
                    counts.extend(row(last, ts, &session, &model));
                }
                _ => {}
            }
        },
    )?;
    emit.extend(if records.is_empty() { counts } else { records });
    Ok(())
}

/// Folds one file's calls into daily rollups keyed by date + tool + model.
fn aggregate(records: Vec<Record>) -> BTreeMap<String, DayRollup> {
    let mut days: BTreeMap<String, DayRollup> = BTreeMap::new();
    for r in records {
        let key = format!("{}|{}|{}", r.date, r.tool, r.model);
        let d = days.entry(key).or_insert_with(|| DayRollup {
            date: r.date.clone(),
            tool: r.tool.to_string(),
            model: r.model.clone(),
            calls: 0,
            input: 0,
            output: 0,
            cache_write: 0,
            cache_read: 0,
            reasoning: 0,
            sessions: Vec::new(),
            hours: Some(Vec::new()),
        });
        d.calls += 1;
        d.input += r.input;
        d.output += r.output;
        d.cache_write += r.cache_write;
        d.cache_read += r.cache_read;
        d.reasoning += r.reasoning;
        if !d.sessions.contains(&r.session) {
            d.sessions.push(r.session);
        }
        let hours = d.hours.get_or_insert_with(Vec::new);
        if !hours.contains(&r.hour) {
            hours.push(r.hour);
        }
    }
    days
}

fn mtime_ms(meta: &fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as f64 * 1e3 + d.subsec_nanos() as f64 / 1e6)
        .unwrap_or(0.0)
}

/// Re-reads every log file that changed since the last scan and keeps a per-file daily rollup.
/// Files that disappeared (Claude Code prunes old transcripts) keep their rollup, so history is
/// never lost. Returns the new file state and the dates whose totals changed.
///
/// `require_hours` re-reads files whose rollups have no `hours` (written by the CLI); the local
/// view needs them, pushing doesn't.
pub fn scan(env: &Env, only: Option<&str>, state: &State, all: bool, require_hours: bool) -> Result<ScanResult, String> {
    let mut files = state.files.clone();
    let mut touched = BTreeSet::new();
    let mut sources = Vec::new();

    for (key, label) in SOURCES {
        if only.is_some_and(|o| o != key) {
            continue;
        }
        let mut dirs = Vec::new();
        for dir in source_dirs(env, key) {
            match fs::metadata(&dir) {
                Ok(_) => dirs.push(dir),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                Err(e) => return Err(format!("Cannot read {}: {e}", dir.display())),
            }
        }
        sources.push(SourceInfo { key, label, dirs: dirs.iter().map(|d| d.to_string_lossy().to_string()).collect() });

        let mut paths = Vec::new();
        for dir in &dirs {
            walk(dir, &mut paths)?;
        }
        for file in paths {
            let meta = fs::metadata(&file).map_err(|e| format!("Cannot read {}: {e}", file.display()))?;
            let fkey = hash(&[&file.to_string_lossy()]);
            let size = meta.len();
            let mtime = mtime_ms(&meta);
            let prev = files.get(&fkey);

            let unchanged = prev.is_some_and(|p| {
                p.size == size
                    && (p.mtime_ms - mtime).abs() < 0.01
                    && (!require_hours || p.days.values().all(|d| d.hours.is_some()))
            });
            if unchanged {
                if all {
                    touched.extend(prev.into_iter().flat_map(|p| p.days.values().map(|d| d.date.clone())));
                }
                continue;
            }

            let mut records = Vec::new();
            if key == "claude" {
                parse_claude(&file, &mut records)?;
            } else {
                parse_codex(&file, &mut records)?;
            }
            let days = aggregate(records);
            if let Some(p) = prev {
                touched.extend(p.days.values().map(|d| d.date.clone()));
            }
            touched.extend(days.values().map(|d| d.date.clone()));
            files.insert(fkey, FileEntry { tool: key.to_string(), size, mtime_ms: mtime, days });
        }
    }

    Ok(ScanResult { files, touched, sources })
}

/// Daily totals per date + tool + model across every file, limited to `dates`.
pub fn daily_rows(files: &HashMap<String, FileEntry>, dates: &BTreeSet<String>, only: Option<&str>) -> Vec<DayRow> {
    let mut rows: BTreeMap<(String, String, String), (DayRow, HashSet<String>)> = BTreeMap::new();
    for f in files.values() {
        for d in f.days.values() {
            if !dates.contains(&d.date) || only.is_some_and(|o| o != d.tool) {
                continue;
            }
            let (row, sessions) = rows.entry((d.date.clone(), d.tool.clone(), d.model.clone())).or_insert_with(|| {
                (
                    DayRow {
                        date: d.date.clone(),
                        tool: d.tool.clone(),
                        model: d.model.clone(),
                        calls: 0,
                        sessions: 0,
                        input: 0,
                        output: 0,
                        cache_write: 0,
                        cache_read: 0,
                        reasoning: 0,
                    },
                    HashSet::new(),
                )
            });
            row.calls += d.calls;
            row.input += d.input;
            row.output += d.output;
            row.cache_write += d.cache_write;
            row.cache_read += d.cache_read;
            row.reasoning += d.reasoning;
            sessions.extend(d.sessions.iter().cloned());
        }
    }
    rows.into_values().map(|(mut row, sessions)| {
        row.sessions = sessions.len() as u64;
        row
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("usagerank-scan-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, lines: &[&str]) {
        let mut f = fs::File::create(path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn hash_matches_the_cli() {
        // node -e "console.log(require('crypto').createHash('sha256').update(['claude','abc'].join('\0')).digest('hex').slice(0,32))"
        assert_eq!(hash(&["claude", "abc"]).len(), 32);
        assert_eq!(hash(&["a"]), "ca978112ca1bbdcafac231b39a23dc4d");
    }

    #[test]
    fn claude_messages_are_counted_once() {
        let dir = temp_dir();
        let file = dir.join("s.jsonl");
        let msg = r#"{"type":"assistant","timestamp":"2026-10-05T10:00:00Z","sessionId":"s1","requestId":"r1","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":30,"cache_read_input_tokens":40}}}"#;
        let synthetic = r#"{"type":"assistant","timestamp":"2026-10-05T10:00:00Z","message":{"id":"m2","model":"<synthetic>","usage":{"input_tokens":1}}}"#;
        write(&file, &[msg, msg, synthetic, "{\"usage\": partial"]);

        let mut records = Vec::new();
        parse_claude(&file, &mut records).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!((records[0].input, records[0].output, records[0].cache_write, records[0].cache_read), (10, 20, 30, 40));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_prefers_usage_records_and_splits_cache() {
        let dir = temp_dir();
        let file = dir.join("c.jsonl");
        write(
            &file,
            &[
                r#"{"type":"session_meta","payload":{"id":"sess"}}"#,
                r#"{"type":"turn_context","payload":{"model":"gpt-5-codex"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-10-05T10:00:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":5},"last_token_usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":5}}}}"#,
                r#"{"type":"token_usage_record","timestamp":"2026-10-05T10:00:00Z","payload":{"response_id":"resp_1","usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":5,"reasoning_output_tokens":2}}}"#,
            ],
        );
        let mut records = Vec::new();
        parse_codex(&file, &mut records).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!((records[0].input, records[0].cache_read, records[0].reasoning), (40, 60, 2));
        assert_eq!(records[0].model, "gpt-5-codex");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_skips_repeated_token_counts() {
        let dir = temp_dir();
        let file = dir.join("c.jsonl");
        let count = r#"{"type":"event_msg","timestamp":"2026-10-05T10:00:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":5},"last_token_usage":{"input_tokens":1,"output_tokens":1}}}}"#;
        write(&file, &[count, count]);
        let mut records = Vec::new();
        parse_codex(&file, &mut records).unwrap();
        assert_eq!(records.len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn daily_rows_merge_files_without_double_counting_sessions() {
        let day = |sessions: &[&str]| DayRollup {
            date: "2026-10-05".into(),
            tool: "claude".into(),
            model: "m".into(),
            calls: 2,
            input: 1,
            output: 1,
            cache_write: 0,
            cache_read: 0,
            reasoning: 0,
            sessions: sessions.iter().map(|s| s.to_string()).collect(),
            hours: Some(vec![9, 10]),
        };
        let mut files = HashMap::new();
        for (k, s) in [("a", vec!["s1"]), ("b", vec!["s1", "s2"])] {
            files.insert(
                k.to_string(),
                FileEntry { tool: "claude".into(), size: 1, mtime_ms: 1.0, days: BTreeMap::from([("x".into(), day(&s))]) },
            );
        }
        let rows = daily_rows(&files, &BTreeSet::from(["2026-10-05".to_string()]), None);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].calls, rows[0].sessions), (4, 2));
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn failed_log_read_preserves_saved_totals_and_retries() {
        let root = std::env::temp_dir().join(format!("ur-errors-{}", uuid::Uuid::new_v4()));
        let env = crate::config::test_env(&root);
        let dir = root.join("claude/projects");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("test.jsonl");
        let row = |id: &str| serde_json::json!({"type":"assistant","timestamp":"2026-10-07T10:00:00Z","message":{"id":id,"model":"claude-sonnet-4","usage":{"input_tokens":10}}}).to_string();
        let good = format!("{}\n{}\n", row("one"), row("two"));
        fs::write(&file, &good).unwrap();
        let scanned = scan(&env, Some("claude"), &State::default(), false, true).unwrap();
        let state = State { files: scanned.files, extra: Default::default() };
        state.save(&env.local_cache_path()).unwrap();
        let saved = fs::read(env.local_cache_path()).unwrap();
        let mut broken = format!("{}\n", row("one")).into_bytes();
        broken.extend([0xff, b'\n']);
        broken.extend(format!("{}\n", row("two")).into_bytes());
        fs::write(&file, broken).unwrap();
        assert!(scan(&env, Some("claude"), &state, false, true).is_err());
        assert_eq!(fs::read(env.local_cache_path()).unwrap(), saved);
        fs::write(&file, format!("{good}{}\n", row("three"))).unwrap();
        let retried = scan(&env, Some("claude"), &state, false, true).unwrap();
        let calls: u64 = retried.files.values().flat_map(|f| f.days.values()).map(|d| d.calls).sum();
        assert_eq!(calls, 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_file_is_an_error_for_parser() {
        let file = std::env::temp_dir().join(format!("ur-missing-{}", uuid::Uuid::new_v4()));
        assert!(parse_claude(&file, &mut Vec::new()).is_err());
        assert!(parse_codex(&file, &mut Vec::new()).is_err());
    }
}
