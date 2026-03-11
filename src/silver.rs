use crate::models::{CloudAuditEvent, SilverEvent};

pub fn to_silver(event: &CloudAuditEvent, now_epoch: u64) -> SilverEvent {
    SilverEvent {
        provider: event.provider.to_lowercase(),
        event_id: event.event_id.clone(),
        action: event.event_name.to_lowercase(),
        actor: event.actor.clone(),
        source_ip: event.source_ip.clone(),
        resource: event.resource.clone(),
        event_time_epoch: event.event_time_epoch,
        severity: event.severity.to_lowercase(),
        normalized_at: now_epoch,
    }
}
