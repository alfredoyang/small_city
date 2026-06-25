//! Citizen daily schedule — the "what does this citizen want to do?" layer
//! (see `docs/citizen-schedule-plan.md`). v1 is commute-only.
//!
//! This module owns **only** the time→intent decision. It knows nothing about
//! roads, Dijkstra, route caches, or local-vs-remote resolution — those are
//! pathfinding/movement-side decisions (pathfinding plan §5/§7). The movement
//! system (P3) consumes the intent and resolves it to a concrete route.
//!
//! ```text
//!  hour:   00 ─────── 09 ─────────── 15 ─────── 24
//!  range:  [00:00,09:00) [09:00,15:00) [15:00,24:00)
//!  phase:      HOME          WORK          HOME
//!  intent:     Home       Work(entity)     Home
//! ```
//! (v1: the deferred 15:00–22:00 leisure block is folded into the HOME range.)
//!
//! Two entry points, by who is asking:
//! - `schedule_phase(hour)` — pure phase from the hour alone. Used by a P5
//!   visiting token in a host region that does **not** own the citizen's
//!   `Citizen` record; it only needs to detect the workday end (phase → Home).
//! - `schedule_intent(hour, citizen)` — semantic intent for a **local** citizen
//!   the movement system then resolves. Emits `Work(workplace_entity)` without
//!   deciding local vs remote; the movement system calls
//!   `workplace.as_local(world.region_id)` for that.

use crate::core::components::Citizen;
use crate::core::entity::Entity;

/// Coarse time-of-day phase, independent of any citizen data.
///
/// `Leisure` is reserved for the deferred free-time block (15:00–22:00); v1's
/// `schedule_phase` never returns it (free time collapses to `Home`), but the
/// variant exists so the deferred resolver and `schedule_intent` can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchedulePhase {
    Work,
    Home,
    // ponytail: reserved for the deferred 15:00–22:00 free-time block; v1's
    // `schedule_phase` never constructs it (free time collapses to Home). Drop
    // this variant if the leisure resolver never lands.
    #[allow(dead_code)]
    Leisure,
}

/// Semantic intent for a local citizen. The schedule emits this; the movement
/// system resolves it to a concrete target/route.
///
/// `Work(Entity)` carries the workplace entity **as-is** (local or remote) — the
/// schedule does not call `as_local` or pick a border-exit cell. `Leisure` is
/// deferred: a future destination resolver (pathfinding-side) turns it into a
/// specific commercial building.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleIntent {
    Home,
    Work(Entity),
    Leisure,
}

/// Pure phase from the hour alone — no citizen data needed.
///
/// v1: `[09:00, 15:00)` is `Work`, everything else is `Home` (the deferred
/// leisure block collapses to `Home`).
//
// ponytail: leisure (15:00–22:00) collapses to Home in v1; the Leisure phase/
// intent is wired but unreachable until the free-time destination resolver
// lands (pathfinding plan §7 future work).
pub(crate) fn schedule_phase(hour: u8) -> SchedulePhase {
    if (9..15).contains(&hour) {
        SchedulePhase::Work
    } else {
        SchedulePhase::Home
    }
}

/// Semantic intent for a local citizen at `hour`.
///
/// During the work phase a citizen with an assignment wants its workplace; a
/// jobless citizen stays home. Outside the work phase everyone wants home.
#[allow(dead_code)] // P1-of-schedule; the movement system (pathfinding P3) wires this.
pub(crate) fn schedule_intent(hour: u8, citizen: &Citizen) -> ScheduleIntent {
    match schedule_phase(hour) {
        SchedulePhase::Work => match &citizen.workplace_assignment {
            Some(assignment) => ScheduleIntent::Work(assignment.workplace),
            None => ScheduleIntent::Home, // jobless → home
        },
        SchedulePhase::Home => ScheduleIntent::Home,
        // Unreachable in v1: schedule_phase never returns Leisure. Mapped for
        // completeness so the deferred free-time block has a place to land.
        SchedulePhase::Leisure => ScheduleIntent::Leisure,
    }
}

#[cfg(test)]
mod tests {
    use super::{ScheduleIntent, SchedulePhase, schedule_intent, schedule_phase};
    use crate::core::city_refs::CityCellRef;
    use crate::core::components::{Citizen, Morale, WorkplaceAssignment};
    use crate::core::entity::Entity;
    use crate::core::regions::RegionId;

    fn citizen(local: u32, workplace_assignment: Option<WorkplaceAssignment>) -> Citizen {
        let id = Entity::new(RegionId(0), local);
        Citizen {
            id,
            age: 1,
            home: Entity::new(RegionId(0), 0),
            workplace_assignment,
            morale: Morale::default(),
            money: 0,
        }
    }

    fn citizen_with_job(workplace: Entity) -> Citizen {
        citizen(
            1,
            Some(WorkplaceAssignment {
                workplace,
                // Invariant (components.rs): location.region == workplace.region().
                location: CityCellRef::local(workplace.region(), 0, 0),
                salary: 100,
            }),
        )
    }

    fn jobless_citizen() -> Citizen {
        citizen(2, None)
    }

    /// The phase boundaries are half-open `[09:00, 15:00)`: 09 and 14 are Work,
    /// 08 and 15 are Home. This pins the off-by-one at both ends.
    #[test]
    fn schedule_phase_boundaries_are_half_open() {
        assert_eq!(
            schedule_phase(8),
            SchedulePhase::Home,
            "08:00 is before work"
        );
        assert_eq!(schedule_phase(9), SchedulePhase::Work, "09:00 starts work");
        assert_eq!(schedule_phase(14), SchedulePhase::Work, "14:00 still work");
        assert_eq!(schedule_phase(15), SchedulePhase::Home, "15:00 ends work");
        assert_eq!(schedule_phase(0), SchedulePhase::Home, "midnight is home");
        assert_eq!(
            schedule_phase(23),
            SchedulePhase::Home,
            "late night is home"
        );
    }

    /// v1 never produces the deferred Leisure phase — free time collapses to Home.
    #[test]
    fn schedule_phase_never_returns_leisure_in_v1() {
        for hour in 0u8..24 {
            assert_ne!(
                schedule_phase(hour),
                SchedulePhase::Leisure,
                "hour {hour} must not be Leisure in v1"
            );
        }
    }

    /// During the work phase, an employed citizen is sent to its workplace
    /// entity verbatim — the schedule does not resolve local vs remote.
    #[test]
    fn employed_citizen_wants_workplace_during_work_hours() {
        let workplace = Entity::new(RegionId(0), 42);
        let citizen = citizen_with_job(workplace);
        assert_eq!(
            schedule_intent(10, &citizen),
            ScheduleIntent::Work(workplace)
        );
    }

    /// A remote workplace (different birth region) is still emitted as
    /// `Work(entity)` unchanged — the schedule never calls `as_local`.
    #[test]
    fn remote_workplace_is_emitted_unresolved() {
        let remote = Entity::new(RegionId(7), 3);
        let citizen = citizen_with_job(remote);
        assert_eq!(schedule_intent(12, &citizen), ScheduleIntent::Work(remote));
    }

    /// A jobless citizen stays home even during work hours.
    #[test]
    fn jobless_citizen_stays_home_during_work_hours() {
        let citizen = jobless_citizen();
        assert_eq!(schedule_intent(10, &citizen), ScheduleIntent::Home);
    }

    /// Outside the work phase, everyone (employed or not) wants home.
    #[test]
    fn everyone_goes_home_outside_work_hours() {
        let employed = citizen_with_job(Entity::new(RegionId(0), 42));
        let jobless = jobless_citizen();
        assert_eq!(schedule_intent(8, &employed), ScheduleIntent::Home);
        assert_eq!(schedule_intent(22, &employed), ScheduleIntent::Home);
        assert_eq!(schedule_intent(8, &jobless), ScheduleIntent::Home);
    }
}
