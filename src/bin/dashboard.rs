use axum::{response::Html, routing::get, Json, Router};
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_dynamodb::{types::AttributeValue, Client};
use serde_json::Value;
use std::net::SocketAddr;

async fn list_stage(
    client: Client,
    stage_prefix: &str,
) -> Result<Json<Vec<Value>>, (axum::http::StatusCode, String)> {
    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());

    let mut expr_vals = std::collections::HashMap::new();
    expr_vals.insert(":prefix".to_string(), AttributeValue::S(stage_prefix.to_string()));

    let resp = client
        .scan()
        .table_name(table_name)
        .filter_expression("begins_with(sk, :prefix)")
        .set_expression_attribute_values(Some(expr_vals))
        .send()
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Dynamo error: {e}")))?;

    let mut out = vec![];
    let items = resp.items();
    if items.is_empty() {
        // no items
    } else {
        for it in items {
            if let Some(payload_attr) = it.get("payload") {
                if let Ok(s) = payload_attr.as_s() {
                    if let Ok(json) = serde_json::from_str::<Value>(s) {
                        out.push(json);
                        continue;
                    }
                }
            }

            let mut m = serde_json::Map::new();
            for (k, v) in it {
                let j = match v {
                    AttributeValue::S(s) => Value::String(s.clone()),
                    AttributeValue::N(n) => Value::String(n.clone()),
                    _ => Value::String(format!("<{}>", v.as_s().ok().map(|s| s.as_str()).unwrap_or(""))),
                };
                m.insert(k.clone(), j);
            }
            out.push(Value::Object(m));
        }
    }

    Ok(Json(out))
}

async fn list_stats(client: Client) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());

    // Scan only the `sk` attribute to build simple counts per stage prefix
    let resp = client
        .scan()
        .table_name(table_name)
        .projection_expression("sk")
        .send()
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Dynamo error: {e}")))?;

    let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let items = resp.items();
    if items.is_empty() {
        // no items
    } else {
        for it in items {
            if let Some(sk_attr) = it.get("sk") {
                if let Ok(s) = sk_attr.as_s() {
                    if s.starts_with("stage#bronze_cleaned") {
                        *counts.entry("bronze_cleaned".to_string()).or_default() += 1;
                    } else if s.starts_with("stage#bronze") {
                        *counts.entry("bronze".to_string()).or_default() += 1;
                    } else if s.starts_with("stage#silver") {
                        *counts.entry("silver".to_string()).or_default() += 1;
                    } else if s.starts_with("stage#gold") {
                        *counts.entry("gold".to_string()).or_default() += 1;
                    } else {
                        *counts.entry("other".to_string()).or_default() += 1;
                    }
                }
            }
        }
    }

    Ok(Json(serde_json::json!({"counts": counts})))
}

async fn index_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/index.html"))
}

async fn bronze_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/bronze.html"))
}

async fn silver_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/silver.html"))
}

#[tokio::main]
async fn main() {
    let region_provider = RegionProviderChain::default_provider().or_else("us-east-1");
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;
    let client = Client::new(&config);

    let app = Router::new()
        .route("/", get(index_html))
        .route("/bronze", get(bronze_html))
        .route("/silver", get(silver_html))
        .route("/api/stats", get({
            let client = client.clone();
            move || {
                let client = client.clone();
                async move { list_stats(client).await }
            }
        }))
        .route("/api/gold", get({
            let client = client.clone();
            move || {
                let client = client.clone();
                async move { list_stage(client, "stage#gold").await }
            }
        }))
        .route("/api/bronze", get({
            let client = client.clone();
            move || {
                let client = client.clone();
                async move { list_stage(client, "stage#bronze").await }
            }
        }))
        .route("/api/silver", get({
            let client = client.clone();
            move || {
                let client = client.clone();
                async move { list_stage(client, "stage#silver").await }
            }
        }));

    // Cloud runtimes (e.g., Cloud Run) provide the listen port via PORT.
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("Dashboard running at http://{addr}");
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}
