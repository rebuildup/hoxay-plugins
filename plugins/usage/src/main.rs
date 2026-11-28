fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        // `usage tui [--open]` — interactive TUI / open action.
        Some("tui") => std::process::exit(usage::tui::run(&args[1..])),
        // Everything else is the herdr-plugins.usage/v1 CLI.
        _ => std::process::exit(usage::cli::run(&args)),
    }
}
