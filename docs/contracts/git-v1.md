# Contract `herdr-plugins.git/v1`

Structured JSON CLI contract for the `rebuildup.git` herdr plugin.
Read commands (`status`, `files`, `diff`, `log`, `compare`, `remote`,
`pr list`, `pr status`, `pr view`, `pr reviews`, `pr comments`,
`pr checks`, `pr logs`, `issue list`, `issue view`, `issue links`,
`project`) are side-effect free;
mutation commands (`stage`, `unstage`, `commit`, `branch`, `upstream`,
`fetch`, `push`, `pr create`, `pr review`, `pr comment`, `pr reply`,
`pr reviewers`, `pr merge`, `issue create`, `issue edit`,
`issue comment`, `issue close`, `issue link`, `issue unlink`) change
the pane repository's index/refs/remotes, the forge on the herdr
host, or (delegated) inbox plugin state. Follows the same
conventions as `herdr-plugins.inbox/v1` (`docs/contracts/inbox-v1.md`).

## Discovery

1. `herdr plugin list --json` → find `plugin_id == "rebuildup.git"` →
   `plugin_root`.
2. `cd <plugin_root> && ./bin/git capability` → verify
   `contract == "herdr-plugins.git/v1"` and the commands you need exist.
3. Run `status` / `diff` / `project` with the target pane.

## Invocation

```bash
cd "$(herdr plugin list --json | jq -r '.result.plugins[] | select(.plugin_id=="rebuildup.git") | .plugin_root')"
./bin/git status <pane_id> [--session NAME] [--socket PATH] [--timeout MS]
./bin/git files <pane_id> [--path PATH]... [--session NAME]
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
    <pane_id> [args] [--repo owner/name]
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
./bin/git issue edit <pane_id> <number> [args]
    [--repo owner/name] [--title T] [--body B | --body-file PATH|-]
    [--add-label L]... [--remove-label L]...
    [--add-assignee U]... [--remove-assignee U]... [--state open|closed]
./bin/git issue comment <pane_id> <number>
    --body B|--body-file PATH|- [--repo owner/name]
./bin/git issue close <pane_id> <number>
    [--reason completed|"not planned"] [--repo owner/name]
./bin/git issue link <pane_id> <owner>/<repo>#<n> [--verify]
./bin/git issue unlink <pane_id> <owner>/<repo>#<n>
./bin/git issue links <pane_id>
./bin/git project <pane_id> [--session NAME]
./bin/git capability
```

- Pane id may be omitted when `HERDR_PANE_ID` or the plugin action context
  supplies it.
- The plugin ships a committed native binary at `bin/git` (no Bun or
  other runtime prerequisite). Requires `git` on `PATH`; `pr`/`issue`
  additionally require `gh` ≥ 2.81.0 with authentication on the herdr
  host.
- All commands print exactly one JSON document on stdout. Handled errors
  print `{contract, ok:false, error:{code,message}}` on **stderr**.
- **Exit codes:** `0` success; `1` handled runtime/environment failure
  (`error.code`/`kind` carries `git_error`, `git_missing`, `timeout`,
  `not_a_repo`, `bare_repo`, `pane_not_found`, `no_pane_cwd`,
  `gh_missing`, `gh_error`, `stale_head`, `inbox_unavailable`,
  `inbox_error`, `target_conflict`, `target_required`,
  `ambient_identity_mismatch`, or `error` for transport/unexpected
  failures); `2` usage error (`error.code == "usage"` or
  `invalid_session`).
- **Strict option parsing:** every `-`-prefixed token must be a flag the
  command documents — unknown options are usage errors, never silently
  ignored. `pr`/`issue` flags are whitelisted per action (`--state` on
  `pr create`, `--title` on `pr list`/`issue list`, etc. are usage
  errors). Boolean
  flags reject `=value`; value flags accept both `--name value` and
  `--name=value` (the `=` form is required for values that themselves
  start with `-`, e.g. `--grep=--foo`). A `--` separator ends option
  parsing; tokens after it are positional. Extra positionals beyond the
  documented arity are usage errors. Global flags (`--session`,
  `--socket`, `--timeout`) accept the same `=` form and are validated
  (`--timeout` must be a positive number).

## Target selection

Same rules as inbox (see `docs/contracts/inbox-v1.md` → Target
selection): explicit `--socket`/`--session` beat ambient
`HERDR_SOCKET_PATH`/`HERDR_SESSION`; contradictions fail closed with
`target_conflict`. Mutating commands (`stage`, `unstage`, `commit`,
`branch create|switch`, `upstream`, `fetch`, `push`, `pr create`,
`issue create|edit|comment|close|link|unlink`, `project`) require an
explicit or ambient target — the implicit default socket is rejected
with `target_required`. Ambient pane ids
(`HERDR_PANE_ID`/plugin context) are verified against the resolved
session's snapshot (`ambient_identity_mismatch` on workspace/tab
disagreement). Remote hosts follow the inbox rule: run the CLI **on the
herdr host**; socket forwarding is not a supported path.

## Git/gh child environment

Every `git` and `gh` child process runs with a sanitized environment
(`plugins/git/src/gitenv.rs`): ambient `GIT_*` variables that could
retarget the repository, index, object store, discovery, or config
(`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_CONFIG_COUNT`/
`GIT_CONFIG_KEY_*`/`GIT_CONFIG_VALUE_*`/`GIT_CONFIG_PARAMETERS`,
`GIT_EXEC_PATH`, `GIT_EXTERNAL_DIFF`, pathspec-magic vars, etc.) are
stripped so the pane cwd's repository is always the target — issue #96.
`gh` additionally strips `GH_REPO`, `GH_DEBUG`, `GH_FORCE_TTY`, and
`GH_PAGER`. Preserved intentionally: user-level config selection
(`GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`/`GIT_CONFIG_NOSYSTEM`), commit
identity (`GIT_AUTHOR_*`/`GIT_COMMITTER_*`), auth/transport
(`GIT_SSH*`/`GIT_ASKPASS`/`GIT_SSL_*`/proxy vars), and all `GH_*` auth
(`GH_TOKEN`, `GH_HOST`, `GH_ENTERPRISE_TOKEN`, …). The plugin's own pins
(`GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0`, `LC_ALL=C`,
`GH_PROMPT_DISABLED=1`) are applied after stripping.

## `status`

```json
{
  "contract": "herdr-plugins.git/v1",
  "command": "status",
  "pane_id": "w1:p1",
  "generated_at": "2026-11-25T00:00:00.000Z",
  "repo": {
    "root": "/path/to/worktree",
    "git_dir": "/path/to/.git",
    "git_common_dir": "/path/to/repo/.git",
    "bare": false,
    "inside_work_tree": true
  },
  "branch": {
    "name": "feature/x",
    "detached": false,
    "upstream": "origin/feature/x",
    "ahead": 2,
    "behind": 0
  },
  "changes": { "staged": 1, "unstaged": 2, "untracked": 3, "conflicted": 0 },
  "warnings": []
}
```

Fields:

- `repo.root` — `git rev-parse --show-toplevel` of the pane cwd.
- `repo.git_dir` / `git_common_dir` — distinguish linked worktrees.
- `branch.name` — `HEAD` when detached; `upstream`/`ahead`/`behind` are
  `null` when no upstream is configured.
- `changes` — file counts from `git status --porcelain=v1` (see spec).

Error codes (in `error.code` or a structured `error.kind`): `not_a_repo`
(pane cwd outside any work tree), `bare_repo`, `git_missing`,
`timeout`, `git_error` (non-zero git exit, message includes stderr),
`pane_not_found`, `usage`.

## `files`

```json
{
  "contract": "herdr-plugins.git/v1",
  "command": "files",
  "pane_id": "w1:p1",
  "repo_root": "/path/to/worktree",
  "pathspec": ["src/"],
  "files": [
    {
      "path": "src/new name.ts",
      "old_path": "src/old.ts",
      "index": "R",
      "worktree": " ",
      "kind": "renamed",
      "staged": true,
      "unstaged": false,
      "binary": false
    }
  ],
  "generated_at": "2026-11-25T00:00:00.000Z"
}
```

`files` lists every changed path in the pane's repository, parsed from
`git status --porcelain=v1 -z` (paths are verbatim, never C-quoted).
`kind` is one of `added`, `modified`, `deleted`, `renamed`, `copied`,
`typechange`, `untracked`, `conflicted`, `ignored`. `old_path` is set only
for renames/copies. `binary` is `true`/`false` for tracked changes
(derived from `git diff --numstat`) and `null` for
untracked/conflicted/ignored entries.

`--path <pathspec>` (repeatable) limits the listing via
`git status -- <path>...`. Option-like pathspec values (`-`-prefixed)
are rejected with a usage error.

## `diff`

Returns the patch (or a machine-readable summary) for the pane's
repository.

Options:

- `--staged` — diff index vs HEAD instead of worktree vs index.
- `--stat` / `--name-status` / `--numstat` — output format (exactly one;
  default is the unified patch). `--name-status` and `--numstat` use `-z`
  NUL-delimited output so unusual paths are unquoted.
- `--path PATH` — repeatable pathspec filter, passed to git after `--`.
  Option-like values are rejected.
- `--max-bytes N` — output cap (default 262144).
- positional revisions — `diff [--staged] [rev]` or `[revA revB]`; passed
  to `git diff` verbatim after `--` safety is not needed because they are
  argv elements, not shell text. Revisions are validated to reject
  option-like values (`-`-prefixed); use `--` pathspecs via `--path`
  instead of revision/path ambiguity.

Result fields:

```json
{
  "format": "patch",
  "output": "…patch text…",
  "stat_output": null,
  "name_status_output": null,
  "numstat_output": null,
  "paths": ["src/a.ts"],
  "truncated": false,
  "bytes": 1234
}
```

Exactly one of `output` (patch), `stat_output`, `name_status_output`, or
`numstat_output` carries the git output; the others are `null`.
`truncated: true` means the output was cut at the byte cap — clients must
not treat it as complete.

## `log`

```json
{
  "contract": "herdr-plugins.git/v1",
  "command": "log",
  "pane_id": "w1:p1",
  "repo_root": "/path/to/worktree",
  "limit": 50,
  "skip": 0,
  "commits": [
    {
      "sha": "abc123…",
      "parents": ["def456…"],
      "author": { "name": "…", "email": "…", "date": "2026-01-01T00:00:00+00:00" },
      "committer": { "name": "…", "email": "…", "date": "2026-01-01T00:00:01+00:00" },
      "subject": "commit subject"
    }
  ],
  "has_more": false,
  "generated_at": "…"
}
```

Options: `--limit N` (default 50, max 1000), `--skip N`, `--all`,
`--first-parent`, `--author`, `--grep`, `--since`, `--until`, and
`--path` pathspecs (repeatable). `has_more` is computed by fetching one
extra commit. There is no arbitrary revision-range positional in v1.

## `compare`

```json
{
  "contract": "herdr-plugins.git/v1",
  "command": "compare",
  "pane_id": "w1:p1",
  "repo_root": "/path/to/worktree",
  "base": "origin/main",
  "head": "HEAD",
  "merge_base": "abc123…",
  "ahead": 3,
  "behind": 1,
  "files": [ { "status": "M", "path": "src/a.ts", "old_path": null } ],
  "stat": " src/a.ts | 2 +-",
  "patch": "…bounded unified diff…",
  "patch_truncated": false,
  "patch_bytes": 1234,
  "generated_at": "…"
}
```

`compare [base [head]]` diffs `head` against the merge-base of `base` and
`head`. With no revisions, `base` is the current branch's upstream
(`@{upstream}`); a missing upstream is a `git_error`, never an empty
diff. `files` entries carry raw name-status letters (`M`, `A`, `D`,
`R100`, …); renames/copies populate `old_path` (source) and `path`
(destination). `ahead` counts head-only commits, `behind` base-only
commits. `patch` is byte-capped (`--max-bytes`, default 262144).

## Mutations: `stage` / `unstage` / `commit` / `branch` / `upstream` / `fetch` / `push`

These commands mutate the index, refs, or remote-tracking refs of the
pane's repository on the herdr host. Success payloads share the shape
`{contract, command, pane_id, repo_root, ok:true, …, generated_at}`:

- `stage`/`unstage` — `{all, paths, output}` (`output` is git's trimmed
  stdout, usually empty).
- `commit` — `{sha, summary}`; `sha` is `HEAD` after the commit.
- `branch` — `{action: "create"|"switch", name, output}`; `create
  --switch` maps to `git switch -c`.
- `upstream` — `{upstream, output}` (`git branch --set-upstream-to`).
- `fetch` — `{remote, prune, output}`; `remote` defaults to the
  configured default remote when omitted.
- `push` — `{remote, refspec, set_upstream, force_with_lease, output}`.
  Both positional args are optional; with none, `git push` uses the
  branch's configured upstream and fails structurally when none exists.
- `remote` (read-only) — `{remotes: [{name, url, pushUrl}]}`; URLs have
  any embedded userinfo credentials stripped before reporting.

Rules:

- `stage`/`unstage` require `--all` or ≥1 `--path`; bare invocation and
  the `--all` + `--path` combination are usage errors (exit 2).
- `commit` requires ≥1 non-empty `-m`; `--allow-empty` is explicit-only.
  Missing `-m` is a usage error (exit 2), not a git failure. Identity
  comes from git config, never fabricated.
- Branch/upstream/remote/refspec values and pathspecs are argv elements;
  empty or `-`-prefixed values are rejected (usage error, exit 2).
- Force push exists only as `--force-with-lease` (lease-checked); plain
  `--force` is never exposed. No other destructive operations (no
  `reset --hard`, `clean`, branch delete).
- `fetch`/`push` use `remoteTimeoutMs` (default 60000) instead of
  `timeoutMs` because they are network-bound; `GIT_TERMINAL_PROMPT=0`
  still prevents credential prompts.
- Git failures — including hook failures, "nothing to commit", missing
  upstream, non-fast-forward rejections — return `{ok:false,
  error:{code:"error",kind:"git_error",message}}` on stderr with exit 1;
  message includes git stderr.
- Mutations are not manifest actions; they are invoked via CLI or agents
  with explicit arguments.

For `branch`/`upstream`, the target value may be given as the sole
positional when a pane is resolvable from `HERDR_PANE_ID` or plugin
context; otherwise the first positional is the pane and the second is
the value (`branch create <pane> <name>`). `fetch`/`push` take the pane
as the first positional followed by at most one (`fetch`) or two
(`push`) extra positionals.

## Forge: `pr list` / `pr status` / `pr create` / `pr view`

Pull-request surface backed by the host's `gh` CLI (ADR-0008). All
subcommands share `pr <action> <pane_id>`; the pane resolves the repo
exactly like other commands, and the forge repo (`owner/name`) comes
from `gh repo view` inside the repo root — honoring the user's remotes,
`GH_HOST`, and `gh auth` configuration (github.com and GHES). An
explicit `--repo owner/name` overrides detection; malformed values are
rejected before any `gh` call.

- Every `pr` invocation first runs `gh --version` (minimum 2.81.0) and
  `gh auth status`; a missing binary, old version, or missing
  authentication is a structured error (`kind: "gh_missing"` or
  `"gh_error"`), never a fabricated empty result.
- All `gh` calls are argv spawn with `GH_PROMPT_DISABLED=1`, a hard
  timeout (`remoteTimeoutMs`, default 60s), and stdin closed
  immediately — gh can never block on an interactive prompt.
- `pr list` — `gh pr list --json <fixed fields>` with `--limit`
  (1–1000), `--state open|closed|merged|all` (default `open`), and
  `--head <branch>` filters. Result: `{action:"list", repo, prs[]}`.
- `pr status` — resolves the pane's current branch (`HEAD` detached →
  `{branch:"HEAD", detached:true, pr:null}`), finds its open PR, and
  returns the same bounded view as `pr view`. No open PR is
  `{pr:null}`, not an error.
- `pr view <number>` — bounded `gh pr view --json` projection: body is
  truncated at 8000 chars (`body_truncated`), reviews capped at the
  last 10 (author/state/submittedAt only), `statusCheckRollup` capped
  at 50 entries (`checks_truncated` when cut).
- `pr reviews <number>` — submitted reviews plus review requests via
  `gh api` (REST). Reviews carry `id`, `author`, `state`, `body`
  (≤4000 chars, per-item `body_truncated`), `submitted_at`,
  `commit_id`, `url`; the newest 50 are kept (`reviews_truncated`
  when capped or the fetched page was full). `review_requests` lists
  requested reviewer logins and team names. `head`/`base` echo
  `{ref, sha}` for freshness checks.
- `pr comments <number>` — PR conversation (`issues/{n}/comments`)
  and line-level review comments (`pulls/{n}/comments`) as separate
  bounded lists (newest 50 each; `issue_comments_truncated` /
  `line_comments_truncated`). Issue comments carry `id`, `author`,
  `body` (≤4000 chars), `created_at`, `updated_at`, `url`. Line
  comments additionally carry `path`, `line`, `original_line`,
  `side`, `in_reply_to_id`, and a bounded `diff_hunk` (≤8000 chars,
  `diff_hunk_truncated`).
- `pr checks <number>` — per-check detail via `gh pr checks --json`:
  `name`, `bucket` (pass/fail/pending/skipping/cancel), `state`,
  `workflow`, `link`, `description`, `event`, `started_at`,
  `completed_at`; capped at 100 (`checks_truncated`). Pending/failed
  checks are data, not errors; a PR with no reported checks returns
  `checks: []`.
- `pr logs <number>` — Actions log access, three modes:
  - default — workflow runs for the PR head sha via
    `gh run list --commit` (≤20, `runs_truncated`): `id`, `name`,
    `workflow`, `status`, `conclusion`, `url`, `head_sha`,
    `created_at`, `event`, `display_title`.
  - `--run ID` — that run's job list via `gh run view --json jobs`
    (≤50, `jobs_truncated`): `id`, `name`, `status`, `conclusion`,
    `url`, `started_at`, `completed_at`.
  - `--job ID [--lines N]` — the job's log tail via
    `gh run view --job --log` (default 200 lines, max 2000; hard cap
    256 KiB). Returns `log`, `lines_returned`, `total_lines`, `bytes`,
    and `truncated` when the tail was cut.
  All modes echo `head`/`base` `{ref, sha}` and `head_sha` so callers
  can verify freshness before any later mutation.
- `pr create` — forge mutation. Requires `--title`; body via
  `--body` (inline argv) or `--body-file PATH` / `--body-file -`
  (stdin) — long bodies never appear in argv. `--draft`, `--base`,
  `--head` map to the same gh flags. The branch must already be pushed;
  gh's own push/prompt flow is not driven by the plugin. Success returns
  `{action:"create", repo, ok:true, url, number}` parsed from gh's
  output.
- `pr review <number>` — submit a review (`--approve`,
  `--request-changes`, or `--comment`; exactly one required) via
  `POST repos/{repo}/pulls/{n}/reviews`. Optional review body via
  `--body`/`--body-file`; the whole JSON payload is piped to
  `gh api --input -`, so bodies never appear in gh argv. Optional
  `--comments-file PATH|-` supplies a JSON array of line comments
  (`{path, line, side?: "RIGHT"|"LEFT", body}`) delivered in the same
  review's `comments[]` — this is the only way to attach line comments,
  since `gh pr review` cannot express them. The POST pins `commit_id`
  to the verified head sha.
- `pr comment <number>` — PR-conversation comment via
  `POST issues/{n}/comments` (`gh pr comment` equivalent; issues and
  PRs share the endpoint). `--body` or `--body-file` required.
- `pr reply <number> <comment-id>` — reply inside a review thread via
  `POST pulls/{n}/comments/{id}/replies`. `--body`/`--body-file`
  required.
- `pr reviewers <number>` — request or dismiss reviewers via
  `gh pr edit --add-reviewer/--remove-reviewer`
  (`--add-reviewer LOGIN[,LOGIN]...`, `--remove-reviewer …`, at least
  one required; comma-separated or repeated). The payload echoes the
  post-mutation `review_requests` re-fetched from the PR.
- `pr merge <number>` — merge or arm/disarm GitHub auto-merge (Issue
  #173, ADR-0030). Three modes:
  - immediate merge — `PUT pulls/{n}/merge` with `sha` pinned to the
    verified head; optional `--squash|--merge|--rebase` (`merge_method`,
    omitted → repo default).
  - `--auto` — `POST pulls/{n}/auto-merge`; a method flag is required.
    GitHub merges later when branch protection clears; the PR is **not**
    merged by this call.
  - `--disable-auto` — `DELETE pulls/{n}/auto-merge`; method flags are
    rejected.
  Immediately before the write the plugin re-fetches
  `gh pr view --json` (`mergeStateStatus`, `reviewDecision`,
  `isDraft`, `headRefOid`, …) on top of the usual `--expect-head`
  stale-head guard. Immediate merge proceeds only for
  `mergeStateStatus` `CLEAN`/`UNSTABLE`; `BLOCKED`, `DIRTY`, `BEHIND`,
  `UNKNOWN`, `DRAFT`, `HAS_HOOKS`, drafts, and non-`OPEN` states are
  refused with `error.kind: "merge_blocked"` and a message carrying the
  observed state values. Auto-merge refuses only `DIRTY`/`DRAFT`/
  `UNKNOWN` — `BLOCKED` (pending/failed required checks) is exactly
  what auto-merge exists for, and the result reports the armed
  `auto_merge_request` rather than claiming a merge. gh HTTP
  405/409/422 write refusals also map to `merge_blocked`; other gh
  failures stay `gh_error`.
  The success payload echoes `head`/`base`, the pre-merge
  `merge_state_status`/`review_decision`/`mergeable`, the REST
  response under `merge`, the post-write `state`/`is_draft`/
  `auto_merge_request` (re-fetched — merge-queue/pending semantics are
  reported as GitHub states them, never guessed), and `result` ∈
  `merged` | `pending` | `auto_merge_enabled` | `auto_merge_disabled`.
  `result: "merged"` is emitted only when GitHub's merge response says
  `merged: true`; a `merged: false`/still-open response is `pending`.
- **Stale-head guard (mandatory on every `pr` mutation).**
  `review`/`comment`/`reply`/`reviewers`/`merge` all require
  `--expect-head <full sha>` (40- or 64-hex; short shas rejected with a
  usage error). For review/comment/reply/reviewers the plugin re-fetches
  `GET pulls/{n}` and refuses with `error.kind: "stale_head"` when the
  live `head.sha` differs — a review/comment can never land on a commit
  the caller did not last read. `pr merge` performs the equivalent
  check against `headRefOid` from the merge-state fetch and additionally
  pins `sha` in the REST merge body. Callers obtain the sha from
  `head.sha` (REST-shaped reads) or `headRefOid` (`pr view` merge
  fields) in any `pr` read payload (`view`/`reviews`/`comments`/
  `checks`/`logs`). A missing/unreadable head sha is `gh_error`, never
  a proceed.
- **github-delivery boundary.** These commands exist for explicit
  user/agent invocations only; there is no automation path that
  submits reviews, posts comments, requests reviewers, merges, or
  auto-merges. `pr merge` hard-refuses any PR whose **head** branch
  matches the release-train pattern `release-<digits>(-<digits>)*`
  with `error.kind: "release_boundary"` — for both merge and
  auto-merge — because `release-* -> main` approval is a maintainer
  decision per the github-delivery skill and the plugin must never
  automate or proxy it. Ticket PRs that target a release branch as
  *base* are unaffected (the guard keys on the head ref only). The
  plugin never approves its own release PR.
- `pr` appears in `mutation_commands` because `pr create`/`review`/
  `comment`/`reply`/`reviewers`/`merge` mutate the forge; `list`/
  `status`/`view`/`reviews`/`comments`/`checks`/`logs` are read-only.
  PR state is never used to bypass merge/review gates — the plugin
  exposes state, it does not act on it. Editing or deleting existing
  reviews/comments, merge-queue manipulation, GitHub stack merges,
  agent credits, and non-GitHub forges are out of scope (later
  tickets).
- gh error messages echo only the subcommand words, never argv content
  (titles/bodies may be sensitive); `gh api` mutation payloads travel
  on stdin (`--input -`) for the same reason.

## Forge: `issue list` / `issue view` / `issue create` / `issue edit` / `issue comment` / `issue close`

GitHub issue surface via the same `gh` CLI discipline as `pr`
(ADR-0008): same pane→repo resolution, `--repo owner/name` override,
`gh --version` ≥ 2.81.0 + `gh auth status` preflight, argv spawn with
`GH_PROMPT_DISABLED=1`, hard `remoteTimeoutMs` timeout, and sanitized
child env. All subcommands share `issue <action> <pane_id>`; per-action
flag whitelists apply (`issue` flags are validated exactly like `pr`).

- `issue list` — `gh issue list --json <fixed fields>` (number, title,
  state, labels, assignees, author, createdAt, updatedAt, url) with
  `--limit` (1–1000), `--state open|closed|all` (default `open`),
  repeatable `--label`/`--assignee` (comma-separated values allowed),
  and `--author`. Result: `{action:"list", repo, issues[]}` — gh's own
  row shape, already field-bounded.
- `issue view <number>` — bounded `gh issue view --json` projection
  (`boundIssueView`): `body` capped at 8000 chars (`body_truncated`),
  `comments` reduced to the newest 50 with per-comment body cap 4000
  (`comments_truncated`), `reactions` flattened to `{content, count}`
  (≤20, `reactions_truncated` when cut), and author/labels/assignees/
  milestone compacted to login/name strings. State fields are
  snake_case (`state`, `state_reason`, `closed`, `created_at`,
  `updated_at`).
- `issue create` — forge mutation. Requires `--title`; body via
  `--body` or `--body-file PATH`/`-` (stdin) with the same
  never-in-argv discipline as `pr create`; `--label`/`--assignee`
  repeatable. Success: `{action:"create", repo, ok:true, url, number}`
  parsed from gh's output URL.
- `issue edit <number>` — forge mutation. At least one field flag is
  required: `--title`, `--body`/`--body-file`, `--add-label`/
  `--remove-label`, `--add-assignee`/`--remove-assignee` (all
  repeatable, comma-separated), or `--state open|closed` (mapped to
  `gh issue close`/`gh issue reopen`). Field updates run via
  `gh issue edit`, then the state transition, then a bounded
  `issue view` re-fetch — the payload echoes the **post-mutation**
  state (`{action:"edit", repo, ok:true, number, issue}`), never the
  request echoed back.
- `issue comment <number>` — `POST issues/{n}/comments` via
  `gh api --input -` (body never in argv); `--body`/`--body-file`
  required. Echoes `{action:"comment", repo, ok:true, number, comment}`
  with the created comment bounded like `pr comment`.
- `issue close <number>` — `gh issue close` (+ optional
  `--reason completed|"not planned"`), then the same post-state view
  echo as `issue edit` (`{action:"close", ...}`).
- **No stale-head analog.** Issues have no head sha, so
  `--expect-head` does not exist here: the guard against acting on a
  stale read is the mutation gate (`options.mutation` target
  resolution — explicit or consistent ambient target required) plus
  explicit invocation only. `edit`/`close` always re-fetch and echo
  post-mutation state so callers see what actually landed; concurrent
  last-writer-wins on the forge side is inherent and documented, not
  hidden.
- gh failures surface as `kind: "gh_error"` (or `gh_missing`/`timeout`)
  exactly like `pr`; a failed mutation never reports success.

## Pane↔issue links: `issue link` / `issue unlink` / `issue links`

These three actions are **delegated to `rebuildup.inbox`** — the
pane-scoped tracker-link store already defined by `inbox-v1`
(`link --issue`, `unlink --issue`, `links`). The git plugin keeps **no
link state of its own**: a second store would duplicate the same
pane→issue association under a divergent staleness/retention regime
(the #177 boundary: inbox links are pane work-associations; forge
operations here are repo-scoped tracker mutations — they complement,
never merge).

- `issue link <pane> <owner>/<repo>#<n> [--verify]` → inbox
  `link <pane> --issue <repo>#<n> [--verify]`.
- `issue unlink <pane> <owner>/<repo>#<n>` → inbox
  `unlink <pane> --issue <repo>#<n>`.
- `issue links <pane>` → inbox `links <pane>`.
- The `<owner>/<repo>#<n>` grammar is identical to inbox-v1 tracker
  refs (deeper forge paths allowed); `issue link` fixes `kind: "issue"`
  — use `inbox link --pr` directly for PR associations.
- Link resolution follows the ADR-0013 sibling-plugin pattern:
  explicit `inboxPluginRoot` in `config.json` → sibling `../inbox` →
  `herdr plugin list --json`. The child runs with the inbox plugin's
  own config/state dirs pinned and this plugin's `HERDR_PLUGIN_*`
  values scrubbed; `--session`/`--socket`/`--timeout` and ambient
  socket env pass through so target resolution stays consistent.
- Success payloads re-wrap the inbox result under the git contract:
  `{contract:"herdr-plugins.git/v1", command:"issue", action,
  pane_id, delegated:"rebuildup.inbox", ok:true, link?|removed?,
  links?, warnings?, generated_at}`.
- Inbox failures propagate honestly: exit 2/`usage` → usage error;
  other failures → exit 1 with `error.kind` carrying the inbox error
  code (`target_required`, `target_conflict`,
  `ambient_identity_mismatch`, …) or `inbox_error`. An unresolvable
  inbox plugin is `kind: "inbox_unavailable"` — never a fabricated
  empty link list. A pane with no inbox state at all surfaces
  inbox's own `no inbox state for pane` failure as `inbox_error`
  (inbox-v1 `links` semantics), again never an invented `links: []`.
- **Staleness:** link entries follow inbox-v1 rules verbatim — they
  survive routine `state_change_seq` advances but are dropped on
  `agent_session` fingerprint change, seq regression, `pane.closed`,
  and retention pruning; they are never applied to a different pane or
  agent generation. `issue links` reports the staleness-resolved view.
- `issue link`/`unlink` are mutations (inbox state write): they
  require an explicit or ambient target, enforced by the delegated
  inbox CLI's own mutation gate. `issue links` is read-only.

## `project`

Computes `status` for the pane and reports metadata tokens via
`pane.report_metadata` (source `rebuildup.git`):

| Token | Meaning |
| --- | --- |
| `git_branch` | branch short name or `HEAD` (detached); cleared when not a repo |
| `git_changes` | total changed files as decimal string; `"0"` when clean |
| `git_ahead_behind` | `+A/-B` vs upstream; cleared when unknown |

Result echoes the `status` payload plus `metadata: {applied: bool,
warnings: []}`. Projection failure is a warning, not a command failure —
the status data remains authoritative.

## `capability`

```json
{
  "contract": "herdr-plugins.git/v1",
  "plugin": "rebuildup.git",
  "version": "0.7.0",
  "commands": [
    "capability", "status", "files", "diff", "log", "compare",
    "stage", "unstage", "commit", "branch", "upstream", "fetch",
    "push", "remote", "pr", "issue", "project"
  ],
  "read_only": false,
  "mutation_commands": [
    "stage", "unstage", "commit", "branch", "upstream", "fetch",
    "push", "pr", "issue"
  ],
  "requires": { "git": true },
  "defaults": {
    "timeoutMs": 10000,
    "remoteTimeoutMs": 60000,
    "diffMaxBytes": 262144
  }
}
```

## Versioning

`herdr-plugins.git/v1` is stable within v0.x. Additive fields may appear;
clients must ignore unknown fields. Breaking changes bump the contract
suffix. Mutation, forge (`pr`/`issue`), and delegated link commands
extend v1 only while every read command's output stays intact
(ADR-0007, ADR-0008); otherwise a `v2` contract will be introduced.
