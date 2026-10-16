# UsageRank for macOS

Menu bar app for your Claude Code and Codex token usage. It works entirely locally out of the box. Sharing with the [UsageRank](https://usagerank.com) leaderboard is opt-in and does what `npx usagerank` does.

```bash
pnpm install
pnpm dev                     # run
pnpm build                   # UsageRank.app + .dmg in src-tauri/target/release/bundle
scripts/release-mac.sh       # signed + notarized universal .dmg (needs .env.signing, see .env.signing.example)
```

## What it does

The popover has three tabs.

- **This Mac** (default, no account needed): Today / 7d / 30d tokens (CC/CX split) and spend at list price (per active hour), sessions and calls, per-tool rows, a 20-week activity heatmap and model mix. Everything comes from `~/.claude/projects` and `~/.codex/sessions`, rescanned every 2 min and whenever the popover opens. Spend uses a copy of the server's price table (`src-tauri/src/pricing.rs`).
- **Limits** (no account needed): your live Claude and Codex subscription windows, laid out like [OpenUsage](https://github.com/robinebers/openusage). It shows Session (5 h) and Weekly with % left and the reset countdown, plus model-scoped weeks (Sonnet, Opus, Spark…) behind the caret. Bars turn amber ("~N% spare") or red ("Limit in …") from burn-rate pacing, and a tick marks even pace. It reuses the Claude Code login (keychain `Claude Code-credentials`, else `~/.claude/.credentials.json`, then a read-only Claude Desktop fallback) and the Codex CLI login (`$CODEX_HOME/auth.json`, else the `Codex Auth` keychain item), and calls `api.anthropic.com/api/oauth/usage` and `chatgpt.com/backend-api/wham/usage`. An expired token is refreshed once and written back only if that login is unchanged. It fetches only after the tab is first opened, then every 5 min, and backs off on HTTP 429. Code: `src-tauri/src/limits.rs`.
- **Leaderboard**: when sharing is off, an explainer of what is and isn't sent, a handle field and "Share with usagerank.com". That starts the same device-code login as `usagerank login`. Once linked, it shows your server-side totals across all your Macs, your rank with ▲/▼ and the burners above and below you, plus Open leaderboard and Sync now.

**Menu bar item**: tokens today (default), spend today, today's rank (only when sharing) or icon only. It shows "Syncing" while it pushes.

**Preferences** (⚙ or ⌘,):
- menu bar display
- launch at login
- where the logs are read from
- Claude Code log retention

When sharing is on, Preferences also has:
- handle
- push interval (15 min to 6 h, default 30 min)
- share Claude Code / Codex (`--only`)
- re-send all history (`--all`)
- stop sharing
- delete shared data (`forget`)

## Claude login recovery

Claude limits prefer the Claude Code Keychain login, then its credentials file. Empty reads are retried briefly. If a login is missing, expired, or cannot read usage, UsageRank tries the next source, including Claude Desktop's encrypted login cache, following OpenUsage's behavior. Network failures and rate limits keep the previous limits and do not trigger more requests through another login.

The Desktop fallback checks the active account and organization and uses only a valid access token. UsageRank never renews Desktop tokens or changes its files or Keychain item. Newer cache entries and deletion markers override older copies of the same login. If Desktop's token expires, open Claude Desktop to renew it, then refresh limits.

Background checks never show a Desktop Keychain prompt. If access is needed, click the Limits refresh button and choose **Always Allow** for **Claude Safe Storage**. Subsequent checks can then read the login silently.

## Local data vs. shared data

- The local view keeps its own scan cache and settings in `~/Library/Application Support/com.usagerank.menubar/`. With sharing off, nothing is written to `~/.usagerank` and nothing is sent.
- Sharing uses `~/.usagerank/` in exactly the CLI's formats. A Mac already set up with `npx usagerank login` opens with sharing on and the same key, and `state.json` stays the record of what was pushed.

`.env` in `~/.usagerank/` and the env vars `USAGERANK_HOME`, `USAGERANK_API_URL`, `USAGERANK_URL`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME` work the same as for the CLI. `USAGERANK_APP_DIR` moves the app's own folder. To try sharing against a local server without touching your real key:

```bash
open --env USAGERANK_HOME=/tmp/ur --env USAGERANK_URL=http://usagerank.test \
     --env USAGERANK_API_URL=http://usagerank.test/api/cli/usage \
     src-tauri/target/debug/bundle/macos/UsageRank.app
```

## Server

The Leaderboard tab reads `GET /api/cli/stats?tz=…`, signed like every CLI request (signature over an empty body). It returns the user, `periods.{1,7,30}` (tokens, cost, claude/codex split, rank, previous_rank, of, neighbors, models) and `activity` (daily tokens for 20 weeks). A server without this endpoint still accepts pushes. The tab then says the server doesn't serve stats yet.

## Layout

- `src-tauri/src/scan.rs`: port of the CLI's `scan.js`
- `src-tauri/src/local.rs`: the This Mac numbers, from the local cache
- `src-tauri/src/pricing.rs`: list prices, mirrored from the server's `config/pricing.php`
- `src-tauri/src/api.rs`: signed requests, batching, login
- `src-tauri/src/config.rs`: `~/.usagerank` files, ed25519 keys, settings
- `src-tauri/src/lib.rs`: tray, popover window, sync scheduler, commands
- `dist/`: the popover UI (plain HTML/CSS/JS)
- `scripts/make-icons.mjs`: draws the tray and app icons (`pnpm icons`)
- `scripts/preview.html`: the popover with sample data, for UI work in a browser (`python3 -m http.server`, then open `/scripts/preview.html?tab=local|share&mode=main|local-out|prefs|prefs-out|onboarding|login|error|stats-error`)
