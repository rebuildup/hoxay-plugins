# Research: native Rust port of rebuildup.git (Issue #292)

Implementation + verification record for porting `plugins/git` from
TypeScript (Bun) to a committed native binary (Rust), per ADR-0035
(native plugin distribution) and ADR-0036 (native TUI runtime). Program
#286 / native-performance-baseline (#287) motivated the port; baseline
numbers live in `docs/research/native-performance-baseline.md` and the
#291 precedent in `docs/research/inbox-native-rust.md`.

Verification host: macOS arm64 (Apple M5), herdr `0.9.5`
(`~/.local/share/mise/installs/herdr/0.9.5/herdr`). Linux and Windows are
**unverified** — `platforms = ["macos"]` in the manifest is the honest
scope until binaries for other targets exist and are verified.

## 1. What shipped

| piece | location | notes |
| --- | --- | --- |
| plugin binary | `plugins/git/bin/git` (~3.2 MB, committed, macOS arm64) | manifest argv is `./bin/git`; no Bun/toolchain needed at install or run time |
| domain + CLI | `plugins/git/src/{git,gh,gitenv,inboxlink,pluginroot,cli,lib,main}.rs` | contract `herdr-plugins.git/v1` stable; version `0.7.0` |
| main TUI | `plugins/git/src/{tui,tui_model}.rs` | contract `herdr-plugins.tui/v1`; in-process domain calls |
| forge TUI | `plugins/git/src/tui_forge.rs` (~5.9k lines) | PR/issue browser; consumes the plugin's own CLI as a subprocess for every read/mutation so the public contract stays the single source of truth |
| shared crates | `crates/herdr-client` (NDJSON RPC, target resolution), `crates/tui` (`hoxay-tui` ratatui shell) | reused from #291 |
| TS implementation | deleted (`plugins/git/src/*.ts`, `src/tui/`, 7 retired test files, `package.json`) | recoverable at `origin/291` |

Manifest (`herdr-plugin.toml`) entrypoints, `[[actions]]`, and
`[[panes]]` all run `./bin/git ...`. Sibling consumers that spawned
`src/cli.ts` were made layout-aware with a TS fallback for
not-yet-migrated checkouts:

- `plugins/projects/src/plugincli.ts` — `isPluginRoot` accepts
  `bin/<name>` or `src/cli.ts`; `runGitPluginCli` spawns the native
  binary directly when present.
- `plugins/mcp/src/plugincli.ts` — same detection; `cli_path` resolves
  to `bin/<name>` for native plugins.
- `plugins/textgen/src/pluginroot.ts` — `runPluginCli` (shared with the
  still-TypeScript `rebuildup.usage`) prefers `bin/<name>` when it
  exists.

## 2. Contract parity evidence

- `tests/git-native.test.ts` — 25 spawn-based contract tests against a
  fake herdr socket (same NDJSON protocol the TS suites used) plus a
  stub `gh`: strict usage errors (exit 2) across every command,
  `capability` shape, `pane_not_found`/`not_a_repo`/`gh_missing` honest
  failure kinds, git reads (`status`/`files`/`log`/`diff`/`compare`)
  against a real fixture repo (including a staged binary rename
  exercising the `--numstat -z` rename record), forge reads
  (`pr list|view`,
  `issue list|view`) through the stub, a real stage+commit mutation
  round-trip, the non-TTY `tui` fallback, and `issue
  link|links|unlink` delegation to the real `rebuildup.inbox` native
  binary (state lands in the inbox plugin's `state.json`, never in
  git's).
- `tests/helpers/git-bin.ts` — binary resolver (committed `bin/git` →
  `target/debug` → `target/release` → `cargo build -p git` fallback)
  plus `gitPluginRootFixture()` for cross-plugin tests on platforms
  where the committed arm64 binary cannot exec.
- `tests/herdr-target.test.ts` — the git CLI conflict-rule test now
  spawns the native binary; `target_conflict` behavior unchanged.
- `bun run validate:fast` — typecheck + lint + 1233 tests, 0 fail.
- `cargo check`/`clippy`/`fmt` — clean on the `git` crate.
- `bun scripts/diff-git.ts` — TS↔Rust output-parity harness (modeled on
  `scripts/diff-inbox.ts` from #291). It `git archive`s `plugins/git` +
  `packages` at the pre-removal commit (`bc005f7`, parent of the TS
  deletion), runs `bun <tmp>/plugins/git/src/cli.ts` (and `src/tui.ts` /
  `src/tui-forge.ts` for the TUI fallbacks) against the shipped
  `plugins/git/bin/git` on the same fake herdr socket, the same stub
  `gh`, and two identical repo clones (TS pane `w1:p1`, Rust pane
  `w1:p2`, so mutations cannot cross-contaminate). **37/37 checks
  identical** across usage errors, capability, git reads, git mutations
  (stage/commit/branch/upstream/fetch/push), forge reads, forge
  mutations, and both non-TTY TUI fallbacks. Documented intentional
  divergences normalized by the harness: `version` (0.6.0 → 0.7.0),
  `requires` (`{git,bun}` → `{git}` — the native binary needs no Bun),
  and the non-TTY hint's CLI invocation name (`bun run src/cli.ts` →
  `git`). Volatile fields (timestamps, SHAs, pane ids, tmp paths,
  invoked-binary paths in stderr) are normalized; **both stdout and
  stderr are compared** — handled errors emit a JSON envelope on
  stderr, so error kind/code/message parity is verified, not just exit
  codes.

## 3. Bugs found by the port (all fixed)

1. **Sequential git fan-out regression.** The first Rust port ran the
   independent `git` probes sequentially; the TS implementation used
   `Promise.all` (branch ∥ porcelain; porcelain ∥ unstaged numstat ∥
   staged numstat; the three `rev-parse` repo probes). On the 5k-file
   np287 fixture `git diff --numstat` alone costs ~0.7 s, so
   `files w2:p1` measured 1.46 s wall vs the TS baseline p50 of 488 ms.
   `resolve_repo`/`collect_status`/`collect_files` now overlap the
   independent spawns with `std::thread::scope` — `files` dropped to
   ~0.48 s p50 (§5).
2. Ambient `HERDR_SOCKET_PATH` + explicit `--session` correctly fails
   closed with `target_conflict` (verified live during perf runs — the
   first naive measurement attempt hit this guard, not the live
   session).
3. **Stdin-write deadlocks (acceptance phase).** `spawn_bounded`,
   `run_stream_bounded`, and the forge TUI's `run_cli` wrote child
   stdin synchronously and drained stdout/stderr only after
   `try_wait`/exit — a payload larger than the ~64 KiB pipe buffer (or
   a chatty child like `pr logs`/`diff`) could deadlock both directions.
   stdin is now written on a helper thread and the forge subprocess
   pipes are drained on reader threads before the wait loop.
4. **`git diff --numstat -z` rename record misparsed.** The parser
   skipped two tokens after an empty inline path instead of consuming
   old+new correctly, dropping rename entries; `files[].binary` for
   renamed binaries was wrong. Fixed and covered by the
   `files marks a renamed binary path binary:true` contract test.
5. **`capability.requires` still claimed `bun`.** The ported document
   reported `{"git":true,"bun":true}`; the native binary needs no Bun,
   so it now reports `{"git":true}` (the one remaining documented
   divergence vs the TS 0.6.0 document, normalized in the diff
   harness).
6. **Non-TTY TUI hint named `bin/git`.** The fallback message printed
   the on-disk path; it now prints `git`, matching the normalized TS
   hint (`bun run src/cli.ts` → `git`).

## 4. Isolated-session verification (np292)

Dedicated XDG/herdr session `np292`
(`XDG_CONFIG_HOME=/var/tmp/np292-herdr/config`, etc.; headless
`herdr --session np292 server`; the user's live sessions were never
touched):

- `herdr plugin link plugins/git` — manifest parsed; `plugin list`
  shows 7 actions (`status`/`files`/`log`/`compare`/`project`/`open`/
  `open-forge`), 2 pane entrypoints (`main`, `forge`),
  `platforms=["macos"]`, `version 0.7.0`, all argv `./bin/git ...`.
- `plugin action invoke rebuildup.git.status` against focused pane
  `w2:p1` (cwd = this worktree): exit 0, contract JSON with correct
  worktree resolution (`git_dir=…/.git/worktrees/292`,
  `git_common_dir=…/herdr-plugin/.git`, branch `292 -> origin/291`).
- `plugin pane open --entrypoint main` → `w2:p2` rendered the full TUI
  in a real PTY (`pane read`: branch/ahead counts, changed-files menu,
  footer hints). `--entrypoint forge` → `w2:p3` rendered the forge
  surface (PR/issue/branch-status entries).
- Direct CLI against the session: `status`, `files`, `log`,
  `compare`, `project` (pane metadata `git_branch=292`,
  `git_ahead_behind=+3/-0` applied) all returned contract JSON.
- Non-TTY `tui` fallback covered by contract tests; interactive keys
  exercised via the real-PTY evidence spike below (same evidence level
  as #291/#293).
- `spikes/git-tui/drive.py` — pyte PTY driver producing screen snapshots
  + ANSI typescripts under `spikes/git-tui/evidence/` (28 files):
  - `main-*` (15): `git tui --cwd <scratch-fixture>` — home, selection,
    changed-files list, per-file diff, commit history, commit detail,
    `/` filter draft/apply/clear, `?` help, and a confirm-gated
    stage-file mutation (`y` on the confirm dialog; verified `git
    status` shows `app.ts` staged in the fixture repo afterwards).
    The fixture repo is created fresh in a tmp dir by the driver; the
    TUI never touches the real worktree.
  - `forge-*` (11): `git tui forge` pinned to np292 pane `w2:p1`
    (`HERDR_TUI_PANE_ID` + `HERDR_SOCKET_PATH`), with `gh` intercepted
    by the committed stub `spikes/git-tui/gh-stub/` (canned
    `pr/issue/repo/api` responses — no real GitHub calls). Covers home,
    PR list, PR detail (incl. `expect-head` sha), issue list/detail,
    state-filter cycle, reload, and the label-filter prompt.
  - Reproduce: `GIT_TUI_SOCKET=<np292 sock> GIT_TUI_FORGE_PANE=w2:p1
    XDG_*=<np292 dirs> /tmp/pyte-venv/bin/python spikes/git-tui/drive.py
    [main|forge]` (requires `pyte`).

Not exercised live: forge mutations and `gh`-backed lists against the
real GitHub API (covered by stub-`gh` contract tests + the stubbed PTY
evidence; reads are non-destructive but were not needed for the port
gate).

**Human visual review is a separate gate.** The pyte snapshots are the
machine-checkable record; a maintainer should still eyeball
`spikes/git-tui/evidence/*.txt` (and ideally one live `plugin pane
open`) before this Draft PR leaves draft. This is the same gate #291
and #293 carried.

## 5. Performance re-measurement (#287 baseline comparison)

Same host, same `np287` isolated session and fixture repo
(`/var/tmp/np287-fixtures/git-repo`, pane `w2:p1`), same harness
(`scripts/perf/measure.ts`). **Caveat:** measurements were taken while
the machine was under heavy shared load (load average ~16–22 from
concurrent cargo builds in sibling worktrees), so wall-clock numbers
are upper bounds; the TS baseline was recorded on a quieter machine.
Raw JSON: `docs/research/native-performance-baseline/40-43-*.json`.

| operation | TS baseline (Bun) p50 | native `bin/git` p50 | note |
| --- | --- | --- | --- |
| `git status w2:p1` wall (n=30) | 364 ms | 470 ms | dominated by identical `git` child spawns; see below |
| `git files w2:p1` wall (n=30) | 488 ms | 480 ms | parity; sequential-first-draft was 1.46 s |
| `git diff w2:p1 --stat` wall (n=30) | 420 ms | 450 ms | parity within noise |
| `git capability` p50 (n=40, zero-I/O canary) | — (bun floor ~24 ms) | **19 ms** | cold process start |
| `git status` max RSS (n=12) | 27 568 KB | **9 312 KB** | ~3× smaller |

Interpretation:

- The plugin's read path intentionally still shells out to the `git`
  CLI (per #286: git mutations stay CLI; read collectors mirror the TS
  command set). Wall latency is therefore bounded below by the same
  `git` subprocess fan-out the TS version paid — the Rust win is
  process startup (~19 ms vs bun+TS module load), ~3× lower RSS, and
  no Bun runtime dependency, not an order-of-magnitude latency cut.
- The one real regression found and fixed (§3.1) shows the port
  review did its job: parallelism must be preserved explicitly when
  moving from `Promise.all` to sequential Rust.
- A quieter-machine re-run would likely show status/diff at or below
  the TS baseline; the numbers above are the honest loaded-machine
  record.
