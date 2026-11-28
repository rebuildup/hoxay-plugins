fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        // `inbox hook` — herdr event-hook entrypoint (HERDR_PLUGIN_EVENT_JSON).
        Some("hook") => inbox::hook::run(&args[1..]),
        // `inbox tui [--open]` — interactive TUI / open action helper.
        Some("tui") => inbox::tui::run(&args[1..]),
        // Everything else is the herdr-plugins.inbox/v1 CLI surface.
        _ => inbox::cli::run(&args),
    }
}
