//! Binary entry point that selects between the TUI and fallback ASCII frontends.

fn main() -> std::io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("ascii") => small_city::ui::ascii::run(),
        Some("regional") => small_city::ui::tui::run_regional(),
        Some("regional-ascii") => small_city::ui::ascii::run_regional(),
        Some("tui") | None => small_city::ui::tui::run(),
        Some(other) => {
            eprintln!(
                "Unknown frontend '{other}'. Use 'ascii', 'tui', 'regional', or 'regional-ascii'."
            );
            Ok(())
        }
    }
}
