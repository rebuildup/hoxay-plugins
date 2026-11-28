//! Shared TUI helpers for native (Rust) plugin surfaces.
//!
//! Scope per ADR-0036: this crate mirrors the *safety* module of
//! `packages/tui` (`safety.ts`) — the pieces every `tui-v1` surface must
//! have before untrusted text can reach the renderer:
//!
//! - `safety::sanitize_display` — strip CSI/OSC/DCS and control
//!   characters from untrusted text (contract `tui-v1` §5 item 8).
//! - `safety::bound_text` / `BoundedBuffer` — bounded output and log
//!   surfaces (contract item 10).
//!
//! Deliberately NOT here yet (recorded boundary, ADR-0036 §1):
//! - the app shell / screen stack / components — each plugin currently
//!   owns its ratatui widget tree; a shared shell is extracted when a
//!   second native consumer exists (AGENTS.md "only genuinely shared
//!   code" rule).
//! - `openExternal` / `createThrottle` — no current native consumer;
//!   port on demand with the same argv-array + scrubbed-env semantics.
//!
//! The implementations are line-for-line ports of `packages/tui/src/
//! safety.ts` so TS and Rust surfaces sanitize identically during
//! coexistence.

pub mod safety;
