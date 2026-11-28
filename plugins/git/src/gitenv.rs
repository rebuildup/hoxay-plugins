//! Child-process environment hygiene for `git` (and `gh`, which shells out
//! to git for repo/remote resolution). Rust port of
//! `packages/git-core/src/git-env.ts`.
//!
//! `runGit`/`runGh` spawn with argv only — no shell — but the child still
//! inherits the environment. Ambient `GIT_*` variables exported by agent
//! panes, CI wrappers, or `git -c` parents can silently redirect repository,
//! index, object-store, discovery, or config resolution to a different
//! target than the pane cwd the plugin resolved (issue #96). This module
//! strips every `GIT_*` variable that can retarget or inject into git's
//! view of the repository while preserving the user's intentional
//! auth/identity config.

use std::collections::HashMap;

/// Exact `GIT_*` names removed from the inherited environment (verbatim
/// port of STRIPPED_ENV_VARS).
const STRIPPED_ENV_VARS: &[&str] = &[
    // repository / index / object store
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_INDEX_VERSION",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_QUARANTINE_PATH",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    // discovery
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    // config injection
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    // pathspec / object semantics
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_NO_LAZY_FETCH",
    "GIT_DEFAULT_HASH",
    "GIT_DEFAULT_REF_FORMAT",
    "GIT_REFERENCE_BACKEND",
    "GIT_ATTR_SOURCE",
    "GIT_ATTR_NOSYSTEM",
    // program / output redirection
    "GIT_EXEC_PATH",
    "GIT_EXTERNAL_DIFF",
    "GIT_EXTERNAL_DIFF_TRUST_EXIT_CODE",
    "GIT_DIFF_OPTS",
    "GIT_DIFF_PATH_COUNTER",
    "GIT_DIFF_PATH_TOTAL",
    "GIT_EDITOR",
    "GIT_SEQUENCE_EDITOR",
    "GIT_PAGER",
    "GIT_TEMPLATE_DIR",
    "GIT_REFLOG_ACTION",
    "GIT_REF_PARANOIA",
    "GIT_COMMIT_GRAPH_PARANOIA",
    "GIT_PROTOCOL",
    "GIT_PROTOCOL_FROM_USER",
    "GIT_REDIRECT_STDIN",
    "GIT_REDIRECT_STDOUT",
    "GIT_REDIRECT_STDERR",
    "GIT_CURL_VERBOSE",
];

/// Prefixes removed from the inherited environment (GIT_TRACE*,
/// GIT_CONFIG_KEY_/GIT_CONFIG_VALUE_ indexed config injection, GIT_TEST_*).
const STRIPPED_ENV_PREFIXES: &[&str] = &[
    "GIT_TRACE",
    "GIT_CONFIG_KEY_",
    "GIT_CONFIG_VALUE_",
    "GIT_TEST_",
];

/// gh-specific variables removed in addition to the GIT_* set.
const STRIPPED_GH_ENV_VARS: &[&str] = &[
    // GH_REPO is an explicit repo-targeting override; the plugin resolves
    // the forge repo itself and must not let ambient env retarget it.
    "GH_REPO",
    // GH_DEBUG writes request/response logs (potentially auth headers).
    "GH_DEBUG",
    // GH_FORCE_TTY/GH_PAGER re-enable interactive behavior pinned off.
    "GH_FORCE_TTY",
    "GH_PAGER",
];

fn is_stripped(key: &str, extra_strip: &[&str]) -> bool {
    STRIPPED_ENV_VARS.contains(&key)
        || extra_strip.contains(&key)
        || STRIPPED_ENV_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// Build the environment for a git/gh child process: inherited env minus
/// every repo/index/object/config-redirecting `GIT_*` variable (plus any
/// `extra_strip` names), with `overrides` applied last. Mirrors
/// `sanitizedChildEnv()`.
pub fn sanitized_child_env(
    overrides: &[(String, String)],
    extra_strip: &[&str],
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    for (key, value) in std::env::vars() {
        if is_stripped(&key, extra_strip) {
            continue;
        }
        env.insert(key, value);
    }
    for (key, value) in overrides {
        env.insert(key.clone(), value.clone());
    }
    env
}

/// Environment for `gh`: git hygiene plus GH_* targeting/debug stripping.
pub fn gh_child_env(overrides: &[(String, String)]) -> HashMap<String, String> {
    sanitized_child_env(overrides, STRIPPED_GH_ENV_VARS)
}
