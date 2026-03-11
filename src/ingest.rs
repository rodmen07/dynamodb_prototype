use crate::models::CloudAuditEvent;

pub fn load_event_from_env(now_epoch: u64) -> CloudAuditEvent {
    let event_id = std::env::var("EVENT_ID").unwrap_or_else(|_| "event-123".to_string());
    let payload = std::env::var("EVENT_PAYLOAD").unwrap_or_else(|_| {
        "{\"provider\":\"aws\",\"event_name\":\"ConsoleLogin\",\"actor\":\"user@example.com\",\"source_ip\":\"203.0.113.10\",\"resource\":\"iam:user/example\",\"severity\":\"medium\"}".to_string()
    });

    let parsed = serde_json::from_str::<serde_json::Value>(&payload).ok();
    let get = |field: &str, default: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(field))
            .and_then(|v| v.as_str())
            .unwrap_or(default)
            .to_string()
    };

    CloudAuditEvent {
        provider: get("provider", "aws"),
        event_id,
        event_name: get("event_name", "UnknownEvent"),
        actor: get("actor", "unknown"),
        source_ip: get("source_ip", "0.0.0.0"),
        resource: get("resource", "unknown"),
        event_time_epoch: now_epoch,
        severity: get("severity", "low"),
        raw_payload: payload,
    }
}
