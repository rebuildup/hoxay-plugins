//! rebuildup.inbox — native Rust port (Issue #291, Program #286).
//!
//! Single binary serving three herdr surfaces:
//! - CLI contract `herdr-plugins.inbox/v1` (`inbox <command>`)
//! - event hooks (`inbox hook`, HERDR_PLUGIN_EVENT[_JSON])
//! - interactive TUI (`inbox tui [--open]`, contract herdr-plugins.tui/v1)
//!
//! Domain modules mirror the frozen TypeScript implementation
//! (`plugins/inbox/src/*.ts`) one-to-one so the `inbox-v1` wire contract
//! and the on-disk state schema (v2) stay compatible.

pub mod model;
pub mod pluginroot;
pub mod state;
pub mod transitions;
pub mod tui_model;
pub mod watchpr;

pub mod cli;
pub mod hook;
pub mod tui;

pub use state::PLUGIN_ID;
