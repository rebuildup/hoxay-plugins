//! rebuildup.usage — native Rust port (Issue #294).
//!
//! Single binary serving the `herdr-plugins.usage/v1` CLI surface
//! (`usage <command>`) and the interactive TUI (`usage tui [--open]`,
//! contract herdr-plugins.tui/v1).
//!
//! Domain modules mirror the frozen TypeScript implementation
//! (`plugins/usage/src/*.ts`) one-to-one so the wire contract stays
//! compatible. The TUI calls the domain core in-process (no
//! self-respawn), per the #291/#293 pattern; the conversations index
//! is still consumed exclusively through the rebuildup.conversations
//! public CLI contract (bin-first, bun-fallback) — never its state
//! files (ADR-0013).

pub mod aggregate;
pub mod cli;
pub mod cost;
pub mod divergence;
pub mod hub;
pub mod model;
pub mod pricing;
pub mod quota;
pub mod source;
pub mod state;
pub mod tui;
