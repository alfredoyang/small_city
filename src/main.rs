//! Binary entry point that launches the cursor-based ASCII terminal UI.

fn main() -> std::io::Result<()> {
    small_city::ui::ascii::run()
}
