use aws_config::meta::region::RegionProviderChain;
use aws_sdk_dynamodb::{types::AttributeValue, Client};
use serde_json::{Map, Value};
use dynamodb_prototype::processing::{remove_nulls, apply_defaults};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

// cleaning and defaults are provided by `processing` lib

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // scanning bronze items, cleaning, and writing cleaned bronze items
    let region_provider = RegionProviderChain::default_provider().or_else("us-east-1");
    let config = aws_config::from_env().region(region_provider).load().await;
    let client = Client::new(&config);

    let table = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());

    // scan for raw bronze items: sk begins_with "stage#bronze"
    let mut expr_vals = HashMap::new();
    expr_vals.insert(":prefix".to_string(), AttributeValue::S("stage#bronze".to_string()));

    let resp = client
        .scan()
        .table_name(&table)
        .filter_expression("begins_with(sk, :prefix)")
        .set_expression_attribute_values(Some(expr_vals))
        .send()
        .await?;

    let items = resp.items();
    if items.is_empty() {
        println!("No bronze items found");
    } else {
        for it in items {
            // skip already-cleaned markers by checking sk value pattern; keep processing generic
            let pk = it.get("pk").and_then(|v| v.as_s().ok().map(|s| s.to_string())).unwrap_or_else(|| "unknown".to_string());
            let payload = it.get("payload").and_then(|v| v.as_s().ok().map(|s| s.to_string()));

            if let Some(mut s) = payload {
                match serde_json::from_str::<Value>(&s) {
                    Ok(mut json) => {
                        remove_nulls(&mut json);
                        if let Value::Object(ref mut map) = json {
                            apply_defaults(map);
                        }

                        let cleaned = serde_json::to_string(&json)?;
                        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                        let sk_new = format!("stage#bronze_cleaned#{}", now);

                        client
                            .put_item()
                            .table_name(&table)
                            .item("pk", AttributeValue::S(pk.clone()))
                            .item("sk", AttributeValue::S(sk_new.clone()))
                            .item("payload_cleaned", AttributeValue::S(cleaned.clone()))
                            .item("when", AttributeValue::N(now.to_string()))
                            .send()
                            .await?;

                        println!("Cleaned bronze -> {} {}", pk, sk_new);
                    }
                    Err(e) => {
                        eprintln!("Invalid JSON for {}: {}", pk, e);
                    }
                }
            } else {
                eprintln!("No payload for {}", pk);
            }
        }
    }

    Ok(())
}
