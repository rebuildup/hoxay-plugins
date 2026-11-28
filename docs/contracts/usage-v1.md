# Contract `herdr-plugins.usage/v1`

Token-usage aggregation + subscription quota + estimated-cost CLI
contract for the `rebuildup.usage` herdr plugin (Issue #90 T1,
Issue #91 T2, Issue #92 T3, Issue #93 T4 / parent #28). Follows the
same conventions as `herdr-plugins.conversations/v1` and the other
plugin contracts.

The plugin is **read-only**: `usage`/`cost` aggregation keeps no
durable index and recomputes every aggregate from the
`rebuildup.conversations` normalized index on each call. `quota`
caches successful provider readings briefly under the plugin state
dir (quota numbers only — never credentials, ADR-0014); `cost`
caches the public model price list the same way (ADR-0016). It never
reads PTY text, never scans provider transcripts itself, and never
fabricates usage numbers.

## Data source (ADR-0013)

`rebuildup.usage` consumes `rebuildup.conversations` **only through its
public read-only CLI contract** — the `usage` event-paging command plus
the `capability` provider matrix — never its state files. The
conversations plugin root resolves in this order:

1. `--conversations-root DIR` / `HERDR_USAGE_CONVERSATIONS_ROOT` /
   `conversationsRoot` in `config.json` — explicit override.
2. Sibling `../conversations` next to this plugin's root (covers the
   repo layout and managed `plugins/<name>` installs).
3. `herdr plugin list --json` discovery (`HERDR_BIN_PATH` or `herdr`
   on `PATH`).

State/config dirs for the conversations child are pinned explicitly
(`HERDR_PLUGIN_STATE_DIR`/`_CONFIG_DIR` are set to the *conversations*
plugin dirs), so ambient plugin env can never redirect its state.
Overridable via `--conversations-state-dir` /
`--conversations-config-dir`, `HERDR_USAGE_CONVERSATIONS_STATE_DIR` /
`_CONFIG_DIR`, or `config.json` keys `conversationsStateDir` /
`conversationsConfigDir`.

## Discovery

1. `herdr plugin list --json` → find `plugin_id == "rebuildup.usage"` →
   `plugin_root`.
2. `cd <plugin_root> && ./bin/usage capability` → verify
   `contract == "herdr-plugins.usage/v1"` and `sources` reports
   `conversations-index` `available`.

## Invocation

```bash
cd <plugin_root>
./bin/usage capability
./bin/usage usage [--pane PANE] [--session-id ID] [--provider ID] \
  [--from ISO] [--to ISO] [--session NAME] [--socket PATH] [--timeout MS] \
  [--offline] [--conversations-root DIR] [--conversations-state-dir DIR]
./bin/usage quota [--provider ID] [--refresh] [--timeout MS]
./bin/usage cost [--pane PANE] [--session-id ID] [--provider ID] \
  [--from ISO] [--to ISO] [--session NAME] [--socket PATH] [--timeout MS] \
  [--offline] [--refresh] [--conversations-root DIR] [--conversations-state-dir DIR]
./bin/usage divergence [--pane PANE] [--session-id ID] [--provider ID] \
  [--from ISO] [--to ISO] [--session NAME] [--socket PATH] [--timeout MS] \
  [--offline] [--refresh] [--conversations-root DIR] [--conversations-state-dir DIR]
```

- All commands print exactly one JSON document on stdout. Handled
  errors print `{contract, ok:false, error:{code,message,kind?}}` on
  **stderr** and exit 1; usage errors exit 2.
- `--session`/`--socket`/`--timeout` select the herdr session/socket
  for live-binding checks inside the conversations read (best-effort;
  unreachable degrades to `live: null`, never an error). `--offline`
  skips live checks entirely.
- `--pane`/`--session-id`/`--provider` are forwarded to the
  conversations `usage` selector (`--session-id` takes a record key
  `<provider>:<id>` or bare session id — same semantics as
  `conversations`).
- `--from`/`--to` are ISO-8601 timestamps compared as instants,
  inclusive on both ends, against each event's provider timestamp.
  `--from` after `--to`, or an unparseable bound, is a usage error.

## `capability`

```json
{
  "contract": "herdr-plugins.usage/v1",
  "command": "capability",
  "plugin": "rebuildup.usage",
  "version": "0.6.0",
  "read_only": true,
  "mutation_commands": [],
  "commands": ["capability", "usage", "quota", "divergence", "cost"],
  "sources": [
    {
      "id": "conversations-index",
      "kind": "normalized-events",
      "contract": "herdr-plugins.conversations/v1",
      "status": "available",
      "plugin_root": "/abs/…/plugins/conversations",
      "resolved_via": "sibling",
      "freshness": "as of the last `rebuildup.conversations index`/`refresh` run…"
    },
    {
      "id": "subscription-quota-api",
      "kind": "provider-quota",
      "status": "available",
      "credential_boundary": "docs/adr/0014-usage-quota-credential-boundary.md",
      "providers": {
        "claude": "implemented …",
        "codex": "implemented …",
        "opencode": "unsupported …"
      },
      "freshness": "short-term cache (default 300s) …"
    },
    {
      "id": "cliproxy-api",
      "kind": "http-quota-hub",
      "status": "available",
      "credential_boundary": "docs/adr/0015-usage-hub-credential-boundary.md",
      "hubs": [
        {
          "id": "home",
          "endpoint": "https://hub.example.com/",
          "key_env": "HERDR_USAGE_QUOTA_HUB_KEY",
          "key_configured": true
        }
      ],
      "providers": {
        "claude": "implemented via hub api-call …",
        "codex": "implemented via hub api-call …",
        "note": "hub accounts of other providers are reported per-account as unsupported"
      },
      "freshness": "shares the quota cache TTL …"
    },
    {
      "id": "model-pricing",
      "kind": "price-table",
      "status": "available",
      "source": "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json",
      "freshness": "cached under the plugin state dir (default 24h TTL, 30-day retention); stale tables are flagged, never silently trusted",
      "detail": "Estimated API cost only — list-price math over standard rates, never a bill. Unknown models report unpriced, never a guessed price. User overrides via config `modelPrices`."
    }
  ],
  "aggregation": { "dimensions": ["provider","model","host","project","session","pane"], "…": "…" },
  "generated_at": "…"
}
```

`sources` is the authoritative data-source matrix and extends
additively: `conversations-index` reports `available` (with
`plugin_root`/`resolved_via`) or `unavailable` (with `detail`) — the
command never fails on source resolution. `subscription-quota-api`
reports `available` with a per-provider implementation matrix and a
pointer to the credential-boundary ADR. `cliproxy-api` reports
`available` with the configured hub list (`hubs[]`: `id`, sanitized
`endpoint`, `key_env` name, `key_configured` boolean — never the key
value) and a per-provider implementation note; it implements the
CLIProxyAPI management-API path (`auth-files` + `api-call`) for claude
and codex accounts, with other hub providers reported per-account as
`unsupported` (ADR-0015). `model-pricing` (T4 / #93, ADR-0016)
describes the public LiteLLM rate table the `cost` command prices
against — `kind: "price-table"`, `source` URL, cache freshness, and
the estimate-not-billing disclaimer; it carries no credentials.

## `usage`

Pages the entire conversations `usage` export (opaque per-session byte
cursors, 500 events/page), deduplicates, applies the optional time
window, and aggregates.

```json
{
  "contract": "herdr-plugins.usage/v1",
  "command": "usage",
  "window": { "from": "2026-01-01T00:00:00Z", "to": null },
  "filters": { "pane_id": null, "session_id": null, "provider": null },
  "totals": {
    "events": 3,
    "sessions_with_usage": 2,
    "input_tokens": 130,
    "output_tokens": 22,
    "cache_read_input_tokens": 43,
    "cache_creation_input_tokens": 8,
    "thinking_tokens": 6,
    "thinking_token_events": 2,
    "total_tokens": 203,
    "provider_reported_cost_usd": 0.0123,
    "provider_reported_cost_events": 1,
    "excluded_untimestamped_events": 0,
    "duplicate_events_skipped": 0,
    "events_without_event_id": 0
  },
  "cache_tokens": {
    "providers": [
      {
        "provider": "claude",
        "status": "reported",
        "events": 2,
        "cache_read_input_tokens": 43,
        "cache_creation_input_tokens": 8,
        "detail": null
      },
      {
        "provider": "nocache-prop",
        "status": "unsupported",
        "events": 0,
        "cache_read_input_tokens": 0,
        "cache_creation_input_tokens": 0,
        "detail": "provider adapter declares no cache-token accounting"
      }
    ]
  },
  "breakdowns": {
    "by_provider":  [{ "key": "claude", "events": 2, "input_tokens": 30, "…": "…" }],
    "by_model":     [{ "key": "claude-test-1", "…": "…" }],
    "by_host":      [{ "key": "macbook", "…": "…" }],
    "by_project":   [{ "key": "/repo/proj", "…": "…" }],
    "by_session":   [{ "key": "claude:<id>", "…": "…" }],
    "by_pane":      [{ "key": "w1:p1", "…": "…" }]
  },
  "coverage": {
    "sessions_total": 2,
    "sessions_ok": 2,
    "sessions_with_issues": 0,
    "sessions_stale": 0,
    "sessions_events_missing": 0,
    "providers": [
      { "provider": "claude", "provider_status": "supported", "sessions": 1, "sessions_ok": 1, "sessions_with_issues": 0, "usage_events": 1 }
    ]
  },
  "sessions": [ { "…": "contributing session registry entries" } ],
  "source": {
    "plugin": "rebuildup.conversations",
    "contract": "herdr-plugins.conversations/v1",
    "plugin_root": "/abs/…/plugins/conversations",
    "resolved_via": "sibling",
    "pages": 1,
    "complete": true
  },
  "warnings": [],
  "generated_at": "…"
}
```

### Totals semantics

- `events` — usage-carrying events summed (after dedup + window).
- `input_tokens` / `output_tokens` / `cache_read_input_tokens` /
  `cache_creation_input_tokens` — sums of the provider-reported fields
  carried on `assistant_message` events by the conversations contract.
- `total_tokens` — sum of the four canonical buckets. `thinking_tokens`
  is **not** added on top: it is a provider detail of output/reasoning
  accounting, reported separately.
- `thinking_tokens` / `provider_reported_cost_usd` — `null` when **no**
  contributing event reported them; never zero-fabricated.
  `provider_reported_cost_usd` sums the provider-reported `cost_usd`
  field only — it is **not** a billing amount. Estimated API cost is a
  separate figure computed by the `cost` command (T4 / #93) — see
  below; the two are never blended.
- `excluded_untimestamped_events` — events dropped by `--from`/`--to`
  because the provider timestamp was absent/unparseable. Without a
  window they are included and this stays 0.
- `duplicate_events_skipped` — events dropped because the same
  `(session_key, event_id)` was already counted (index rewrites can
  replay events mid-export).
- `events_without_event_id` — events lacking a usable `event_id`;
  included in totals but cannot be deduplicated (contract violation
  upstream — surfaced, not hidden).

### Breakdowns

`breakdowns` has six dimensions, each an array of entries sorted by
`total_tokens` desc:

| key | source |
| --- | --- |
| `by_provider` | session record key prefix (`<provider>:<id>`) |
| `by_model` | event `model`; `null` when unreported |
| `by_host` | `event.identity.host`; `null` when absent |
| `by_project` | session record `cwd` (the pane cwd at index time); `null` when unknown |
| `by_session` | session record key |
| `by_pane` | `event.identity.pane_id` (historical binding at index time); `null` when unbound |

Each entry: `{key, events, input_tokens, output_tokens,
cache_read_input_tokens, cache_creation_input_tokens, thinking_tokens
|null, total_tokens, provider_reported_cost_usd|null}`.

### Cache tokens

`cache_tokens.providers[]` is the per-provider cache-token view
(T3 / #92): `{provider, status, events, cache_read_input_tokens,
cache_creation_input_tokens, detail}`. Token sums come from the same
measured events as `breakdowns.by_provider`; `status` comes from the
conversations capability matrix (`providers[].usage.cache_tokens`,
additive in `herdr-plugins.conversations/v1`):

| status | meaning |
| --- | --- |
| `reported` | adapter is supported and declares cache-token mapping; sums are measured (zeros mean "no cache tokens measured", a real measurement) |
| `unsupported` | provider adapter is unsupported, or declares no cache-token accounting — explicit, never a silent zero |
| `unknown` | capability could not be verified (provider absent from the matrix or capability fetch failed) |

`events` counts usage-carrying events for the provider (0 = none
measured — coverage, not a zeroed measurement).

### Coverage — missing is not zero

`coverage` reports registry truth, separate from measured totals:

- `sessions_total` / `sessions_ok` / `sessions_with_issues` — indexed
  sessions and how many have `status != "ok"`, `error != null`, or
  `events_present == false`.
- `sessions_stale` — sessions whose pane binding changed generation
  (`agent_session_changed`); their historical usage still counts (it is
  a fact about the past), flagged here.
- `sessions_events_missing` — normalized event logs absent.
- `providers[]` — per-provider `{provider, provider_status,
  sessions, sessions_ok, sessions_with_issues, usage_events}`;
  `provider_status` comes from the conversations capability matrix
  (`supported`/`unsupported`), so "declared but unsupported" is
  distinguishable from "supported, nothing measured". Providers with
  zero indexed sessions still appear.

`sessions[]` lists only sessions that contributed ≥1 usage event, with
their registry status — a session with no measured usage is visible in
`coverage` but contributes no tokens.

### Warnings

`warnings[]` names degraded-but-successful conditions:
`export_incomplete_page_limit` (export hit the 2M-event safety cap —
totals are partial, `source.complete: false`),
`index_malformed_lines`, `index_duplicate_events_skipped`,
`index_sessions_restarted` (opencode-style event-log rewrites replayed
events — dedup absorbed them), `untimestamped_events_excluded_by_window`,
`duplicate_events_skipped`, `events_without_event_id`.

An entirely empty index returns `totals.events: 0`,
`coverage.sessions_total: 0`, and `hint` suggesting
`rebuildup.conversations index` — zeros here mean "nothing indexed",
not "zero usage measured".

## `quota`

Subscription quota/reset report per provider. Independent of the
conversations index — it does not resolve the conversations plugin and
needs no herdr session at all.

```bash
./bin/usage quota [--provider ID] [--refresh] [--timeout MS]
```

```json
{
  "contract": "herdr-plugins.usage/v1",
  "command": "quota",
  "cache": { "ttl_seconds": 300 },
  "providers": [
    {
      "provider": "claude",
      "status": "ok",
      "detail": null,
      "plan": "max",
      "windows": [
        {
          "id": "five_hour",
          "label": "Session (5-hour)",
          "kind": "session",
          "used_percent": 42.5,
          "resets_at": "2026-01-01T05:00:00.000Z",
          "window_duration_mins": 300
        }
      ],
      "checked_at": "2026-01-01T00:30:00.000Z",
      "fetched_at": "2026-01-01T00:31:00.000Z",
      "stale": false,
      "stale_reason": null,
      "reset_credits_available": null,
      "cache": { "hit": false, "age_seconds": null, "ttl_seconds": 300 }
    }
  ],
  "hubs": [
    {
      "id": "home",
      "kind": "cliproxy-api",
      "endpoint": "https://hub.example.com/",
      "status": "ok",
      "detail": null,
      "checked_at": "2026-01-01T00:30:00.000Z",
      "fetched_at": "2026-01-01T00:31:00.000Z",
      "stale": false,
      "stale_reason": null,
      "cache": { "hit": false, "age_seconds": null, "ttl_seconds": 300 },
      "accounts": [
        {
          "account": "acct-file-id",
          "provider": "claude",
          "status": "ok",
          "detail": null,
          "plan": "Claude Subscription",
          "windows": [ { "…": "same QuotaWindow shape as providers[].windows" } ],
          "checked_at": "2026-01-01T00:30:00.000Z",
          "reset_credits_available": null
        },
        {
          "account": "acct-file-id-2",
          "provider": "xai",
          "status": "unsupported",
          "detail": "hub provider \"xai\" has no quota mapper in this plugin",
          "plan": null,
          "windows": [],
          "checked_at": null,
          "reset_credits_available": null
        }
      ]
    }
  ],
  "generated_at": "…"
}
```

`hubs[]` (T3 / #92, ADR-0015) reports every configured
CLIProxyAPI-equivalent hub; empty when none are configured. Hub
`status` values: `ok` (auth-files answered; per-account outcomes are in
`accounts[]`), `no_credential` (the `keyEnv` env var is unset — the
detail names the env var, never a value), `unauthorized` (hub answered
401/403), `request_failed` (other HTTP/transport/parse failure, or an
invalid hub URL). `stale`/`stale_reason`/`cache` follow the same
semantics as provider entries; the cache file is
`quota-cache/hub-<id>.json`.

Per-account `status` reuses the provider status taxonomy plus
`skipped` is never emitted — disabled auth-files are omitted entirely.
Accounts of providers without a mapper (`xai`, `antigravity`, …)
report `unsupported` with the reason. `account` is the hub-side
auth-file id — a local label, not a credential.

### Provider statuses

`status` is per provider and is **data, not an error** — `quota` exits
0 whenever it can emit the envelope:

| status | meaning |
| --- | --- |
| `ok` | fresh (`cache.hit:false`) or cached (`cache.hit:true`) provider reading; `stale:true` marks a cached reading served because the endpoint failed |
| `unsupported` | no quota artifact exists (opencode API-key store, codex `auth_mode:"apikey"`, unknown provider) — `detail` records why |
| `no_credential` | the provider credential store has no usable login |
| `credential_expired` | a token exists but is past expiry; refresh is out of scope (ADR-0014) |
| `request_failed` | endpoint unreachable/HTTP error/unparseable and no cache to serve — `detail` carries `HTTP <status>` or a transport summary only |

- `windows[]` — provider-reported quota windows: `id`, `label`,
  `kind` (`session`/`weekly`/`monthly`/`other`), `used_percent`
  (0-100), `resets_at` (ISO instant or `null`), `window_duration_mins`.
  Non-`ok` statuses always carry `windows: []` — missing is never a
  fabricated zero.
- `checked_at` — when the provider endpoint last answered; `null`
  unless `status` is `ok`.
- `plan` — provider plan label (Claude `subscriptionType`, Codex plan
  name); `null` when unreported.
- `reset_credits_available` — banked Codex reset-credit count when the
  credits endpoint answers; `null` otherwise (never fabricated).
- `stale`/`stale_reason` — `true` only when serving a cached reading
  older than the fresh TTL because the endpoint failed; the reason is
  recorded, the data is labeled.

### Quota freshness & rate limiting

Successful readings are cached at `<HERDR_PLUGIN_STATE_DIR>/quota-cache/
<provider>.json` for `ttl_seconds` (default 300, config
`quotaCacheTtlSeconds`). `--refresh` bypasses the fresh-cache read.
The cache stores quota numbers only — never credential material
(ADR-0014). Requests time out after `--timeout` ms (default 10s);
endpoint overrides exist only as test/proxy hooks:

- env: `HERDR_USAGE_QUOTA_CLAUDE_URL`, `HERDR_USAGE_QUOTA_CODEX_USAGE_URL`,
  `HERDR_USAGE_QUOTA_CODEX_CREDITS_URL`
- config.json: `quotaEndpoints: {claude, codexUsage, codexCredits}`

When `codexUsage` is overridden without `codexCredits`, the credits
call is skipped (a mock must opt in explicitly).

### State retention & GC (T4 / #93, ADR-0016)

Every plugin cache entry — `quota-cache/<provider>.json`,
`quota-cache/hub-<id>.json`, `pricing-cache/litellm.json` — shares one
retention window: **30 days** (`PLUGIN_CACHE_RETENTION_SECONDS`). An
entry is *referenced* while its payload timestamp is inside
retention, so fresh reads, in-TTL hits, and stale-serve-on-failure
are never collected. `gcPluginStateCache` runs best-effort on every
quota/pricing collection and removes only entries **older** than
retention, orphaned `*.tmp-*` write temps, and unparseable files.
Reads independently refuse entries past retention, so a failed GC
can never resurrect ancient data. `usage`/`cost` aggregation itself
keeps no index — totals are recomputed per call; the conversations
event index is owned by `rebuildup.conversations`.

### Quota hubs (T3 / #92, ADR-0015)

`quotaHubs` in `config.json` declares CLIProxyAPI-compatible hubs:

```json
{
  "quotaHubs": [
    { "id": "home", "url": "https://hub.example.com", "keyEnv": "MY_HUB_KEY" }
  ]
}
```

- `url` — hub base URL; `management.html` suffixes, query, and hash
  are normalized away. Non-http(s) URLs are rejected per hub.
- `keyEnv` — **name** of the env var holding the management key
  (default `HERDR_USAGE_QUOTA_HUB_KEY`). The key value is never stored
  in config, state, cache, logs, or output (ADR-0015).
- Protocol: `GET {url}/v0/management/auth-files` then
  `POST {url}/v0/management/api-call` per enabled account whose
  provider has a mapper (claude, codex). Upstream calls always use
  `method: "GET"`; provider tokens stay hub-side behind the `$TOKEN$`
  placeholder.
- Hub cache: `quota-cache/hub-<id>.json`, same TTL/`--refresh`
  semantics; only `ok`/`unsupported` account readings are cached —
  transient `request_failed` accounts are not.

### Credential boundary (ADR-0014)

`quota` reads provider credential artifacts **in memory only** to
authenticate the provider's own read-only quota endpoints: Claude
`$CLAUDE_CONFIG_DIR/.credentials.json` (macOS Keychain
`Claude Code-credentials` fallback only when `CLAUDE_CONFIG_DIR` is
unset) and Codex `$CODEX_HOME/auth.json`
(ChatGPT auth only). Tokens are never logged, persisted, cached,
emitted, or passed to subprocesses; expired tokens report
`credential_expired` rather than triggering a refresh. Full rules:
`docs/adr/0014-usage-quota-credential-boundary.md`. Hub sources follow
the parallel boundary in
`docs/adr/0015-usage-hub-credential-boundary.md`.

## `cost`

Estimated API cost report (T4 / #93, ADR-0016). Runs the same
aggregation as `usage` (same selectors/window), then prices each
`by_model` bucket against a per-model rate table. The estimate is
**always labeled as an estimate — never a bill**.

```bash
./bin/usage cost [--pane PANE] [--session-id ID] [--provider ID] \
  [--from ISO] [--to ISO] [--session NAME] [--socket PATH] [--timeout MS] \
  [--offline] [--refresh] [<source flags>]
```

```json
{
  "contract": "herdr-plugins.usage/v1",
  "command": "cost",
  "window": { "from": null, "to": null },
  "filters": { "pane_id": null, "session_id": null, "provider": null },
  "cost": {
    "estimate": true,
    "disclaimer": "estimated_cost_usd is a list-price ESTIMATE … not a bill, invoice, or subscription charge …",
    "estimated_cost_usd": 0.0002784,
    "estimate_complete": false,
    "priced_models": 1,
    "unpriced_models": [
      { "key": "gpt-6-test", "events": 1, "total_tokens": 156 }
    ],
    "provider_reported_cost_usd": 0.0123,
    "pricing": {
      "status": "ok",
      "source": "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json",
      "fetched_at": "…",
      "cache": { "hit": false, "age_seconds": null, "ttl_seconds": 86400 },
      "known_models": 7175,
      "user_overrides": 0,
      "detail": null
    },
    "by_model": [
      {
        "key": "claude-test-1",
        "events": 2,
        "input_tokens": 30,
        "output_tokens": 12,
        "cache_read_input_tokens": 3,
        "cache_creation_input_tokens": 2,
        "total_tokens": 47,
        "thinking_tokens": 1,
        "provider_reported_cost_usd": 0.0123,
        "estimated_cost_usd": 0.0002784,
        "cost_source": "model_priced"
      }
    ]
  },
  "totals": { "…": "same UsageTotals as `usage`" },
  "breakdowns": { "…": "same UsageBreakdowns as `usage`" },
  "coverage": { "…": "same UsageCoverage as `usage`" },
  "sessions": [ "…" ],
  "source": { "…": "same source block as usage" },
  "warnings": ["unpriced_models"],
  "generated_at": "…"
}
```

### Estimate semantics

- `cost.estimate` is always `true` and `disclaimer` is a fixed string
  stating the figure is a list-price estimate, not a bill — it
  excludes speed premiums, context-tier repricing, provider
  discounts, taxes, and quota consumption.
- `estimated_cost_usd` sums `cost.by_model[].estimated_cost_usd` over
  **priced models only**; `null` when nothing could be priced.
  `estimate_complete: false` discloses that unpriced models
  contributed tokens, so a partial total is never mistaken for a
  complete one. `unpriced_models[]` lists them (`{key, events,
  total_tokens}`; `key: null` = the event reported no model).
- `cost_source` per model row: `model_priced` (rate table or user
  override produced a price) or `unpriced` (`estimated_cost_usd:
  null` — **never a guessed price**).
- `provider_reported_cost_usd` appears both per row and at cost
  level, mirroring `totals` — provider-reported cost is a separate
  figure and is never blended into the estimate.
- `pricing` reports provenance: `status` (`ok` fresh/in-TTL hit,
  `stale` cached table served because the source failed — flagged,
  `unavailable` no usable table), `source` URL, `fetched_at`,
  `cache.{hit,age_seconds,ttl_seconds}`, `known_models`, and
  `user_overrides` (config `modelPrices` entries applied).

### Pricing table (ADR-0016)

The rate table is LiteLLM's public
`model_prices_and_context_window.json` (same upstream T3 Code
prices from), fetched at runtime and cached at
`<state>/pricing-cache/litellm.json` — fresh TTL 24h (config
`pricingCacheTtlSeconds`), shared 30-day retention. Lookup order per
model: user `modelPrices` override → exact normalized key → bare
name (provider prefix dropped) → date-suffix-stripped variants.
Speed/context-tier rates are not applied (aggregates carry no
per-request signal); 1-hour cache writes price at the standard
cache-creation rate. `thinking_tokens` is never double-counted —
providers report it inside `output_tokens`.

User overrides (config.json, USD per million tokens):

```json
{
  "modelPrices": {
    "my-model": {
      "inputCostPerMillionTokens": 3,
      "outputCostPerMillionTokens": 15,
      "cacheReadCostPerMillionTokens": 0.3,
      "cacheWriteCostPerMillionTokens": 3.75
    }
  }
}
```

Source override for tests/proxies: env `HERDR_USAGE_PRICING_URL` or
config `pricingUrl` (env wins). An unavailable table is data, not an
error: `pricing.status: "unavailable"`, every model `unpriced`,
`estimated_cost_usd: null`, `pricing_unavailable` warning, exit 0.
Stale tables add `pricing_table_stale`; unpriced contributors add
`unpriced_models` to `warnings[]`.

## `divergence`

Quota-vs-measured divergence report (T3 / #92). Runs the same
aggregation as `usage` (same selectors/window) plus the direct
provider quota probes, then places both sides side by side per
provider. Exit semantics match `usage`/`quota`: per-provider outcomes
are data, not errors.

```bash
./bin/usage divergence [--provider ID] [--from ISO] [--to ISO] \
  [--refresh] [--offline] [<usage selector/source flags>]
```

```json
{
  "contract": "herdr-plugins.usage/v1",
  "command": "divergence",
  "window": { "from": "2026-01-01T00:00:00Z", "to": "2026-01-01T05:00:00Z" },
  "filters": { "pane_id": null, "session_id": null, "provider": null },
  "comparisons": [
    {
      "provider": "claude",
      "verdict": "different_units",
      "detail": "quota windows report provider-side subscription utilization … never reconciled into each other.",
      "quota": {
        "status": "ok",
        "detail": null,
        "plan": "max",
        "checked_at": "…",
        "stale": false,
        "windows": [ { "…": "QuotaWindow" } ]
      },
      "measured": {
        "events": 2,
        "input_tokens": 30,
        "output_tokens": 12,
        "cache_read_input_tokens": 3,
        "cache_creation_input_tokens": 2,
        "total_tokens": 47
      },
      "window_alignment": "matches_quota_window"
    }
  ],
  "hubs": [ "…same shape as quota.hubs…" ],
  "notes": ["…"],
  "source": { "…": "same source block as usage" },
  "generated_at": "…"
}
```

### Verdicts

| verdict | meaning |
| --- | --- |
| `different_units` | both sides present — quota `used_percent` and transcript token sums are **different units from different sources** (a quota window counts every client on the subscription; the index covers local sessions only). Reported side by side, never reconciled, never converted. |
| `no_measured_usage` | quota answered `ok` but the index has no usage events for the provider |
| `quota_unavailable` | measured usage exists but quota status is non-`ok` (`detail` carries the status reason) |
| `no_data` | neither side has data |

`window_alignment` describes only whether the `--from`/`--to` span
matches a reported quota window's `window_duration_mins`:
`unbounded` (no window), `matches_quota_window`, `custom_window`,
`unknown` (span not computable or no durations reported). A match does
not imply the measurements cover the same traffic.

`hubs[]` carries the same hub reports as `quota`, but hub accounts are
**excluded from `comparisons`**: a hub account cannot be proven to be
the same subscription as the locally indexed transcripts (noted in
`notes[]`).

## Errors

`kind` values: `conversations_unavailable` (plugin root not resolvable
or override invalid), `conversations_error` (the conversations CLI
exited non-zero — stderr detail carried in `message`), plus usage
errors (exit 2) for bad `--from`/`--to`. A missing conversations
**index** is not an error — it is the honest empty result above.

## Versioning

`herdr-plugins.usage/v1` is **stable** as of plugin v0.5.0: command
surface (`capability`/`usage`/`quota`/`divergence`/`cost`),
totals/breakdown/cache-token/coverage, cost-estimate, quota/hub, and
divergence report shapes, and error `kind` taxonomy are the v1
contract. Additive fields may appear anywhere; clients must ignore
unknown fields. `sources`, `breakdowns` dimensions,
`coverage.providers`, `cache_tokens.providers`, `cost.by_model`,
quota `windows`/`hubs`/`accounts`, divergence
`comparisons`/`verdicts`, provider `status` values, and pricing
`status` values extend additively.

A breaking change — removing/renaming a command or field, changing an
error `kind`'s meaning, altering dedup/window semantics — bumps to
`herdr-plugins.usage/v2`, shipped alongside v1 for at least one
release; v1 is then frozen, not deleted.
