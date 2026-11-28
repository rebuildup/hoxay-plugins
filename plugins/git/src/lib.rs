//! rebuildup.git — native Rust port (Issue #292, Program #286).
//!
//! Single binary serving two herdr surfaces:
//! - CLI contract `herdr-plugins.git/v1` (`git <command>`): read commands
//!   (status/files/diff/log/compare/remote), local mutations
//!   (stage/unstage/commit/branch/upstream/fetch/push), the GitHub forge
//!   surface via the host `gh` CLI (`pr`, `issue`), `project` pane
//!   metadata projection, and `capability`.
//! - interactive TUIs (`git tui main|forge [--open]`, contract
//!   herdr-plugins.tui/v1).
//!
//! Domain modules mirror the frozen TypeScript implementation
//! (`plugins/git/src/*.ts`) one-to-one so the wire contract stays
//! compatible.
//
// CLI-domain functions mirror the TS collectors, whose signatures carry
// one parameter per git/gh flag; grouping them would hurt parity.
#![allow(clippy::too_many_arguments)]

pub mod gh;
pub mod git;
pub mod gitenv;
pub mod inboxlink;
pub mod pluginroot;

pub mod cli;
pub mod tui;
pub mod tui_forge;
pub mod tui_model;
