use crate::models::{BronzeRecord, CloudAuditEvent};

pub fn to_bronze(event: &CloudAuditEvent, now_epoch: u64) -> BronzeRecord {
    BronzeRecord {
        stage: "bronze".to_string(),
        event_id: event.event_id.clone(),
        provider: event.provider.clone(),
        received_at: now_epoch,
        raw_payload: event.raw_payload.clone(),
    }
}
