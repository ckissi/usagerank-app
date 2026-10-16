// UsageRank popover. Renders the snapshot the Rust side emits; every action is a Tauri command.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const card = document.getElementById('card');
const shell = document.getElementById('shell');

const PERIODS = [
  { key: '1', label: 'Today', word: 'today', note: 'since yesterday' },
  { key: '7', label: '7d', word: '7d', note: 'vs last week' },
  { key: '30', label: '30d', word: '30d', note: 'vs last month' },
];
const INTERVALS = [[15, '15 min'], [30, '30 min'], [60, '1 hour'], [180, '3 hours'], [360, '6 hours']];
const DISPLAYS = [['tokens', 'Tokens'], ['cost', 'Spend'], ['rank', 'Rank'], ['limits', 'Limits'], ['icon', 'Icon']];
const WEEKDAYS = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

const store = {
  get(key, fallback) {
    try { return localStorage.getItem(key) ?? fallback; } catch { return fallback; }
  },
  set(key, value) {
    try { localStorage.setItem(key, value); } catch {}
    void handle(() => invoke('set_ui_pref', { key, value: String(value) }));
  },
};

// app.json is the durable copy (localStorage doesn't reliably survive a relaunch); apply it once.
let prefsApplied = false;
function applyPrefs(prefs) {
  if (prefsApplied || !prefs) return;
  prefsApplied = true;
  if (['local', 'limits', 'share'].includes(prefs.tab)) ui.tab = prefs.tab;
  if (PERIODS.some((p) => p.key === prefs.period)) ui.period = prefs.period;
  if (['', 'claude', 'codex'].includes(prefs.tool)) ui.tool = prefs.tool || null;
  if (typeof prefs.expanded === 'string') ui.expanded = new Set(prefs.expanded.split(',').filter((key) => ['claude', 'codex'].includes(key)));
}

const ui = {
  view: 'main', // main | prefs
  tab: store.get('tab', 'local'), // local | limits | share
  period: store.get('period', '1'),
  tool: store.get('tool', '') || null, // null = all tools; 'claude' | 'codex' filters This Mac
  handleDraft: null,
  keepLogs: false,
  busy: null, // name of the action in flight
  message: null, // { text, error } shown in preferences / onboarding
  confirmForget: false,
  laterUpdate: null, // version whose banner was dismissed until the next launch
  expanded: new Set(store.get('expanded', '').split(',').filter(Boolean)), // providers showing every limit
};

let snap = null;
const objectList = (value) => Array.isArray(value)
  ? value.filter((item) => item && typeof item === 'object' && !Array.isArray(item))
  : [];

function acceptSnapshot(value) {
  if (!value || typeof value !== 'object' || !value.settings || typeof value.settings !== 'object') {
    throw new Error('The app sent an invalid snapshot. Try again.');
  }
  snap = value;
  applyPrefs(snap.ui);
  render();
}

function reportError(error) {
  console.error(error);
  flash(error instanceof Error ? error.message : String(error), true);
}

function handle(task) {
  return Promise.resolve().then(task).catch(reportError);
}


// ── formatting ─────────────────────────────────────────────────────────────

const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

function fmtTokens(n) {
  n = Number(n) || 0;
  if (n >= 1e9) return `${(n / 1e9).toFixed(2)}B`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}K`;
  return `${Math.round(n)}`;
}

function fmtCost(n) {
  n = Number(n) || 0;
  if (n >= 100) return `$${Math.round(n).toLocaleString('en-US')}`;
  return `$${n.toFixed(2)}`;
}

const fmtInt = (n) => Number(n || 0).toLocaleString('en-US');

function ago(ms) {
  if (!ms) return null;
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 45) return 'just now';
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}

const parseDate = (iso) => {
  const [y, m, d] = String(iso).split('-').map(Number);
  return new Date(y, m - 1, d);
};
const isoDate = (d) => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
const toolBadge = (tool) => (tool === 'codex' ? 'CX' : 'CC');

// Mini brand marks shown inside .tool-badge, before the CC/CX label.
const TOOL_LOGO = {
  claude: '<svg class="tool-logo" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" aria-hidden="true"><path d="M12.4 9.8L14 2.2M13.5 10.4L17.7 6.1M14.1 11.5L21.2 9.9M14.1 12.7L19.2 14.6M13.3 13.7L18.1 19.9M12.2 14.2L12.7 20.6M11 13.9L7.8 20M10.1 13.1L5.2 15.8M9.8 11.9L2.2 11.4M10.2 10.7L5.2 7.1M11.2 9.9L8.7 3.4"/></svg>',
  codex: '<svg class="tool-logo" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z"/></svg>',
};
const toolBadgeHtml = (tool) => `${TOOL_LOGO[tool === 'codex' ? 'codex' : 'claude']}${toolBadge(tool)}`;

// ── icons ──────────────────────────────────────────────────────────────────

const ICON = {
  gear: '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>',
  out: '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M7 17 17 7"/><path d="M8 7h9v9"/></svg>',
  chevronRight: '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m9 18 6-6-6-6"/></svg>',
  caret: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m6 9 6 6 6-6"/></svg>',
  chevron: '<svg class="chev" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m9 18 6-6-6-6"/></svg>',
};

// ── shared pieces ──────────────────────────────────────────────────────────

function statusView() {
  if (snap.syncing) return '<div class="status"><span class="dot pulse"></span><span>Syncing</span></div>';
  if (ui.tab === 'limits') {
    if (snap.limitsFetching) return '<div class="status"><span class="dot pulse"></span><span>Checking</span></div>';
    const at = Math.max(snap.limits?.claude?.fetchedAt ?? 0, snap.limits?.codex?.fetchedAt ?? 0);
    const when = ago(at);
    return when
      ? `<div class="status" title="Read live from Anthropic and OpenAI with your Claude Code and Codex logins."><span class="dot"></span><span>Updated ${when}</span></div>`
      : '<div class="status"><span class="dot idle"></span><span>Not checked yet</span></div>';
  }
  if (ui.tab === 'local' || !snap.signedIn) {
    if (snap.scanning && !snap.local) return '<div class="status"><span class="dot pulse"></span><span>Reading logs</span></div>';
    return '<div class="status" title="Read from the logs on this Mac. Nothing is sent unless you share."><span class="dot idle"></span><span>Local only</span></div>';
  }
  if (snap.lastError) return `<div class="status" title="${esc(snap.lastError)}"><span class="dot error"></span><span>Sync failed</span></div>`;
  const when = ago(snap.lastSyncAt);
  if (!when) return '<div class="status"><span class="dot idle"></span><span>Not synced yet</span></div>';
  return `<div class="status"><span class="dot"></span><span>Synced ${when}</span></div>`;
}

function header() {
  return `<header class="head">
    <div class="logo" aria-label="UsageRank">UsageRank<span>.</span></div>
    <div class="head-right">
      ${statusView()}
      <button type="button" class="icon-btn" data-act="prefs" aria-label="Preferences" aria-pressed="${ui.view === 'prefs'}" title="Preferences (⌘,)">${ICON.gear}</button>
    </div>
  </header>
  <nav class="tabs" role="tablist" aria-label="View">
    <button type="button" role="tab" data-tab="local" aria-selected="${ui.tab === 'local'}">This Mac</button>
    <button type="button" role="tab" data-tab="limits" aria-selected="${ui.tab === 'limits'}">Limits</button>
    <button type="button" role="tab" data-tab="share" aria-selected="${ui.tab === 'share'}">Leaderboard${snap.signedIn ? '' : '<span class="tab-note">off</span>'}</button>
  </nav>`;
}

const keys = (...k) => `<span class="keys">${k.map((x) => `<kbd>${x}</kbd>`).join('')}</span>`;

function periodToolbar(right) {
  return `<div class="toolbar">
    <div class="seg" role="group" aria-label="Period">
      ${PERIODS.map((x) => `<button type="button" data-period="${x.key}" aria-pressed="${x.key === ui.period}">${x.label}</button>`).join('')}
    </div>
    ${right}
  </div>`;
}

function tilesView(d, p, { perHour, tokenNote } = {}) {
  const tokens = d?.tokens ?? 0;
  const cc = tokens > 0 ? Math.round(((d.claude ?? 0) / tokens) * 100) : null;
  return `<div class="tiles">
    <div class="tile">
      <div class="label">Tokens · ${p.word}</div>
      <div class="big">${d ? fmtTokens(tokens) : '<span class="skeleton">—</span>'}</div>
      <div class="sub">${tokenNote ?? (cc === null ? 'CC — · CX —' : `CC ${cc}% · CX ${100 - cc}%`)}</div>
    </div>
    <div class="tile">
      <div class="label">Spent · ${p.word}</div>
      <div class="big">${d ? fmtCost(d.cost) : '<span class="skeleton">—</span>'}</div>
      <div class="sub" title="At list price">${perHour ?? 'at list price'}</div>
    </div>
  </div>`;
}

function heatmapView(source) {
  const act = source?.activity;
  const today = source?.today ? parseDate(source.today) : new Date();
  const from = act?.from ? parseDate(act.from) : (() => {
    const d = new Date(today);
    d.setDate(d.getDate() - d.getDay() - 19 * 7);
    return d;
  })();
  const byDate = new Map(objectList(act?.days).map((d) => [d.date, d.tokens]));

  // Quartiles of active days, so one huge day doesn't wash out the rest.
  const values = [...byDate.values()].filter((v) => v > 0).sort((a, b) => a - b);
  const q = (f) => values[Math.min(values.length - 1, Math.floor(values.length * f))] ?? 0;
  const cuts = [q(0.25), q(0.5), q(0.75)];
  const level = (v) => (v <= 0 ? 0 : v <= cuts[0] ? 1 : v <= cuts[1] ? 2 : v <= cuts[2] ? 3 : 4);

  const todayIso = isoDate(today);
  const cells = [];
  const months = [];
  let total = 0;
  let lastLabelCol = -9;
  for (let col = 0; col < 20; col++) {
    for (let row = 0; row < 7; row++) {
      const day = new Date(from);
      day.setDate(from.getDate() + col * 7 + row);
      const iso = isoDate(day);
      const tokens = byDate.get(iso) ?? 0;
      total += tokens;
      if (row === 0) {
        const prevMonth = new Date(day);
        prevMonth.setDate(day.getDate() - 7);
        const starts = col === 0 ? day.getDate() <= 7 : prevMonth.getMonth() !== day.getMonth();
        if (starts && col - lastLabelCol >= 3) {
          months.push(`<span style="grid-column: ${col + 1}">${MONTHS[day.getMonth()]}</span>`);
          lastLabelCol = col;
        }
      }
      const cls = iso > todayIso ? 'future' : `l${level(tokens)}${iso === todayIso ? ' today' : ''}`;
      const date = `${WEEKDAYS[day.getDay()]}, ${MONTHS[day.getMonth()]} ${day.getDate()}${iso === todayIso ? ' · today' : ''}`;
      const value = iso > todayIso ? 'Upcoming' : tokens > 0 ? `${fmtTokens(tokens)} tokens` : 'No usage';
      cells.push(`<i class="cell ${cls}" data-tip="${esc(value)}" data-tip-title="${esc(date)}"></i>`);
    }
  }

  return `<section class="section" aria-label="Activity">
    <div class="label"><span>Activity</span><span class="rule"></span><span>20 wk</span></div>
    <div class="heat">
      <span></span>
      <div class="heat-months">${months.join('')}</div>
      <div class="heat-days"><span></span><span>Mon</span><span></span><span>Wed</span><span></span><span>Fri</span><span></span></div>
      <div class="heat-grid">${cells.join('')}</div>
    </div>
    <div class="heat-foot">
      <span class="num">${fmtTokens(total)} tokens since ${MONTHS[from.getMonth()]}</span>
      <span class="legend">less ${[0, 1, 2, 3, 4].map((l) => `<i style="background: var(--h${l})"></i>`).join('')} more</span>
    </div>
  </section>`;
}

function modelsView(d, p) {
  const list = objectList(d?.models);
  const top = list.slice(0, 3);
  const used = top.reduce((s, m) => s + m.share, 0);
  if (list.length > 3 || (list.length && used < 100)) top.push({ name: 'other', share: Math.max(0, 100 - used) });
  const shown = top.filter((m) => m.share > 0);
  const swatch = (i) => `var(--h${4 - Math.min(i, 3)})`;

  return `<section class="section" aria-label="By model">
    <div class="label"><span>By model</span><span class="rule"></span><span>${p.word}</span></div>
    <div class="bar">${shown.map((m, i) => `<span style="width: ${m.share}%; background: ${swatch(i)}" data-tip-title="${esc(m.name)}" data-tip="${m.share}% of spend"></span>`).join('')}</div>
    ${shown.length
      ? `<div class="models">${shown.map((m, i) => `<span data-tip-title="${esc(m.name)}" data-tip="${m.share}% of spend"><i style="background: ${swatch(i)}"></i><span class="name">${esc(m.name)}</span><span class="muted num">${m.share}%</span></span>`).join('')}</div>`
      : '<p class="hint">No spend in this period yet.</p>'}
  </section>`;
}

const currentPeriod = () => PERIODS.find((x) => x.key === ui.period) ?? PERIODS[0];

// ── This Mac (local logs only) ─────────────────────────────────────────────

const TOOL_NAMES = { claude: 'Claude Code', codex: 'Codex' };

function breakdownView(d, p, all) {
  const sessions = d?.sessions ?? 0;
  // Rows always list every tool (from the unfiltered period) so the filter can be switched or cleared.
  const rows = objectList(all?.tools)
    .map((t) => {
      const on = ui.tool === t.tool;
      return `<li><button type="button" class="tool-row${on ? ' on' : ''}" data-tool="${esc(t.tool)}" aria-pressed="${on}" data-tip="${on ? 'Show all tools' : `Show only ${esc(TOOL_NAMES[t.tool] ?? t.tool)}`}">
      <span class="tool-badge">${toolBadgeHtml(t.tool)}</span>
      <span class="who"><span>${TOOL_NAMES[t.tool] ?? t.tool}</span><span class="faint">${fmtInt(t.sessions)} sess</span></span>
      <span class="r">${fmtCost(t.cost)}</span>
      <span class="t">${fmtTokens(t.tokens)}</span>
    </button></li>`;
    })
    .join('');
  const split = d && d.tokens > 0
    ? `in ${fmtTokens(d.input)} · out ${fmtTokens(d.output)} · cache ${fmtTokens(d.cache_write + d.cache_read)}`
    : '';

  return `<section class="rank-section" aria-label="Breakdown">
      <div class="label"><span>Sessions</span><span class="rule"></span><span class="num">${d ? `${fmtInt(d.calls)} calls` : ''}</span></div>
      <div class="rank-row">
        <span class="rank-num${d ? '' : ' skeleton'}">${d ? fmtInt(sessions) : '—'}</span>
        <span class="rank-note">${split || (d ? `no usage ${p.key === '1' ? 'today' : `in ${p.word}`} yet` : 'reading your logs…')}</span>
      </div>
    </section>
    ${rows ? `<ul class="neighbors tools">${rows}</ul>` : '<div class="empty"></div>'}`;
}

function localView() {
  const p = currentPeriod();
  const view = ui.tool && snap.local?.by_tool?.[ui.tool];
  const source = view ? { ...view, today: snap.local.today } : snap.local;
  const all = snap.local?.periods?.[ui.period] ?? null;
  const d = source?.periods?.[ui.period] ?? null;
  const hours = d?.active_hours ?? 0;
  const perHour = d && hours > 0 ? `≈ ${fmtCost(d.cost / hours)}/hr active` : '≈ — /hr active';
  const noLogs = snap.local && objectList(snap.sources).every((s) => !Array.isArray(s.dirs) || s.dirs.length === 0);
  return `${header()}
    ${noLogs ? '<div class="banner"><span class="dot idle"></span><p>No Claude Code or Codex logs found in ~/.claude/projects or ~/.codex/sessions.</p></div>' : ''}
    ${periodToolbar('<span class="handle">on this Mac</span>')}
    ${tilesView(d, p, { perHour, tokenNote: view ? `${TOOL_NAMES[ui.tool]} only` : undefined })}
    ${breakdownView(d, p, all)}
    ${heatmapView(source)}
    ${modelsView(d, p)}
    <div class="actions">
      ${snap.signedIn
        ? `<button type="button" class="action primary" data-act="leaderboard"><span>Open leaderboard</span>${ICON.out}</button>`
        : `<button type="button" class="action primary" data-tab="share"><span>Share on the leaderboard</span>${ICON.chevronRight}</button>`}
      <button type="button" class="action" data-act="sync" ${snap.syncing || snap.scanning ? 'disabled' : ''}><span>${snap.syncing ? 'Syncing…' : snap.scanning ? 'Reading logs…' : snap.signedIn ? 'Refresh & sync' : 'Refresh'}</span>${keys('⌘', 'R')}</button>
      <button type="button" class="action" data-act="quit"><span>Quit UsageRank</span>${keys('⌘', 'Q')}</button>
    </div>`;
}

// ── Limits (live 5-hour and weekly windows, à la OpenUsage) ────────────────

function fmtDuration(ms) {
  const m = Math.max(1, Math.ceil(ms / 60000));
  const d = Math.floor(m / 1440);
  const h = Math.floor((m % 1440) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m % 60}m`;
  return `${m}m`;
}

// Burn-rate pacing, ported from OpenUsage's Pace: project usage at the current rate to the end of the
// window. Blue while ≥10% is projected to spare, amber inside the last 10%, red when it runs out first.
function meterState(w, now = Date.now()) {
  const used = Math.min(100, Math.max(0, w.used));
  const level = () => (used >= 90 ? 'bad' : used >= 80 ? 'warn' : 'ok');
  if (Math.round(100 - used) <= 0) return { tone: 'bad', note: 'Limit reached', tip: 'Limit reached' };
  if (!w.resetsAt || !w.periodMs) return { tone: level() };
  const elapsed = now - (w.resetsAt - w.periodMs);
  const remaining = w.resetsAt - now;
  const tick = Math.min(100, Math.max(0, (remaining / w.periodMs) * 100)); // where an even pace would be
  if (used <= 0 || remaining <= 0 || elapsed < Math.max(60000, w.periodMs * 0.01)) return { tone: level() };

  const projected = (used / elapsed) * w.periodMs;
  // Whole-percent meters read 1–4% early in a window; extrapolating that is noise.
  if (projected > 90 && used < 5) return { tone: level() };
  if (projected <= 90) return { tone: 'ok', tip: `~${Math.round(100 - projected)}% left at reset` };
  if (projected <= 100) {
    const spare = Math.round(100 - projected);
    if (spare >= 1) return { tone: 'warn', note: `~${spare}% spare`, tick, tip: `~${Math.round(projected)}% used at reset` };
    return { tone: 'bad', tick, tip: '~100% used at reset' };
  }
  const eta = (100 - used) / (projected / w.periodMs);
  return {
    tone: 'bad',
    note: eta < remaining ? `Limit in ${fmtDuration(eta)}` : null,
    tick,
    tip: `~${Math.round(projected - 100)}% over limit at reset`,
  };
}

function limitRow(w, stale) {
  const left = Math.max(0, Math.min(100, 100 - w.used));
  const st = meterState(w);
  const notStarted = !w.resetsAt && w.label === 'Session';
  const reset = notStarted ? 'Not started' : w.resetsAt ? (w.resetsAt > Date.now() ? `Resets in ${fmtDuration(w.resetsAt - Date.now())}` : 'Resetting…') : '';
  return `<div class="lim${stale ? ' stale' : ''}">
    <div class="lim-top"><span class="lim-label">${esc(w.label)}</span>${st.note ? `<span class="lim-note ${st.tone}">${esc(st.note)}</span>` : ''}</div>
    <div class="meter ${st.tone}"${st.tip ? ` data-tip="${esc(st.tip)}"` : ''}>
      <span class="fill" style="width: ${left}%"></span>
      ${st.tick != null ? `<i class="tick" style="left: ${st.tick}%"></i>` : ''}
    </div>
    <div class="lim-foot"><span class="num">${Math.round(left)}% left</span><span class="muted"${notStarted ? ' data-tip="Starts with your first message"' : ''}>${reset}</span></div>
  </div>`;
}

const LIMIT_PROVIDERS = [
  { key: 'claude', name: 'Claude', badge: 'CC', login: 'claude' },
  { key: 'codex', name: 'Codex', badge: 'CX', login: 'codex' },
];

function providerView(def) {
  const p = snap.limits?.[def.key];
  const open = ui.expanded.has(def.key);
  // A failed check over earlier numbers: keep showing them, flag it in the header.
  const windows = objectList(p?.windows);
  const warn = p?.error && !p.missing && windows.length
    ? `<span class="prov-warn" tabindex="0" aria-label="${esc(p.error)}" data-tip-wrap data-tip-title="${esc(p.error)}" data-tip="${p.fetchedAt ? `Showing data from ${ago(p.fetchedAt)}` : 'Showing earlier data'}">!</span>`
    : '';
  let body;
  if (!p) {
    body = `<div class="lim-empty">${snap.limitsFetching ? 'Checking your limits…' : 'Not checked yet.'}</div>`;
  } else if (p.missing) {
    body = `<div class="lim-empty">${esc(p.error)}</div>`;
  } else if (p.error && !windows.length) {
    body = `<div class="lim-error"><span class="dot error"></span><span>${esc(p.error)}</span></div>`;
  } else {
    const main = windows.filter((w) => w.primary);
    const extra = windows.filter((w) => !w.primary);
    const shown = open ? windows : main;
    body = `${shown.length ? shown.map((w) => limitRow(w, false)).join('') : '<div class="lim-empty">No limits reported for this plan.</div>'}
      ${extra.length ? `<button type="button" class="lim-more" data-expand="${def.key}" aria-expanded="${open}" aria-label="${open ? 'Show fewer limits' : `Show ${extra.length} more`}">${ICON.caret}</button>` : ''}`;
  }
  return `<section class="prov" aria-label="${def.name} limits">
    <div class="prov-head"><span class="tool-badge">${toolBadgeHtml(def.key)}</span><span class="prov-name">${def.name}</span>${p?.plan ? `<span class="prov-plan">${esc(p.plan)}</span>` : ''}${warn}</div>
    <div class="prov-card">${body}</div>
  </section>`;
}

function limitsView() {
  return `${header()}
    <div class="provs">${LIMIT_PROVIDERS.map(providerView).join('')}</div>
    <p class="lim-src">Live from Anthropic and OpenAI using your Claude Code and Codex logins. Not shared.</p>
    <div class="actions">
      <button type="button" class="action" data-act="refresh-limits" ${snap.limitsFetching ? 'disabled' : ''}><span>${snap.limitsFetching ? 'Checking…' : 'Refresh limits'}</span>${keys('⌘', 'R')}</button>
      <button type="button" class="action" data-act="quit"><span>Quit UsageRank</span>${keys('⌘', 'Q')}</button>
    </div>`;
}

// ── Leaderboard (shared with usagerank.com) ────────────────────────────────

function rankView(d, p) {
  const of = d ? `of ${fmtInt(d.of)}` : '';
  let headline;
  if (!d) {
    headline = '<span class="rank-num skeleton">#—</span>';
  } else if (d.rank == null) {
    headline = `<span class="rank-num skeleton">#—</span><span class="rank-note">no usage ${p.key === '1' ? 'today' : `in ${p.word}`} yet</span>`;
  } else {
    const prev = d.previous_rank;
    let delta;
    if (prev == null) delta = '<span class="rank-delta"><b>●</b> new</span>';
    else if (prev > d.rank) delta = `<span class="rank-delta"><b>▲</b> ${prev - d.rank}</span>`;
    else if (prev < d.rank) delta = `<span class="rank-delta down"><b>▼</b> ${d.rank - prev}</span>`;
    else delta = '<span class="rank-delta down"><b>–</b> 0</span>';
    headline = `<span class="rank-num">#${fmtInt(d.rank)}</span>${delta}<span class="rank-note">${p.note}</span>`;
  }

  const rows = objectList(d?.neighbors)
    .map((n) => `<li class="${n.you ? 'you' : ''}">
      <span class="r">${n.rank}</span>
      <span class="who"><span>${esc(n.handle ?? n.name)}</span>${n.you ? '<span class="you-badge">you</span>' : ''}</span>
      <span class="tool-badge">${toolBadgeHtml(n.tool)}</span>
      <span class="t">${fmtTokens(n.tokens)}</span>
    </li>`)
    .join('');

  return `<section class="rank-section" aria-label="Your rank">
      <div class="label"><span>Your rank</span><span class="rule"></span><span class="num">${of}</span></div>
      <div class="rank-row">${headline}</div>
    </section>
    ${rows ? `<ul class="neighbors">${rows}</ul>` : `<div class="empty">${d ? 'Burn some tokens and you’ll show up here.' : ''}</div>`}`;
}

// "Update available" at the top of every view, until Update or Later.
function updateBanner() {
  const u = snap?.update;
  if (u?.installing) {
    const progress = u.progress != null ? ` (${Math.round(u.progress * 100)}%)` : '';
    return `<div class="banner" role="status"><span class="dot pulse"></span><p>Updating to ${esc(u.version)}${progress}. UsageRank restarts when it's done.</p></div>`;
  }
  if (!u?.version || ui.laterUpdate === u.version) return '';
  const notes = u.notes ? ` data-tip="${esc(u.notes)}" data-tip-title="What's new in ${esc(u.version)}" data-tip-wrap` : '';
  return `<div class="banner" role="status"><span class="dot ${u.error ? 'error' : ''}"></span>
    <p${notes}>${u.error ? esc(u.error) : `UsageRank ${esc(u.version)} is available.`}</p>
    <button type="button" data-act="update">${u.error ? 'Retry' : 'Update'}</button>
    <button type="button" data-act="later-update">Later</button>
  </div>`;
}

function bannerView(text, retry = true) {
  return `<div class="banner" role="alert"><span class="dot error"></span><p>${esc(text)}</p>${retry ? '<button type="button" data-act="sync">Retry</button>' : ''}</div>`;
}

function sharedView() {
  const p = currentPeriod();
  const d = snap.stats?.periods?.[ui.period] ?? null;
  const handle = snap.stats?.user?.handle ?? snap.handle;
  const banners = [
    snap.lastError && !snap.syncing ? bannerView(snap.lastError) : '',
    snap.statsError && !snap.lastError && !snap.syncing ? bannerView(snap.statsError, false) : '',
  ].join('');
  return `${header()}
    ${banners}
    ${periodToolbar(handle ? `<span class="handle" title="${esc(snap.account?.name ?? '')}">@${esc(handle)}</span>` : '')}
    ${tilesView(d, p, { perHour: 'across all your Macs' })}
    ${rankView(d, p)}
    ${d?.models ? modelsView(d, p) : ''}
    <div class="actions">
      <button type="button" class="action primary" data-act="leaderboard"><span>Open leaderboard</span>${ICON.out}</button>
      <button type="button" class="action" data-act="sync" ${snap.syncing ? 'disabled' : ''}><span>${snap.syncing ? 'Syncing…' : 'Sync now'}</span>${keys('⌘', 'R')}</button>
      <button type="button" class="action" data-act="quit"><span>Quit UsageRank</span>${keys('⌘', 'Q')}</button>
    </div>`;
}

// ── Leaderboard, not shared yet ────────────────────────────────────────────

function onboardingView() {
  const login = snap.login;
  const retention = snap.retentionDays;
  const draft = ui.handleDraft ?? snap.suggestedHandle ?? '';
  const host = String(snap.webUrl ?? '').replace(/^https?:\/\//, '');

  const body = login
    ? `<div class="pane">
        <div class="label"><span>Approve this code</span><span class="rule"></span></div>
        <div class="code" aria-label="Login code">${esc(login.userCode)}</div>
        <div class="waiting"><span class="dot pulse"></span><span>Waiting for approval in your browser · <span id="expires">${expiresIn(login.expiresAt)}</span></span></div>
        <div class="two">
          <button type="button" class="btn ghost" data-act="reopen">Open browser again</button>
          <button type="button" class="btn ghost" data-act="cancel-login">Cancel</button>
        </div>
      </div>`
    : `<div class="pane">
        <label class="label" for="handle"><span>Pick a handle</span><span class="rule"></span></label>
        <div class="row">
          <div class="field grow"><span>@</span><input id="handle" value="${esc(draft)}" maxlength="32" autocomplete="off" spellcheck="false" aria-describedby="handle-hint"></div>
        </div>
        <p class="hint" id="handle-hint">Shown on the leaderboard. 2–32 characters: a–z, 0–9, dot, underscore, dash.</p>
        ${retention != null && retention < 365
          ? `<label class="check"><input type="checkbox" id="keep-logs" ${ui.keepLogs ? 'checked' : ''}><span>Claude Code deletes logs after ${retention} days. Keep them for a year so no usage is lost between syncs.</span></label>`
          : ''}
        <button type="button" class="btn wide" data-act="login" style="margin-top: 14px" ${ui.busy === 'login' ? 'disabled' : ''}>${ui.busy === 'login' ? 'Starting…' : `Share with ${esc(host)}`}</button>
        
      </div>`;

  return `${header()}
    <div class="hero">
      <h2>See where you rank.</h2>
      <p>Sharing is off. Your usage stays on this Mac until you link it to ${esc(host)}.</p>
      <ul class="facts">
        <li><b>Sent:</b> daily totals per tool and model: date, calls, sessions and token counts.</li>
        <li><b>Never sent:</b> prompts, code, file paths or project names.</li>
        <li>Stop any time in Preferences, or delete everything you shared.</li>
      </ul>
    </div>
    ${body}
    <div class="actions">
      <button type="button" class="action" data-act="leaderboard"><span>Browse the leaderboard</span>${ICON.out}</button>
      <button type="button" class="action" data-act="quit"><span>Quit UsageRank</span>${keys('⌘', 'Q')}</button>
    </div>`;
}

function expiresIn(at) {
  const s = Math.max(0, Math.round((at - Date.now()) / 1000));
  return `expires in ${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

// ── preferences ────────────────────────────────────────────────────────────

const toggle = (id, on, label) => `<button type="button" class="switch" role="switch" id="${id}" aria-checked="${on}" aria-label="${esc(label)}"></button>`;

function prefsView() {
  const s = snap.settings;
  const draft = ui.handleDraft ?? snap.handle ?? '';
  const sources = Object.fromEntries(objectList(snap.sources).map((x) => [x.key, Array.isArray(x.dirs) ? x.dirs.filter((d) => typeof d === 'string') : []]));
  const where = (key, fallback) => {
    const dirs = sources[key];
    if (!dirs) return fallback;
    return dirs.length ? dirs.map((d) => d.replace(/^\/Users\/[^/]+/, '~')).join(', ') : 'no logs found';
  };
  const profile = snap.stats?.user?.profile_url;
  const retention = snap.retentionDays;
  const host = String(snap.webUrl ?? '').replace(/^https?:\/\//, '');
  const displays = DISPLAYS.filter(([v]) => v !== 'rank' || snap.signedIn);

  const general = `
    <div class="pane">
      <div class="label"><span>Menu bar</span><span class="rule"></span></div>
      <div class="row">
        <span>Show</span>
        <div class="seg" role="group" aria-label="Menu bar shows">
          ${displays.map(([v, l]) => `<button type="button" data-display="${v}" aria-pressed="${s.display === v}">${l}</button>`).join('')}
        </div>
      </div>
      <div class="row"><span>Launch at login</span>${toggle('autostart', snap.autostart, 'Launch at login')}</div>
    </div>

    <div class="pane">
      <div class="label"><span>Logs on this Mac</span><span class="rule"></span></div>
      <dl class="kv">
        <dt>Claude</dt><dd>${esc(where('claude', '~/.claude/projects'))}</dd>
        <dt>Codex</dt><dd>${esc(where('codex', '~/.codex/sessions'))}</dd>
      </dl>
      ${retention != null ? `<div class="row">
        <div class="grow"><div>Claude Code log retention</div><div class="hint" style="margin: 0">${retention >= 365 ? `Kept for ${retention} days.` : `Claude Code deletes logs after ${retention} days. This app keeps the totals it has already read.`}</div></div>
        ${retention < 365 ? '<button type="button" class="btn ghost" data-act="retention">Keep 1 year</button>' : ''}
      </div>` : ''}
    </div>`;

  const sharing = snap.signedIn ? `
    <div class="pane">
      <div class="label"><span>Sharing · ${esc(host)}</span><span class="rule"></span></div>
      <div class="row">
        <div class="field grow"><span>@</span><input id="handle" value="${esc(draft)}" maxlength="32" autocomplete="off" spellcheck="false" aria-label="Handle"></div>
        <button type="button" class="btn" data-act="save-handle" ${ui.busy === 'handle' || draft === snap.handle ? 'disabled' : ''}>${ui.busy === 'handle' ? 'Saving…' : 'Save'}</button>
      </div>
      <dl class="kv">
        <dt>Account</dt><dd>${esc(snap.account?.name || '—')}${snap.account?.email ? ` · ${esc(snap.account.email)}` : ''}</dd>
        <dt>Machine</dt><dd title="${esc(snap.machineId)}">${esc((snap.machineId ?? '—').slice(0, 18))}…</dd>
      </dl>
      ${profile ? `<button type="button" class="action" data-act="profile" style="margin-top: 8px; padding: 0"><span>Open my public profile</span>${ICON.out}</button>` : ''}
      <div class="row">
        <label for="interval">Sync every</label>
        <select id="interval">${INTERVALS.map(([v, l]) => `<option value="${v}" ${s.intervalMinutes === v ? 'selected' : ''}>${l}</option>`).join('')}</select>
      </div>
      <div class="row"><span>Share Claude Code usage</span>${toggle('src-claude', s.claude, 'Share Claude Code usage')}</div>
      <div class="row"><span>Share Codex usage</span>${toggle('src-codex', s.codex, 'Share Codex usage')}</div>
      <div class="row">
        <div class="grow"><div>Re-send all history</div><div class="hint" style="margin: 0">Safe: the server replaces each day.</div></div>
        <button type="button" class="btn ghost" data-act="sync-all" ${snap.syncing ? 'disabled' : ''}>Re-send</button>
      </div>
      <div class="row">
        <div class="grow"><div>Stop sharing</div><div class="hint" style="margin: 0">Unlinks this Mac. Remove the device on your dashboard to revoke its key.</div></div>
        <button type="button" class="btn ghost" data-act="sign-out">Stop</button>
      </div>
      <div class="row">
        <div class="grow"><div>Delete shared data</div><div class="hint" style="margin: 0">Removes your row and history from ${esc(host)}.</div></div>
        <button type="button" class="btn ${ui.confirmForget ? 'danger' : 'ghost'}" data-act="forget" ${ui.busy === 'forget' ? 'disabled' : ''}>${ui.busy === 'forget' ? 'Deleting…' : ui.confirmForget ? 'Really delete' : 'Delete…'}</button>
      </div>
    </div>` : `
    <div class="pane">
      <div class="label"><span>Sharing</span><span class="rule"></span></div>
      <div class="row">
        <div class="grow"><div>Off</div><div class="hint" style="margin: 0">Nothing leaves this Mac.</div></div>
        <button type="button" class="btn ghost" data-act="go-share">Set up…</button>
      </div>
    </div>`;

  return `<header class="sub-head">
      <button type="button" class="icon-btn" data-act="back" aria-label="Back">${ICON.chevron}</button>
      <h1>Preferences</h1>
      ${statusView()}
    </header>
    ${general}
    ${sharing}
    ${updatesPane()}
    <div class="actions">
      <button type="button" class="action" data-act="quit"><span>Quit UsageRank</span>${keys('⌘', 'Q')}</button>
    </div>`;
}

function updatesPane() {
  const u = snap.update ?? {};
  const hint = u.installing ? 'Updating…'
    : u.checking ? 'Checking…'
      : u.version ? `${u.version} is available.`
        : u.error ? u.error
          : u.upToDate ? "You're up to date."
            : 'Checks for updates automatically.';
  const button = u.version
    ? `<button type="button" class="btn" data-act="update" ${u.installing ? 'disabled' : ''}>${u.installing ? 'Updating…' : 'Update'}</button>`
    : `<button type="button" class="btn ghost" data-act="check-update" ${u.checking ? 'disabled' : ''}>Check now</button>`;
  return `
    <div class="pane">
      <div class="label"><span>Updates</span><span class="rule"></span></div>
      <div class="row">
        <div class="grow"><div class="mono">v${esc(snap.version)}</div><div class="hint" style="margin: 0">${esc(hint)}</div></div>
        ${button}
      </div>
    </div>`;
}

// ── tooltip ────────────────────────────────────────────────────────────────

// One floating tooltip for any element with data-tip (and optional data-tip-title), instead of the slow native title.
const tip = document.createElement('div');
tip.className = 'tip';
tip.setAttribute('role', 'tooltip');
document.body.appendChild(tip);
let tipFor = null;

function showTip(el) {
  tipFor = el;
  const title = el.dataset.tipTitle;
  tip.innerHTML = `${title ? `<b>${esc(title)}</b>` : ''}<span>${esc(el.dataset.tip)}</span>`;
  tip.classList.toggle('wrap', 'tipWrap' in el.dataset);
  tip.classList.add('on');
  const r = el.getBoundingClientRect();
  const w = tip.offsetWidth;
  const h = tip.offsetHeight;
  const left = Math.min(Math.max(6, r.left + r.width / 2 - w / 2), window.innerWidth - w - 6);
  const above = r.top - h - 6;
  tip.style.left = `${left}px`;
  tip.style.top = `${above >= 4 ? above : r.bottom + 6}px`;
}

function hideTip() {
  tipFor = null;
  tip.classList.remove('on');
}

card.addEventListener('mouseover', (e) => {
  const el = e.target.closest('[data-tip]');
  if (el && el !== tipFor) showTip(el);
  else if (!el && tipFor) hideTip();
});
card.addEventListener('mouseleave', hideTip);

// ── render ─────────────────────────────────────────────────────────────────

function notices() {
  const errors = Array.isArray(snap?.errors) ? snap.errors.filter((e) => typeof e === 'string') : [];
  if (snap?.loginError) errors.push(snap.loginError);
  if (snap?.lastError && !(ui.tab === 'share' && ui.view === 'main' && snap.signedIn)) errors.push(snap.lastError);
  if (snap?.statsError && ui.view === 'prefs') errors.push(snap.statsError);
  const banners = [...new Set(errors)].map((error) => bannerView(error, false)).join('');
  const message = ui.message ? `<div class="banner" role="${ui.message.error ? 'alert' : 'status'}"><span class="dot ${ui.message.error ? 'error' : 'idle'}"></span><p>${esc(ui.message.text)}</p>${ui.message.error ? '<button type="button" data-act="retry">Reload</button>' : ''}</div>` : '';
  return updateBanner() + banners + message;
}

function render() {
  hideTip();
  if (!snap) {
    card.innerHTML = `${notices()}<button type="button" class="action" data-act="retry">Try again</button>`;
    return;
  }
  try {
    const focused = document.activeElement?.id;
    const caret = document.activeElement?.selectionStart;
    const content = ui.view === 'prefs'
      ? prefsView()
      : ui.tab === 'local'
        ? localView()
        : ui.tab === 'limits'
          ? limitsView()
          : snap.signedIn ? sharedView() : onboardingView();
    card.innerHTML = notices() + content;
    if (focused) {
      const el = document.getElementById(focused);
      if (el) {
        el.focus();
        if (caret != null && el.setSelectionRange) el.setSelectionRange(caret, caret);
      }
    }
  } catch (error) {
    console.error(error);
    card.innerHTML = `${bannerView(`Cannot show this view: ${String(error)}`, false)}<button type="button" class="action" data-act="retry">Try again</button>`;
  }
}

new ResizeObserver(() => { void handle(() => invoke('set_popover_height', { height: shell.offsetHeight })); }).observe(shell);

function flash(text, error = false) {
  ui.message = { text, error };
  render();
}

async function run(name, fn) {
  ui.busy = name;
  ui.message = null;
  render();
  try {
    const result = await fn();
    if (result && typeof result === 'object' && 'signedIn' in result) snap = result;
    return result;
  } catch (e) {
    ui.message = { text: String(e), error: true };
  } finally {
    ui.busy = null;
    render();
  }
}

const actions = {
  retry: () => loadSnapshot(),
  prefs: () => {
    ui.view = ui.view === 'prefs' ? 'main' : 'prefs';
    ui.handleDraft = null;
    ui.message = null;
    ui.confirmForget = false;
    render();
  },
  back: () => actions.prefs(),
  'go-share': () => {
    ui.view = 'main';
    ui.tab = 'share';
    store.set('tab', 'share');
    render();
  },
  sync: () => (ui.tab === 'limits' && ui.view === 'main' ? actions['refresh-limits']() : invoke('sync_now', { all: false })),
  'refresh-limits': () => invoke('refresh_limits', { force: true }),
  'sync-all': async () => {
    await invoke('sync_now', { all: true });
    flash('Re-sending every day…');
  },
  leaderboard: () => invoke('open_leaderboard'),
  profile: () => snap.stats?.user?.profile_url && invoke('open_url', { url: snap.stats.user.profile_url }),
  quit: () => invoke('quit'),
  update: () => invoke('install_update'),
  'check-update': () => invoke('check_update'),
  'later-update': () => {
    ui.laterUpdate = snap.update?.version ?? null;
    render();
  },
  login: () => run('login', () => invoke('start_login', {
    handle: document.getElementById('handle')?.value ?? '',
    keepLogs: !!document.getElementById('keep-logs')?.checked,
  })),
  reopen: () => snap.login && invoke('open_url', { url: snap.login.url }),
  'cancel-login': async () => {
    snap = await invoke('cancel_login');
    render();
  },
  'save-handle': () => run('handle', async () => {
    const result = await invoke('set_handle', { handle: document.getElementById('handle')?.value ?? '' });
    ui.handleDraft = null;
    ui.message = { text: 'Handle saved.', error: false };
    return result;
  }),
  retention: () => run('retention', () => invoke('extend_retention')),
  'sign-out': () => run('sign-out', () => invoke('sign_out')),
  forget: () => {
    if (!ui.confirmForget) {
      ui.confirmForget = true;
      render();
      return;
    }
    return run('forget', async () => {
      const result = await invoke('forget');
      ui.confirmForget = false;
      ui.view = 'main';
      return result;
    });
  },
};

async function saveSettings(patch) {
  return run('settings', async () => {
    acceptSnapshot(await invoke('set_settings', { settings: { ...snap.settings, ...patch } }));
  });
}

card.addEventListener('click', (e) => handle(async () => {
  const t = e.target.closest('button');
  if (!t || t.disabled) return;
  if (t.dataset.tab) {
    ui.tab = t.dataset.tab;
    store.set('tab', ui.tab);
    ui.message = null;
    render();
    if (ui.tab === 'limits') void handle(() => invoke('refresh_limits', { force: false }));
  } else if (t.dataset.expand) {
    const key = t.dataset.expand;
    if (ui.expanded.has(key)) ui.expanded.delete(key);
    else ui.expanded.add(key);
    store.set('expanded', [...ui.expanded].join(','));
    render();
  } else if (t.dataset.tool) {
    ui.tool = ui.tool === t.dataset.tool ? null : t.dataset.tool;
    store.set('tool', ui.tool ?? '');
    render();
  } else if (t.dataset.period) {
    ui.period = t.dataset.period;
    store.set('period', ui.period);
    render();
  } else if (t.dataset.display) {
    await saveSettings({ display: t.dataset.display });
  } else if (t.id === 'autostart') {
    await run('autostart', () => invoke('set_autostart', { enabled: !snap.autostart }));
  } else if (t.id === 'src-claude') {
    await saveSettings({ claude: !snap.settings.claude });
  } else if (t.id === 'src-codex') {
    await saveSettings({ codex: !snap.settings.codex });
  } else if (t.dataset.act && actions[t.dataset.act]) {
    try {
      await actions[t.dataset.act]();
    } catch (err) {
      flash(String(err), true);
    }
  }
}));

card.addEventListener('input', (e) => {
  if (e.target.id === 'handle') {
    ui.handleDraft = e.target.value;
    const save = card.querySelector('[data-act="save-handle"]');
    if (save) save.disabled = e.target.value === snap.handle;
  }
  if (e.target.id === 'keep-logs') ui.keepLogs = e.target.checked;
});

card.addEventListener('change', (e) => {
  if (e.target.id === 'interval') saveSettings({ intervalMinutes: Number(e.target.value) });
});

card.addEventListener('keydown', (e) => {
  if (e.key !== 'Enter' || e.target.id !== 'handle') return;
  if (snap) void handle(() => actions[snap.signedIn ? 'save-handle' : 'login']());
});

window.addEventListener('keydown', (e) => {
  if (e.metaKey && e.key.toLowerCase() === 'r') {
    e.preventDefault();
    if (snap) void handle(() => actions.sync());
  } else if (e.metaKey && e.key.toLowerCase() === 'q') {
    e.preventDefault();
    void handle(() => actions.quit());
  } else if (e.metaKey && e.key === ',') {
    e.preventDefault();
    if (snap) actions.prefs();
  } else if (e.key === 'Escape') {
    if (ui.view === 'prefs') actions.back();
    else void handle(() => invoke('hide_popover'));
  }
});

const subscriptions = new Set();
async function subscribe(name, callback) {
  if (subscriptions.has(name)) return;
  await listen(name, (event) => { void handle(() => callback(event)); });
  subscriptions.add(name);
}

async function loadSnapshot() {
  ui.message = null;
  const registrations = await Promise.allSettled([
    subscribe('snapshot', (event) => acceptSnapshot(event.payload)),
    subscribe('popover-shown', () => {
      ui.confirmForget = false;
      render();
      if (ui.tab === 'limits') return invoke('refresh_limits', { force: false });
    }),
  ]);
  acceptSnapshot(await invoke('get_snapshot'));
  const failed = registrations.find((result) => result.status === 'rejected');
  if (failed) throw failed.reason;
  if (ui.tab === 'limits') await invoke('refresh_limits', { force: false });
}

window.addEventListener('error', (event) => { reportError(event.error ?? event.message); });
window.addEventListener('unhandledrejection', (event) => {
  event.preventDefault();
  reportError(event.reason);
});

// Keep "Synced 2m ago", the login countdown and the limit countdowns fresh while the popover is open.
let ticks = 0;
setInterval(() => {
  if (!snap) return;
  if (++ticks % 30 === 0 && ui.view === 'main' && ui.tab === 'limits' && !tipFor) {
    render();
    return;
  }
  const expires = document.getElementById('expires');
  if (expires && snap.login) expires.textContent = expiresIn(snap.login.expiresAt);
  const status = card.querySelector('.status');
  if (status) status.outerHTML = statusView();
}, 1000);

void handle(loadSnapshot);
