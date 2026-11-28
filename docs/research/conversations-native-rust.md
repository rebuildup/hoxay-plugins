# Research: native Rust port of rebuildup.conversations (Issue #293)

Implementation + verification record for porting `plugins/conversations`
from TypeScript (Bun) to a committed native binary (Rust), per ADR-0035
(native plugin distribution) and ADR-0036 (native TUI runtime). Program
#286 / native-performance-baseline (#287) motivated the port; baseline
numbers live in `docs/research/native-performance-baseline.md`.

Verification host: macOS arm64 (Apple M5), herdr `0.9.5`
(`/Users/rebui/.local/share/mise/installs/herdr/0.9.5/herdr`). Linux and
Windows are **unverified** — `platforms = ["macos"]` in the manifest is
the honest scope until binaries for other targets exist and are verified.

## 1. What shipped

| piece | location | notes |
| --- | --- | --- |
| plugin binary | `plugins/conversations/bin/conversations` (~4.3 MB, committed, macOS arm64) | manifest argv is `./bin/conversations`; no Bun/toolchain needed at install or run time |
| domain + CLI | `plugins/conversations/src/{model,state,indexer,claude,codex,opencode,providers,read,cli}.rs` | contract `herdr-plugins.conversations/v1` stable; version `0.7.0` |
| TUI | `plugins/conversations/src/tui.rs` | contract `herdr-plugins.tui/v1`; in-process `cli::run_domain` calls (no self-respawn), same pattern as `plugins/inbox` |
| shared crates | `crates/herdr-client`, `crates/tui` (`safety`) | reused unchanged from #291 |
| TS implementation | deleted (`plugins/conversations/src/*.ts`, 4 retired test files) | recoverable at `4d6e08c~1` |

Manifest (`herdr-plugin.toml`) entrypoints: `[[panes]] main` →
`./bin/conversations tui main`, `[[actions]] open` →
`./bin/conversations tui --open`. Sibling plugins that delegated to
conversations (`plugins/usage/src/source.ts`,
`plugins/mcp/src/plugincli.ts`, `plugins/handoff/src/refs.ts`) now
resolve and spawn `bin/<name>` first with a `bun src/cli.ts` fallback —
the same ADR-0035 pattern `plugins/git/src/inbox.ts` established for
inbox in #291. `plugins/textgen` only consumes git/usage (still TS) and
needed no change.

## 2. Contract parity evidence

- `tests/conversations-native.test.ts` — 20 spawn-based contract tests
  against a fake herdr socket (same NDJSON protocol the TS suite used):
  usage errors, index/refresh/sessions/history/search/show/usage for
  claude+codex+opencode fixtures, cursor paging, stale/missing states,
  the #306 cursor-termination regression, and the non-TTY `tui`
  fallback.
- `scripts/diff-conversations.ts` — fixture-driven TS↔Rust
  output-parity harness. `git archive`s `plugins/conversations` +
  `packages` at the pinned pre-removal commit, runs both
  implementations against the same fake socket and fresh state dirs,
  and deep-compares normalized stdout JSON, exit codes, final
  `state.json`, and every `events/*.jsonl` line (each line parsed and
  key-sorted before comparison). Result: **40/40 checks identical**
  (usage-errors ×6, capability, index-all-providers,
  read-claude ×9, read-codex-opencode ×6, tui-fallback ×2, plus
  per-scenario state/events parity). Re-run: `bun
  scripts/diff-conversations.ts`.

**Parity boundary (honest scope):** the harness proves *normalized
JSON/schema parity* — same fields, values, counts, cursors, and exit
behavior — not byte-level identity. `serde_json::Map` is a `BTreeMap`
(keys sorted; `preserve_order` is not enabled), so object key order
inside emitted JSON can differ from JS insertion order, and float
formatting follows serde rules. All consumers parse JSON, so this is
safe; it is documented here rather than "fixed" because byte-order
parity would require `preserve_order` + hand-rolled number emission
for zero consumer benefit.

Parity work that made normalized-identical output possible (all in this PR):

- `state_change_seq`/`cost_usd` serialize with JS `JSON.stringify`
  number semantics — serde's `1.0` vs JS `1` cost +2 bytes/event-line
  and diverged `events_size`/`next_cursor` offsets.
- `encode_offset_cursor` emits `{"v":1,"o":{…}}` with `v` first
  (serde_json::Map sorts keys; the token is built manually now).
- `mtime_ms` uses `sec*1000 + nsec/1e6` f64 math, not
  `Duration::as_secs_f64()*1000` (last-bit rounding differed).
- `index`/`refresh` success outcomes carry exactly the TS
  `IndexOutcome` fields — `pane_id` appears only on skip outcomes.
- `tui --help` has no special case in TS (non-TTY notice wins); the
  Rust port matches. The only accepted stdout divergence is the
  non-TTY notice's CLI-equivalent hint (`bun run src/cli.ts` →
  `conversations`), normalized by the harness.

## 3. Bugs found by the port (all fixed)

1. **#306 cursor defect** — `scan_paged_sessions` now records
   `next_offset` for every processed session including fully-consumed
   ones; page 2+ no longer restarts finished files at byte 0. Covered by
   a dedicated regression test (usage export terminates, zero
   duplicates).
2. **O(n²) event paging** — the first `read_event_page` port drained a
   `Vec<u8>` pending buffer per line (full memmove per line): search
   miss on the 10k-event fixture took **1408 ms p50**. Rewritten on
   `BufReader::read_until`; same fixture now **260 ms p50**.
3. **Per-field `to_lowercase()` allocations** — the search matcher
   allocated a lowered `String` for every searchable field of every
   event. ASCII needle+haystack now takes an allocation-free
   `memchr2`-accelerated path (`field_contains_ci`); Unicode falls back
   to `to_lowercase().contains()` exactly like TS.

## 4. Isolated-session verification (np293)

Dedicated XDG/herdr session `np293`
(`XDG_CONFIG_HOME=/var/tmp/np293-herdr/config`, etc.; the user's live
sessions were never touched — all herdr CLI calls pinned
`HERDR_SOCKET_PATH` to the np293 socket after discovering `pane
current` resolves the attached session differently):

- `herdr plugin link plugins/conversations` — manifest parsed;
  `plugin list`/`plugin action list` show the `open` action
  (`./bin/conversations tui --open`), the `main` pane entrypoint
  (`./bin/conversations tui main`), `platforms=["macos"]`,
  `version 0.7.0`.
- `capability`, `index`, `refresh`, `sessions`, `history`, `search`,
  `show`, `usage` against the real socket: honest envelopes —
  `index`/`refresh` report `skipped: [{reason: "no_agent_session"}]`
  for the shell pane, read commands return explicit empty/not-indexed
  states with the `run index` hint.
- `herdr plugin action invoke open --plugin rebuildup.conversations`
  spawned `./bin/conversations tui --open`, exit 0; the TUI rendered
  in a real herdr pane (`rebuildup.conversations — sessions`, empty
  state, keymap footer). `?` opened the help overlay, `i` opened the
  confirm-gated reindex dialog, `y` ran the in-process index, `q`
  quit cleanly.
- `herdr plugin disable`/`enable rebuildup.conversations` toggled
  cleanly.
- **PTY visual evidence** (`spikes/conversations-tui/drive.py`,
  pyte screen snapshots in `spikes/conversations-tui/evidence/`,
  same driver pattern as `spikes/inbox-tui/` on #291): the np293
  plugin state dir was seeded with a read-only copy of the np287
  perf fixture (25 sessions / 10k events), provider roots pinned to
  empty dirs, `HERDR_SOCKET_PATH` pinned to the np293 socket. The
  `main` scenario captured: populated sessions list, cursor
  selection, session event log (`history` loader), scroll, event
  detail overlay, `s` search prompt + results, `/` filter
  draft/applied/cleared, `?` help overlay, and the confirm-gated
  `i` → dialog → `y` → post-index screens; clean `q` quit (exit 0).
  Human visual review of these snapshots remains a separate
  acceptance gate — the PR stays Draft until then.
- **Not exercised live:** a real indexed session. `agent_session` is
  not populated by `pane.report_agent_session` on herdr 0.9.5 (known
  upstream gap, ADR-0012 / herdr-socket-findings §agent_session) — it
  only appears for real agent integrations, so end-to-end indexing was
  verified via the fake-socket contract suite and the np287 perf
  fixture instead.
- `herdr plugin unlink` + `herdr plugin install
  rebuildup/hoxay-plugins/plugins/conversations --ref 293 --yes` —
  managed GitHub install resolved branch `293` → `4d6e08c`; the
  installed `bin/conversations` ran `capability`/`sessions` against
  the np293 session correctly.

## 5. Performance re-measurement (#287 baseline comparison)

Same host, same canonical fixture
(`/var/tmp/np287-herdr/state/herdr/plugins/rebuildup.conversations` —
25 sessions / 10,000 normalized events / ~5.5 MB `events/*.jsonl`),
same harness (`scripts/perf/measure.ts`, `scripts/perf/tui-probe.py`),
warm page cache, shared machine. Raw JSON:
`docs/research/native-performance-baseline/40`–`47-*.json`.

| operation | TS baseline (Bun) | native `bin/conversations` | delta |
| --- | --- | --- | --- |
| `capability` p50 (n=40) | 38 ms | 17 ms | ~2.2× faster |
| `search "database migration"` hit p50 | 169 ms | 36 ms | ~4.7× faster |
| `search "zzzqqq-nomatch"` miss p50 (full 10k scan) | 185 ms | 260 ms | **~0.7× — still slower, see notes** |
| `sessions --offline` p50 | 49 ms | 20 ms | ~2.5× faster |
| `history --limit 200` p50 | 143 ms | 25 ms | ~5.7× faster |
| `capability` max RSS (n=12) | 20 640 KB | 5 728 KB | ~3.6× smaller |
| `search` miss max RSS (n=12) | 39 648 KB | 7 440 KB | ~5.3× smaller |
| TUI `first_byte` p50 (n=12) | 55.4 ms | 27.8 ms | ~2× |
| TUI `data_usable` p50 | 1 539.1 ms | **29.0 ms** (p95 348.2, one outlier) | ~53× |
| TUI pgid peak RSS p50 | ~49 MB | ~7.7 MB | ~6.4× smaller |
| `usage` full export (1 651 events, 4 pages, #306-fixed) | >30 000 ms timeout (defect D1) | **~467 ms median, terminates** | unbounded → bounded |

Index/refresh on a synthetic provider-format fixture (25 claude
sessions × 200 user/assistant pairs ≈ 10k transcript lines → 15 060
normalized events, fake herdr socket, fresh state dirs; single runs,
shared machine):

| operation | TS `cli.ts` | native `bin/conversations` |
| --- | --- | --- |
| `index` first build | ~450–540 ms | ~940–1060 ms (**~2× slower**) |
| `index`/`refresh` no-op | ~215–370 ms | **~18–31 ms (~10× faster)** |
| `refresh` after +20 appended lines | ~236–280 ms | **~30–32 ms (~8× faster)** |

Notes:

- **Search miss is the one regression.** A miss scans and
  `serde_json::Value`-parses every event line; serde's per-line
  materialization is ~40 % slower than V8 `JSON.parse` on this corpus.
  The drain O(n²) fix took it from 1408 ms → 260 ms; the remaining gap
  is parse-bound. Hit-limited searches (the common case) are ~5×
  faster. A `simd-json`/streaming-parse follow-up is the obvious next
  step if miss-scan latency matters.
- **First-build index is ~2× slower** (one-time cost per transcript;
  normalized output is identical). The hot path — `refresh`/incremental
  reindex, which is what hooks and repeated `index` calls actually
  pay — is ~10× faster because cursors skip unchanged bytes instead of
  re-statting/re-hashing every transcript.
- `data_usable` p95 (348 ms) had one shared-machine outlier; p50 is
  the stable signal per the baseline doc's own guidance.
- TUI interactive keys were additionally exercised live in np293 (§4):
  `?` help, `i` confirm-gated reindex, `y` apply, `q` quit.

## 6. Known limitations / follow-ups

- **macOS arm64 binary only.** `platforms = ["macos"]`; Linux/Windows
  need verified builds before widening the manifest — ADR-0035.
- **Committed `bin/conversations` must be rebuilt on every source
  change** (`cargo build --release -p conversations && cp
  target/release/conversations plugins/conversations/bin/conversations`);
  no CI freshness gate yet — same follow-up as `bin/inbox`.
- Search-miss full-scan parse cost (§5) — candidate for `simd-json` or
  a two-phase raw-line prefilter that respects JSON escaping.
- opencode DB discovery order approximates `localeCompare` (ICU root)
  via a lowercase-alphanumeric skeleton + byte tiebreak
  (`db_name_cmp`); exact quaternary punctuation weighting is not
  replicated. Impact is nil in practice — each scan filters rows by
  `session_id`, so at most one DB can contain a given session.
- First-build index cost (§5) — candidate for parallel per-session
  scans or cheaper event materialization.
- ~~Managed-install verification~~ — done post-push (§4).
- ~~`state.rs` doc comment referencing a planned `index.sqlite`
  search index (`indexdb.rs`)~~ — resolved: the comment already
  records that the derived-index experiment was dropped in favor of
  byte-offset cursors over canonical `state.json` + `events/*.jsonl`.

## 7. Rollback

The TS implementation is intact at `4d6e08c~1` (`8ddb7db`). To roll a
user installation back: `herdr plugin install
rebuildup/hoxay-plugins/plugins/conversations --ref <previous-release-sha>`.
`state.json` + `events/*.jsonl` are schema-compatible in both
directions (the diff harness proves normalized parity — parsed,
key-sorted JSON equality, not byte identity; see §2), so no state
migration is needed either direction. Sibling plugins fall back to
`bun src/cli.ts` automatically if a checkout without `bin/` is
resolved.
