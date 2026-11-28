//! This plugin's own root (`plugins/git`). Rust port of the
//! `gitPluginRoot()` slice of `plugins/git/src/inbox.ts`.

use std::path::PathBuf;

use hoxay_herdr_client::dirs::env_nonempty;

/// This plugin's own root. `HERDR_PLUGIN_ROOT` wins, otherwise the parent
/// of the `bin/` dir the executable lives in (`bin/git` → `plugins/git`).
pub fn plugin_root() -> PathBuf {
    if let Some(env_root) = env_nonempty("HERDR_PLUGIN_ROOT") {
        return PathBuf::from(env_root);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            if let Some(root) = bin_dir.parent() {
                return root.to_path_buf();
            }
        }
    }
    // Fallback for `cargo run` from the crate dir.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
