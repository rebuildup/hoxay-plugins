//! SPIKE for GitHub Issue #288 (Program #286 — Native Performance).
//!
//! Minimal dependency-free Rust binary packaged as a herdr plugin via the
//! official `herdr-plugin.toml` manifest. It exists only to produce real
//! install / start / exit evidence for the native-distribution decision
//! recorded in `docs/research/np288-native-distribution-spike.md` and
//! `docs/adr/0035-*.md`. It is NOT a product plugin.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn state_dir() -> Option<PathBuf> {
    env::var_os("HERDR_PLUGIN_STATE_DIR").map(PathBuf::from)
}

fn env_summary() -> String {
    let keys = [
        "HERDR_PLUGIN_ID",
        "HERDR_PLUGIN_ROOT",
        "HERDR_PLUGIN_CONFIG_DIR",
        "HERDR_PLUGIN_STATE_DIR",
        "HERDR_PLUGIN_EVENT",
        "HERDR_PLUGIN_ACTION_ID",
        "HERDR_PLUGIN_ENTRYPOINT_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_TAB_ID",
        "HERDR_PANE_ID",
        "HERDR_SOCKET_PATH",
        "HERDR_BIN_PATH",
    ];
    let mut out = String::new();
    for key in keys {
        if let Ok(v) = env::var(key) {
            out.push_str(&format!("{key}={v}\n"));
        }
    }
    if let Ok(ctx) = env::var("HERDR_PLUGIN_CONTEXT_JSON") {
        out.push_str(&format!("HERDR_PLUGIN_CONTEXT_JSON={ctx}\n"));
    }
    if let Ok(ev) = env::var("HERDR_PLUGIN_EVENT_JSON") {
        out.push_str(&format!("HERDR_PLUGIN_EVENT_JSON={ev}\n"));
    }
    out
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(cmd) = args.next() else {
        eprintln!("usage: np288-dist <version|capability|ping|startup|hook>");
        return ExitCode::from(2);
    };
    match cmd.as_str() {
        "version" | "--version" | "-V" => {
            println!("np288-dist {VERSION} (rust spike, issue #288)");
            ExitCode::SUCCESS
        }
        "capability" => {
            println!(
                "{{\"contract\":\"herdr-plugins.np288-dist-spike/v0\",\"version\":\"{VERSION}\",\"commands\":[\"ping\",\"hook\",\"startup\"]}}"
            );
            ExitCode::SUCCESS
        }
        "ping" => {
            println!("np288-dist ping ok (v{VERSION})");
            print!("{}", env_summary());
            ExitCode::SUCCESS
        }
        "startup" => {
            println!("np288-dist startup hook ran (v{VERSION})");
            print!("{}", env_summary());
            if let Some(dir) = state_dir() {
                let _ = fs::create_dir_all(&dir);
                let _ = fs::write(dir.join("startup.marker"), format!("v{VERSION}\n"));
            }
            ExitCode::SUCCESS
        }
        "hook" => {
            println!("np288-dist event hook ran (v{VERSION})");
            print!("{}", env_summary());
            if let Some(dir) = state_dir() {
                let _ = fs::create_dir_all(&dir);
                let line = format!(
                    "{}\\t{}\\n",
                    VERSION,
                    env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default()
                );
                use std::io::Write;
                if let Ok(mut f) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("hooks.log"))
                {
                    let _ = f.write_all(line.as_bytes());
                }
            }
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("np288-dist: unknown subcommand {other:?}");
            ExitCode::from(2)
        }
    }
}
