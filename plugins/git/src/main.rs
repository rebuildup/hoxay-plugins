fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        // `git tui [main|forge] [--open]` — interactive TUI surfaces.
        Some("tui") => git::tui::run(&args[1..]),
        // Everything else is the herdr-plugins.git/v1 CLI surface.
        _ => git::cli::run_cli(&args),
    }
}
