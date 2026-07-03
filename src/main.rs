//! Binary entry point that selects between the TUI and fallback ASCII frontends.

fn main() -> std::io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("ascii") => small_city::ui::ascii::run(),
        Some("tui") | None => small_city::ui::tui::run(),
        Some(other) => {
            eprintln!("Unknown frontend '{other}'. Use 'tui' or 'ascii'.");
            Ok(())
        }
    }
}
