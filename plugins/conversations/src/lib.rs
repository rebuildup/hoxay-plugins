//! rebuildup.conversations — native Rust port (Issue #293).
//!
//! Single binary serving the `herdr-plugins.conversations/v1` CLI
//! surface (`conversations <command>`) and the interactive TUI
//! (`conversations tui [--open]`, contract herdr-plugins.tui/v1).
//!
//! Domain modules mirror the frozen TypeScript implementation
//! (`plugins/conversations/src/*.ts`) one-to-one so the wire contract
//! and the on-disk state/event-log schemas stay compatible. The TUI
//! calls the domain core in-process (no self-respawn), per the #291
//! inbox pattern.

pub mod claude;
pub mod cli;
pub mod codex;
pub mod indexer;
pub mod model;
pub mod opencode;
pub mod providers;
pub mod read;
pub mod state;
pub mod tui;
