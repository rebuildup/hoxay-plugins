fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        // `conversations tui [--open]` — interactive TUI / open action.
        Some("tui") => std::process::exit(conversations::tui::run(&args[1..])),
        // Everything else is the herdr-plugins.conversations/v1 CLI.
        _ => std::process::exit(conversations::cli::run(&args)),
    }
}
