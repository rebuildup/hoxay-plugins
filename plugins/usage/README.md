# rebuildup.usage

Token usage aggregation for herdr panes
(`herdr-plugins.usage/v1`).

## Install

```bash
herdr plugin install rebuildup/hoxay-plugins/plugins/usage
```

Runtime: a committed prebuilt native binary (`bin/usage`, Rust —
Issue #294, ADR-0035). No Bun or toolchain required to install or run.
Verified platform: macOS arm64 (`platforms = ["macos"]` — honest until
Linux/Windows binaries exist and are verified).

Prerequisites: herdr ≥ 0.9.0 and the
[`rebuildup.conversations`](../conversations/) plugin installed and
indexed (`conversations index`/`refresh`) — `usage` aggregates its
normalized event index through the public read-only contract, never
its state files (ADR-0013). `capability`, `usage`, `quota`,
`divergence`, and `cost` work without a reachable herdr session.

## What it does

- Sums provider-reported token usage — `input_tokens`,
  `output_tokens`, `cache_read_input_tokens`,
  `cache_creation_input_tokens`, plus `thinking_tokens` detail — from
  usage-carrying conversation events.
- Breaks totals down by provider / model / host / project (pane cwd) /
  session / pane.
- Filters by `--pane`, `--session-id`, `--provider`, and an inclusive
  `--from`/`--to` time window on provider timestamps.
- Deduplicates on `(session_key, event_id)` so index rewrites cannot
  double count.
- Reports coverage honestly: unsupported providers, sessions with
  missing/stale/error index state, and missing event logs are counted
  separately — missing usage is never zero-fabricated, and
  `provider_reported_cost_usd` is labeled as provider-reported, not a
  billing amount.

T4 (#93) adds `cost`: an **estimated** API-cost figure per model —
list-price math over a LiteLLM rate table (same upstream T3 Code
prices from), cached 24h under the plugin state dir. Every payload is
labeled `estimate: true` with a fixed disclaimer; it is never a bill,
never blended with `provider_reported_cost_usd`, and unknown models
report `unpriced`/`null` — never a guessed price. User prices can be
supplied via config `modelPrices` (USD per million tokens). All
plugin caches (quota readings, hub readings, the price table) share a
30-day retention with best-effort GC; referenced entries are never
collected (ADR-0016).

Subscription
quota/reset surfacing (`quota`) shipped in #91 behind the ADR-0014
credential boundary: provider credential artifacts are read in memory
only to authenticate the providers' own read-only quota endpoints —
tokens are never logged, persisted, cached, or emitted. Claude
(`~/.claude` OAuth credentials / macOS Keychain) and Codex
(`$CODEX_HOME/auth.json`, ChatGPT auth) are implemented; opencode and
unknown providers report `unsupported` with a reason.

T3 (#92) adds three surfaces on the same credential-boundary model:
`usage` reports a per-provider `cache_tokens` breakdown
(`reported`/`unsupported`/`unknown` from the adapter capability
matrix); `quotaHubs` in config.json declares CLIProxyAPI-compatible
HTTP quota hubs (`GET /v0/management/auth-files` +
`POST /v0/management/api-call`) whose per-account readings appear in
`quota.hubs[]` / `divergence.hubs[]` — the hub management key is read
from the env var named by `keyEnv` and is never stored, cached, or
emitted (ADR-0015); and `divergence` compares subscription-quota
windows against transcript-measured token sums per provider, reporting
both sides with a `different_units`-style verdict and never
reconciling them.

## Usage

```bash
# from the plugin root (see `herdr plugin list --json` → plugin_root)
./bin/usage capability
./bin/usage usage [--pane PANE] [--session-id ID] [--provider ID] \
  [--from ISO] [--to ISO] [--session NAME] [--socket PATH] [--timeout MS] \
  [--offline] [--conversations-root DIR] [--conversations-state-dir DIR]
./bin/usage quota [--provider ID] [--refresh] [--timeout MS]
./bin/usage divergence [--provider ID] [--from ISO] [--to ISO] \
  [--refresh] [--offline] [<usage selector flags>]
./bin/usage cost [--provider ID] [--from ISO] [--to ISO] \
  [--refresh] [--offline] [<usage selector flags>]
```

- `capability` → contract banner, `sources[]` data-source matrix
  (`conversations-index` available/unavailable, `subscription-quota-api`
  per-provider coverage, `cliproxy-api` hubs, `model-pricing` rate
  table), aggregation dimensions.
- `usage` → `{totals, breakdowns, cache_tokens, coverage, sessions,
  source, warnings}`. Selectors forward to `conversations usage`; the
  whole export is paged (500 events/page) and aggregated in one call.
  `cache_tokens.providers[]` gives per-provider cache read/write token
  sums with `reported`/`unsupported`/`unknown` status from the
  conversations capability matrix.
- `quota` → `{providers[], hubs[], cache}` with per-provider `status`
  (`ok`/`unsupported`/`no_credential`/`credential_expired`/
  `request_failed`), usage `windows[]` with `used_percent` +
  `resets_at`, plan label, and freshness (`checked_at`, cache hit/age,
  `stale` flag). Successful readings cache for 300s under the plugin
  state dir; `--refresh` bypasses. Endpoint overrides for tests/proxies:
  `HERDR_USAGE_QUOTA_*` env or `quotaEndpoints` in config.json.
  `hubs[]` reports configured CLIProxyAPI-equivalent hubs
  (`quotaHubs` config: `{id, url, keyEnv}`) with per-account outcomes.
- `divergence` → `{comparisons[], hubs[], notes[], source}` placing
  quota `used_percent` windows next to measured transcript token sums
  per provider. Verdicts: `different_units` (both present — never
  reconciled), `no_measured_usage`, `quota_unavailable`, `no_data`;
  `window_alignment` notes only whether the `--from`/`--to` span
  matches a reported quota window duration.
- `cost` → `{cost, totals, breakdowns, coverage, sessions, source,
  warnings}` where `cost` carries `estimate: true`, the disclaimer,
  `estimated_cost_usd` (priced models only), `estimate_complete`,
  `unpriced_models[]`, `provider_reported_cost_usd` (separate figure),
  `pricing` provenance (`status`/`source`/`fetched_at`/`cache`), and
  `by_model[]` rows with `estimated_cost_usd` + `cost_source`
  (`model_priced`/`unpriced`). Pricing source override:
  `HERDR_USAGE_PRICING_URL` env or `pricingUrl` config; user prices:
  `modelPrices` config.

The conversations plugin is located via `--conversations-root` /
`HERDR_USAGE_CONVERSATIONS_ROOT` / `conversationsRoot` config → sibling
`../conversations` → `herdr plugin list --json`. Its state dir defaults
to the standard plugin state location and can be overridden with
`--conversations-state-dir` / `HERDR_USAGE_CONVERSATIONS_STATE_DIR` /
`conversationsStateDir`.

Full contract: `docs/contracts/usage-v1.md`.
Spec: `docs/specs/usage.md`. Design: ADR-0013 (index reuse),
ADR-0014 (quota credential boundary), ADR-0015 (hub credential
boundary), ADR-0016 (cost pricing + retention/GC).

## Interactive TUI

A report browser for `usage`/`quota`/`cost`/`divergence`, per
`docs/contracts/tui-v1.md` (ADR-0031/0032/0033). The plugin is
read-only (`mutation_commands: []`), so the TUI performs reads only;
source refresh (`f`) is always an explicit keypress:

```bash
herdr plugin pane open --plugin rebuildup.usage --entrypoint main --placement tab
# or via the action:
herdr plugin action invoke rebuildup.usage.open
```

Keys:

| key | action |
| --- | --- |
| `enter` | open report (usage / quota / cost / divergence) |
| `w` | cycle period (all / 24h / 7d / 30d) on windowed reports |
| `p` | cycle provider filter |
| `b` | browse breakdown dimensions → entries → JSON detail (usage/cost) |
| `f` | refresh quota/pricing sources (`--refresh`, explicit only) |
| `r` | reload |
| `?`/`h`, `esc`/`q`, `ctrl+c` | help / back / cancel-exit (reserved) |

Quota and hub detail views redact credential-shaped fields before
display (ADR-0014/0015 boundary). Non-TTY invocations print the CLI
equivalent and exit.
