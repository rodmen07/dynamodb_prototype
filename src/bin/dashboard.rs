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
    if let Some(items) = resp.items() {
        for it in items {
            if let Some(payload_attr) = it.get("payload") {
                if let Some(s) = payload_attr.as_s() {
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
                    _ => Value::String(format!("<{}>", v.as_s().unwrap_or(""))),
                };
                m.insert(k.clone(), j);
            }
            out.push(Value::Object(m));
        }
    }

    Ok(Json(out))
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
    let config = aws_config::from_env().region(region_provider).load().await;
    let client = Client::new(&config);

    let app = Router::new()
        .route("/", get(index_html))
        .route("/bronze", get(bronze_html))
        .route("/silver", get(silver_html))
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

    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));
    println!("Dashboard running at http://{addr}");
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}
