use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::Html,
    routing::{get, post},
    Json, Router,
};
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_cloudwatch::types::{Dimension, Metric, MetricDataQuery, MetricStat};
use aws_sdk_dynamodb::{types::AttributeValue, Client};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, net::SocketAddr};
use tower_http::cors::{AllowOrigin, CorsLayer};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct DashState {
    ddb: Client,
    cw: aws_sdk_cloudwatch::Client,
    ce: aws_sdk_costexplorer::Client,
    http: reqwest::Client,
    github_token: Option<String>,
}

// ---------------------------------------------------------------------------
// GitHub build monitoring structs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GhRun {
    status: String,
    conclusion: Option<String>,
    html_url: String,
    created_at: String,
}

#[derive(Deserialize)]
struct GhRunsResponse {
    workflow_runs: Vec<GhRun>,
}

#[derive(Serialize, Deserialize, Clone)]
struct BuildStatus {
    repo: String,
    display_status: String, // "green" | "yellow" | "red" | "unknown"
    run_at: String,
    html_url: String,
    cached: bool,
}

// ---------------------------------------------------------------------------
// Admin auth helper
// ---------------------------------------------------------------------------

fn require_admin(headers: &HeaderMap) -> Result<(), StatusCode> {
    let key = std::env::var("DASHBOARD_ADMIN_KEY").unwrap_or_default();
    if key.is_empty() {
        return Ok(()); // no key configured → open in dev mode
    }
    match headers.get("X-Admin-Key").and_then(|v| v.to_str().ok()) {
        Some(k) if k == key => Ok(()),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

// ---------------------------------------------------------------------------
// Existing helpers
// ---------------------------------------------------------------------------

async fn list_stage(
    ddb: &Client,
    stage_prefix: &str,
) -> Result<Vec<Value>, (StatusCode, String)> {
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
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Dynamo error: {e}")))?;

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
                _ => Value::String(format!(
                    "<{}>",
                    v.as_s().ok().map(|s| s.as_str()).unwrap_or("")
                )),
            };
            m.insert(k.clone(), j);
        }
        out.push(Value::Object(m));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Existing stage handlers
// ---------------------------------------------------------------------------

async fn handler_gold(
    State(s): State<DashState>,
) -> Result<Json<Vec<Value>>, (StatusCode, String)> {
    list_stage(&s.ddb, "stage#gold").await.map(Json)
}

async fn handler_silver(
    State(s): State<DashState>,
) -> Result<Json<Vec<Value>>, (StatusCode, String)> {
    list_stage(&s.ddb, "stage#silver").await.map(Json)
}

async fn handler_bronze(
    State(s): State<DashState>,
) -> Result<Json<Vec<Value>>, (StatusCode, String)> {
    list_stage(&s.ddb, "stage#bronze").await.map(Json)
}

async fn handler_stats(
    State(s): State<DashState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());
    let resp = s
        .ddb
        .scan()
        .table_name(table_name)
        .projection_expression("sk")
        .send()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Dynamo error: {e}")))?;

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
) -> Result<StatusCode, (StatusCode, String)> {
    if !body
        .source
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err((
            StatusCode::BAD_REQUEST,
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
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Dynamo error: {e}")))?;

    Ok(StatusCode::ACCEPTED)
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

// ---------------------------------------------------------------------------
// GitHub build status
// ---------------------------------------------------------------------------

async fn fetch_build_status(state: &DashState, repo: &str) -> BuildStatus {
    let token = match &state.github_token {
        Some(t) => t.clone(),
        None => {
            return BuildStatus {
                repo: repo.to_string(),
                display_status: "unknown".to_string(),
                run_at: String::new(),
                html_url: format!("https://github.com/rodmen07/{repo}/actions"),
                cached: false,
            }
        }
    };

    let url = format!(
        "https://api.github.com/repos/rodmen07/{repo}/actions/runs?per_page=1"
    );
    let result = state
        .http
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<GhRunsResponse>().await {
                Ok(body) if !body.workflow_runs.is_empty() => {
                    let run = &body.workflow_runs[0];
                    let display_status = match run.status.as_str() {
                        "queued" | "in_progress" | "waiting" | "requested" => "yellow",
                        "completed" => match run.conclusion.as_deref() {
                            Some("success") | Some("skipped") => "green",
                            _ => "red",
                        },
                        _ => "unknown",
                    };
                    BuildStatus {
                        repo: repo.to_string(),
                        display_status: display_status.to_string(),
                        run_at: run.created_at.clone(),
                        html_url: run.html_url.clone(),
                        cached: false,
                    }
                }
                _ => BuildStatus {
                    repo: repo.to_string(),
                    display_status: "unknown".to_string(),
                    run_at: String::new(),
                    html_url: format!("https://github.com/rodmen07/{repo}/actions"),
                    cached: false,
                },
            }
        }
        _ => BuildStatus {
            repo: repo.to_string(),
            display_status: "unknown".to_string(),
            run_at: String::new(),
            html_url: format!("https://github.com/rodmen07/{repo}/actions"),
            cached: false,
        },
    }
}

async fn get_cached_or_fetch(state: &DashState, repo: &str) -> BuildStatus {
    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());
    let pk = format!("monitor#github#{repo}");
    let now_secs = chrono::Utc::now().timestamp();

    // Try cache
    if let Ok(resp) = state
        .ddb
        .get_item()
        .table_name(&table_name)
        .key("pk", AttributeValue::S(pk.clone()))
        .key("sk", AttributeValue::S("latest".to_string()))
        .send()
        .await
    {
        if let Some(item) = resp.item() {
            let cached_at = item
                .get("cached_at")
                .and_then(|v| v.as_n().ok())
                .and_then(|n| n.parse::<i64>().ok())
                .unwrap_or(0);
            if now_secs - cached_at < 300 {
                if let Some(payload_attr) = item.get("payload") {
                    if let Ok(s) = payload_attr.as_s() {
                        if let Ok(mut status) = serde_json::from_str::<BuildStatus>(s) {
                            status.cached = true;
                            return status;
                        }
                    }
                }
            }
        }
    }

    // Cache miss — fetch from GitHub
    let mut status = fetch_build_status(state, repo).await;
    let payload = serde_json::to_string(&status).unwrap_or_default();

    let _ = state
        .ddb
        .put_item()
        .table_name(&table_name)
        .item("pk", AttributeValue::S(pk))
        .item("sk", AttributeValue::S("latest".to_string()))
        .item("payload", AttributeValue::S(payload))
        .item("cached_at", AttributeValue::N(now_secs.to_string()))
        .item(
            "ttl",
            AttributeValue::N((now_secs + 300).to_string()),
        )
        .send()
        .await;

    status.cached = false;
    status
}

async fn handler_builds(State(s): State<DashState>) -> Json<Vec<BuildStatus>> {
    const REPOS: &[&str] = &[
        "microservices",
        "backend-service",
        "frontend-service",
        "auth-service",
        "ai-orchestrator-service",
        "dynamodb_prototype",
    ];

    let (r0, r1, r2, r3, r4, r5) = tokio::join!(
        get_cached_or_fetch(&s, REPOS[0]),
        get_cached_or_fetch(&s, REPOS[1]),
        get_cached_or_fetch(&s, REPOS[2]),
        get_cached_or_fetch(&s, REPOS[3]),
        get_cached_or_fetch(&s, REPOS[4]),
        get_cached_or_fetch(&s, REPOS[5]),
    );

    Json(vec![r0, r1, r2, r3, r4, r5])
}

// ---------------------------------------------------------------------------
// CloudWatch infrastructure metrics
// ---------------------------------------------------------------------------

fn compute_trend(values: &[f64]) -> &'static str {
    if values.len() < 2 {
        return "stable";
    }
    let last = values[values.len() - 1];
    let prev = values[values.len() - 2];
    if prev == 0.0 {
        return "stable";
    }
    let change = (last - prev) / prev;
    if change >= 0.1 {
        "up"
    } else if change <= -0.1 {
        "down"
    } else {
        "stable"
    }
}

fn make_metric_query(
    id: &str,
    namespace: &str,
    metric_name: &str,
    dim_name: &str,
    dim_value: &str,
    stat: &str,
) -> MetricDataQuery {
    MetricDataQuery::builder()
        .id(id)
        .metric_stat(
            MetricStat::builder()
                .metric(
                    Metric::builder()
                        .namespace(namespace)
                        .metric_name(metric_name)
                        .dimensions(
                            Dimension::builder()
                                .name(dim_name)
                                .value(dim_value)
                                .build(),
                        )
                        .build(),
                )
                .period(3600)
                .stat(stat)
                .build(),
        )
        .build()
}

async fn handler_infrastructure(State(s): State<DashState>) -> Json<Value> {
    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());
    let now = chrono::Utc::now();
    let start = now - chrono::Duration::hours(24);

    let lambda_functions = [
        ("ProcessBronzeFunction", "process-bronze"),
        ("ProcessSilverFunction", "process-silver"),
        ("RunPipelineFunction", "run-pipeline"),
    ];

    let mut queries: Vec<MetricDataQuery> = Vec::new();

    // Lambda metrics
    for (fn_name, fn_label) in &lambda_functions {
        let safe = fn_label.replace('-', "_");
        queries.push(make_metric_query(
            &format!("{safe}_inv"),
            "AWS/Lambda",
            "Invocations",
            "FunctionName",
            fn_name,
            "Sum",
        ));
        queries.push(make_metric_query(
            &format!("{safe}_err"),
            "AWS/Lambda",
            "Errors",
            "FunctionName",
            fn_name,
            "Sum",
        ));
        queries.push(make_metric_query(
            &format!("{safe}_dur"),
            "AWS/Lambda",
            "Duration",
            "FunctionName",
            fn_name,
            "Average",
        ));
    }

    // DynamoDB metrics
    queries.push(make_metric_query(
        "ddb_read",
        "AWS/DynamoDB",
        "ConsumedReadCapacityUnits",
        "TableName",
        &table_name,
        "Sum",
    ));
    queries.push(make_metric_query(
        "ddb_write",
        "AWS/DynamoDB",
        "ConsumedWriteCapacityUnits",
        "TableName",
        &table_name,
        "Sum",
    ));
    queries.push(make_metric_query(
        "ddb_lat",
        "AWS/DynamoDB",
        "SuccessfulRequestLatency",
        "TableName",
        &table_name,
        "Average",
    ));

    let resp = s
        .cw
        .get_metric_data()
        .set_metric_data_queries(Some(queries))
        .start_time(aws_sdk_cloudwatch::primitives::DateTime::from_millis(
            start.timestamp_millis(),
        ))
        .end_time(aws_sdk_cloudwatch::primitives::DateTime::from_millis(
            now.timestamp_millis(),
        ))
        .send()
        .await;

    let results_map: HashMap<String, Vec<f64>> = match resp {
        Ok(r) => r
            .metric_data_results()
            .iter()
            .map(|mdr| {
                let id = mdr.id().unwrap_or("").to_string();
                let vals: Vec<f64> = mdr.values().to_vec();
                (id, vals)
            })
            .collect(),
        Err(_) => HashMap::new(),
    };

    let metric_obj = |id: &str| {
        let vals = results_map.get(id).cloned().unwrap_or_default();
        let trend = compute_trend(&vals);
        serde_json::json!({ "values": vals, "trend": trend })
    };

    let lambda_data: Vec<Value> = lambda_functions
        .iter()
        .map(|(_, label)| {
            let safe = label.replace('-', "_");
            serde_json::json!({
                "name": label,
                "invocations": metric_obj(&format!("{safe}_inv")),
                "errors": metric_obj(&format!("{safe}_err")),
                "duration_ms": metric_obj(&format!("{safe}_dur")),
            })
        })
        .collect();

    Json(serde_json::json!({
        "lambda": lambda_data,
        "dynamodb": {
            "read_capacity": metric_obj("ddb_read"),
            "write_capacity": metric_obj("ddb_write"),
            "latency_ms": metric_obj("ddb_lat"),
        },
        "fetched_at": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    }))
}

// ---------------------------------------------------------------------------
// AWS Cost Explorer spend (admin-only)
// ---------------------------------------------------------------------------

async fn handler_spend(
    State(s): State<DashState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    require_admin(&headers)?;

    let now = chrono::Utc::now();
    let start = (now - chrono::Duration::days(30))
        .format("%Y-%m-%d")
        .to_string();
    let end = now.format("%Y-%m-%d").to_string();

    let resp = s
        .ce
        .get_cost_and_usage()
        .time_period(
            aws_sdk_costexplorer::types::DateInterval::builder()
                .start(&start)
                .end(&end)
                .build()
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
        )
        .granularity(aws_sdk_costexplorer::types::Granularity::Monthly)
        .group_by(
            aws_sdk_costexplorer::types::GroupDefinition::builder()
                .r#type(aws_sdk_costexplorer::types::GroupDefinitionType::Dimension)
                .key("SERVICE")
                .build(),
        )
        .metrics("UnblendedCost")
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut by_service: Vec<Value> = Vec::new();
    let mut total: f64 = 0.0;
    let mut currency = "USD".to_string();

    for result in resp.results_by_time() {
        for group in result.groups() {
            let service = group.keys().first().cloned().unwrap_or_default();
            let metrics = group.metrics();
            if let Some(cost) = metrics.as_ref().and_then(|m| m.get("UnblendedCost")) {
                let amount_str: &str = cost.amount().unwrap_or("0");
                let unit: &str = cost.unit().unwrap_or("USD");
                if let Ok(amount) = amount_str.parse::<f64>() {
                    if amount > 0.0 {
                        total += amount;
                        currency = unit.to_string();
                        by_service.push(serde_json::json!({
                            "service": service,
                            "amount": format!("{amount:.4}"),
                            "unit": unit,
                        }));
                    }
                }
            }
        }
    }

    // Sort descending by amount
    by_service.sort_by(|a, b| {
        let a_val: f64 = a["amount"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let b_val: f64 = b["amount"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        b_val.partial_cmp(&a_val).unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(Json(serde_json::json!({
        "period": { "start": start, "end": end },
        "by_service": by_service,
        "total": format!("{total:.4}"),
        "currency": currency,
        "fetched_at": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    })))
}

// ---------------------------------------------------------------------------
// Static HTML handlers
// ---------------------------------------------------------------------------

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

async fn builds_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/builds.html"))
}

async fn infrastructure_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/infrastructure.html"))
}

async fn spend_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/spend.html"))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let region_provider = RegionProviderChain::default_provider().or_else("us-east-1");
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;

    let ddb = Client::new(&config);
    let cw = aws_sdk_cloudwatch::Client::new(&config);
    // Cost Explorer is a global service — always use us-east-1
    let ce_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .load()
        .await;
    let ce = aws_sdk_costexplorer::Client::new(&ce_config);

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("dynamodb-dashboard/1.0")
        .build()
        .expect("failed to build HTTP client");

    let github_token = std::env::var("GITHUB_TOKEN").ok();

    let state = DashState { ddb, cw, ce, http, github_token };

    // CORS
    let cors = match std::env::var("ALLOWED_ORIGINS")
        .as_deref()
        .unwrap_or_default()
    {
        "*" => CorsLayer::permissive(),
        origins if !origins.is_empty() => {
            let headers: Vec<HeaderValue> = origins
                .split(',')
                .filter_map(|o| o.trim().parse().ok())
                .collect();
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(headers))
                .allow_methods([Method::GET])
        }
        _ => CorsLayer::new(),
    };

    let app = Router::new()
        .route("/", get(index_html))
        .route("/bronze", get(bronze_html))
        .route("/silver", get(silver_html))
        .route("/overview", get(overview_html))
        .route("/builds", get(builds_html))
        .route("/infrastructure", get(infrastructure_html))
        .route("/spend", get(spend_html))
        .route("/api/stats", get(handler_stats))
        .route("/api/gold", get(handler_gold))
        .route("/api/bronze", get(handler_bronze))
        .route("/api/silver", get(handler_silver))
        .route("/api/overview", get(handler_overview))
        .route("/api/builds", get(handler_builds))
        .route("/api/infrastructure", get(handler_infrastructure))
        .route("/api/spend", get(handler_spend))
        .route("/ingest", post(handler_ingest))
        .with_state(state)
        .layer(cors);

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
