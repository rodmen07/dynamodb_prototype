// DynamoDB idempotency prototype with an initial medallion pipeline flow.
//
// The pipeline keeps exactly-once ingestion and writes stage records:
// - Bronze: raw event payload
// - Silver: normalized/enriched event
// - Gold: derived metrics for reporting/alerts

mod bronze;
mod gold;
mod ingest;
mod models;
mod sink;
mod silver;

use aws_config::{meta::region::RegionProviderChain, BehaviorVersion};
use aws_sdk_dynamodb::{error::SdkError, types::AttributeValue, Client};
use serde::Serialize;
use std::collections::HashMap;
use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};

async fn dynamodb_init() -> Client {
    let region_provider = RegionProviderChain::default_provider().or_else("us-east-1");
    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;
    Client::new(&config)
}

async fn create_item(
    client: &Client,
    table_name: &str,
    item: HashMap<String, AttributeValue>,
) -> Result<(), aws_sdk_dynamodb::Error> {
    client
        .put_item()
        .table_name(table_name)
        .set_item(Some(item))
        .send()
        .await?;
    Ok(())
}

async fn read_item(
    client: &Client,
    table_name: &str,
    key: HashMap<String, AttributeValue>,
) -> Result<Option<HashMap<String, AttributeValue>>, aws_sdk_dynamodb::Error> {
    let response = client
        .get_item()
        .table_name(table_name)
        .set_key(Some(key))
        .send()
        .await?;
    Ok(response.item)
}

async fn update_item(
    client: &Client,
    table_name: &str,
    key: HashMap<String, AttributeValue>,
    update_expression: &str,
    expression_attribute_names: HashMap<String, String>,
    expression_attribute_values: HashMap<String, AttributeValue>,
) -> Result<(), aws_sdk_dynamodb::Error> {
    client
        .update_item()
        .table_name(table_name)
        .set_key(Some(key))
        .update_expression(update_expression)
        .set_expression_attribute_names(Some(expression_attribute_names))
        .set_expression_attribute_values(Some(expression_attribute_values))
        .send()
        .await?;
    Ok(())
}

async fn delete_item(
    client: &Client,
    table_name: &str,
    key: HashMap<String, AttributeValue>,
) -> Result<(), aws_sdk_dynamodb::Error> {
    client
        .delete_item()
        .table_name(table_name)
        .set_key(Some(key))
        .send()
        .await?;
    Ok(())
}

fn handle_error(error: &aws_sdk_dynamodb::Error) {
    eprintln!("DynamoDB operation failed: {error}");
    let mut source = error.source();
    while let Some(cause) = source {
        eprintln!("Caused by: {cause}");
        source = cause.source();
    }
}

fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn build_idempotency_key(event_id: &str) -> HashMap<String, AttributeValue> {
    let mut key = HashMap::new();
    key.insert("pk".to_string(), AttributeValue::S(event_id.to_string()));
    key.insert("sk".to_string(), AttributeValue::S("state".to_string()));
    key
}

fn build_stage_key(event_id: &str, stage: &str) -> HashMap<String, AttributeValue> {
    let mut key = HashMap::new();
    key.insert("pk".to_string(), AttributeValue::S(event_id.to_string()));
    key.insert("sk".to_string(), AttributeValue::S(format!("stage#{stage}")));
    key
}

async fn try_acquire_idempotency(
    client: &Client,
    table_name: &str,
    event_id: &str,
    ttl_seconds: u64,
) -> Result<bool, aws_sdk_dynamodb::Error> {
    let now = epoch_seconds();
    let mut item = build_idempotency_key(event_id);
    item.insert("status".to_string(), AttributeValue::S("processing".to_string()));
    item.insert("created_at".to_string(), AttributeValue::N(now.to_string()));
    item.insert(
        "expires_at".to_string(),
        AttributeValue::N((now + ttl_seconds).to_string()),
    );

    let result = client
        .put_item()
        .table_name(table_name)
        .set_item(Some(item))
        .condition_expression("attribute_not_exists(pk)")
        .send()
        .await;

    match result {
        Ok(_) => Ok(true),
        Err(SdkError::ServiceError(service_err))
            if service_err.err().is_conditional_check_failed_exception() => Ok(false),
        Err(err) => Err(err.into()),
    }
}

async fn mark_processed(
    client: &Client,
    table_name: &str,
    event_id: &str,
) -> Result<(), aws_sdk_dynamodb::Error> {
    let mut expression_attribute_names = HashMap::new();
    expression_attribute_names.insert("#status".to_string(), "status".to_string());

    let mut expression_attribute_values = HashMap::new();
    expression_attribute_values.insert(
        ":status".to_string(),
        AttributeValue::S("done".to_string()),
    );
    expression_attribute_values.insert(
        ":processed_at".to_string(),
        AttributeValue::N(epoch_seconds().to_string()),
    );

    update_item(
        client,
        table_name,
        build_idempotency_key(event_id),
        "SET #status = :status, processed_at = :processed_at",
        expression_attribute_names,
        expression_attribute_values,
    )
    .await
}

async fn mark_failed(
    client: &Client,
    table_name: &str,
    event_id: &str,
    error_message: &str,
) -> Result<(), aws_sdk_dynamodb::Error> {
    let mut expression_attribute_names = HashMap::new();
    expression_attribute_names.insert("#status".to_string(), "status".to_string());
    expression_attribute_names.insert("#error".to_string(), "error".to_string());

    let mut expression_attribute_values = HashMap::new();
    expression_attribute_values.insert(
        ":status".to_string(),
        AttributeValue::S("failed".to_string()),
    );
    expression_attribute_values.insert(
        ":error".to_string(),
        AttributeValue::S(error_message.to_string()),
    );
    expression_attribute_values.insert(
        ":processed_at".to_string(),
        AttributeValue::N(epoch_seconds().to_string()),
    );

    update_item(
        client,
        table_name,
        build_idempotency_key(event_id),
        "SET #status = :status, #error = :error, processed_at = :processed_at",
        expression_attribute_names,
        expression_attribute_values,
    )
    .await
}

async fn increment_duplicate_count(
    client: &Client,
    table_name: &str,
    event_id: &str,
) -> Result<(), aws_sdk_dynamodb::Error> {
    let mut expression_attribute_values = HashMap::new();
    expression_attribute_values.insert(":inc".to_string(), AttributeValue::N("1".to_string()));

    update_item(
        client,
        table_name,
        build_idempotency_key(event_id),
        "ADD duplicate_count :inc",
        HashMap::new(),
        expression_attribute_values,
    )
    .await
}

fn to_json_payload<T: Serialize>(value: &T) -> String {
    match serde_json::to_string(value) {
        Ok(payload) => payload,
        Err(err) => {
            eprintln!("Failed to serialize stage payload: {err}");
            "{}".to_string()
        }
    }
}

async fn persist_stage_record(
    client: &Client,
    table_name: &str,
    event_id: &str,
    stage: &str,
    payload_json: &str,
) -> Result<(), aws_sdk_dynamodb::Error> {
    let mut item = build_stage_key(event_id, stage);
    item.insert("payload".to_string(), AttributeValue::S(payload_json.to_string()));
    item.insert(
        "created_at".to_string(),
        AttributeValue::N(epoch_seconds().to_string()),
    );

    create_item(client, table_name, item).await
}

async fn process_medallion_pipeline(client: &Client) -> Result<(), aws_sdk_dynamodb::Error> {
    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());
    let event = ingest::load_event_from_env(epoch_seconds());
    let event_id = event.event_id.clone();
    let ttl_seconds = std::env::var("DDB_TTL_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(86_400);

    let acquired = try_acquire_idempotency(client, &table_name, &event_id, ttl_seconds).await?;
    if !acquired {
        increment_duplicate_count(client, &table_name, &event_id).await?;
        let existing = read_item(client, &table_name, build_idempotency_key(&event_id)).await?;
        println!("Duplicate event skipped: {event_id} {existing:?}");
        return Ok(());
    }

    let bronze = bronze::to_bronze(&event, epoch_seconds());
    let bronze = to_json_payload(&bronze);
    persist_stage_record(client, &table_name, &event_id, "bronze", &bronze).await?;

    let silver = silver::to_silver(&event, epoch_seconds());
    let silver_json = to_json_payload(&silver);
    persist_stage_record(client, &table_name, &event_id, "silver", &silver_json).await?;

    let gold = gold::to_gold(&silver, epoch_seconds());
    let gold_json = to_json_payload(&gold);
    persist_stage_record(client, &table_name, &event_id, "gold", &gold_json).await?;

    match sink::send_to_splunk_hec(&gold_json).await {
        Ok(()) => {
            mark_processed(client, &table_name, &event_id).await?;
            println!("Event processed: {event_id}");
        }
        Err(err) => {
            mark_failed(client, &table_name, &event_id, &err).await?;
            eprintln!("Failed to send event {event_id} to Splunk: {err}");
        }
    }

    Ok(())
}

async fn demo_crud_operations(client: &Client) -> Result<(), aws_sdk_dynamodb::Error> {
    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());

    let mut item = HashMap::new();
    item.insert("pk".to_string(), AttributeValue::S("user#1".to_string()));
    item.insert("sk".to_string(), AttributeValue::S("profile#1".to_string()));
    item.insert("name".to_string(), AttributeValue::S("Ada".to_string()));

    create_item(client, &table_name, item).await?;

    let mut key = HashMap::new();
    key.insert("pk".to_string(), AttributeValue::S("user#1".to_string()));
    key.insert("sk".to_string(), AttributeValue::S("profile#1".to_string()));

    let item = read_item(client, &table_name, key.clone()).await?;
    if let Some(attrs) = item {
        println!("Read item: {attrs:?}");
    } else {
        println!("Item not found");
    }

    let mut expression_attribute_names = HashMap::new();
    expression_attribute_names.insert("#name".to_string(), "name".to_string());

    let mut expression_attribute_values = HashMap::new();
    expression_attribute_values
        .insert(":name".to_string(), AttributeValue::S("Ada Lovelace".to_string()));

    update_item(
        client,
        &table_name,
        key.clone(),
        "SET #name = :name",
        expression_attribute_names,
        expression_attribute_values,
    )
    .await?;

    delete_item(client, &table_name, key).await?;
    Ok(())
}

#[tokio::main]
async fn main() {
    let client = dynamodb_init().await;
    println!("DynamoDB client initialized successfully.");

    let run_demo = std::env::var("DEMO_CRUD")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let result = if run_demo {
        demo_crud_operations(&client).await
    } else {
        process_medallion_pipeline(&client).await
    };

    if let Err(error) = result {
        handle_error(&error);
    }
}
