# Research: herdr official install path for Rust-native plugin binaries (Issue #288)

Spike evidence for the native-distribution decision feeding #291
(Program #286). Question: can a compiled Rust binary be distributed and
updated through herdr's **official** install path, with no Bun/Node/
compiler required of the end user?

Verification host: macOS arm64 only. Linux and Windows are **unverified**
and are marked as such throughout; nothing below should be read as
cross-platform proof.

## 1. Pinned revisions

| artifact | pin | rationale |
| --- | --- | --- |
| installed herdr | `0.9.5` (`/Users/rebui/.local/share/mise/installs/herdr/0.9.5/herdr`, via mise shim `~/.local/bin/herdr`) | the actually-adopted version on the dev machine; `min_herdr_version = "0.9.0"` across all 14 manifests |
| upstream release | tag `v0.9.5` = commit `2ecf19a92fc3a7fff75b6ce3655ef04f4625ec01` (`git ls-remote https://github.com/herdrdev/herdr`) | matches installed version; stable docs index (`herdr.dev/llms.txt`) declares "Current stable release: 0.9.5" |
| official docs | `docs/next/website/src/content/docs/plugins.mdx` @ `v0.9.5` (raw.githubusercontent.com) | the plugin authoring/install contract |
| prior verified inventory | `docs/adr/0031-tui-surface-architecture.md` (verified against herdr source `e8d7d970`, post-v0.9.5 master) | manifest schema details already measured |

Upstream `master` HEAD at spike time: `e8d7d9705a7a2c89cef4dd42079379f05685af87`.
No newer stable release exists; 0.9.5 is both installed and latest stable.

## 2. Official install model (docs + live verification)

Source: plugins.mdx @ v0.9.5 plus live runs against herdr 0.9.5.

- `herdr plugin install <OWNER/REPO[/SUBDIR]> [--ref <REF>] [-y]` —
  **GitHub shorthand only**. No release-asset, URL, or local-path install.
  It clones the repo, shows a preview (id, ref→commit, entrypoint counts),
  runs `[[build]]` argv commands, then registers the plugin.
- `command` fields are **argv arrays executed without a shell**; cwd is
  the plugin root. `HERDR_PLUGIN_ROOT` is injected as env but argv cannot
  interpolate env vars, so portable manifests use relative paths like
  `./bin/<name>` (cwd = plugin root) — verified working.
- `[[build]]` commands run at install time in the plugin directory, get
  **no** runtime env/socket, and a failure aborts registration. `plugin
  link` never runs build commands.
- Registry: `plugins.json` under the herdr config dir records
  `source{kind:"github", owner, repo, subdir, requested_ref,
  resolved_commit, managed_path}`. `requested_ref` may be a branch, tag,
  or SHA; `resolved_commit` is recorded at install time.
- Managed installs live under
  `plugins/github-installations/<plugin-id>-<hash>/<install-id>/checkout`
  and contain a **full clone of the entire repository** (the plugin
  subdir is the plugin root; sibling dirs remain reachable). Each install
  generation gets its own directory.
- `plugin update [id...]` refetches `requested_ref`, skips unchanged
  commits, ignores linked plugins in bulk mode. New invocations use the
  new installation; **running processes keep their old files** — old
  generations are retained until the servers using them exit.
- `plugin disable` makes invocations fail with `plugin_disabled`;
  `plugin enable` restores. `plugin uninstall` unregisters and retains
  files; config/state dirs are preserved.
- Startup hooks run once per enabled plugin after session restore/API
  socket ready (and again on live handoff), never on client attach,
  config reload, or link/enable. Event hooks fire per `[[events]]`.
  `plugin.pane.open` launches manifest `[[panes]]` entrypoints.
- Plugins are global to the user across sessions, but **all plugin CLI
  commands resolve a session**: mutating/query commands without
  `--session`/`HERDR_SOCKET_PATH` hit the *default* session's running
  server. `plugin install`/`link` write the registry file directly and
  work with no server; `plugin update`/`list`/`action invoke`/`pane open`
  go through the resolved session's server.
- Isolation for testing: `XDG_CONFIG_HOME` + `XDG_STATE_HOME` +
  `HERDR_CONFIG_PATH` + `herdr --session <name> server` (headless) gives a
  fully isolated registry, config, state, and socket
  (`$XDG_CONFIG_HOME/herdr/sessions/<name>/herdr.sock`). Note
  `HERDR_SESSION` alone does **not** retarget CLI commands — pass
  `--session`.

## 3. Prototype

`spikes/np288-dist/` — dependency-free Rust binary (`cargo`, std only),
committed **prebuilt** Mach-O arm64 binary at `bin/np288-dist`
(302,912 bytes, stripped, `opt-level="z"`, LTO). No `[[build]]` commands:
install requires no toolchain. Manifest `herdr-plugin.toml` declares one
action (`ping`), one startup hook, one `pane.exited` event hook (v0.1.0),
plus a `[[panes]]` overlay entrypoint (added in v0.1.1). Top-level
`platforms = ["macos"]` — honest scope; other OSes unverified.

## 4. Live evidence (all in isolated session `np288-spike`, env
`XDG_CONFIG_HOME=/tmp/np288/xdg XDG_STATE_HOME=/tmp/np288/state
HERDR_CONFIG_PATH=/tmp/np288/config.toml`)

| step | command | result |
| --- | --- | --- |
| install v0.1.0 | `herdr plugin install rebuildup/hoxay-plugins/spikes/np288-dist --ref 8922935… --yes` | preview → `Installed`; registry entry `resolved_commit=8922935`, managed path `…/76079-0/checkout` |
| server start | `herdr --session np288-spike server` (headless) | startup hook ran: `plugin log list` shows `plugin-log-1`, `command=["./bin/np288-dist","startup"]`, `status=succeeded`, `exit_code=0`; stdout includes full injected env (`HERDR_PLUGIN_ID/ROOT/CONFIG_DIR/STATE_DIR/EVENT`, `HERDR_SOCKET_PATH=/tmp/np288/…/herdr.sock`, `HERDR_BIN_PATH=…/herdr/0.9.5/herdr`); wrote `startup.marker` under `HERDR_PLUGIN_STATE_DIR` |
| action invoke | `herdr --session np288-spike plugin action invoke rebuildup.np288-dist.ping` | `plugin_action_invoked`, `plugin-log-2` succeeded exit 0, v0.1.0 output |
| event hook | `workspace create` → `pane send-text w2:p1 "exit"` + Enter | shell exit → `pane_exited`; `plugin-log-3` `pane.exited` hook succeeded; appended `0.1.0\t{"event":"pane_exited","data":{…}}` to `hooks.log` in state dir |
| reinstall v0.1.1 | `plugin install … --ref 288 --yes` (branch) | preview `replaces: …@8922935`; new generation `23268-0`; **both `76079-0` and `23268-0` checkouts coexist**; `requested_ref="288"`, `resolved_commit=4fe67b2` |
| pane entrypoint | `plugin pane open --plugin rebuildup.np288-dist --entrypoint main` | `plugin_pane_opened`, pane `w1:p2` cwd = new checkout `…/23268-0/checkout/spikes/np288-dist`; process printed v0.1.1 and exited → second `pane_exited` hook logged `0.1.1` |
| disable | `plugin disable rebuildup.np288-dist` | `enabled:false`; `action invoke` → `{"error":{"code":"plugin_disabled"}}` |
| enable | `plugin enable rebuildup.np288-dist` | `enabled:true` restored |
| offline link | `plugin link /tmp/np288-link` (local copy, renamed id) + `action invoke` + `plugin unlink` | link registers with `source.kind:"local"`, no clone/build; invoke succeeded (plugin-log-5, v0.1.1); unlink `removed:true`. Fully offline-capable dev path |
| update (moved branch) | docs commit pushed → `plugin update rebuildup.np288-dist --yes` | refetched `requested_ref="288"` → `resolved_commit=72b046b`; new generation `80355-0`; **three generations coexisted** (`76079-0`, `23268-0`, `80355-0`); running server's next `action invoke` (plugin-log-6) resolved `HERDR_PLUGIN_ROOT=…/80355-0/…` — live-update-while-running confirmed |
| uninstall | `plugin uninstall rebuildup.np288-dist` | registry emptied (`plugins.json` → `[]`); subsequent invoke → `plugin_action_not_found`; config/state dirs preserved per docs; install files retained for running processes |
| teardown | `herdr --session np288-spike server stop` + `rm -rf /tmp/np288*` | isolated session/socket/state fully removed; user sessions untouched |

## 5. Distribution candidates

| candidate | verdict | reason |
| --- | --- | --- |
| **A. Individual prebuilt binary per plugin, committed to the repo** | **selected** | Works today with zero manifest changes beyond argv paths; no toolchain at install; per-plugin independence preserved; git SHA pinning gives content integrity; spike-verified end to end. Cost: repo carries binaries per OS/arch (size, review noise). |
| **B. Multicall binary** (one `hoxay` binary dispatched by first argv / plugin id) | viable fallback, not selected for #291 | Same install mechanics as A (each plugin dir gets its own immutable copy — no mutable global sharing). Wins on single-build/single-artifact maintenance when ≥2 plugins share domain code; loses on per-plugin independence (every plugin update republishes all code) and forces a cross-plugin command convention. Keep as Wave-2 consolidation option once shared Rust crates exist. |
| **C. Feature-split binaries** (separate cli/tui/bin per feature) | excluded | Multiples artifact count per plugin for no install-path benefit; manifest already selects entrypoints by argv. Revisit only if a plugin's surfaces have genuinely different dependency/size profiles. |
| **D. Hash-pinned shared release artifacts** (GitHub Releases asset fetched by `[[build]]` script) | excluded for v1 | Official install has no asset-fetch primitive; would require a `[[build]]` curl/wget step → adds a mutable network dependency at install, breaks offline `plugin link` parity, and duplicates what git SHA pinning already provides. Reconsider only if repo-binary size becomes a measured problem. |
| mutable global/shared binary path (e.g. `~/.local/bin/hoxay`) | **prohibited** | violates the program invariant (no mutable global binary sharing); also breaks per-plugin version coexistence. |

## 6. Evaluation matrix

| axis | finding |
| --- | --- |
| signing / checksum | herdr performs **no** signature or checksum verification; integrity = git object identity (`resolved_commit`). A SHA-pinned `--ref` is immutable; a branch/tag ref is mutable until resolved. Supply-chain trust = GitHub repo + TLS. Optional hardening (commit signing, release-attestation) is repo-side policy, not enforced by herdr. macOS: binaries arrive via `git clone` (no quarantine xattr); Gatekeeper assessment of unsigned Mach-O run from terminal is untested here — **unverified**. |
| OS / arch selection | manifest `platforms` is OS-granular only (`linux`/`macos`/`windows`); **no arch field**. Per-arch binaries need either (a) a committed launcher script that dispatches on `uname`, or (b) separate plugin dirs/manifests per arch. Item-level `platforms` cannot duplicate an action/pane id, so one manifest cannot declare per-arch argv directly — **gap recorded**; launcher-script pattern recommended, POSIX-verified pattern only, Windows unverified. |
| supply chain | install = `git clone` of public GitHub repo at recorded ref + optional `[[build]]` argv (no shell). Build commands get no secrets by default but inherit user env — same trust model as today. No dependency fetching beyond what build commands do themselves. |
| offline install | `plugin install` requires GitHub (online). `plugin link <local-dir>` works fully offline, runs no build commands, and was verified live — the dev/fallback path. A prebuilt-binary plugin linked from a local checkout needs no toolchain. |
| distribution size | **each managed install clones the entire repo**: measured 11 MB checkout (3.2 MB `.git`) per plugin generation for this repo today, ×2 generations coexisting during update. Binary itself ≈ 0.3 MB. Repo-binary growth is linear in plugins×targets; the monorepo clone overhead dominates and is a herdr-model property, not fixable per-plugin. |
| live update while running | verified structurally: update creates a new generation dir; running commands keep old files; new invocations resolve the new `plugin_root`. Old generations GC'd after servers exit. No restart of running plugin processes. |
| rollback | `plugin install --ref <previous-sha>` (or `plugin update` after resetting the ref) reinstalls the prior generation; retained old dirs make rollback immediate. Config/state preserved across install/update/uninstall. |

## 7. Selected option

**Per-plugin prebuilt binaries committed to the repo, dispatched through
a small committed launcher for multi-arch (A + launcher)**, i.e. the
shape the spike already uses for single-arch. For #291 (Inbox
vertical slice, macOS arm64 dev host): ship `bin/` artifacts per
target under the plugin dir and select via launcher; until Linux/Windows
binaries exist, declare honest `platforms`.

Confidence: **high for macOS arm64** (fully exercised live);
**medium for the mechanism generally** (same argv/install path is
OS-agnostic in docs, but Windows argv/PATHEXT and Linux execution are
unverified); arch-selection launcher is designed but only
single-arch-verified.

Remaining verification before Wave-1 reliance:

1. ~~`plugin update` pulling a moved branch ref~~ — **done**: branch
   `288` advanced `4fe67b2 → 72b046b`, update produced generation
   `80355-0` and the live server's next invocation used it.
2. Linux x86_64/arm64 and Windows builds: compile, manifest
   `platforms`, launcher dispatch, `PATHEXT`/`.cmd` handling — all
   unverified.
3. Launcher-script dispatch vs. per-platform manifest duplication:
   confirm whether herdr accepts same-id entries differing only by
   `platforms` (likely rejected — "ids must be unique"); else launcher.
4. Signing/notarization decision for released binaries (macOS
   Gatekeeper behavior for git-cloned unsigned binaries on a clean
   machine).
5. Repo-size budget: measure binary×target growth vs. the 11 MB
   per-install clone overhead; decide binary-in-git vs. `[[build]]`
   fetch later if it becomes material.
6. `min_herdr_version` floor for any manifest fields Wave-1 needs
   (currently 0.9.0 everywhere; spike used 0.9.0).

## 8. Path to normal use without Bun/compiler

User-facing flow after Wave-1: `herdr plugin install
rebuildup/hoxay-plugins/plugins/inbox` → clone + register, **no build
commands** → `./bin/<target-binary>` argv runs the Rust binary directly.
No Bun, no Node, no cargo on the user machine. Developer flow unchanged:
`cargo build --release` in the plugin dir + `plugin link` for local
testing (verified). Release engineering (cross-compile matrix, checksums,
optional signing) is CI work for #296+, not an install-path blocker.
