use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::sleep;

fn env_or_default_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

pub async fn send_to_splunk_hec(payload: &str) -> Result<(), String> {
    if payload.trim().is_empty() {
        return Err("cannot send empty payload".to_string());
    }

    let hec_url = std::env::var("SPLUNK_HEC_URL").ok();
    let hec_token = std::env::var("SPLUNK_HEC_TOKEN").ok();

    if hec_url.as_deref().unwrap_or_default().is_empty()
        || hec_token.as_deref().unwrap_or_default().is_empty()
    {
        println!(
            "SPLUNK_HEC_URL/SPLUNK_HEC_TOKEN not configured. Skipping HEC send and treating as success."
        );
        return Ok(());
    }

    let hec_url = hec_url.unwrap_or_default();
    let hec_token = hec_token.unwrap_or_default();
    let timeout_secs = env_or_default_u64("SPLUNK_HEC_TIMEOUT_SECS", 5);
    let max_attempts = env_or_default_u64("SPLUNK_HEC_RETRIES", 3).max(1);

    let event_value = serde_json::from_str::<Value>(payload).unwrap_or_else(|_| json!(payload));
    let mut body = json!({
        "event": event_value,
        "source": "dynamodb-prototype",
        "sourcetype": "cloud:audit:medallion"
    });

    if let Ok(index) = std::env::var("SPLUNK_HEC_INDEX") {
        if !index.trim().is_empty() {
            body["index"] = json!(index);
        }
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let mut last_error = String::new();
    for attempt in 1..=max_attempts {
        let response = client
            .post(&hec_url)
            .header("Authorization", format!("Splunk {hec_token}"))
            .json(&body)
            .send()
            .await;

        match response {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                last_error = format!("attempt {attempt}/{max_attempts}: status={status}, body={text}");
            }
            Err(err) => {
                last_error = format!("attempt {attempt}/{max_attempts}: request error: {err}");
            }
        }

        if attempt < max_attempts {
            // Linear backoff keeps retry behavior predictable for a prototype.
            sleep(Duration::from_millis(500 * attempt)).await;
        }
    }

    Err(format!("failed to deliver event to Splunk HEC: {last_error}"))
}
