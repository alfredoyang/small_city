//! Pure keyboard action mapping for the ratatui terminal frontend.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::interface::input::BuildingKind;

/// UI-level actions after raw terminal keys have been translated.
///
/// Keeping this separate from `Game` commands makes keyboard handling easy to test and keeps
/// crossterm details out of the main TUI event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TuiAction {
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    SelectBuild(BuildingKind),
    Build,
    Replace,
    Upgrade,
    Bulldoze,
    Tick,
    Save,
    Load,
    ToggleHelp,
    CycleOverlay,
    ToggleRun,
    IncreaseSpeed,
    DecreaseSpeed,
    Quit,
    None,
}

/// Converts one crossterm key event into a game-agnostic TUI action.
///
/// `crossterm` reports physical key presses as `KeyEvent` values. The rest of the UI should not
/// care whether movement came from `W` or the up-arrow key, so this function normalizes those
/// details into a small action enum.
pub(crate) fn map_key_event(event: KeyEvent) -> TuiAction {
    // Ctrl-C should quit even though it is represented as a modified character key.
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        return match event.code {
            KeyCode::Char('c') | KeyCode::Char('C') => TuiAction::Quit,
            _ => TuiAction::None,
        };
    }

    // Uppercase `S` is save because lowercase `s` is reserved for moving the cursor down.
    match event.code {
        KeyCode::Up | KeyCode::Char('w') | KeyCode::Char('W') => TuiAction::MoveUp,
        KeyCode::Down | KeyCode::Char('s') => TuiAction::MoveDown,
        KeyCode::Left | KeyCode::Char('a') | KeyCode::Char('A') => TuiAction::MoveLeft,
        KeyCode::Right | KeyCode::Char('d') | KeyCode::Char('D') => TuiAction::MoveRight,
        KeyCode::Char('1') => TuiAction::SelectBuild(BuildingKind::Road),
        KeyCode::Char('2') => TuiAction::SelectBuild(BuildingKind::Residential),
        KeyCode::Char('3') => TuiAction::SelectBuild(BuildingKind::Commercial),
        KeyCode::Char('4') => TuiAction::SelectBuild(BuildingKind::Industrial),
        KeyCode::Char('5') => TuiAction::SelectBuild(BuildingKind::PowerPlant),
        KeyCode::Char('6') => TuiAction::SelectBuild(BuildingKind::Park),
        KeyCode::Enter | KeyCode::Char('b') | KeyCode::Char('B') => TuiAction::Build,
        KeyCode::Char('r') | KeyCode::Char('R') => TuiAction::Replace,
        KeyCode::Char('u') | KeyCode::Char('U') => TuiAction::Upgrade,
        KeyCode::Char('x') | KeyCode::Char('X') => TuiAction::Bulldoze,
        KeyCode::Char('n') | KeyCode::Char('N') => TuiAction::Tick,
        KeyCode::Char('S') => TuiAction::Save,
        KeyCode::Char('l') | KeyCode::Char('L') => TuiAction::Load,
        KeyCode::Char('h') | KeyCode::Char('H') => TuiAction::ToggleHelp,
        KeyCode::Char('o') | KeyCode::Char('O') => TuiAction::CycleOverlay,
        KeyCode::Char(' ') => TuiAction::ToggleRun,
        KeyCode::Char('+') | KeyCode::Char('=') => TuiAction::IncreaseSpeed,
        KeyCode::Char('-') => TuiAction::DecreaseSpeed,
        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => TuiAction::Quit,
        _ => TuiAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn maps_cursor_movement_keys() {
        assert_eq!(map_key_event(key(KeyCode::Char('w'))), TuiAction::MoveUp);
        assert_eq!(map_key_event(key(KeyCode::Up)), TuiAction::MoveUp);
        assert_eq!(map_key_event(key(KeyCode::Char('s'))), TuiAction::MoveDown);
        assert_eq!(map_key_event(key(KeyCode::Down)), TuiAction::MoveDown);
        assert_eq!(map_key_event(key(KeyCode::Char('a'))), TuiAction::MoveLeft);
        assert_eq!(map_key_event(key(KeyCode::Left)), TuiAction::MoveLeft);
        assert_eq!(map_key_event(key(KeyCode::Char('d'))), TuiAction::MoveRight);
        assert_eq!(map_key_event(key(KeyCode::Right)), TuiAction::MoveRight);
    }

    #[test]
    fn maps_build_selection_keys() {
        assert_eq!(
            map_key_event(key(KeyCode::Char('1'))),
            TuiAction::SelectBuild(BuildingKind::Road)
        );
        assert_eq!(
            map_key_event(key(KeyCode::Char('2'))),
            TuiAction::SelectBuild(BuildingKind::Residential)
        );
        assert_eq!(
            map_key_event(key(KeyCode::Char('3'))),
            TuiAction::SelectBuild(BuildingKind::Commercial)
        );
        assert_eq!(
            map_key_event(key(KeyCode::Char('4'))),
            TuiAction::SelectBuild(BuildingKind::Industrial)
        );
        assert_eq!(
            map_key_event(key(KeyCode::Char('5'))),
            TuiAction::SelectBuild(BuildingKind::PowerPlant)
        );
        assert_eq!(
            map_key_event(key(KeyCode::Char('6'))),
            TuiAction::SelectBuild(BuildingKind::Park)
        );
    }

    #[test]
    fn maps_gameplay_actions() {
        assert_eq!(map_key_event(key(KeyCode::Enter)), TuiAction::Build);
        assert_eq!(map_key_event(key(KeyCode::Char('b'))), TuiAction::Build);
        assert_eq!(map_key_event(key(KeyCode::Char('R'))), TuiAction::Replace);
        assert_eq!(map_key_event(key(KeyCode::Char('U'))), TuiAction::Upgrade);
        assert_eq!(map_key_event(key(KeyCode::Char('X'))), TuiAction::Bulldoze);
        assert_eq!(map_key_event(key(KeyCode::Char('N'))), TuiAction::Tick);
        assert_eq!(map_key_event(key(KeyCode::Char('S'))), TuiAction::Save);
        assert_eq!(map_key_event(key(KeyCode::Char('L'))), TuiAction::Load);
        assert_eq!(
            map_key_event(key(KeyCode::Char('H'))),
            TuiAction::ToggleHelp
        );
        assert_eq!(
            map_key_event(key(KeyCode::Char('O'))),
            TuiAction::CycleOverlay
        );
        assert_eq!(map_key_event(key(KeyCode::Char(' '))), TuiAction::ToggleRun);
        assert_eq!(map_key_event(key(KeyCode::Char('Q'))), TuiAction::Quit);
    }

    #[test]
    fn maps_speed_keys() {
        assert_eq!(
            map_key_event(key(KeyCode::Char('+'))),
            TuiAction::IncreaseSpeed
        );
        assert_eq!(
            map_key_event(key(KeyCode::Char('='))),
            TuiAction::IncreaseSpeed
        );
        assert_eq!(
            map_key_event(key(KeyCode::Char('-'))),
            TuiAction::DecreaseSpeed
        );
    }
}
