# rebuildup.git

Git status/diff for herdr panes (`herdr-plugins.git/v1`).

## Install

```bash
herdr plugin install rebuildup/hoxay-plugins/plugins/git
```

Prerequisites: herdr ≥ 0.9.0, `git` on `PATH`. The plugin is a
self-contained native binary (`bin/git`, macOS arm64); no Bun required.

## Usage

```bash
# from the plugin root (see `herdr plugin list --json` → plugin_root)
./bin/git capability
./bin/git status <pane_id>
./bin/git files <pane_id> [--path PATH]...
./bin/git diff <pane_id> [--staged] [--stat|--name-status|--numstat]
    [--path PATH]... [--max-bytes N] [rev [rev]]
./bin/git log <pane_id> [--limit N] [--skip N] [--all]
    [--first-parent] [--author X] [--grep X] [--since X] [--until X]
    [--path PATH]...
./bin/git compare <pane_id> [base [head]] [--max-bytes N]
./bin/git stage <pane_id> [--all | --path PATH]...
./bin/git unstage <pane_id> [--all | --path PATH]...
./bin/git commit <pane_id> -m MSG [-m MSG]... [--allow-empty]
./bin/git branch create <pane_id> <name> [--switch]
./bin/git branch switch <pane_id> <name>
./bin/git upstream <pane_id> <upstream-ref>
./bin/git fetch <pane_id> [remote] [--prune]
./bin/git push <pane_id> [--set-upstream] [--force-with-lease]
    [remote] [refspec]
./bin/git remote <pane_id>
./bin/git pr list|status|create|view|reviews|comments|checks|logs
    <pane_id> [--repo owner/name]
    pr create: --title T [--body B | --body-file PATH|-] [--draft]
        [--base B] [--head B]
    pr reviews|comments|checks <pane> <number>
    pr logs <pane> <number> [--run ID | --job ID [--lines N]]
./bin/git pr review|comment|reply|reviewers|merge <pane_id> [args]
    [--repo owner/name] --expect-head <full-sha>   (required)
    pr review <pane> <number> --approve|--request-changes|--comment
        [--body B | --body-file PATH|-]
        [--comments-file PATH|-]
    pr comment <pane> <number> --body B|--body-file PATH|-
    pr reply <pane> <number> <comment-id> --body B|--body-file PATH|-
    pr reviewers <pane> <number>
        [--add-reviewer LOGIN[,LOGIN]...] [--remove-reviewer LOGIN...]
    pr merge <pane> <number> [--squash|--merge|--rebase]
        [--auto | --disable-auto]
./bin/git issue list|view|create|edit|comment|close
    <pane_id> [args] [--repo owner/name]
    issue list: [--limit N] [--state open|closed|all]
        [--label L]... [--assignee U]... [--author U]
    issue view <pane> <number>
    issue edit <pane> <number> [--title T] [--body B | --body-file P|-]
        [--add-label L]... [--remove-label L]... [--add-assignee U]...
        [--remove-assignee U]... [--state open|closed]
    issue comment <pane> <number> --body B|--body-file PATH|-
    issue close <pane> <number> [--reason completed|"not planned"]
./bin/git issue link|unlink|links <pane_id> [args]
    issue link <pane> <owner>/<repo>#<n> [--verify]
    issue unlink <pane> <owner>/<repo>#<n>
./bin/git project <pane_id>
```

`status` returns repo identity, branch/upstream/ahead-behind, and
file-level change counts. `files` returns the structured per-file change
list (staged/unstaged/untracked/conflicted, renames, binary flags).
`diff` returns a byte-capped patch or machine-readable `--stat`,
`--name-status`, or `--numstat` output (`truncated` flag when cut),
optionally limited by `--path` pathspecs and revisions. `log` returns
structured commit history (sha/parents/author/committer/subject) with
`--limit`/`--skip` paging and `has_more`. `compare` diffs a revision range
against its merge-base — defaulting to the branch's upstream — returning
changed files, stat, ahead/behind, and a bounded patch. Mutation
commands (`stage`, `unstage`, `commit`, `branch`, `upstream`, `fetch`,
`push`) apply allow-listed local git operations; `remote` lists
remotes with credentials stripped from URLs; `pr` exposes GitHub pull
requests through the host's `gh` CLI (requires `gh` ≥ 2.81.0 and
`gh auth login`; see ADR-0007/0008) — `list`/`status`/`create`/`view`
plus read-only `reviews` (submitted reviews + review requests),
`comments` (issue + line comments), `checks` (per-check CI detail), and
`logs` (workflow runs / job list / bounded job-log tail). `pr review`
submits APPROVE/REQUEST_CHANGES/COMMENT reviews — with optional line
comments via `--comments-file` — `pr comment` posts conversation
comments, `pr reply` answers review threads, and `pr reviewers`
requests/dismisses reviewers (`gh pr edit`). `pr merge` performs an
immediate merge (`--squash|--merge|--rebase`), arms GitHub auto-merge
(`--auto`, method flag required), or disarms it (`--disable-auto`).
Every forge mutation requires `--expect-head <full sha>` (the
`head.sha`/`headRefOid` from a prior read); the plugin re-fetches the
PR head and refuses with `stale_head` when it moved. `pr merge`
additionally re-verifies `mergeStateStatus`/`reviewDecision`/`isDraft`
immediately before writing — non-mergeable states fail with
`merge_blocked`, and PRs whose head branch is a release train
(`release-x-y-z`) are refused with `release_boundary` so release
approval stays a maintainer decision (ADR-0030).
`issue` exposes GitHub issues through the same `gh` path —
`list`/`view`/`create`/`edit`/`comment`/`close` with the same repo
resolution, bounded reads, and never-in-argv body discipline. Issues
have no head sha, so there is no `--expect-head` analog: forge
mutations are gated by the mutation target-resolution rule and always
echo the re-fetched post-mutation state. `issue link`/`unlink`/`links`
delegate to `rebuildup.inbox` (ADR-0013 sibling-plugin resolution,
`inboxPluginRoot` config override) — the git plugin keeps no link state;
staleness follows inbox-v1 rules verbatim.
`project` additionally reports `git_branch`, `git_changes`, and
`git_ahead_behind` pane metadata tokens.

Full contract: `docs/contracts/git-v1.md`. Spec: `docs/specs/git.md`.
Design: ADR-0006/0007/0008/0030.

## Interactive TUI

`rebuildup.git` also ships interactive terminal surfaces per
`docs/contracts/tui-v1.md` (issues #238/#239, ADR-0031/0032/0033):
manifest pane entrypoints `main` (repository status, `placement =
"tab"`) and `forge` (GitHub PR/issue browser, `placement = "tab"`),
plus the `open` / `open-forge` actions, which open each UI for the
focused pane's repository via `plugin.pane.open`.

The `main` UI covers status, changed files with per-file
stage/unstage/diff, a paged diff viewer, commit history + per-commit
show, branch comparison, branch operations, fetch/push, and remote
listing. The target repo/HEAD is pinned at launch and revalidated
before every mutation; commit, push, bulk stage/unstage, and branch
switches require in-TUI confirmation.

The `forge` UI covers PR list (state filter), PR-for-branch status,
PR detail with comments (incl. review-thread reply), reviews, checks,
and workflow run/job/log browsers, issue list (state + label filters),
and issue detail. Every mutation is confirm-gated (exact target +
consequence, Esc cancels), checked against `capability`/
`mutation_commands` (fail closed when unavailable), and re-validated
immediately before writing: PR mutations re-read `pr reviews`+`pr
view` and bind the fresh `head.sha` via `--expect-head` (a moved head
or `updatedAt` asks before applying), preserving the CLI's
`stale_head` / `merge_blocked` / `release_boundary` guards; issue
mutations re-read `issue view` and ask on `updated_at` drift (issues
have no head sha). `p`/`o` repoint the forge target pane /
`--repo owner/name` override in-session.

Both UIs consume this plugin's own CLI (`bin/git`) for every read
and mutation, so the public contract stays the single source of truth.
Opening performs reads only. Non-TTY stdin degrades to plain-text
lists; `gh` missing renders an honest unsupported state, never fake
data.

Opt-in keybinds (user-owned; plugins never write herdr config):

```toml
[[keys.command]]
key = "prefix+g"
type = "plugin_action"
command = "rebuildup.git.open"
description = "open git TUI"

[[keys.command]]
key = "prefix+G"
type = "plugin_action"
command = "rebuildup.git.open-forge"
description = "open git forge TUI"
```

For a repo checkout without herdr: `./bin/git tui --cwd <path>`
(repo surface) or `./bin/git tui forge --cwd <path>` (forge surface).
Non-interactive use is unchanged — `bin/git` remains the CLI/MCP/GUI
contract surface.

## Notes

- v0.2.0 adds local mutations (stage/commit/branch/upstream), remote
  operations (fetch/push/remote list), and the GitHub PR foundation
  (`pr` via `gh`). v0.3.0 adds read-only PR review/check/log access
  (`pr reviews|comments|checks|logs`). v0.4.0 adds forge review/comment
  mutations (`pr review|comment|reply|reviewers`) behind the mandatory
  `--expect-head` stale-head guard — explicit invocations only; the
  plugin never approves its own release PR or merges release branches.
  v0.5.0 adds `pr merge`
  (immediate merge / auto-merge enable / disable) with merge-state
  re-verification and a hard refusal on release-train head branches —
  release approval stays a maintainer decision. Merge-queue
  manipulation, GitHub stack merges, and non-GitHub forges are later
  tickets. No destructive git operations are exposed (ADR-0007);
  force push requires the explicit `--force-with-lease` flag.
  v0.6.0 adds the GitHub issue surface (`issue list|view|create|edit|
  comment|close` via `gh`) and pane↔issue links (`issue link|unlink|
  links`) delegated to `rebuildup.inbox` — inbox owns the link store
  and its staleness/retention rules; this plugin stores no link state.
  v0.7.0 adds the forge TUI surface (`[[panes]] id="forge"` +
  `open-forge` action): PR/issue list, detail, review/comment/merge,
  and issue edit/close flows per `tui-v1.md`, all routed through the
  plugin CLI with confirm gating and freshness revalidation.
- Remote herdr hosts: run the CLI on the target host (same rule as
  `rebuildup.inbox`).
- Optional `config.json` in the plugin config dir:
  `{"timeoutMs": 10000, "remoteTimeoutMs": 60000, "diffMaxBytes": 262144,
  "inboxPluginRoot": "/path/to/plugins/inbox"}` — `inboxPluginRoot`
  overrides sibling-plugin resolution for `issue link|unlink|links`
  (default: `../inbox`, then `herdr plugin list --json`).
