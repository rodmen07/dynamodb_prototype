use crate::models::{GoldMetric, SilverEvent};

pub fn to_gold(silver: &SilverEvent, now_epoch: u64) -> GoldMetric {
    let risk_score = match silver.severity.as_str() {
        "critical" => 95,
        "high" => 80,
        "medium" => 55,
        _ => 25,
    };

    let metric_bucket = if risk_score >= 80 {
        "high_risk"
    } else if risk_score >= 50 {
        "elevated"
    } else {
        "baseline"
    }
    .to_string();

    GoldMetric {
        event_id: silver.event_id.clone(),
        provider: silver.provider.clone(),
        risk_score,
        metric_bucket,
        generated_at: now_epoch,
    }
}
