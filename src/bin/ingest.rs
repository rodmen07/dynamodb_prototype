use aws_config::meta::region::RegionProviderChain;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let region_provider = RegionProviderChain::default_provider().or_else("us-east-1");
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;
    let client = Client::new(&config);

    let table = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());
    let data_dir = Path::new("data/sample");

    if !data_dir.exists() {
        eprintln!("No sample data found at {}", data_dir.display());
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
        // validate JSON
        let _v: Value = serde_json::from_str(&content)?;

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let pk = format!("event#{}", filename);
        let sk = format!("stage#bronze#{}", now);

        let req = client
            .put_item()
            .table_name(&table)
            .item("pk", AttributeValue::S(pk.clone()))
            .item("sk", AttributeValue::S(sk.clone()))
            .item("payload", AttributeValue::S(content.clone()))
            .item("when", AttributeValue::N(now.to_string()));

        match req.send().await {
            Ok(_) => println!("Wrote bronze record: {} {}", pk, sk),
            Err(e) => eprintln!("Dynamo put error for {}: {}", pk, e),
        }
    }

    Ok(())
}
