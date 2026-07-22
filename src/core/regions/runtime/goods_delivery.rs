use super::{
    GoodsOrderId, GoodsSupplyAllocationRelease, GoodsSupplyAllocationRequest, OutboundMessage,
    RegionEvent, RegionRuntime,
};
use crate::core::components::TravelerId;
use crate::core::entity::Entity;
use crate::core::regions::coordinator::{RegionRecipients, RoutedRegionEvent};
use crate::core::regions::{ExportAllocationKey, GoodsSupplyGrant};

impl RegionRuntime {
    pub(super) fn handle_process_goods_supply_request(
        &mut self,
        request: GoodsSupplyAllocationRequest,
    ) -> Vec<OutboundMessage> {
        let grant = self.process_goods_supply_request(&request);
        vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
            recipients: RegionRecipients::One(request.request.caller_region),
            event: RegionEvent::ApplyGoodsSupplyGrant { request, grant },
        })]
    }

    pub(super) fn handle_release_goods_supply_allocations(
        &mut self,
        release: GoodsSupplyAllocationRelease,
    ) -> Vec<OutboundMessage> {
        self.goods_supply_allocations
            .release_stale_for_caller(release.caller_region, release.request_id);
        Vec::new()
    }

    pub(super) fn handle_apply_goods_supply_grant(
        &mut self,
        request: GoodsSupplyAllocationRequest,
        grant: GoodsSupplyGrant,
    ) -> Vec<OutboundMessage> {
        self.apply_goods_supply_result(request, grant)
    }

    pub(super) fn handle_apply_goods_delivery(
        &mut self,
        traveler: TravelerId,
        order: GoodsOrderId,
        allocation_key: ExportAllocationKey,
        commercial: Entity,
        units: i32,
    ) -> Vec<OutboundMessage> {
        let applied = commercial == order.commercial
            && self.state.apply_goods_delivery(traveler, order, units);
        if !applied {
            self.state.retarget_goods_truck_home(traveler);
        }
        let event = if applied {
            RegionEvent::ConfirmGoodsDelivery {
                traveler,
                order,
                allocation_key,
                units,
            }
        } else {
            RegionEvent::RejectGoodsDelivery {
                traveler,
                order,
                allocation_key,
                units,
            }
        };
        vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
            recipients: RegionRecipients::One(traveler.entity.region()),
            event,
        })]
    }

    pub(super) fn handle_confirm_goods_delivery(
        &mut self,
        traveler: TravelerId,
        allocation_key: ExportAllocationKey,
        units: i32,
    ) -> Vec<OutboundMessage> {
        // Producer-owned confirmation: release the producer allocation, queue
        // delivery revenue, and clear factory shipment/stock. The commercial
        // region already applied stock and retargeted the parked token.
        self.goods_supply_allocations.release_key(allocation_key);
        self.state.confirm_goods_delivery(traveler, units);
        Vec::new()
    }

    pub(super) fn handle_reject_goods_delivery(
        &mut self,
        traveler: TravelerId,
        order: GoodsOrderId,
        allocation_key: ExportAllocationKey,
        units: i32,
    ) -> Vec<OutboundMessage> {
        // Allocations are producer-owned; this is a no-op in a consumer
        // runtime, but keeps confirm/reject terminal handling uniform.
        self.goods_supply_allocations.release_key(allocation_key);
        if traveler.entity.region() == self.region_id() {
            // Factory side: clear shipment/reserved outbound goods and retarget
            // the local token if the truck is still here.
            self.state.cancel_goods_delivery(traveler, units);
            if order.commercial.region() == self.region_id() {
                // Same-region order: this runtime also owns the consumer-side
                // inbound reservation.
                self.state
                    .reject_goods_delivery_at_host(traveler, order, units);
            }
        } else {
            // Remote consumer side: release inbound reservation and retarget the
            // parked token; factory truth is owned by the producer.
            self.state
                .reject_goods_delivery_at_host(traveler, order, units);
        }
        Vec::new()
    }
}
