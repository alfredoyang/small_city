//! Happiness system deriving city happiness from citizens when present, otherwise city effects.

use crate::core::systems::citizens;
use crate::core::world::World;

pub(crate) fn run(world: &mut World) {
    if let Some(average_happiness) = citizens::average_happiness(world) {
        world.stats.happiness = (average_happiness - world.stats.unemployment * 2).clamp(0, 100);
        return;
    }

    let park_bonus: i32 = world
        .happiness_effects
        .values()
        .map(|effect| effect.amount)
        .sum();
    world.stats.happiness =
        (50 + park_bonus - world.stats.pollution - world.stats.unemployment * 2).clamp(0, 100);
}

#[cfg(test)]
mod tests {
    use crate::core::components::HappinessEffect;
    use crate::core::systems::happiness;
    use crate::core::world::World;

    #[test]
    fn happiness_is_clamped_between_zero_and_one_hundred() {
        let mut low = World::new(1, 1);
        low.stats.pollution = 80;
        low.stats.unemployment = 20;
        happiness::run(&mut low);
        assert_eq!(low.stats.happiness, 0);

        let mut high = World::new(1, 1);
        let entity = high.spawn();
        high.attach_happiness_effect(entity, HappinessEffect { amount: 90 });
        happiness::run(&mut high);
        assert_eq!(high.stats.happiness, 100);
    }
}
