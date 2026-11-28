//! Typed client for the herdr local socket API.
//!
//! Rust port of `packages/herdr-client` (TS), scoped to the surface the
//! inbox plugin needs: newline-delimited JSON over a Unix domain socket
//! (Windows named pipe `\\.\pipe\<path>` on win32), one request per
//! connection, `events.subscribe` long-lived stream, target resolution
//! (`resolveTarget`), session namespacing (`sessionKeyFor`), plugin
//! config/state dirs, and ambient pane identity.
//!
//! Semantics mirror the TS implementation exactly — contradictions fail
//! closed, mutations require an explicit/ambient target, and nothing is
//! fabricated.

pub mod commands;
pub mod dirs;
pub mod snapshot;
pub mod socket;
pub mod target;

pub use commands::*;
pub use socket::*;
pub use target::*;
