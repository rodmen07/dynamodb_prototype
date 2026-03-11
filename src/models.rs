use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudAuditEvent {
    pub provider: String,
    pub event_id: String,
    pub event_name: String,
    pub actor: String,
    pub source_ip: String,
    pub resource: String,
    pub event_time_epoch: u64,
    pub severity: String,
    pub raw_payload: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BronzeRecord {
    pub stage: String,
    pub event_id: String,
    pub provider: String,
    pub received_at: u64,
    pub raw_payload: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SilverEvent {
    pub provider: String,
    pub event_id: String,
    pub action: String,
    pub actor: String,
    pub source_ip: String,
    pub resource: String,
    pub event_time_epoch: u64,
    pub severity: String,
    pub normalized_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldMetric {
    pub event_id: String,
    pub provider: String,
    pub risk_score: u8,
    pub metric_bucket: String,
    pub generated_at: u64,
}
