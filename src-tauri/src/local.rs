//! Usage on this Mac, built from the local scan cache only. Nothing here is sent anywhere.

use crate::pricing;
use crate::scan::FileEntry;
use chrono::{Datelike, Duration, Local, NaiveDate};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, Default, Serialize)]
pub struct ToolTotals {
    pub tool: &'static str,
    pub tokens: u64,
    pub cost: f64,
    pub calls: u64,
    pub sessions: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelShare {
    pub name: String,
    pub tool: String,
    pub share: u32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Period {
    pub tokens: u64,
    pub cost: f64,
    pub claude: u64,
    pub codex: u64,
    pub calls: u64,
    pub sessions: usize,
    pub active_hours: usize,
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub tools: Vec<ToolTotals>,
    pub models: Vec<ModelShare>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DayTokens {
    pub date: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Activity {
    pub from: String,
    pub to: String,
    pub days: Vec<DayTokens>,
}

/// The same numbers restricted to one tool, for the per-tool filter in the popover.
#[derive(Debug, Clone, Serialize)]
pub struct ToolView {
    pub periods: BTreeMap<String, Period>,
    pub activity: Activity,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalStats {
    pub today: String,
    pub periods: BTreeMap<String, Period>,
    pub activity: Activity,
    pub first_day: Option<String>,
    pub by_tool: BTreeMap<&'static str, ToolView>,
}

const ACTIVITY_WEEKS: i64 = 20;

/// Strip snapshot suffixes so dated model ids group under one name, like the server does.
fn model_family(model: &str) -> String {
    let m = model.to_lowercase();
    if let Some(base) = m.strip_suffix("-latest") {
        return base.to_string();
    }
    match m.rsplit_once('-') {
        Some((base, tail)) if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) => base.to_string(),
        _ => m,
    }
}

fn tool_key(tool: &str) -> &'static str {
    if tool == "codex" { "codex" } else { "claude" }
}

fn period(files: &HashMap<String, FileEntry>, from: &str, only: Option<&str>) -> Period {
    let mut p = Period::default();
    let mut sessions: HashSet<&str> = HashSet::new();
    let mut hours: HashSet<(&str, u8)> = HashSet::new();
    let mut tools: BTreeMap<&str, (ToolTotals, HashSet<&str>)> = BTreeMap::new();
    let mut models: BTreeMap<(String, String), f64> = BTreeMap::new();

    for f in files.values() {
        for d in f.days.values() {
            if d.date.as_str() < from || only.is_some_and(|t| t != tool_key(&d.tool)) {
                continue;
            }
            let tokens = d.input + d.output + d.cache_write + d.cache_read;
            let cost = pricing::cost(&d.tool, &d.model, d.input, d.output, d.cache_write, d.cache_read);
            p.tokens += tokens;
            p.cost += cost;
            p.calls += d.calls;
            p.input += d.input;
            p.output += d.output;
            p.cache_write += d.cache_write;
            p.cache_read += d.cache_read;
            if d.tool == "codex" {
                p.codex += tokens;
            } else {
                p.claude += tokens;
            }
            sessions.extend(d.sessions.iter().map(String::as_str));
            for h in d.hours.iter().flatten() {
                hours.insert((d.date.as_str(), *h));
            }
            let tool = tool_key(&d.tool);
            let (t, s) = tools.entry(tool).or_insert_with(|| (ToolTotals { tool, ..Default::default() }, HashSet::new()));
            t.tokens += tokens;
            t.cost += cost;
            t.calls += d.calls;
            s.extend(d.sessions.iter().map(String::as_str));
            *models.entry((d.tool.clone(), model_family(&d.model))).or_default() += cost;
        }
    }

    p.sessions = sessions.len();
    p.active_hours = hours.len();
    p.tools = tools
        .into_values()
        .map(|(mut t, s)| {
            t.sessions = s.len();
            t
        })
        .collect();
    p.tools.sort_by(|a, b| b.tokens.cmp(&a.tokens));

    let total: f64 = models.values().sum();
    if total > 0.0 {
        let mut shares: Vec<_> = models.into_iter().collect();
        shares.sort_by(|a, b| b.1.total_cmp(&a.1));
        p.models = shares
            .into_iter()
            .take(6)
            .map(|((tool, name), cost)| ModelShare { name, tool, share: (cost / total * 100.0).round() as u32 })
            .filter(|m| m.share > 0)
            .collect();
    }
    p
}

pub fn build(files: &HashMap<String, FileEntry>) -> LocalStats {
    build_for(files, Local::now().date_naive())
}

fn build_for(files: &HashMap<String, FileEntry>, today: NaiveDate) -> LocalStats {
    let fmt = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
    let week_start = today - Duration::days(today.weekday().num_days_from_sunday() as i64);
    let from = fmt(week_start - Duration::weeks(ACTIVITY_WEEKS - 1));

    let periods = |only: Option<&str>| -> BTreeMap<String, Period> {
        [1i64, 7, 30]
            .into_iter()
            .map(|days| (days.to_string(), period(files, &fmt(today - Duration::days(days - 1)), only)))
            .collect()
    };
    let activity = |only: Option<&str>| {
        let mut per_day: BTreeMap<String, u64> = BTreeMap::new();
        for f in files.values() {
            for d in f.days.values() {
                if d.date >= from && only.is_none_or(|t| t == tool_key(&d.tool)) {
                    *per_day.entry(d.date.clone()).or_default() += d.input + d.output + d.cache_write + d.cache_read;
                }
            }
        }
        Activity {
            from: from.clone(),
            to: fmt(today),
            days: per_day.into_iter().map(|(date, tokens)| DayTokens { date, tokens }).collect(),
        }
    };

    let first_day = files.values().flat_map(|f| f.days.values()).map(|d| d.date.clone()).min();
    let by_tool = ["claude", "codex"]
        .into_iter()
        .map(|t| (t, ToolView { periods: periods(Some(t)), activity: activity(Some(t)) }))
        .collect();

    LocalStats { today: fmt(today), periods: periods(None), activity: activity(None), first_day, by_tool }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::DayRollup;

    fn file(date: &str, tool: &str, model: &str, input: u64, session: &str) -> FileEntry {
        let day = DayRollup {
            date: date.into(),
            tool: tool.into(),
            model: model.into(),
            calls: 1,
            input,
            output: 0,
            cache_write: 0,
            cache_read: 0,
            reasoning: 0,
            sessions: vec![session.into()],
            hours: Some(vec![10]),
        };
        FileEntry { tool: tool.into(), size: 1, mtime_ms: 1.0, days: BTreeMap::from([("k".into(), day)]) }
    }

    #[test]
    fn periods_split_by_tool_and_price_locally() {
        let files = HashMap::from([
            ("a".into(), file("2026-10-07", "claude", "claude-opus-5-5-20260101", 1_000_000, "s1")),
            ("b".into(), file("2026-10-07", "codex", "gpt-5", 1_000_000, "s2")),
            ("c".into(), file("2026-10-01", "claude", "claude-opus-5-5", 2_000_000, "s1")),
        ]);
        let stats = build_for(&files, NaiveDate::from_ymd_opt(2026, 10, 7).unwrap());
        let today = &stats.periods["1"];
        assert_eq!((today.tokens, today.claude, today.codex, today.sessions), (2_000_000, 1_000_000, 1_000_000, 2));
        assert!((today.cost - 5.25).abs() < 1e-9);
        assert_eq!(today.models[0].name, "claude-opus-5-5");
        assert_eq!((today.models[0].share, today.models[1].share), (76, 24));
        assert_eq!(stats.periods["7"].tokens, 4_000_000);
        assert_eq!(stats.periods["7"].sessions, 2);
        assert_eq!(stats.activity.from, "2026-05-24");
        assert_eq!(stats.first_day.as_deref(), Some("2026-10-01"));
        let codex = &stats.by_tool["codex"];
        assert_eq!((codex.periods["7"].tokens, codex.periods["7"].sessions), (1_000_000, 1));
        assert_eq!(codex.periods["7"].models.len(), 1);
        assert_eq!(stats.by_tool["claude"].activity.days.len(), 2);
    }

    #[test]
    fn model_families_drop_snapshot_dates() {
        assert_eq!(model_family("claude-sonnet-4-5-20250929"), "claude-sonnet-4-5");
        assert_eq!(model_family("gpt-5-codex"), "gpt-5-codex");
        assert_eq!(model_family("o3-latest"), "o3");
    }
}
