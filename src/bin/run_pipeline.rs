use aws_config::meta::region::RegionProviderChain;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use reqwest::Client as HttpClient;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn remove_nulls(v: &mut Value) {
    match v {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                if let Some(mut vv) = map.remove(&k) {
                    remove_nulls(&mut vv);
                    match &vv {
                        Value::Null => {}
                        _ => {
                            map.insert(k, vv);
                        }
                    }
                }
            }
        }
        Value::Array(arr) => {
            arr.retain(|x| !x.is_null());
            for x in arr.iter_mut() {
                remove_nulls(x);
            }
        }
        _ => {}
    }
}

fn apply_defaults(obj: &mut Map<String, Value>) {
    if !obj.contains_key("event_type") {
        obj.insert("event_type".to_string(), Value::String("unknown".to_string()));
    }
    if !obj.contains_key("created_at") {
        let now = chrono::Utc::now().to_rfc3339();
        obj.insert("created_at".to_string(), Value::String(now));
    }
    if !obj.contains_key("when") {
        if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
            obj.insert("when".to_string(), Value::Number(serde_json::Number::from(now.as_secs())));
        }
    }
}

async fn ingest_samples(client: &Client, table: &str) -> Result<(), anyhow::Error> {
    let data_dir = Path::new("data/sample");
    if !data_dir.exists() {
        println!("No sample data at {}", data_dir.display());
        return Ok(());
    }
    for entry in fs::read_dir(data_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let filename = path.file_stem().and_then(|s| s.to_str()).unwrap_or("event");
        let content = fs::read_to_string(&path)?;
        // validate
        let _v: Value = serde_json::from_str(&content)?;

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let pk = format!("event#{}", filename);
        let sk = format!("stage#bronze#{}", now);

        client
+            .put_item()
+            .table_name(table)
+            .item("pk", AttributeValue::S(pk.clone()))
+            .item("sk", AttributeValue::S(sk.clone()))
+            .item("payload", AttributeValue::S(content.clone()))
+            .item("when", AttributeValue::N(now.to_string()))
+            .send()
+            .await?;
+
+        println!("ingest: wrote {} {}", pk, sk);
    }
    Ok(())
}

async fn clean_bronze(client: &Client, table: &str) -> Result<(), anyhow::Error> {
    let mut expr_vals = HashMap::new();
    expr_vals.insert(":prefix".to_string(), AttributeValue::S("stage#bronze".to_string()));

    let resp = client
        .scan()
        .table_name(table)
        .filter_expression("begins_with(sk, :prefix)")
        .set_expression_attribute_values(Some(expr_vals))
        .send()
        .await?;

    if let Some(items) = resp.items() {
        for it in items {
            let pk = it.get("pk").and_then(|v| v.as_s().map(|s| s.to_string())).unwrap_or_else(|| "unknown".to_string());
            let payload = it.get("payload").and_then(|v| v.as_s().map(|s| s.to_string()));
            if let Some(s) = payload {
                if let Ok(mut json) = serde_json::from_str::<Value>(&s) {
                    remove_nulls(&mut json);
                    if let Value::Object(ref mut map) = json {
                        apply_defaults(map);
                    }
                    let cleaned = serde_json::to_string(&json)?;
                    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                    let sk_new = format!("stage#bronze_cleaned#{}", now);

                    client
                        .put_item()
                        .table_name(table)
                        .item("pk", AttributeValue::S(pk.clone()))
                        .item("sk", AttributeValue::S(sk_new.clone()))
                        .item("payload_cleaned", AttributeValue::S(cleaned.clone()))
                        .item("when", AttributeValue::N(now.to_string()))
                        .send()
                        .await?;

                    println!("clean: {} -> {}", pk, sk_new);
                }
            }
        }
    }
    Ok(())
}

fn normalize_amount(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

async fn promote_silver(client: &Client, table: &str) -> Result<(), anyhow::Error> {
    let mut expr_vals = HashMap::new();
    expr_vals.insert(":prefix".to_string(), AttributeValue::S("stage#bronze_cleaned".to_string()));

    let resp = client
        .scan()
        .table_name(table)
        .filter_expression("begins_with(sk, :prefix)")
        .set_expression_attribute_values(Some(expr_vals))
        .send()
        .await?;

    if let Some(items) = resp.items() {
        for it in items {
            let pk = it.get("pk").and_then(|v| v.as_s().map(|s| s.to_string())).unwrap_or_else(|| "unknown".to_string());
            let payload = it.get("payload_cleaned").and_then(|v| v.as_s().map(|s| s.to_string()));
            if let Some(s) = payload {
                if let Ok(json_val) = serde_json::from_str::<Value>(&s) {
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
                    silver["original"] = json_val.clone();

                    let cleaned = serde_json::to_string(&silver)?;
                    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                    let sk_new = format!("stage#silver#{}", now);

                    client
                        .put_item()
                        .table_name(table)
                        .item("pk", AttributeValue::S(pk.clone()))
                        .item("sk", AttributeValue::S(sk_new.clone()))
                        .item("payload", AttributeValue::S(cleaned.clone()))
                        .item("when", AttributeValue::N(now.to_string()))
                        .send()
                        .await?;

                    println!("silver: {} -> {}", pk, sk_new);
                }
            }
        }
    }
    Ok(())
}

async fn promote_gold_and_sink(client: &Client, http: &HttpClient, table: &str) -> Result<(), anyhow::Error> {
    let mut expr_vals = HashMap::new();
    expr_vals.insert(":prefix".to_string(), AttributeValue::S("stage#silver".to_string()));

    let resp = client
        .scan()
        .table_name(table)
        .filter_expression("begins_with(sk, :prefix)")
        .set_expression_attribute_values(Some(expr_vals))
        .send()
        .await?;

    let splunk_url = std::env::var("SPLUNK_HEC_URL").ok();
    let splunk_token = std::env::var("SPLUNK_HEC_TOKEN").ok();

    if let Some(items) = resp.items() {
        for it in items {
            let pk = it.get("pk").and_then(|v| v.as_s().map(|s| s.to_string())).unwrap_or_else(|| "unknown".to_string());
            let payload = it.get("payload").and_then(|v| v.as_s().map(|s| s.to_string()));
            if let Some(s) = payload {
                if let Ok(json_val) = serde_json::from_str::<Value>(&s) {
                    // example metric extraction: revenue or event_count
                    let metric = if let Some(amount) = json_val.get("amount").and_then(|v| normalize_amount(v)) {
                        json!({"metric":"revenue","value": amount})
                    } else {
                        json!({"metric":"event_count","value":1})
                    };

                    let gold = json!({
                        "when": json_val.get("when").cloned().unwrap_or(Value::Number(serde_json::Number::from(0u64))),
                        "metric": metric,
                        "original": json_val.clone(),
                    });

                    let cleaned = serde_json::to_string(&gold)?;
                    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                    let sk_new = format!("stage#gold#{}", now);

                    client
                        .put_item()
                        .table_name(table)
                        .item("pk", AttributeValue::S(pk.clone()))
                        .item("sk", AttributeValue::S(sk_new.clone()))
                        .item("payload", AttributeValue::S(cleaned.clone()))
                        .item("when", AttributeValue::N(now.to_string()))
                        .send()
                        .await?;

                    println!("gold: {} -> {}", pk, sk_new);

                    if let (Some(url), Some(token)) = (splunk_url.as_ref(), splunk_token.as_ref()) {
                        let body = json!({"event": gold, "time": now});
                        let res = http
                            .post(format!("{}/services/collector/event", url.trim_end_matches('/')))
                            .bearer_auth(token)
                            .json(&body)
                            .send()
                            .await;
                        match res {
                            Ok(r) => println!("sent to splunk: {} -> {} status {}", pk, sk_new, r.status()),
                            Err(e) => eprintln!("splunk send error for {}: {}", pk, e),
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let region_provider = RegionProviderChain::default_provider().or_else("us-east-1");
    let config = aws_config::from_env().region(region_provider).load().await;
    let client = Client::new(&config);
    let http = HttpClient::builder().build()?;

    let table = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());

    println!("Starting pipeline against table={}", table);

    ingest_samples(&client, &table).await?;
    clean_bronze(&client, &table).await?;
    promote_silver(&client, &table).await?;
    promote_gold_and_sink(&client, &http, &table).await?;

    println!("Pipeline complete");
    Ok(())
}
