#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub x: usize,
    pub y: usize,
}

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

    pub fn jobs(self) -> i32 {
        match self {
            Self::Commercial => 2,
            Self::Industrial => 3,
            _ => 0,
        }
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Building {
    pub kind: BuildingKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Population {
    pub current: i32,
    pub max: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerProvider {
    pub radius: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerConsumer {
    pub powered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollutionSource {
    pub amount: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HappinessEffect {
    pub amount: i32,
}
