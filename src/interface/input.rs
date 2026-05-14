/// Building kind is shared by UI input, Game API calls, and core building components.
/// It lives in the interface layer so UI code never needs to import ECS component modules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildingKind {
    Road,
    Residential,
    Commercial,
    Industrial,
    PowerPlant,
    Park,
}

impl BuildingKind {
    /// Money spent immediately when the building is placed.
    pub fn cost(self) -> i32 {
        match self {
            Self::Road => 1,
            Self::Residential => 5,
            Self::Commercial => 8,
            Self::Industrial => 10,
            Self::PowerPlant => 20,
            Self::Park => 6,
        }
    }

    /// Jobs contributed to city statistics by workplace buildings.
    pub fn jobs(self) -> i32 {
        match self {
            Self::Commercial => 2,
            Self::Industrial => 3,
            _ => 0,
        }
    }

    /// Single-character map representation used by view adapters and terminal UI.
    pub fn symbol(self) -> char {
        match self {
            Self::Road => '=',
            Self::Residential => 'R',
            Self::Commercial => 'C',
            Self::Industrial => 'I',
            Self::PowerPlant => 'T',
            Self::Park => 'P',
        }
    }

    /// Human-readable name used in view models and command feedback.
    pub fn label(self) -> &'static str {
        match self {
            Self::Road => "Road",
            Self::Residential => "Residential",
            Self::Commercial => "Commercial",
            Self::Industrial => "Industrial",
            Self::PowerPlant => "Power Plant",
            Self::Park => "Park",
        }
    }
}

/// Parsed command vocabulary for text-based frontends and command tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    Build {
        kind: BuildingKind,
        x: usize,
        y: usize,
    },
    Next,
    Inspect {
        x: usize,
        y: usize,
    },
    Status,
    Quit,
    Help,
}

/// Parses a user-facing command string into UI-safe input types.
pub fn parse_command(input: &str) -> Result<UiCommand, String> {
    let parts: Vec<_> = input.split_whitespace().collect();
    match parts.as_slice() {
        ["build", kind, x, y] => Ok(UiCommand::Build {
            kind: parse_building_kind(kind)?,
            x: parse_coordinate(x)?,
            y: parse_coordinate(y)?,
        }),
        ["next"] => Ok(UiCommand::Next),
        ["inspect", x, y] => Ok(UiCommand::Inspect {
            x: parse_coordinate(x)?,
            y: parse_coordinate(y)?,
        }),
        ["status"] => Ok(UiCommand::Status),
        ["quit"] => Ok(UiCommand::Quit),
        ["help"] => Ok(UiCommand::Help),
        [] => Ok(UiCommand::Help),
        _ => Err("Unknown command".to_string()),
    }
}

fn parse_coordinate(value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("Invalid coordinate: {value}"))
}

fn parse_building_kind(value: &str) -> Result<BuildingKind, String> {
    match value {
        "road" => Ok(BuildingKind::Road),
        "residential" => Ok(BuildingKind::Residential),
        "commercial" => Ok(BuildingKind::Commercial),
        "industrial" => Ok(BuildingKind::Industrial),
        "power" => Ok(BuildingKind::PowerPlant),
        "park" => Ok(BuildingKind::Park),
        _ => Err(format!("Unknown building kind: {value}")),
    }
}
