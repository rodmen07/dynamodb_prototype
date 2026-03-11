use axum::{
    extract::State,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_dynamodb::{types::AttributeValue, Client};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, net::SocketAddr};

#[derive(Clone)]
struct DashState {
    ddb: Client,
    http: reqwest::Client,
}

async fn list_stage(
    ddb: &Client,
    stage_prefix: &str,
) -> Result<Vec<Value>, (axum::http::StatusCode, String)> {
    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());

    let mut expr_vals = HashMap::new();
    expr_vals.insert(":prefix".to_string(), AttributeValue::S(stage_prefix.to_string()));

    let resp = ddb
        .scan()
        .table_name(table_name)
        .filter_expression("begins_with(sk, :prefix)")
        .set_expression_attribute_values(Some(expr_vals))
        .send()
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Dynamo error: {e}")))?;

    let mut out = vec![];
    for it in resp.items() {
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
    Ok(out)
}

async fn handler_gold(
    State(s): State<DashState>,
) -> Result<Json<Vec<Value>>, (axum::http::StatusCode, String)> {
    list_stage(&s.ddb, "stage#gold").await.map(Json)
}

async fn handler_silver(
    State(s): State<DashState>,
) -> Result<Json<Vec<Value>>, (axum::http::StatusCode, String)> {
    list_stage(&s.ddb, "stage#silver").await.map(Json)
}

async fn handler_bronze(
    State(s): State<DashState>,
) -> Result<Json<Vec<Value>>, (axum::http::StatusCode, String)> {
    list_stage(&s.ddb, "stage#bronze").await.map(Json)
}

async fn handler_stats(
    State(s): State<DashState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());
    let resp = s
        .ddb
        .scan()
        .table_name(table_name)
        .projection_expression("sk")
        .send()
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Dynamo error: {e}")))?;

    let mut counts: HashMap<String, u64> = HashMap::new();
    for it in resp.items() {
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
    Ok(Json(serde_json::json!({"counts": counts})))
}

#[derive(Deserialize)]
struct IngestBody {
    source: String,
    event_type: String,
    payload: Value,
}

async fn handler_ingest(
    State(s): State<DashState>,
    Json(body): Json<IngestBody>,
) -> Result<axum::http::StatusCode, (axum::http::StatusCode, String)> {
    // Restrict source to safe identifier characters to prevent key injection.
    if !body
        .source
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "source must contain only alphanumeric, dash, or underscore characters".to_string(),
        ));
    }

    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let pk = format!("source#{}", body.source);
    let sk = format!("stage#bronze#{}", id);
    let payload_str = serde_json::json!({
        "source": body.source,
        "event_type": body.event_type,
        "ingested_at": now,
        "data": body.payload,
    })
    .to_string();

    s.ddb
        .put_item()
        .table_name(table_name)
        .item("pk", AttributeValue::S(pk))
        .item("sk", AttributeValue::S(sk))
        .item("payload", AttributeValue::S(payload_str))
        .item("ingested_at", AttributeValue::S(now))
        .send()
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Dynamo error: {e}")))?;

    Ok(axum::http::StatusCode::ACCEPTED)
}

async fn handler_overview(State(s): State<DashState>) -> Json<Value> {
    let token = std::env::var("MICROSERVICES_API_TOKEN").unwrap_or_default();
    let auth = format!("Bearer {}", token);

    let fetch = |env_var: &'static str, path: &'static str| {
        let http = s.http.clone();
        let auth = auth.clone();
        async move {
            let base = match std::env::var(env_var) {
                Ok(u) => u,
                Err(_) => return serde_json::json!({ "error": "service URL not configured" }),
            };
            let url = format!("{}{}", base.trim_end_matches('/'), path);
            match http.get(&url).header("Authorization", &auth).send().await {
                Ok(r) if r.status().is_success() => {
                    r.json::<Value>().await.unwrap_or(Value::Null)
                }
                Ok(r) => serde_json::json!({ "error": format!("HTTP {}", r.status()) }),
                Err(e) => serde_json::json!({ "error": e.to_string() }),
            }
        }
    };

    let (accounts, contacts, activities, opportunities) = tokio::join!(
        fetch("ACCOUNTS_SERVICE_URL", "/api/v1/accounts?limit=100"),
        fetch("CONTACTS_SERVICE_URL", "/api/v1/contacts?limit=100"),
        fetch("ACTIVITIES_SERVICE_URL", "/api/v1/activities"),
        fetch("OPPORTUNITIES_SERVICE_URL", "/api/v1/opportunities"),
    );

    Json(serde_json::json!({
        "accounts": accounts,
        "contacts": contacts,
        "activities": activities,
        "opportunities": opportunities,
    }))
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

async fn overview_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/overview.html"))
}

#[tokio::main]
async fn main() {
    let region_provider = RegionProviderChain::default_provider().or_else("us-east-1");
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;
    let ddb = Client::new(&config);
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("dynamodb-dashboard/1.0")
        .build()
        .expect("failed to build HTTP client");
    let state = DashState { ddb, http };

    let app = Router::new()
        .route("/", get(index_html))
        .route("/bronze", get(bronze_html))
        .route("/silver", get(silver_html))
        .route("/overview", get(overview_html))
        .route("/api/stats", get(handler_stats))
        .route("/api/gold", get(handler_gold))
        .route("/api/bronze", get(handler_bronze))
        .route("/api/silver", get(handler_silver))
        .route("/api/overview", get(handler_overview))
        .route("/ingest", post(handler_ingest))
        .with_state(state);

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
