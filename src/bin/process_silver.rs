use aws_config::meta::region::RegionProviderChain;
use aws_sdk_dynamodb::{types::AttributeValue, Client};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

fn normalize_amount(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let region_provider = RegionProviderChain::default_provider().or_else("us-east-1");
    let config = aws_config::from_env().region(region_provider).load().await;
    let client = Client::new(&config);

    let table = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());

    let mut expr_vals = HashMap::new();
    expr_vals.insert(":prefix".to_string(), AttributeValue::S("stage#bronze_cleaned".to_string()));

    let resp = client
        .scan()
        .table_name(&table)
        .filter_expression("begins_with(sk, :prefix)")
        .set_expression_attribute_values(Some(expr_vals))
        .send()
        .await?;

    if let Some(items) = resp.items() {
        for it in items {
            let pk = it.get("pk").and_then(|v| v.as_s().map(|s| s.to_string())).unwrap_or_else(|| "unknown".to_string());
            let payload = it.get("payload_cleaned").and_then(|v| v.as_s().map(|s| s.to_string()));

            if let Some(s) = payload {
                match serde_json::from_str::<Value>(&s) {
                    Ok(json_val) => {
                        let event_type = json_val.get("event_type").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                        let user_id = json_val.get("user_id").and_then(|v| v.as_str()).map(|s| s.to_string());
                        let when = json_val.get("when").and_then(|v| v.as_u64()).unwrap_or_else(|| {
                            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
                        });

                        let amount = json_val.get("amount").and_then(|v| normalize_amount(v));

                        let mut silver = json!({
                            "event_type": event_type,
                            "when": when,
                        });
                        if let Some(uid) = user_id {
                            silver["user_id"] = Value::String(uid);
                        }
                        if let Some(a) = amount {
                            silver["amount"] = Value::Number(serde_json::Number::from_f64(a).unwrap());
                        }

                        // attach original payload as context
                        silver["original"] = json_val.clone();

                        let cleaned = serde_json::to_string(&silver)?;
                        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                        let sk_new = format!("stage#silver#{}", now);

                        let put = client
                            .put_item()
                            .table_name(&table)
                            .item("pk", AttributeValue::S(pk.clone()))
                            .item("sk", AttributeValue::S(sk_new.clone()))
                            .item("payload", AttributeValue::S(cleaned.clone()))
                            .item("when", AttributeValue::N(now.to_string()));

                        match put.send().await {
                            Ok(_) => println!("Promoted to silver -> {} {}", pk, sk_new),
                            Err(e) => eprintln!("Failed silver write for {}: {}", pk, e),
                        }
                    }
                    Err(e) => eprintln!("Bad cleaned payload for {}: {}", pk, e),
                }
            } else {
                eprintln!("No cleaned payload for {}", pk);
            }
        }
    } else {
        println!("No bronze_cleaned items found");
    }

    Ok(())
}
