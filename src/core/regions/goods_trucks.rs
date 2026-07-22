//! Goods truck fleet, shipment, and delivery helpers for `RegionState`.
//!
//! This module only changes where the `RegionState` impl lives. The region's
//! private `World` remains the source of truth for factory goods, trucks,
//! shipment reservations, and commercial orders.

use std::collections::HashSet;

use super::{
    ExportAllocationKey, GOODS_PER_TRUCK, GOODS_WAREHOUSE_CAPACITY, GoodsSupplyGrant,
    GoodsTruckArrival, RegionRoadNetworkId, RegionState, denied_goods_grant,
};
use crate::core::components::{
    ArrivalAction, BuildingData, FactoryGoodsState, GoodsOrder, GoodsOrderId, PlaceRef, Shipment,
    TravelKind, TravelToken, TravelerId, Truck,
};
use crate::core::entity::Entity;
use crate::core::regional_types::UiRequestId;
use crate::core::regions::runtime::GoodsSupplyRequest;
use crate::core::simulation::ensure_derived_state;
use crate::core::systems::{economy, road_connectivity, travel};
use crate::interface::input::BuildingKind;

impl RegionState {
    // Factory inventory
    fn factory_entities(&self) -> Vec<Entity> {
        let mut factories = self
            .world
            .buildings
            .iter()
            .filter_map(|(entity, building)| {
                (building.kind == BuildingKind::Industrial).then_some(*entity)
            })
            .collect::<Vec<_>>();
        road_connectivity::sort_entities_by_position(&self.world, &mut factories);
        factories
    }

    fn factory_warehouse_capacity(&self, factory: Entity) -> i32 {
        self.world
            .buildings
            .get(&factory)
            .map(|building| GOODS_WAREHOUSE_CAPACITY * i32::from(building.level.max(1)))
            .unwrap_or(0)
    }

    fn factory_goods_mut(&mut self, factory: Entity) -> Option<&mut FactoryGoodsState> {
        self.world
            .buildings
            .get_mut(&factory)
            .and_then(|building| match &mut building.data {
                BuildingData::Industrial { goods, .. } => Some(goods),
                BuildingData::Commercial { .. } | BuildingData::None => None,
            })
    }

    fn factory_available_goods(&self, factory: Entity) -> i32 {
        self.world
            .buildings
            .get(&factory)
            .and_then(|building| match building.data {
                BuildingData::Industrial { goods, .. } => {
                    Some((goods.stored_units - goods.reserved_outbound_units).max(0))
                }
                BuildingData::Commercial { .. } | BuildingData::None => None,
            })
            .unwrap_or(0)
    }

    pub(super) fn factory_goods_units_on_network(&self, network: RegionRoadNetworkId) -> u32 {
        if network.region != self.id {
            return 0;
        }
        self.factory_entities()
            .into_iter()
            .filter(|factory| self.is_effective_factory(*factory))
            .filter(|factory| {
                crate::core::systems::road_network_analysis::access_for(&self.world, *factory)
                    .network_id
                    == Some(network.road_network)
            })
            .map(|factory| self.factory_available_goods(factory).max(0) as u32)
            .sum()
    }

    fn is_effective_factory(&self, factory: Entity) -> bool {
        self.world
            .buildings
            .get(&factory)
            .is_some_and(|building| building.kind == BuildingKind::Industrial)
            && self
                .world
                .power_consumers
                .get(&factory)
                .is_some_and(|consumer| consumer.powered)
            && crate::core::systems::road_network_analysis::access_for(&self.world, factory)
                .network_id
                .is_some()
    }

    // Fleet
    fn reconcile_factory_trucks(&mut self) {
        let factories = self.factory_entities();
        for factory in factories {
            let Some(building) = self.world.buildings.get(&factory) else {
                continue;
            };
            let cargo_capacity = GOODS_PER_TRUCK * i32::from(building.level.max(1));
            let desired = usize::from(
                self.world
                    .building_rules()
                    .industrial_truck_count(building.level),
            );
            for truck in self
                .world
                .trucks
                .values_mut()
                .filter(|truck| truck.factory == factory)
            {
                truck.cargo_capacity = truck
                    .shipment
                    .map(|shipment| cargo_capacity.max(shipment.units))
                    .unwrap_or(cargo_capacity);
            }
            let current = self
                .world
                .trucks
                .values()
                .filter(|truck| truck.factory == factory)
                .count();
            for _ in current..desired {
                let id = self.world.spawn();
                self.world.trucks.insert(
                    id,
                    Truck {
                        id,
                        factory,
                        cargo_capacity,
                        arrival_action: ArrivalAction::ReturnHome,
                        trip_generation: 0,
                        shipment: None,
                    },
                );
            }
            if current > desired {
                let mut excess_idle = self
                    .world
                    .trucks
                    .iter()
                    .filter_map(|(id, truck)| {
                        (truck.factory == factory
                            && truck.shipment.is_none()
                            && !self.world.tokens.contains_key(id))
                        .then_some(*id)
                    })
                    .collect::<Vec<_>>();
                excess_idle.sort_by_key(|id| std::cmp::Reverse(id.0));
                for truck_id in excess_idle.into_iter().take(current - desired) {
                    self.world.trucks.remove(&truck_id);
                    self.world.active_travelers.remove(&truck_id);
                }
            }
        }

        let valid_factories = self.factory_entities().into_iter().collect::<HashSet<_>>();
        self.world.trucks.retain(|id, truck| {
            valid_factories.contains(&truck.factory)
                || truck.shipment.is_some()
                || self.world.tokens.contains_key(id)
        });
    }

    fn first_idle_truck(&self, factory: Entity, units: i32) -> Option<Entity> {
        self.world.trucks.iter().find_map(|(id, truck)| {
            (truck.factory == factory
                && truck.shipment.is_none()
                && !self.world.tokens.contains_key(id)
                && truck.cargo_capacity >= units)
                .then_some(*id)
        })
    }

    // Dispatch
    fn choose_factory_for_shipment(
        &self,
        producer_network: RegionRoadNetworkId,
        commercial: PlaceRef,
        units: i32,
    ) -> Option<Entity> {
        if producer_network.region != self.id {
            return None;
        }
        self.factory_entities()
            .into_iter()
            .filter(|factory| self.is_effective_factory(*factory))
            .filter(|factory| self.factory_available_goods(*factory) >= units)
            .filter(|factory| self.first_idle_truck(*factory, units).is_some())
            .filter_map(|factory| {
                let access =
                    crate::core::systems::road_network_analysis::access_for(&self.world, factory);
                (access.network_id == Some(producer_network.road_network))
                    .then(|| {
                        if commercial.region == self.id {
                            crate::core::systems::road_network_analysis::distance_between_buildings(
                                &self.world,
                                factory,
                                commercial.building,
                            )
                        } else {
                            travel::start_trip_from_building(&self.world, factory, commercial)
                                .map(|_| 0)
                        }
                        .map(|distance| (factory, distance))
                    })
                    .flatten()
            })
            .min_by_key(|(factory, distance)| {
                let position_key = self
                    .world
                    .positions
                    .get(factory)
                    .map(|position| (position.y, position.x, factory.0))
                    .unwrap_or((usize::MAX, usize::MAX, factory.0));
                (*distance, position_key)
            })
            .map(|(factory, _distance)| factory)
    }

    pub fn produce_factory_goods_for_daily_tick(&mut self) {
        ensure_derived_state(&mut self.world, self.id);
        self.reconcile_factory_trucks();
        let factories = self.factory_entities();
        for factory in factories {
            if !self.is_effective_factory(factory) {
                continue;
            }
            let produced = economy::industrial_goods_production(&self.world, factory);
            let capacity = self.factory_warehouse_capacity(factory);
            if let Some(goods) = self.factory_goods_mut(factory) {
                let before = goods.stored_units;
                goods.stored_units = (goods.stored_units + produced).clamp(0, capacity);
                if goods.stored_units != before {
                    self.world.mark_hints_dirty();
                    self.world.mark_goods_exports_dirty();
                }
            }
        }
    }

    pub(crate) fn dispatch_goods_shipment(
        &mut self,
        request: &GoodsSupplyRequest,
        producer_network: RegionRoadNetworkId,
        allocation_key: ExportAllocationKey,
    ) -> GoodsSupplyGrant {
        ensure_derived_state(&mut self.world, self.id);
        self.reconcile_factory_trucks();
        let units_i32 = request.units as i32;
        let commercial_place = PlaceRef {
            region: request.caller_region,
            building: request.commercial,
        };
        let order = GoodsOrderId {
            commercial: request.commercial,
            request_id: request.request_id,
            token: request.token,
        };
        let Some(factory) =
            self.choose_factory_for_shipment(producer_network, commercial_place, units_i32)
        else {
            return denied_goods_grant(request.token);
        };
        let Some(truck_id) = self.first_idle_truck(factory, units_i32) else {
            return denied_goods_grant(request.token);
        };
        let Some(state) = travel::start_trip_from_building(&self.world, factory, commercial_place)
        else {
            return denied_goods_grant(request.token);
        };

        if let Some(goods) = self.factory_goods_mut(factory) {
            goods.reserved_outbound_units += units_i32;
        }
        let shipment = Shipment {
            order,
            allocation_key,
            producer_network,
            commercial: commercial_place,
            units: units_i32,
        };
        let Some(truck) = self.world.trucks.get_mut(&truck_id) else {
            return denied_goods_grant(request.token);
        };
        truck.trip_generation += 1;
        truck.arrival_action = ArrivalAction::DeliverGoods;
        truck.shipment = Some(shipment);
        let traveler = TravelerId {
            entity: truck_id,
            generation: truck.trip_generation,
        };
        self.world.active_travelers.insert(truck_id);
        self.world.tokens.insert(
            truck_id,
            TravelToken {
                state,
                home: PlaceRef {
                    region: self.id,
                    building: factory,
                },
                kind: TravelKind::Shipment {
                    shipment,
                    returning: false,
                },
                trip_gen: traveler.generation,
            },
        );
        GoodsSupplyGrant {
            token: request.token,
            granted: true,
            source_region: Some(self.id),
            units: request.units,
        }
    }

    // Return / rollback
    pub(super) fn accepts_truck_return(&self, traveler: TravelerId) -> bool {
        self.world
            .trucks
            .get(&traveler.entity)
            .is_some_and(|truck| truck.trip_generation == traveler.generation)
            && self.world.active_travelers.contains(&traveler.entity)
    }

    fn apply_truck_return(&mut self, traveler: TravelerId) {
        if !self.accepts_truck_return(traveler) {
            return;
        }
        self.world.active_travelers.remove(&traveler.entity);
        if let Some(truck) = self.world.trucks.get_mut(&traveler.entity) {
            truck.arrival_action = ArrivalAction::ReturnHome;
        }
    }

    pub(super) fn apply_truck_rollback(&mut self, traveler: TravelerId) {
        let Some((factory, shipment)) = self.world.trucks.get(&traveler.entity).and_then(|truck| {
            (truck.trip_generation == traveler.generation)
                .then(|| truck.shipment.map(|shipment| (truck.factory, shipment)))
                .flatten()
        }) else {
            self.apply_truck_return(traveler);
            return;
        };
        self.clear_goods_shipment_reservation(traveler.entity, factory, shipment);
        self.world.active_travelers.remove(&traveler.entity);
    }

    // Delivery
    pub(crate) fn goods_truck_shipment(&self, traveler: TravelerId) -> Option<Shipment> {
        self.world.trucks.get(&traveler.entity).and_then(|truck| {
            (truck.trip_generation == traveler.generation)
                .then_some(truck.shipment)
                .flatten()
        })
    }

    pub(crate) fn record_goods_order(
        &mut self,
        request_id: UiRequestId,
        token: u32,
        commercial: Entity,
        units: u32,
    ) {
        let id = GoodsOrderId {
            commercial,
            request_id,
            token,
        };
        let units = units as i32;
        self.world.goods_orders.insert(
            id,
            GoodsOrder {
                id,
                commercial,
                requested_units: units,
                inbound_reserved_units: units,
                remaining_units: 0,
            },
        );
    }

    pub(crate) fn validate_goods_truck_arrival(
        &mut self,
        traveler: TravelerId,
        destination: PlaceRef,
    ) -> Option<GoodsTruckArrival> {
        let (factory, shipment) = {
            let truck = self.world.trucks.get_mut(&traveler.entity)?;
            if truck.trip_generation != traveler.generation
                || truck.arrival_action != ArrivalAction::DeliverGoods
            {
                return None;
            }
            let shipment = truck.shipment?;
            if shipment.commercial != destination {
                return None;
            }
            truck.arrival_action = ArrivalAction::ReturnHome;
            (truck.factory, shipment)
        };
        if !self
            .world
            .buildings
            .get(&factory)
            .is_some_and(|building| building.kind == BuildingKind::Industrial)
        {
            self.cancel_goods_delivery(traveler, shipment.units);
            return Some(GoodsTruckArrival::Reject(shipment));
        }
        Some(GoodsTruckArrival::Deliver(shipment))
    }

    pub(crate) fn apply_goods_delivery(
        &mut self,
        traveler: TravelerId,
        order: GoodsOrderId,
        units: i32,
    ) -> bool {
        if !self
            .world
            .buildings
            .get(&order.commercial)
            .is_some_and(|building| building.kind == BuildingKind::Commercial)
        {
            self.world.goods_orders.remove(&order);
            return false;
        }
        let remove_order = {
            let Some(stored_order) = self.world.goods_orders.get_mut(&order) else {
                return false;
            };
            if stored_order.commercial != order.commercial
                || stored_order.inbound_reserved_units < units
                || units <= 0
            {
                return false;
            }
            stored_order.inbound_reserved_units -= units;
            stored_order.inbound_reserved_units == 0 && stored_order.remaining_units == 0
        };
        economy::add_commercial_goods(&mut self.world, order.commercial, units);
        self.world.mark_hints_dirty();
        self.world.mark_goods_exports_dirty();
        if remove_order {
            self.world.goods_orders.remove(&order);
        }
        self.retarget_goods_truck_home(traveler);
        true
    }

    pub(crate) fn reject_goods_delivery_at_host(
        &mut self,
        traveler: TravelerId,
        order: GoodsOrderId,
        units: i32,
    ) {
        let remove_order = self
            .world
            .goods_orders
            .get_mut(&order)
            .map(|stored_order| {
                if stored_order.inbound_reserved_units < units || units <= 0 {
                    return false;
                }
                stored_order.inbound_reserved_units =
                    stored_order.inbound_reserved_units.saturating_sub(units);
                stored_order.inbound_reserved_units == 0 && stored_order.remaining_units == 0
            })
            .unwrap_or(false);
        if remove_order {
            self.world.goods_orders.remove(&order);
        }
        self.retarget_goods_truck_home(traveler);
    }

    pub(super) fn clear_goods_orders_for_commercial(&mut self, commercial: Entity) {
        self.world
            .goods_orders
            .retain(|_, order| order.commercial != commercial);
    }

    pub(crate) fn retarget_goods_truck_home(&mut self, traveler: TravelerId) {
        if let Some(token) = self.world.tokens.get_mut(&traveler.entity)
            && let TravelKind::Shipment { returning, .. } = &mut token.kind
        {
            *returning = true;
            token.state.destination = None;
        }
    }

    pub(crate) fn confirm_goods_delivery(&mut self, traveler: TravelerId, units: i32) {
        let Some((factory, shipment)) = self.world.trucks.get(&traveler.entity).and_then(|truck| {
            (truck.trip_generation == traveler.generation)
                .then(|| truck.shipment.map(|shipment| (truck.factory, shipment)))
                .flatten()
        }) else {
            return;
        };
        if shipment.units != units {
            return;
        }
        economy::record_local_goods_delivery_revenue(
            &mut self.world,
            factory,
            shipment.commercial.building,
            units,
        );
        if let Some(goods) = self.factory_goods_mut(factory) {
            goods.stored_units = goods.stored_units.saturating_sub(units);
            goods.reserved_outbound_units = goods.reserved_outbound_units.saturating_sub(units);
        }
        if let Some(truck) = self.world.trucks.get_mut(&traveler.entity) {
            truck.shipment = None;
            truck.arrival_action = ArrivalAction::ReturnHome;
        }
    }

    pub(crate) fn cancel_goods_delivery(&mut self, traveler: TravelerId, units: i32) {
        let Some((factory, shipment)) = self.world.trucks.get(&traveler.entity).and_then(|truck| {
            (truck.trip_generation == traveler.generation)
                .then(|| truck.shipment.map(|shipment| (truck.factory, shipment)))
                .flatten()
        }) else {
            return;
        };
        if shipment.units != units {
            return;
        }
        if let Some(goods) = self.factory_goods_mut(factory) {
            goods.reserved_outbound_units = goods.reserved_outbound_units.saturating_sub(units);
        }
        self.retarget_goods_truck_home(traveler);
        if let Some(truck) = self.world.trucks.get_mut(&traveler.entity) {
            truck.shipment = None;
            truck.arrival_action = ArrivalAction::ReturnHome;
        }
    }

    // Cleanup / recovery
    fn clear_goods_shipment_reservation(
        &mut self,
        truck_id: Entity,
        factory: Entity,
        shipment: Shipment,
    ) {
        if let Some(goods) = self.factory_goods_mut(factory) {
            goods.reserved_outbound_units =
                goods.reserved_outbound_units.saturating_sub(shipment.units);
        }
        let remove_order = self
            .world
            .goods_orders
            .get_mut(&shipment.order)
            .map(|order| {
                order.inbound_reserved_units =
                    order.inbound_reserved_units.saturating_sub(shipment.units);
                order.inbound_reserved_units == 0 && order.remaining_units == 0
            })
            .unwrap_or(false);
        if remove_order {
            self.world.goods_orders.remove(&shipment.order);
        }
        if let Some(truck) = self.world.trucks.get_mut(&truck_id) {
            truck.shipment = None;
            truck.arrival_action = ArrivalAction::ReturnHome;
        }
    }

    pub(super) fn resume_persisted_goods_shipments(&mut self) {
        let truck_ids = self.world.trucks.keys().copied().collect::<Vec<_>>();
        for truck_id in truck_ids {
            if self.world.tokens.contains_key(&truck_id)
                || self.world.active_travelers.contains(&truck_id)
            {
                continue;
            }
            let Some((factory, shipment)) = self
                .world
                .trucks
                .get(&truck_id)
                .and_then(|truck| truck.shipment.map(|shipment| (truck.factory, shipment)))
            else {
                continue;
            };
            let Some(state) =
                travel::start_trip_from_building(&self.world, factory, shipment.commercial)
            else {
                if shipment.commercial.region == self.id {
                    self.clear_goods_shipment_reservation(truck_id, factory, shipment);
                }
                continue;
            };
            let Some(truck) = self.world.trucks.get_mut(&truck_id) else {
                continue;
            };
            truck.trip_generation = truck.trip_generation.saturating_add(1);
            truck.arrival_action = ArrivalAction::DeliverGoods;
            let traveler = TravelerId {
                entity: truck_id,
                generation: truck.trip_generation,
            };
            self.world.active_travelers.insert(truck_id);
            self.world.tokens.insert(
                truck_id,
                TravelToken {
                    state,
                    home: PlaceRef {
                        region: self.id,
                        building: factory,
                    },
                    kind: TravelKind::Shipment {
                        shipment,
                        returning: false,
                    },
                    trip_gen: traveler.generation,
                },
            );
        }
    }
}
