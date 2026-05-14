use crate::core::world::World;

pub(crate) fn run(world: &mut World) {
    // Snapshot providers before mutating consumers to avoid order-dependent borrowing behavior.
    let providers: Vec<_> = world
        .power_providers
        .iter()
        .filter_map(|(entity, provider)| {
            world
                .positions
                .get(entity)
                .map(|position| (*position, provider.radius))
        })
        .collect();

    for (entity, consumer) in world.power_consumers.iter_mut() {
        let Some(position) = world.positions.get(entity) else {
            consumer.powered = false;
            continue;
        };

        // Power uses Manhattan distance so coverage is deterministic on the square grid.
        consumer.powered = providers.iter().any(|(provider_position, radius)| {
            position.x.abs_diff(provider_position.x) + position.y.abs_diff(provider_position.y)
                <= *radius
        });
    }
}
