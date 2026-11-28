//! herdr config/state/plugin directory resolution and path-safe plugin
//! id components. Mirrors `packages/herdr-client/src/socket.ts`.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// herdr config dir: `$XDG_CONFIG_HOME/herdr` or `~/.config/herdr`
/// (win32: `%APPDATA%\herdr`).
pub fn herdr_config_dir() -> PathBuf {
    if let Some(xdg) = env_nonempty("XDG_CONFIG_HOME") {
        return Path::new(&xdg).join("herdr");
    }
    #[cfg(windows)]
    {
        if let Some(app_data) = env_nonempty("APPDATA") {
            return Path::new(&app_data).join("herdr");
        }
        if let Some(profile) = env_nonempty("USERPROFILE") {
            return Path::new(&profile)
                .join("AppData")
                .join("Roaming")
                .join("herdr");
        }
    }
    if let Some(home) = env_nonempty("HOME") {
        return Path::new(&home).join(".config").join("herdr");
    }
    env_nonempty("TMPDIR")
        .map(|t| Path::new(&t).join("herdr"))
        .unwrap_or_else(|| PathBuf::from("/tmp/herdr"))
}

/// herdr state dir: `$XDG_STATE_HOME/herdr` or `~/.local/state/herdr`
/// (win32: `%LOCALAPPDATA%\herdr`).
pub fn herdr_state_dir() -> PathBuf {
    if let Some(xdg) = env_nonempty("XDG_STATE_HOME") {
        return Path::new(&xdg).join("herdr");
    }
    #[cfg(windows)]
    {
        if let Some(local) = env_nonempty("LOCALAPPDATA") {
            return Path::new(&local).join("herdr");
        }
        if let Some(profile) = env_nonempty("USERPROFILE") {
            return Path::new(&profile)
                .join("AppData")
                .join("Local")
                .join("herdr");
        }
    }
    if let Some(home) = env_nonempty("HOME") {
        return Path::new(&home).join(".local").join("state").join("herdr");
    }
    env_nonempty("TMPDIR")
        .map(|t| Path::new(&t).join("herdr-state"))
        .unwrap_or_else(|| PathBuf::from("/tmp/herdr-state"))
}

/// Resolved plugin config/state directories (mirrors
/// `pluginDirs` → `{configDir, stateDir}`).
#[derive(Debug, Clone)]
pub struct PluginDirs {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

/// Plugin config/state directories for external (non-plugin) clients.
/// Mirrors `herdr plugin config-dir`: explicit options win;
/// `HERDR_PLUGIN_CONFIG_DIR`/`HERDR_PLUGIN_STATE_DIR` (plugin runtime)
/// come next; derived platform dirs last. Env vars are used verbatim
/// when set (mirroring TS `??` semantics).
pub fn plugin_dirs(
    plugin_id: &str,
    config_dir: Option<&str>,
    state_dir: Option<&str>,
) -> PluginDirs {
    let component = plugin_path_component(plugin_id);
    let config = config_dir
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HERDR_PLUGIN_CONFIG_DIR")
                .ok()
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| {
            herdr_config_dir()
                .join("plugins")
                .join("config")
                .join(&component)
        });
    let state = state_dir
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HERDR_PLUGIN_STATE_DIR")
                .ok()
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| herdr_state_dir().join("plugins").join(&component));
    PluginDirs {
        config_dir: config,
        state_dir: state,
    }
}

pub fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|v| {
        let t = v.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    })
}

/// Path-safe encoding of a plugin id, mirroring herdr's
/// `plugin_config_path_component`.
pub fn plugin_path_component(value: &str) -> String {
    let mut component = String::new();
    for ch in value.chars() {
        let code = ch as u32;
        if (0x61..=0x7a).contains(&code)
            || (0x30..=0x39).contains(&code)
            || ch == '.'
            || ch == '_'
            || ch == '-'
        {
            component.push(ch);
        } else {
            let mut buf = [0u8; 4];
            for byte in ch.encode_utf8(&mut buf).as_bytes() {
                component.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    if component.is_empty() {
        return "%plugin".to_string();
    }
    if component.ends_with('.') {
        component.pop();
        component.push_str("%2E");
    }
    if has_windows_reserved_stem(&component) {
        component.insert(0, '%');
    }
    const MAX_CHARS: usize = 120;
    if component.chars().count() > MAX_CHARS {
        let hash = Sha256::digest(value.as_bytes());
        let hash_hex: String = hash.iter().take(6).map(|b| format!("{:02x}", b)).collect();
        let prefix: String = component
            .chars()
            .take(MAX_CHARS - hash_hex.len() - 1)
            .collect();
        return format!("{}-{}", prefix, hash_hex);
    }
    component
}

/// Mirrors herdr's Windows reserved device-name guard for path components.
fn has_windows_reserved_stem(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or("").to_lowercase();
    if ["con", "prn", "aux", "nul"].contains(&stem.as_str()) {
        return true;
    }
    let bytes = stem.as_bytes();
    if bytes.len() == 4 && (bytes.starts_with(b"com") || bytes.starts_with(b"lpt")) {
        return matches!(bytes[3], b'1'..=b'9');
    }
    false
}
