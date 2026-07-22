use super::{
    OutboundMessage, PowerExportAllocationRelease, PowerExportAllocationRequest,
    PowerExportRequest, RegionEvent, RegionRuntime,
};
use crate::core::regional_types::UiRequestId;
use crate::core::regions::coordinator::{RegionRecipients, RoutedRegionEvent};
use crate::core::regions::{PendingPowerDemand, PowerExportGrant, RegionId, RegionRoadNetworkId};

impl RegionRuntime {
    pub(super) fn handle_settle_power_imports(
        &mut self,
        request_id: UiRequestId,
    ) -> Vec<OutboundMessage> {
        let mut outbound = self.start_power_import_settlement(request_id);
        outbound.push(OutboundMessage::PowerImportsSettled {
            request_id,
            region_id: self.region_id(),
        });
        outbound
    }

    pub(super) fn handle_process_power_export_request(
        &mut self,
        request: PowerExportAllocationRequest,
    ) -> Vec<OutboundMessage> {
        let grant = self.process_power_export_request(&request);
        vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
            recipients: RegionRecipients::One(request.request.caller_region),
            event: RegionEvent::ApplyPowerExportGrant { request, grant },
        })]
    }

    pub(super) fn handle_release_power_export_allocations(
        &mut self,
        release: PowerExportAllocationRelease,
    ) -> Vec<OutboundMessage> {
        self.power_export_allocations
            .release_stale_for_caller(release.caller_region, release.request_id);
        Vec::new()
    }

    pub(super) fn handle_apply_power_export_grant(
        &mut self,
        request: PowerExportAllocationRequest,
        grant: PowerExportGrant,
    ) -> Vec<OutboundMessage> {
        self.apply_power_export_result(request, grant)
    }

    pub(super) fn handle_power_capacity_recheck(
        &mut self,
        request_id: UiRequestId,
    ) -> Vec<OutboundMessage> {
        self.power_capacity_recheck(request_id)
    }

    /// Time-neutral load-time re-negotiation: reuses `release_and_request_power`
    /// as a plain fire-and-forget call, same as a dirty tick's power phase.
    pub(super) fn start_power_import_settlement(
        &mut self,
        request_id: UiRequestId,
    ) -> Vec<OutboundMessage> {
        let demands = self.state.power_import_settlement_demands();
        self.release_and_request_power(request_id, &demands)
    }

    /// Release this caller's previous-generation power reservations, then
    /// request the current demand batch. Fire-and-forget: stamps
    /// `current_power_request_id` so a later reply can tell "my current
    /// batch" from "a superseded one" (see `apply_power_export_grant`), and
    /// returns immediately without waiting for any reply.
    ///
    /// Shared by a dirty tick's power phase and load-time import settlement
    /// (`start_power_import_settlement`) — both just need "release what I
    /// held, request what I need now," never mind how the demand was
    /// collected.
    pub(super) fn release_and_request_power(
        &mut self,
        request_id: UiRequestId,
        demands: &[PendingPowerDemand],
    ) -> Vec<OutboundMessage> {
        self.current_power_request_id = request_id;
        let producer_regions = std::mem::take(&mut self.power_export_producers);
        let release = PowerExportAllocationRelease {
            caller_region: self.region_id(),
            request_id,
            producer_regions,
        };
        let mut outbound = self.power_release_routes(release);
        for demand in demands {
            outbound.extend(self.begin_power_export(PowerExportRequest {
                request_id,
                caller_region: self.region_id(),
                caller_network: demand.caller_network,
                token: demand.token,
                demand: demand.demand,
                consumer: demand.consumer,
            }));
        }
        outbound
    }

    pub(super) fn begin_power_export(
        &mut self,
        request: PowerExportRequest,
    ) -> Vec<OutboundMessage> {
        let candidates = self.power_candidates(request.caller_network);
        let attempt = PowerExportAllocationRequest {
            request,
            candidates,
            candidate_index: 0,
        };
        let Some(network) = attempt.candidates.first() else {
            let token = attempt.request.token;
            return self.apply_power_export_result(
                attempt,
                PowerExportGrant {
                    token,
                    granted: false,
                    source_region: None,
                },
            );
        };
        vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
            recipients: RegionRecipients::One(network.region),
            event: RegionEvent::ProcessPowerExportRequest(attempt),
        })]
    }

    pub(super) fn power_candidates(&self, caller: RegionRoadNetworkId) -> Vec<RegionRoadNetworkId> {
        let Some(discovery) = self.discovery.as_ref() else {
            return Vec::new();
        };
        discovery
            .component_of(caller)
            .unwrap_or(&[])
            .iter()
            .copied()
            .filter(|network| network.region != self.region_id())
            .filter(|network| {
                discovery
                    .availability_hints
                    .iter()
                    .any(|hint| hint.network == *network && hint.has_spare_power)
            })
            .collect()
    }

    pub(super) fn power_release_routes(
        &self,
        release: PowerExportAllocationRelease,
    ) -> Vec<OutboundMessage> {
        release
            .producer_regions
            .iter()
            .copied()
            .map(|region| {
                OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(region),
                    event: RegionEvent::ReleasePowerExportAllocations(release.clone()),
                })
            })
            .collect()
    }

    /// Retire-tickstate, P-b: the eager nudge's handler. Time-neutral —
    /// unlike a normal tick, a nudge must not advance the game clock as a
    /// side effect, so it collects fresh demand via
    /// `RegionState::power_demand_recheck` (mirrors `power::run` directly,
    /// not `begin_tick_power_phase`) instead of
    /// `begin_tick_power_demand_phase`. Reuses the same
    /// `release_and_request_power` helper as a normal dirty tick — fire the
    /// release/request, don't wait.
    pub(super) fn power_capacity_recheck(
        &mut self,
        request_id: UiRequestId,
    ) -> Vec<OutboundMessage> {
        let demands = self.state.power_demand_recheck();
        self.release_and_request_power(request_id, &demands)
    }

    pub(super) fn process_power_export_request(
        &mut self,
        request: &PowerExportAllocationRequest,
    ) -> PowerExportGrant {
        let Some(producer_network) = request.candidates.get(request.candidate_index).copied()
        else {
            return PowerExportGrant {
                token: request.request.token,
                granted: false,
                source_region: None,
            };
        };
        let allocation_key = super::export_allocation_key(&request.request);
        // TODO(CR2 lifecycle): reservations clear when the caller starts a new tick
        // generation. Add explicit cleanup when caller regions are removed,
        // reassigned, or intentionally stop ticking. Not reachable single-worker;
        // tracked in docs/regional-multi-worker-plan.md (M6).
        self.power_export_allocations
            .release_stale_for_caller(request.request.caller_region, request.request.request_id);
        let active_export_allocations: i32 = self
            .power_export_allocations
            .reserved_units_excluding(allocation_key, producer_network)
            .sum();
        // Producer-owned export capacity is authoritative here:
        // local remaining capacity minus active transient export allocations.
        let remaining = self
            .state
            .power_network_remaining_capacity(producer_network)
            .saturating_sub(active_export_allocations);

        if remaining < request.request.demand {
            return PowerExportGrant {
                token: request.request.token,
                granted: false,
                source_region: None,
            };
        }

        self.power_export_allocations.upsert(
            allocation_key,
            producer_network,
            request.request.demand,
            request.request.request_id,
        );

        PowerExportGrant {
            token: request.request.token,
            granted: true,
            source_region: Some(self.region_id()),
        }
    }

    /// Retire-tickstate, P-a: no continuation to consult — the reply carries
    /// the request it answers. One staleness check against
    /// `current_power_request_id` tells "my current batch" (apply it) from
    /// "a superseded one" (drop it, but see below).
    pub(super) fn apply_power_export_grant(
        &mut self,
        request: PowerExportRequest,
        grant: PowerExportGrant,
    ) -> Vec<OutboundMessage> {
        // The producer reserved capacity as soon as it emitted a granted reply.
        // Remember that producer even if this caller later ignores the grant
        // because its local demand disappeared or was already powered; the next
        // release must still reach the producer and clear that allocation.
        self.remember_power_export_producer(&grant);
        if request.request_id != self.current_power_request_id {
            // Caught in review: a superseded batch's release already fired
            // (`release_and_request_power` stamped a newer generation and
            // released this producer at that time) -- UNLESS this exact
            // grant arrived after that release, in which case the producer
            // reserved capacity no future release will ever target (this
            // caller has moved on and won't repeat an old generation). Send
            // one targeted release, stamped with the CURRENT generation, so
            // the producer's `release_stale_for_caller` drops this stale
            // reservation instead of holding it forever.
            return Self::release_stale_granted_power(
                self.region_id(),
                self.current_power_request_id,
                &grant,
            );
        }
        let demand = PendingPowerDemand {
            token: request.token,
            consumer: request.consumer,
            demand: request.demand,
            caller_network: request.caller_network,
        };
        self.state.apply_power_export_grant(demand, grant);
        Vec::new()
    }

    pub(super) fn apply_power_export_result(
        &mut self,
        mut attempt: PowerExportAllocationRequest,
        grant: PowerExportGrant,
    ) -> Vec<OutboundMessage> {
        if grant.granted || attempt.request.request_id != self.current_power_request_id {
            return self.apply_power_export_grant(attempt.request, grant);
        }
        attempt.candidate_index += 1;
        let Some(network) = attempt.candidates.get(attempt.candidate_index) else {
            return self.apply_power_export_grant(attempt.request, grant);
        };
        vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
            recipients: RegionRecipients::One(network.region),
            event: RegionEvent::ProcessPowerExportRequest(attempt),
        })]
    }

    /// A stale but *granted* reply reserved producer capacity that no future
    /// release is guaranteed to reach (this caller has already moved past
    /// that generation). Release it now instead of leaving it stuck. A
    /// stale denial reserved nothing, so it needs no release.
    pub(super) fn release_stale_granted_power(
        caller_region: RegionId,
        current_request_id: UiRequestId,
        grant: &PowerExportGrant,
    ) -> Vec<OutboundMessage> {
        match grant.source_region {
            Some(producer) if grant.granted => {
                let release = PowerExportAllocationRelease {
                    caller_region,
                    request_id: current_request_id,
                    producer_regions: vec![producer],
                };
                vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(producer),
                    event: RegionEvent::ReleasePowerExportAllocations(release),
                })]
            }
            _ => Vec::new(),
        }
    }
}
