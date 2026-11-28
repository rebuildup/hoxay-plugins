# ADR-0035: Native plugin distribution — per-plugin prebuilt binaries through the official GitHub install path

## Status

Accepted (spike-backed distribution decision for #288, input to #291;
platform coverage limited to verified macOS arm64 — see Consequences)

## Context

Program #286 (Native Performance) requires the end state where normal
plugin use needs **no Bun/Node/compiler** on the user machine, while
preserving herdr's official per-plugin install/update/disable/remove,
version coexistence, and live-update-while-running semantics. Issue #288
spiked herdr's actual install path; full evidence lives in
`docs/research/np288-native-distribution-spike.md`.

Pinned basis: herdr `0.9.5` = upstream tag `v0.9.5`
(`2ecf19a92fc3a7fff75b6ce3655ef04f4625ec01`), docs
`plugins.mdx` @ v0.9.5, verified live on macOS arm64 in an isolated
named session (`np288-spike`).

Key verified facts:

- `herdr plugin install owner/repo[/subdir] [--ref]` clones the repo,
  runs optional `[[build]]` argv commands, and registers the plugin;
  manifest `command` fields are shell-free argv arrays run with cwd =
  plugin root, so `./bin/<binary>` works directly.
- Each managed install is a **full repo clone** under
  `plugins/github-installations/<id>-<hash>/<gen>/checkout`; update
  creates a new generation and old generations persist until servers
  exit — coexistence and rollback are herdr-native.
- `plugin link <dir>` is a fully offline, build-free path (verified).
- The manifest has **no arch field and no per-arch argv selection**;
  `platforms` is OS-granular only.
- herdr performs no signature/checksum verification; integrity is the
  recorded git `resolved_commit`.

## Decision

1. **Distribution unit: one plugin = one directory = one or more
   prebuilt binaries committed under that plugin's directory** (e.g.
   `plugins/inbox/bin/`). Manifest entrypoints reference them via
   relative argv (`["./bin/inbox", ...]` or a launcher). No `[[build]]`
   step for normal installs → no toolchain required of users.
2. **Multi-arch selection via a committed launcher script** dispatched
   on `uname`/`uname -m` (POSIX) — required because the manifest cannot
   express per-arch argv. Windows handling (`.cmd`/PATHEXT) is designed
   but **unverified**; Linux binaries likewise unverified at ADR time.
3. **Multicall single binary is deferred, not adopted for #291**: it is
   mechanically identical per install (each plugin dir holds its own
   immutable copy) but couples all plugins to one artifact and one
   release. Revisit in Wave 2 if shared Rust crates make a single
   `hoxay` binary clearly cheaper.
4. **Rejected**: feature-split binaries (no install-path benefit),
   hash-pinned release-asset fetch via `[[build]]` (adds a mutable
   network dependency at install; git SHA pinning already provides
   integrity), and any mutable global/shared binary path (invariant
   violation, breaks version coexistence).
5. **Integrity policy**: installs pin `resolved_commit`; releases SHOULD
   prefer SHA/tag `--ref` over moving branches for users, and repo-side
   signing/attestation may be added later — herdr enforces neither.
6. Spike artifacts (`spikes/np288-dist/`) are evidence only and MUST NOT
   ship as product plugins; they may be removed once Wave-1 binaries
   supersede them.

## Consequences

- #291 (Inbox vertical slice) can proceed on this shape: Rust binary +
  manifest argv + optional launcher, installed by the unchanged official
  `herdr plugin install` flow.
- Repo size grows by binaries×targets; the dominant per-install cost is
  herdr's full-repo clone (~11 MB measured), unaffected by this choice.
- Honest `platforms` declarations until each OS/arch binary exists and
  is verified; unverified platforms are never claimed.
- Remaining verification owned by later tickets: Linux/Windows builds
  and launcher dispatch, same-id-per-platform manifest question,
  signing/notarization policy, binary-size budget vs. clone overhead.
- `min_herdr_version = "0.9.0"` is sufficient for everything the spike
  exercised (actions, startup, events, panes, link).
