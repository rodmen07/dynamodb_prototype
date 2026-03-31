use axum::{
    extract::{Path, RawQuery, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::Html,
    routing::{get, patch, post},
    Json, Router,
};
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_cloudwatch::types::{Dimension, Metric, MetricDataQuery, MetricStat};
use aws_sdk_dynamodb::{types::AttributeValue, Client};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, net::SocketAddr};
use tower_http::cors::{AllowOrigin, CorsLayer};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};

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

#[derive(Deserialize, Serialize)]
struct GhPrUser {
    login: String,
}

#[derive(Deserialize, Serialize)]
struct GhPr {
    number: u64,
    title: String,
    html_url: String,
    created_at: String,
    updated_at: String,
    user: GhPrUser,
    draft: bool,
}

#[derive(Serialize)]
struct PrSummary {
    repo: String,
    number: u64,
    title: String,
    html_url: String,
    author: String,
    created_at: String,
    updated_at: String,
    draft: bool,
}

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

#[derive(serde::Deserialize)]
struct DashClaims {
    roles: Vec<String>,
}

fn require_admin(headers: &HeaderMap) -> Result<(), StatusCode> {
    let admin_key = std::env::var("DASHBOARD_ADMIN_KEY")
        .expect("DASHBOARD_ADMIN_KEY must be set");

    // Option 1: X-Admin-Key header (legacy / direct curl access)
    if let Some(k) = headers.get("X-Admin-Key").and_then(|v| v.to_str().ok()) {
        if k == admin_key {
            return Ok(());
        }
    }

    // Option 2: Authorization: Bearer <jwt> issued by auth-service
    let jwt_secret = std::env::var("AUTH_JWT_SECRET").unwrap_or_default();
    if !jwt_secret.is_empty() {
        if let Some(bearer) = headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        {
            let key = DecodingKey::from_secret(jwt_secret.as_bytes());
            let mut validation = Validation::new(Algorithm::HS256);
            validation.validate_exp = true;
            if let Ok(data) = decode::<DashClaims>(bearer, &key, &validation) {
                if data.claims.roles.iter().any(|r| r == "admin") {
                    return Ok(());
                }
            }
        }
    }

    Err(StatusCode::UNAUTHORIZED)
}

/// Like require_admin but also accepts JWTs carrying the "client" role.
/// Used by the portal proxy so that client users can access their own projects.
fn require_admin_or_client(headers: &HeaderMap) -> Result<(), StatusCode> {
    let admin_key = std::env::var("DASHBOARD_ADMIN_KEY")
        .expect("DASHBOARD_ADMIN_KEY must be set");

    if let Some(k) = headers.get("X-Admin-Key").and_then(|v| v.to_str().ok()) {
        if k == admin_key {
            return Ok(());
        }
    }

    let jwt_secret = std::env::var("AUTH_JWT_SECRET").unwrap_or_default();
    if !jwt_secret.is_empty() {
        if let Some(bearer) = headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        {
            let key = DecodingKey::from_secret(jwt_secret.as_bytes());
            let mut validation = Validation::new(Algorithm::HS256);
            validation.validate_exp = true;
            if let Ok(data) = decode::<DashClaims>(bearer, &key, &validation) {
                if data.claims.roles.iter().any(|r| r == "admin" || r == "client") {
                    return Ok(());
                }
            }
        }
    }

    Err(StatusCode::UNAUTHORIZED)
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
    let mut records = list_stage(&s.ddb, "stage#gold").await?;
    if records.is_empty() {
        records = vec![
            serde_json::json!({"event_type":"StopLogging","risk_score":95,"source":"cloudtrail.amazonaws.com","provider":"aws","metric_bucket":"high_risk","control":"CC7.2 Audit Logging","actor":"unknown-actor","resource":"arn:aws:cloudtrail:us-east-1:123456789012:trail/org-audit-trail","processed_at":"2026-03-28T14:22:00Z","stage":"gold"}),
            serde_json::json!({"event_type":"SSHBruteForce","risk_score":95,"source":"guardduty.amazonaws.com","provider":"aws","metric_bucket":"high_risk","control":"CC7.3 Incident Detection","actor":"external","resource":"i-0abc123def456789","detail":"847 failed SSH attempts from 185.220.101.34","processed_at":"2026-03-28T14:18:00Z","stage":"gold"}),
            serde_json::json!({"event_type":"ConsoleLogin","risk_score":95,"source":"signin.amazonaws.com","provider":"aws","metric_bucket":"high_risk","control":"CC6.1 Access Control","actor":"root","resource":"arn:aws:iam::123456789012:root","detail":"Root account login from external IP 203.0.113.42","processed_at":"2026-03-28T14:15:00Z","stage":"gold"}),
            serde_json::json!({"event_type":"AttachRolePolicy","risk_score":80,"source":"iam.amazonaws.com","provider":"aws","metric_bucket":"high_risk","control":"CC6.3 Privileged Access","actor":"dev-engineer-role","resource":"AdministratorAccess policy attached","processed_at":"2026-03-28T14:10:00Z","stage":"gold"}),
            serde_json::json!({"event_type":"PutBucketPolicy","risk_score":80,"source":"s3.amazonaws.com","provider":"aws","metric_bucket":"high_risk","control":"CC6.1 Access Control","actor":"arn:aws:iam::123456789012:user/ops-admin","detail":"Wildcard principal (*) in bucket policy","processed_at":"2026-03-28T14:05:00Z","stage":"gold"}),
            serde_json::json!({"event_type":"SetIamPolicy","risk_score":80,"source":"cloudresourcemanager.googleapis.com","provider":"gcp","metric_bucket":"high_risk","control":"CC6.2 Authentication","actor":"terraform@proj-prod.iam.gserviceaccount.com","resource":"projects/proj-prod","detail":"roles/owner binding added","processed_at":"2026-03-28T13:58:00Z","stage":"gold"}),
            serde_json::json!({"event_type":"DeleteSink","risk_score":80,"source":"logging.googleapis.com","provider":"gcp","metric_bucket":"high_risk","control":"CC7.2 Audit Logging","actor":"admin@company.com","resource":"projects/proj-prod/sinks/audit-export","processed_at":"2026-03-28T13:50:00Z","stage":"gold"}),
            serde_json::json!({"event_type":"CreateServiceAccountKey","risk_score":55,"source":"iam.googleapis.com","provider":"gcp","metric_bucket":"elevated","control":"CC6.7 Secrets Protection","actor":"ci-bot@proj-prod.iam.gserviceaccount.com","resource":"projects/proj-prod/serviceAccounts/deploy-sa","processed_at":"2026-03-28T13:45:00Z","stage":"gold"}),
            serde_json::json!({"event_type":"AuthorizeSecurityGroupIngress","risk_score":55,"source":"ec2.amazonaws.com","provider":"aws","metric_bucket":"elevated","control":"CC6.1 Access Control","actor":"arn:aws:iam::123456789012:user/network-admin","detail":"0.0.0.0/0 on port 22 (SSH)","processed_at":"2026-03-28T13:40:00Z","stage":"gold"}),
            serde_json::json!({"event_type":"GetSecretValue","risk_score":55,"source":"secretsmanager.amazonaws.com","provider":"aws","metric_bucket":"elevated","control":"CC6.7 Secrets Protection","actor":"arn:aws:sts::123456789012:assumed-role/app-role","resource":"prod/db/credentials","processed_at":"2026-03-28T13:35:00Z","stage":"gold"}),
            serde_json::json!({"event_type":"Decrypt","risk_score":25,"source":"kms.amazonaws.com","provider":"aws","metric_bucket":"baseline","control":"CC6.7 Secrets Protection","actor":"arn:aws:sts::123456789012:assumed-role/app-role","resource":"alias/prod-data-key","processed_at":"2026-03-28T13:30:00Z","stage":"gold"}),
            serde_json::json!({"event_type":"CreateSnapshot","risk_score":25,"source":"rds.amazonaws.com","provider":"aws","metric_bucket":"baseline","control":"CC8.1 Change Management","actor":"arn:aws:iam::123456789012:user/dba","resource":"arn:aws:rds:us-east-1:123456789012:db:prod-primary","processed_at":"2026-03-28T13:25:00Z","stage":"gold"}),
            serde_json::json!({"event_type":"CreateBackup","risk_score":25,"source":"sqladmin.googleapis.com","provider":"gcp","metric_bucket":"baseline","control":"A1.2 Availability","actor":"backup-scheduler@proj-prod.iam.gserviceaccount.com","resource":"projects/proj-prod/instances/prod-db","processed_at":"2026-03-28T13:20:00Z","stage":"gold"}),
            serde_json::json!({"event_type":"DeployService","risk_score":25,"source":"run.googleapis.com","provider":"gcp","metric_bucket":"baseline","control":"CC8.1 Change Management","actor":"github-deployer@microservices-489413.iam.gserviceaccount.com","resource":"projects/microservices-489413/locations/us-central1/services/auth-service","processed_at":"2026-03-28T13:15:00Z","stage":"gold"}),
        ];
    }
    Ok(Json(records))
}

async fn handler_silver(
    State(s): State<DashState>,
) -> Result<Json<Vec<Value>>, (StatusCode, String)> {
    let mut records = list_stage(&s.ddb, "stage#silver").await?;
    if records.is_empty() {
        records = vec![
            serde_json::json!({"event_type":"StopLogging","when":"2026-03-28T14:20:00Z","provider":"aws","severity":"critical","actor":"arn:aws:iam::123456789012:user/unknown-actor","source_ip":"45.33.32.156","resource":"arn:aws:cloudtrail:us-east-1:123456789012:trail/org-audit-trail","service":"cloudtrail.amazonaws.com","region":"us-east-1","stage":"silver"}),
            serde_json::json!({"event_type":"SSHBruteForce","when":"2026-03-28T14:16:00Z","provider":"aws","severity":"critical","actor":"external","source_ip":"185.220.101.34","resource":"i-0abc123def456789","service":"guardduty.amazonaws.com","region":"us-east-1","detail":"847 attempts, t3.medium instance","stage":"silver"}),
            serde_json::json!({"event_type":"ConsoleLogin","when":"2026-03-28T14:12:00Z","provider":"aws","severity":"critical","actor":"root","source_ip":"203.0.113.42","resource":"arn:aws:iam::123456789012:root","service":"signin.amazonaws.com","region":"us-east-1","stage":"silver"}),
            serde_json::json!({"event_type":"AttachRolePolicy","when":"2026-03-28T14:08:00Z","provider":"aws","severity":"high","actor":"arn:aws:iam::123456789012:user/dev-engineer","source_ip":"10.0.1.50","resource":"AdministratorAccess → dev-engineer-role","service":"iam.amazonaws.com","region":"global","stage":"silver"}),
            serde_json::json!({"event_type":"PutBucketPolicy","when":"2026-03-28T14:04:00Z","provider":"aws","severity":"high","actor":"arn:aws:iam::123456789012:user/ops-admin","source_ip":"172.16.0.5","resource":"arn:aws:s3:::customer-data-prod","service":"s3.amazonaws.com","region":"us-east-1","detail":"Wildcard principal (*) allows public read","stage":"silver"}),
            serde_json::json!({"event_type":"SetIamPolicy","when":"2026-03-28T13:56:00Z","provider":"gcp","severity":"high","actor":"terraform@proj-prod.iam.gserviceaccount.com","source_ip":"140.82.112.22","resource":"projects/proj-prod","service":"cloudresourcemanager.googleapis.com","region":"global","detail":"roles/owner binding added","stage":"silver"}),
            serde_json::json!({"event_type":"DeleteSink","when":"2026-03-28T13:48:00Z","provider":"gcp","severity":"high","actor":"admin@company.com","source_ip":"35.190.0.1","resource":"projects/proj-prod/sinks/audit-export","service":"logging.googleapis.com","region":"global","stage":"silver"}),
            serde_json::json!({"event_type":"DeleteFirewallRule","when":"2026-03-28T13:44:00Z","provider":"gcp","severity":"high","actor":"network-admin@company.com","source_ip":"35.190.0.1","resource":"projects/proj-prod/global/firewalls/deny-all-ingress","service":"compute.googleapis.com","region":"global","stage":"silver"}),
            serde_json::json!({"event_type":"CreateServiceAccountKey","when":"2026-03-28T13:42:00Z","provider":"gcp","severity":"medium","actor":"ci-bot@proj-prod.iam.gserviceaccount.com","source_ip":"140.82.112.22","resource":"deploy-sa@proj-prod.iam.gserviceaccount.com","service":"iam.googleapis.com","region":"global","stage":"silver"}),
            serde_json::json!({"event_type":"AuthorizeSecurityGroupIngress","when":"2026-03-28T13:38:00Z","provider":"aws","severity":"medium","actor":"arn:aws:iam::123456789012:user/network-admin","source_ip":"172.16.0.5","resource":"sg-0a1b2c3d4e5f6g7h8","service":"ec2.amazonaws.com","region":"us-east-1","detail":"0.0.0.0/0 on port 22","stage":"silver"}),
            serde_json::json!({"event_type":"GetSecretValue","when":"2026-03-28T13:34:00Z","provider":"aws","severity":"medium","actor":"arn:aws:sts::123456789012:assumed-role/app-role/task-abc","source_ip":"10.0.2.100","resource":"arn:aws:secretsmanager:us-east-1:123456789012:secret:prod/db/credentials","service":"secretsmanager.amazonaws.com","region":"us-east-1","stage":"silver"}),
            serde_json::json!({"event_type":"AccessSecretVersion","when":"2026-03-28T13:32:00Z","provider":"gcp","severity":"medium","actor":"app-runtime@proj-prod.iam.gserviceaccount.com","source_ip":"10.128.0.5","resource":"projects/proj-prod/secrets/DATABASE_URL/versions/latest","service":"secretmanager.googleapis.com","region":"global","stage":"silver"}),
            serde_json::json!({"event_type":"UpdateFunctionCode","when":"2026-03-28T13:28:00Z","provider":"aws","severity":"medium","actor":"arn:aws:iam::123456789012:user/deploy-bot","source_ip":"140.82.112.22","resource":"arn:aws:lambda:us-east-1:123456789012:function:event-processor","service":"lambda.amazonaws.com","region":"us-east-1","stage":"silver"}),
            serde_json::json!({"event_type":"Decrypt","when":"2026-03-28T13:26:00Z","provider":"aws","severity":"low","actor":"arn:aws:sts::123456789012:assumed-role/app-role/task-abc","source_ip":"10.0.2.100","resource":"arn:aws:kms:us-east-1:123456789012:key/mrk-abc123","service":"kms.amazonaws.com","region":"us-east-1","stage":"silver"}),
            serde_json::json!({"event_type":"CreateSnapshot","when":"2026-03-28T13:22:00Z","provider":"aws","severity":"low","actor":"arn:aws:iam::123456789012:user/dba","source_ip":"10.0.1.25","resource":"arn:aws:rds:us-east-1:123456789012:db:prod-primary","service":"rds.amazonaws.com","region":"us-east-1","stage":"silver"}),
            serde_json::json!({"event_type":"CreateBackup","when":"2026-03-28T13:18:00Z","provider":"gcp","severity":"low","actor":"backup-scheduler@proj-prod.iam.gserviceaccount.com","source_ip":"10.128.0.1","resource":"projects/proj-prod/instances/prod-db","service":"sqladmin.googleapis.com","region":"us-central1","stage":"silver"}),
            serde_json::json!({"event_type":"UpdateService","when":"2026-03-28T13:14:00Z","provider":"aws","severity":"low","actor":"arn:aws:iam::123456789012:user/deploy-bot","source_ip":"140.82.112.22","resource":"arn:aws:ecs:us-east-1:123456789012:service/prod-cluster/api-service","service":"ecs.amazonaws.com","region":"us-east-1","detail":"Image tag updated to sha-abc1234","stage":"silver"}),
            serde_json::json!({"event_type":"DeployService","when":"2026-03-28T13:10:00Z","provider":"gcp","severity":"low","actor":"github-deployer@microservices-489413.iam.gserviceaccount.com","source_ip":"140.82.112.22","resource":"projects/microservices-489413/locations/us-central1/services/auth-service","service":"run.googleapis.com","region":"us-central1","stage":"silver"}),
            serde_json::json!({"event_type":"PushImage","when":"2026-03-28T13:06:00Z","provider":"gcp","severity":"low","actor":"github-deployer@microservices-489413.iam.gserviceaccount.com","source_ip":"140.82.112.22","resource":"us-central1-docker.pkg.dev/microservices-489413/microservices/auth-service:latest","service":"artifactregistry.googleapis.com","region":"us-central1","stage":"silver"}),
        ];
    }
    Ok(Json(records))
}

async fn handler_bronze(
    State(s): State<DashState>,
) -> Result<Json<Vec<Value>>, (StatusCode, String)> {
    let mut records = list_stage(&s.ddb, "stage#bronze").await?;
    if records.is_empty() {
        records = vec![
            serde_json::json!({"pk":"event#aws_cloudtrail_stop_logging","sk":"stage#bronze#1711062000","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-008\",\"event_name\":\"StopLogging\",\"actor\":\"arn:aws:iam::123456789012:user/unknown-actor\",\"source_ip\":\"45.33.32.156\",\"resource\":\"arn:aws:cloudtrail:us-east-1:123456789012:trail/org-audit-trail\",\"severity\":\"critical\",\"service\":\"cloudtrail.amazonaws.com\",\"region\":\"us-east-1\"}","ingested_at":"2026-03-28T14:20:00Z"}),
            serde_json::json!({"pk":"event#aws_guardduty_finding","sk":"stage#bronze#1711119600","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-014\",\"event_name\":\"UnauthorizedAccess:EC2/SSHBruteForce\",\"actor\":\"external\",\"source_ip\":\"185.220.101.34\",\"resource\":\"i-0abc123def456789\",\"severity\":\"critical\",\"service\":\"guardduty.amazonaws.com\",\"count\":847}","ingested_at":"2026-03-28T14:16:00Z"}),
            serde_json::json!({"pk":"event#aws_root_console_login","sk":"stage#bronze#1711065600","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-009\",\"event_name\":\"ConsoleLogin\",\"actor\":\"root\",\"source_ip\":\"203.0.113.42\",\"resource\":\"arn:aws:iam::123456789012:root\",\"severity\":\"critical\",\"service\":\"signin.amazonaws.com\",\"region\":\"us-east-1\"}","ingested_at":"2026-03-28T14:12:00Z"}),
            serde_json::json!({"pk":"event#aws_iam_attach_policy","sk":"stage#bronze#1711069200","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-002\",\"event_name\":\"AttachRolePolicy\",\"actor\":\"arn:aws:iam::123456789012:user/dev-engineer\",\"source_ip\":\"10.0.1.50\",\"resource\":\"AdministratorAccess\",\"severity\":\"high\",\"service\":\"iam.amazonaws.com\"}","ingested_at":"2026-03-28T14:08:00Z"}),
            serde_json::json!({"pk":"event#aws_s3_put_bucket_policy","sk":"stage#bronze#1711072800","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-010\",\"event_name\":\"PutBucketPolicy\",\"actor\":\"arn:aws:iam::123456789012:user/ops-admin\",\"source_ip\":\"172.16.0.5\",\"resource\":\"arn:aws:s3:::customer-data-prod\",\"severity\":\"high\",\"service\":\"s3.amazonaws.com\"}","ingested_at":"2026-03-28T14:04:00Z"}),
            serde_json::json!({"pk":"event#gcp_iam_set_policy","sk":"stage#bronze#1711080000","payload":"{\"provider\":\"gcp\",\"event_id\":\"evt-gcp-001\",\"event_name\":\"SetIamPolicy\",\"actor\":\"terraform@proj-prod.iam.gserviceaccount.com\",\"source_ip\":\"140.82.112.22\",\"resource\":\"projects/proj-prod\",\"severity\":\"high\",\"service\":\"cloudresourcemanager.googleapis.com\"}","ingested_at":"2026-03-28T13:56:00Z"}),
            serde_json::json!({"pk":"event#gcp_logging_delete_sink","sk":"stage#bronze#1711083600","payload":"{\"provider\":\"gcp\",\"event_id\":\"evt-gcp-007\",\"event_name\":\"DeleteSink\",\"actor\":\"admin@company.com\",\"source_ip\":\"35.190.0.1\",\"resource\":\"projects/proj-prod/sinks/audit-export\",\"severity\":\"high\",\"service\":\"logging.googleapis.com\"}","ingested_at":"2026-03-28T13:48:00Z"}),
            serde_json::json!({"pk":"event#gcp_compute_delete_firewall","sk":"stage#bronze#1711085400","payload":"{\"provider\":\"gcp\",\"event_id\":\"evt-gcp-004\",\"event_name\":\"DeleteFirewallRule\",\"actor\":\"network-admin@company.com\",\"source_ip\":\"35.190.0.1\",\"resource\":\"projects/proj-prod/global/firewalls/deny-all-ingress\",\"severity\":\"high\",\"service\":\"compute.googleapis.com\"}","ingested_at":"2026-03-28T13:44:00Z"}),
            serde_json::json!({"pk":"event#gcp_sa_key_create","sk":"stage#bronze#1711087200","payload":"{\"provider\":\"gcp\",\"event_id\":\"evt-gcp-006\",\"event_name\":\"CreateServiceAccountKey\",\"actor\":\"ci-bot@proj-prod.iam.gserviceaccount.com\",\"source_ip\":\"140.82.112.22\",\"resource\":\"deploy-sa@proj-prod.iam.gserviceaccount.com\",\"severity\":\"medium\",\"service\":\"iam.googleapis.com\"}","ingested_at":"2026-03-28T13:42:00Z"}),
            serde_json::json!({"pk":"event#aws_ec2_authorize_sg","sk":"stage#bronze#1711090800","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-003\",\"event_name\":\"AuthorizeSecurityGroupIngress\",\"actor\":\"arn:aws:iam::123456789012:user/network-admin\",\"source_ip\":\"172.16.0.5\",\"resource\":\"sg-0a1b2c3d4e5f6g7h8\",\"severity\":\"medium\",\"service\":\"ec2.amazonaws.com\"}","ingested_at":"2026-03-28T13:38:00Z"}),
            serde_json::json!({"pk":"event#aws_secretsmanager_get_secret","sk":"stage#bronze#1711094400","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-012\",\"event_name\":\"GetSecretValue\",\"actor\":\"arn:aws:sts::123456789012:assumed-role/app-role/task-abc\",\"source_ip\":\"10.0.2.100\",\"resource\":\"prod/db/credentials\",\"severity\":\"medium\",\"service\":\"secretsmanager.amazonaws.com\"}","ingested_at":"2026-03-28T13:34:00Z"}),
            serde_json::json!({"pk":"event#gcp_secretmanager_access","sk":"stage#bronze#1711096200","payload":"{\"provider\":\"gcp\",\"event_id\":\"evt-gcp-008\",\"event_name\":\"AccessSecretVersion\",\"actor\":\"app-runtime@proj-prod.iam.gserviceaccount.com\",\"source_ip\":\"10.128.0.5\",\"resource\":\"projects/proj-prod/secrets/DATABASE_URL/versions/latest\",\"severity\":\"medium\",\"service\":\"secretmanager.googleapis.com\"}","ingested_at":"2026-03-28T13:32:00Z"}),
            serde_json::json!({"pk":"event#aws_lambda_update_function","sk":"stage#bronze#1711098000","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-005\",\"event_name\":\"UpdateFunctionCode\",\"actor\":\"arn:aws:iam::123456789012:user/deploy-bot\",\"source_ip\":\"140.82.112.22\",\"resource\":\"arn:aws:lambda:us-east-1:123456789012:function:event-processor\",\"severity\":\"medium\",\"service\":\"lambda.amazonaws.com\"}","ingested_at":"2026-03-28T13:28:00Z"}),
            serde_json::json!({"pk":"event#aws_kms_decrypt","sk":"stage#bronze#1711101600","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-004\",\"event_name\":\"Decrypt\",\"actor\":\"arn:aws:sts::123456789012:assumed-role/app-role/task-abc\",\"source_ip\":\"10.0.2.100\",\"resource\":\"alias/prod-data-key\",\"severity\":\"low\",\"service\":\"kms.amazonaws.com\"}","ingested_at":"2026-03-28T13:26:00Z"}),
            serde_json::json!({"pk":"event#aws_rds_create_snapshot","sk":"stage#bronze#1711105200","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-011\",\"event_name\":\"CreateSnapshot\",\"actor\":\"arn:aws:iam::123456789012:user/dba\",\"source_ip\":\"10.0.1.25\",\"resource\":\"arn:aws:rds:us-east-1:123456789012:db:prod-primary\",\"severity\":\"low\",\"service\":\"rds.amazonaws.com\"}","ingested_at":"2026-03-28T13:22:00Z"}),
            serde_json::json!({"pk":"event#gcp_cloudsql_create_backup","sk":"stage#bronze#1711108800","payload":"{\"provider\":\"gcp\",\"event_id\":\"evt-gcp-003\",\"event_name\":\"CreateBackup\",\"actor\":\"backup-scheduler@proj-prod.iam.gserviceaccount.com\",\"source_ip\":\"10.128.0.1\",\"resource\":\"projects/proj-prod/instances/prod-db\",\"severity\":\"low\",\"service\":\"sqladmin.googleapis.com\"}","ingested_at":"2026-03-28T13:18:00Z"}),
            serde_json::json!({"pk":"event#aws_ecs_update_service","sk":"stage#bronze#1711112400","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-006\",\"event_name\":\"UpdateService\",\"actor\":\"arn:aws:iam::123456789012:user/deploy-bot\",\"source_ip\":\"140.82.112.22\",\"resource\":\"arn:aws:ecs:us-east-1:123456789012:service/prod-cluster/api-service\",\"severity\":\"low\",\"service\":\"ecs.amazonaws.com\"}","ingested_at":"2026-03-28T13:14:00Z"}),
            serde_json::json!({"pk":"event#gcp_cloudrun_deploy","sk":"stage#bronze#1711116000","payload":"{\"provider\":\"gcp\",\"event_id\":\"evt-gcp-009\",\"event_name\":\"DeployService\",\"actor\":\"github-deployer@microservices-489413.iam.gserviceaccount.com\",\"source_ip\":\"140.82.112.22\",\"resource\":\"us-central1/services/auth-service\",\"severity\":\"low\",\"service\":\"run.googleapis.com\"}","ingested_at":"2026-03-28T13:10:00Z"}),
            serde_json::json!({"pk":"event#gcp_artifact_push","sk":"stage#bronze#1711119600","payload":"{\"provider\":\"gcp\",\"event_id\":\"evt-gcp-002\",\"event_name\":\"PushImage\",\"actor\":\"github-deployer@microservices-489413.iam.gserviceaccount.com\",\"source_ip\":\"140.82.112.22\",\"resource\":\"us-central1-docker.pkg.dev/microservices-489413/microservices/auth-service:latest\",\"severity\":\"low\",\"service\":\"artifactregistry.googleapis.com\"}","ingested_at":"2026-03-28T13:06:00Z"}),
            serde_json::json!({"pk":"event#aws_dynamodb_create_table","sk":"stage#bronze#1711123200","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-001\",\"event_name\":\"CreateTable\",\"actor\":\"arn:aws:iam::123456789012:user/dba\",\"source_ip\":\"10.0.1.25\",\"resource\":\"arn:aws:dynamodb:us-east-1:123456789012:table/audit-events\",\"severity\":\"low\",\"service\":\"dynamodb.amazonaws.com\"}","ingested_at":"2026-03-28T13:02:00Z"}),
            serde_json::json!({"pk":"event#aws_waf_update_rules","sk":"stage#bronze#1711126800","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-013\",\"event_name\":\"UpdateWebACL\",\"actor\":\"arn:aws:iam::123456789012:user/security-lead\",\"source_ip\":\"10.0.1.10\",\"resource\":\"arn:aws:wafv2:us-east-1:123456789012:regional/webacl/prod-acl\",\"severity\":\"low\",\"service\":\"wafv2.amazonaws.com\"}","ingested_at":"2026-03-28T12:58:00Z"}),
            serde_json::json!({"pk":"event#aws_iam_create_role","sk":"stage#bronze#1711130400","payload":"{\"provider\":\"aws\",\"event_id\":\"evt-aws-007\",\"event_name\":\"CreateRole\",\"actor\":\"arn:aws:iam::123456789012:user/platform-admin\",\"source_ip\":\"10.0.1.50\",\"resource\":\"arn:aws:iam::123456789012:role/new-service-role\",\"severity\":\"low\",\"service\":\"iam.amazonaws.com\"}","ingested_at":"2026-03-28T12:54:00Z"}),
            serde_json::json!({"pk":"event#gcp_storage_set_acl","sk":"stage#bronze#1711134000","payload":"{\"provider\":\"gcp\",\"event_id\":\"evt-gcp-009\",\"event_name\":\"SetBucketAcl\",\"actor\":\"storage-admin@proj-prod.iam.gserviceaccount.com\",\"source_ip\":\"35.190.0.1\",\"resource\":\"projects/proj-prod/buckets/backups-prod\",\"severity\":\"low\",\"service\":\"storage.googleapis.com\"}","ingested_at":"2026-03-28T12:50:00Z"}),
            serde_json::json!({"pk":"event#gcp_gke_scale_nodepool","sk":"stage#bronze#1711137600","payload":"{\"provider\":\"gcp\",\"event_id\":\"evt-gcp-005\",\"event_name\":\"ScaleNodePool\",\"actor\":\"autoscaler@proj-prod.iam.gserviceaccount.com\",\"source_ip\":\"10.128.0.1\",\"resource\":\"projects/proj-prod/zones/us-central1-a/clusters/prod/nodePools/default-pool\",\"severity\":\"low\",\"service\":\"container.googleapis.com\"}","ingested_at":"2026-03-28T12:46:00Z"}),
        ];
    }
    Ok(Json(records))
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
    // Per-stage fallback: match the fallback record counts used by each
    // stage handler so the stat badges stay consistent with the table data.
    if counts.get("bronze").copied().unwrap_or(0) == 0 {
        counts.insert("bronze".to_string(), 24);
    }
    if counts.get("bronze_cleaned").copied().unwrap_or(0) == 0 {
        counts.insert("bronze_cleaned".to_string(), 24);
    }
    if counts.get("silver").copied().unwrap_or(0) == 0 {
        counts.insert("silver".to_string(), 19);
    }
    if counts.get("gold").copied().unwrap_or(0) == 0 {
        counts.insert("gold".to_string(), 14);
    }
    Ok(Json(serde_json::json!({"counts": counts})))
}

// ---------------------------------------------------------------------------
// AI consult logs
// ---------------------------------------------------------------------------

async fn query_by_pk(
    ddb: &Client,
    pk_value: &str,
) -> Result<Vec<Value>, (StatusCode, String)> {
    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());

    let mut expr_vals = HashMap::new();
    expr_vals.insert(":pk".to_string(), AttributeValue::S(pk_value.to_string()));

    let resp = ddb
        .query()
        .table_name(table_name)
        .key_condition_expression("pk = :pk")
        .set_expression_attribute_values(Some(expr_vals))
        .scan_index_forward(false)
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

async fn handler_ai_logs(
    State(s): State<DashState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Value>>, (StatusCode, String)> {
    require_admin(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    query_by_pk(&s.ddb, "source#ai-consult").await.map(Json)
}

// ---------------------------------------------------------------------------
// Contact form
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ContactBody {
    name: String,
    email: String,
    message: String,
}

async fn handler_contact_submit(
    State(s): State<DashState>,
    Json(body): Json<ContactBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let name = body.name.trim().to_string();
    let email = body.email.trim().to_string();
    let message = body.message.trim().to_string();

    if name.is_empty() || email.is_empty() || message.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name, email, and message are required".to_string()));
    }
    if message.len() > 4000 {
        return Err((StatusCode::BAD_REQUEST, "message too long (max 4000 characters)".to_string()));
    }

    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let payload_str = serde_json::json!({
        "id": id,
        "name": name,
        "email": email,
        "message": message,
        "submitted_at": now,
    }).to_string();

    s.ddb
        .put_item()
        .table_name(table_name)
        .item("pk", AttributeValue::S("source#contact-form".to_string()))
        .item("sk", AttributeValue::S(format!("contact#{id}")))
        .item("payload", AttributeValue::S(payload_str))
        .item("submitted_at", AttributeValue::S(now))
        .send()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Dynamo error: {e}")))?;

    Ok(StatusCode::ACCEPTED)
}

async fn handler_contact_inbox(
    State(s): State<DashState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Value>>, (StatusCode, String)> {
    require_admin(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    list_stage(&s.ddb, "contact#").await.map(Json)
}

#[derive(Deserialize)]
struct IngestBody {
    source: String,
    event_type: String,
    payload: Value,
}

async fn handler_ingest(
    State(s): State<DashState>,
    headers: HeaderMap,
    Json(body): Json<IngestBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    require_admin(&headers).map_err(|s| (s, "unauthorized".to_string()))?;
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

#[derive(Deserialize)]
struct PromoteBody {
    pk: String,
    sk: String,
}

async fn handler_promote(
    State(s): State<DashState>,
    headers: HeaderMap,
    Json(body): Json<PromoteBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    require_admin(&headers).map_err(|e| (e, "unauthorized".to_string()))?;

    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());

    // Determine next stage from the current SK prefix
    let next_stage = if body.sk.starts_with("stage#bronze#") {
        "silver"
    } else if body.sk.starts_with("stage#silver#") {
        "gold"
    } else {
        return Err((StatusCode::BAD_REQUEST, "sk is already at gold or has an unrecognized stage prefix".to_string()));
    };

    // Extract the UUID suffix (same UUID, new stage)
    let uuid_part = body.sk.splitn(3, '#').nth(2).ok_or_else(|| {
        (StatusCode::BAD_REQUEST, "sk format must be stage#<tier>#<uuid>".to_string())
    })?;
    let new_sk = format!("stage#{next_stage}#{uuid_part}");

    // Fetch the existing item's payload
    let get_resp = s.ddb
        .get_item()
        .table_name(&table_name)
        .key("pk", AttributeValue::S(body.pk.clone()))
        .key("sk", AttributeValue::S(body.sk.clone()))
        .send()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Dynamo error: {e}")))?;

    let item = get_resp.item.ok_or_else(|| {
        (StatusCode::NOT_FOUND, format!("item not found: pk={} sk={}", body.pk, body.sk))
    })?;

    let payload = item.get("payload")
        .and_then(|v| v.as_s().ok())
        .cloned()
        .unwrap_or_default();

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // Write promoted item
    s.ddb
        .put_item()
        .table_name(&table_name)
        .item("pk", AttributeValue::S(body.pk.clone()))
        .item("sk", AttributeValue::S(new_sk))
        .item("payload", AttributeValue::S(payload))
        .item("ingested_at", AttributeValue::S(now))
        .send()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Dynamo error: {e}")))?;

    // Delete the old item
    s.ddb
        .delete_item()
        .table_name(&table_name)
        .key("pk", AttributeValue::S(body.pk))
        .key("sk", AttributeValue::S(body.sk))
        .send()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Dynamo error: {e}")))?;

    Ok(StatusCode::ACCEPTED)
}

async fn handler_overview(
    State(s): State<DashState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    require_admin(&headers)?;
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

    let all_errors = [&accounts, &contacts, &activities, &opportunities]
        .iter()
        .all(|v| v.get("error").is_some() || v.is_null());

    if all_errors {
        return Ok(Json(serde_json::json!({
            "accounts": {
                "data": [
                    {"id":"acc-001","name":"Acme Corp","domain":"acme.com","status":"active","industry":"Technology"},
                    {"id":"acc-002","name":"Globex Inc","domain":"globex.io","status":"active","industry":"Finance"},
                    {"id":"acc-003","name":"Initech LLC","domain":"initech.com","status":"prospect","industry":"Consulting"}
                ],
                "total": 5
            },
            "contacts": {
                "data": [
                    {"id":"cnt-001","first_name":"Alice","last_name":"Johnson","email":"alice@acme.com","lifecycle_stage":"customer","account_id":"acc-001"},
                    {"id":"cnt-002","first_name":"Bob","last_name":"Smith","email":"bob@globex.io","lifecycle_stage":"lead","account_id":"acc-002"},
                    {"id":"cnt-003","first_name":"Carol","last_name":"White","email":"carol@initech.com","lifecycle_stage":"prospect","account_id":"acc-003"}
                ],
                "total": 12
            },
            "activities": [
                {"id":"act-001","activity_type":"call","subject":"Onboarding call","contact_id":"cnt-001","completed":true,"created_at":"2026-03-25T11:00:00Z"},
                {"id":"act-002","activity_type":"email","subject":"Follow-up on proposal","contact_id":"cnt-002","completed":false,"created_at":"2026-03-26T09:30:00Z"},
                {"id":"act-003","activity_type":"meeting","subject":"Discovery session","contact_id":"cnt-003","completed":true,"created_at":"2026-03-26T14:00:00Z"}
            ],
            "opportunities": [
                {"id":"opp-001","name":"Acme Platform Deal","stage":"Proposal","amount":12500,"account_id":"acc-001"},
                {"id":"opp-002","name":"Globex Integration","stage":"Discovery","amount":8750,"account_id":"acc-002"},
                {"id":"opp-003","name":"Initech CRM Setup","stage":"Closed Won","amount":5200,"account_id":"acc-003"}
            ],
            "_mock": true
        })));
    }

    Ok(Json(serde_json::json!({
        "accounts": accounts,
        "contacts": contacts,
        "activities": activities,
        "opportunities": opportunities,
    })))
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

async fn handler_builds(
    State(s): State<DashState>,
) -> Result<Json<Vec<BuildStatus>>, StatusCode> {
    const REPOS: &[&str] = &[
        "backend-service",
        "frontend-service",
        "auth-service",
        "ai-orchestrator-service",
        "dynamodb_prototype",
        "microservices",
        "event-stream-service",
        "observaboard",
        "go-gateway",
    ];

    let (r0, r1, r2, r3, r4, r5, r6, r7, r8) = tokio::join!(
        get_cached_or_fetch(&s, REPOS[0]),
        get_cached_or_fetch(&s, REPOS[1]),
        get_cached_or_fetch(&s, REPOS[2]),
        get_cached_or_fetch(&s, REPOS[3]),
        get_cached_or_fetch(&s, REPOS[4]),
        get_cached_or_fetch(&s, REPOS[5]),
        get_cached_or_fetch(&s, REPOS[6]),
        get_cached_or_fetch(&s, REPOS[7]),
        get_cached_or_fetch(&s, REPOS[8]),
    );

    Ok(Json(vec![r0, r1, r2, r3, r4, r5, r6, r7, r8]))
}

// ---------------------------------------------------------------------------
// Open pull requests (admin-only)
// ---------------------------------------------------------------------------

async fn fetch_repo_prs(state: &DashState, repo: &str) -> Vec<PrSummary> {
    let token = match &state.github_token {
        Some(t) => t.clone(),
        None => return vec![],
    };
    let url = format!(
        "https://api.github.com/repos/rodmen07/{repo}/pulls?state=open&per_page=20"
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
            resp.json::<Vec<GhPr>>().await.unwrap_or_default()
                .into_iter()
                .map(|pr| PrSummary {
                    repo: repo.to_string(),
                    number: pr.number,
                    title: pr.title,
                    html_url: pr.html_url,
                    author: pr.user.login,
                    created_at: pr.created_at,
                    updated_at: pr.updated_at,
                    draft: pr.draft,
                })
                .collect()
        }
        _ => vec![],
    }
}

async fn handler_prs(
    State(s): State<DashState>,
    headers: HeaderMap,
) -> Result<Json<Vec<PrSummary>>, StatusCode> {
    require_admin(&headers)?;

    const REPOS: &[&str] = &[
        "backend-service",
        "frontend-service",
        "auth-service",
        "ai-orchestrator-service",
        "dynamodb_prototype",
        "microservices",
        "event-stream-service",
        "observaboard",
        "go-gateway",
    ];

    let (r0, r1, r2, r3, r4, r5, r6, r7, r8) = tokio::join!(
        fetch_repo_prs(&s, REPOS[0]),
        fetch_repo_prs(&s, REPOS[1]),
        fetch_repo_prs(&s, REPOS[2]),
        fetch_repo_prs(&s, REPOS[3]),
        fetch_repo_prs(&s, REPOS[4]),
        fetch_repo_prs(&s, REPOS[5]),
        fetch_repo_prs(&s, REPOS[6]),
        fetch_repo_prs(&s, REPOS[7]),
        fetch_repo_prs(&s, REPOS[8]),
    );

    let mut prs: Vec<PrSummary> = [r0, r1, r2, r3, r4, r5, r6, r7, r8].into_iter().flatten().collect();
    // Sort by most recently updated
    prs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(Json(prs))
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

async fn handler_infrastructure(
    State(s): State<DashState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    require_admin(&headers)?;
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

    let all_zero = lambda_functions.iter().all(|(_, label)| {
        let safe = label.replace('-', "_");
        results_map
            .get(&format!("{safe}_inv"))
            .map(|v| v.iter().sum::<f64>() == 0.0)
            .unwrap_or(true)
    });

    if all_zero {
        return Ok(Json(serde_json::json!({
            "lambda": [
                {"name":"process-bronze","invocations":{"values":[42.0],"trend":"stable"},"errors":{"values":[0.0],"trend":"stable"},"duration_ms":{"values":[340.0],"trend":"stable"}},
                {"name":"process-silver","invocations":{"values":[38.0],"trend":"stable"},"errors":{"values":[1.0],"trend":"stable"},"duration_ms":{"values":[520.0],"trend":"stable"}},
                {"name":"run-pipeline","invocations":{"values":[8.0],"trend":"stable"},"errors":{"values":[0.0],"trend":"stable"},"duration_ms":{"values":[1200.0],"trend":"stable"}},
            ],
            "dynamodb": {
                "read_capacity": {"values":[280.0],"trend":"stable"},
                "write_capacity": {"values":[45.0],"trend":"stable"},
                "latency_ms": {"values":[34.0],"trend":"stable"},
            },
            "fetched_at": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "_mock": true
        })));
    }

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

    Ok(Json(serde_json::json!({
        "lambda": lambda_data,
        "dynamodb": {
            "read_capacity": metric_obj("ddb_read"),
            "write_capacity": metric_obj("ddb_write"),
            "latency_ms": metric_obj("ddb_lat"),
        },
        "fetched_at": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    })))
}

// ---------------------------------------------------------------------------
// Portal proxy — forwards /api/portal/* to PROJECTS_API_URL (admin-only)
// ---------------------------------------------------------------------------

fn projects_api_url() -> String {
    std::env::var("PROJECTS_API_URL")
        .unwrap_or_else(|_| "http://localhost:3001".to_string())
}

fn search_service_url() -> String {
    std::env::var("SEARCH_SERVICE_URL")
        .unwrap_or_else(|_| "http://localhost:8001".to_string())
}

fn reporting_service_url() -> String {
    std::env::var("REPORTING_SERVICE_URL")
        .unwrap_or_else(|_| "http://localhost:8002".to_string())
}

fn observaboard_url() -> String {
    std::env::var("OBSERVABOARD_URL")
        .unwrap_or_else(|_| "http://localhost:8003".to_string())
}

async fn proxy_get(
    http: &reqwest::Client,
    url: &str,
    auth: Option<&str>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut req = http.get(url);
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    if status.is_success() {
        Ok(Json(body))
    } else {
        Err((
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            body.to_string(),
        ))
    }
}

async fn handler_security(
    State(s): State<DashState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin_or_client(&headers).map_err(|e| (e, "unauthorized".to_string()))?;

    // Try loading from DynamoDB cache
    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());
    if let Ok(resp) = s
        .ddb
        .get_item()
        .table_name(&table_name)
        .key("pk", AttributeValue::S("cache#security".to_string()))
        .key("sk", AttributeValue::S("latest".to_string()))
        .send()
        .await
    {
        if let Some(item) = resp.item() {
            if let Some(payload) = item.get("payload").and_then(|v| v.as_s().ok()) {
                if let Ok(json) = serde_json::from_str::<Value>(payload) {
                    return Ok(Json(json));
                }
            }
        }
    }

    // Fallback: baseline security posture
    Ok(Json(serde_json::json!({
        "summary": {
            "total_controls": 24,
            "enabled": 22,
            "monitored": 18,
            "last_assessed": "2026-03-28T16:00:00Z"
        },
        "categories": [
            {
                "name": "Access Control",
                "controls": [
                    {"control":"Multi-Factor Authentication (MFA)","status":"enabled","resource":"AWS IAM / GCP Identity","soc2_ref":"CC6.1","detail":"Required for all console and programmatic access"},
                    {"control":"Role-Based Access Control (RBAC)","status":"enabled","resource":"AWS IAM Roles / GCP IAM","soc2_ref":"CC6.2","detail":"Least-privilege roles per service; no standing admin access"},
                    {"control":"Service Account Key Rotation","status":"monitored","resource":"GCP Service Accounts","soc2_ref":"CC6.3","detail":"Keys rotated every 90 days; alerts on stale credentials"},
                    {"control":"Session Timeout Policy","status":"enabled","resource":"AWS SSO / Dashboard JWT","soc2_ref":"CC6.1","detail":"Idle sessions expire after 60 minutes"}
                ]
            },
            {
                "name": "Encryption",
                "controls": [
                    {"control":"Encryption at Rest","status":"enabled","resource":"DynamoDB / S3 / RDS","soc2_ref":"CC6.7","detail":"AES-256 via AWS KMS; customer-managed keys for production"},
                    {"control":"Encryption in Transit","status":"enabled","resource":"All services","soc2_ref":"CC6.7","detail":"TLS 1.2+ enforced on all endpoints; HSTS enabled"},
                    {"control":"KMS Key Rotation","status":"enabled","resource":"AWS KMS","soc2_ref":"CC6.7","detail":"Automatic annual rotation; manual rotation available on demand"},
                    {"control":"Secret Management","status":"enabled","resource":"AWS Secrets Manager / GCP Secret Manager","soc2_ref":"CC6.7","detail":"No plaintext secrets in code or environment variables"}
                ]
            },
            {
                "name": "Network Security",
                "controls": [
                    {"control":"VPC Isolation","status":"enabled","resource":"AWS VPC","soc2_ref":"CC6.6","detail":"Production workloads in private subnets; no public IPs on databases"},
                    {"control":"Security Group Least-Privilege","status":"enabled","resource":"AWS EC2 / GCP Firewall Rules","soc2_ref":"CC6.6","detail":"Ingress restricted to required ports; 0.0.0.0/0 blocked on SSH/RDP"},
                    {"control":"WAF / DDoS Protection","status":"enabled","resource":"Fly.io edge / AWS Shield","soc2_ref":"CC6.6","detail":"Rate limiting and geo-blocking at edge; AWS Shield Standard on all resources"},
                    {"control":"DNS Security","status":"configured","resource":"Route53 / Fly.io DNS","soc2_ref":"CC6.6","detail":"DNSSEC enabled; CAA records restrict certificate issuance"}
                ]
            },
            {
                "name": "Logging & Monitoring",
                "controls": [
                    {"control":"CloudTrail Audit Logging","status":"enabled","resource":"AWS CloudTrail","soc2_ref":"CC7.2","detail":"All API calls logged to S3 with integrity validation"},
                    {"control":"GCP Audit Logs","status":"enabled","resource":"GCP Cloud Logging","soc2_ref":"CC7.2","detail":"Admin Activity and Data Access logs exported to central sink"},
                    {"control":"GuardDuty Threat Detection","status":"monitored","resource":"AWS GuardDuty","soc2_ref":"CC7.3","detail":"Continuous monitoring for brute force, crypto mining, data exfiltration"},
                    {"control":"Alerting Pipeline","status":"monitored","resource":"Medallion Pipeline (Bronze/Silver/Gold)","soc2_ref":"CC7.3","detail":"Risk-scored events surfaced on Gold page within 5 minutes of detection"}
                ]
            },
            {
                "name": "Backup & Recovery",
                "controls": [
                    {"control":"Automated Backups","status":"enabled","resource":"DynamoDB / RDS / GCP SQL","soc2_ref":"A1.2","detail":"Point-in-time recovery enabled; 35-day retention for RDS"},
                    {"control":"Cross-Region Replication","status":"configured","resource":"S3 / DynamoDB Global Tables","soc2_ref":"A1.2","detail":"Critical data replicated to us-west-2 for DR"},
                    {"control":"Recovery Testing","status":"monitored","resource":"All databases","soc2_ref":"A1.2","detail":"Quarterly restore drills; RTO target 4h, RPO target 1h"},
                    {"control":"Infrastructure as Code","status":"enabled","resource":"SAM / Terraform / Dockerfiles","soc2_ref":"CC8.1","detail":"All infrastructure reproducible from version-controlled templates"}
                ]
            },
            {
                "name": "Compliance",
                "controls": [
                    {"control":"SOC 2 Control Mapping","status":"enabled","resource":"Gold Metrics Pipeline","soc2_ref":"CC1.1","detail":"Automated mapping of cloud events to SOC 2 Type II controls"},
                    {"control":"Dependency Scanning","status":"monitored","resource":"GitHub Dependabot / Cargo Audit","soc2_ref":"CC8.1","detail":"Automated vulnerability scanning on every PR; critical CVEs block merge"},
                    {"control":"Container Image Scanning","status":"enabled","resource":"Docker images (Fly.io, GCR)","soc2_ref":"CC8.1","detail":"Images scanned before deployment; no known critical vulnerabilities"},
                    {"control":"Change Management","status":"enabled","resource":"GitHub PRs / CI/CD","soc2_ref":"CC8.1","detail":"All production changes require PR review and passing CI checks"}
                ]
            }
        ],
        "_demo": true
    })))
}

async fn handler_portal_projects(
    State(s): State<DashState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin_or_client(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let url = format!("{}/api/v1/projects", projects_api_url().trim_end_matches('/'));
    proxy_get(&s.http, &url, auth).await
}

async fn handler_portal_milestones(
    State(s): State<DashState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin_or_client(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let url = format!(
        "{}/api/v1/projects/{id}/milestones",
        projects_api_url().trim_end_matches('/')
    );
    proxy_get(&s.http, &url, auth).await
}

async fn handler_portal_deliverables(
    State(s): State<DashState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin_or_client(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let url = format!(
        "{}/api/v1/milestones/{id}/deliverables",
        projects_api_url().trim_end_matches('/')
    );
    proxy_get(&s.http, &url, auth).await
}

async fn handler_portal_messages(
    State(s): State<DashState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin_or_client(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let url = format!(
        "{}/api/v1/projects/{id}/messages",
        projects_api_url().trim_end_matches('/')
    );
    proxy_get(&s.http, &url, auth).await
}

async fn handler_portal_send_message(
    State(s): State<DashState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin_or_client(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let base = projects_api_url();
    let url = format!(
        "{}/api/v1/projects/{id}/messages",
        base.trim_end_matches('/')
    );
    let mut req = s.http.post(&url).json(&body);
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let status = resp.status();
    let resp_body: Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    if status.is_success() {
        Ok(Json(resp_body))
    } else {
        Err((
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            resp_body.to_string(),
        ))
    }
}

async fn proxy_post(
    http: &reqwest::Client,
    url: &str,
    auth: Option<&str>,
    body: Value,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut req = http.post(url).json(&body);
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let status = resp.status();
    let resp_body: Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    if status.is_success() {
        Ok(Json(resp_body))
    } else {
        Err((
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            resp_body.to_string(),
        ))
    }
}

async fn proxy_patch(
    http: &reqwest::Client,
    url: &str,
    auth: Option<&str>,
    body: Value,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut req = http.patch(url).json(&body);
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let status = resp.status();
    let resp_body: Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    if status.is_success() {
        Ok(Json(resp_body))
    } else {
        Err((
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            resp_body.to_string(),
        ))
    }
}

async fn proxy_delete(
    http: &reqwest::Client,
    url: &str,
    auth: Option<&str>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut req = http.delete(url);
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let status = resp.status();
    // DELETE may return 204 with no body
    let text = resp
        .text()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let resp_body: Value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    if status.is_success() {
        Ok(Json(resp_body))
    } else {
        Err((
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            resp_body.to_string(),
        ))
    }
}

async fn handler_provision_create_project(
    State(s): State<DashState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let url = format!("{}/api/v1/projects", projects_api_url().trim_end_matches('/'));
    proxy_post(&s.http, &url, auth, body).await
}

async fn handler_provision_create_milestone(
    State(s): State<DashState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let url = format!(
        "{}/api/v1/projects/{id}/milestones",
        projects_api_url().trim_end_matches('/')
    );
    proxy_post(&s.http, &url, auth, body).await
}

async fn handler_provision_create_deliverable(
    State(s): State<DashState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let url = format!(
        "{}/api/v1/milestones/{id}/deliverables",
        projects_api_url().trim_end_matches('/')
    );
    proxy_post(&s.http, &url, auth, body).await
}

// ---------------------------------------------------------------------------
// AWS Cost Explorer spend (admin-only)
// ---------------------------------------------------------------------------

const SPEND_CACHE_TTL_SECS: i64 = 24 * 3600; // 24 hours

async fn handler_spend(
    State(s): State<DashState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    require_admin(&headers)?;

    let table_name = std::env::var("DDB_TABLE").unwrap_or_else(|_| "example_table".to_string());
    let now_secs = chrono::Utc::now().timestamp();

    // Check cache
    if let Ok(resp) = s
        .ddb
        .get_item()
        .table_name(&table_name)
        .key("pk", AttributeValue::S("cache#spend".to_string()))
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
            if now_secs - cached_at < SPEND_CACHE_TTL_SECS {
                if let Some(payload) = item.get("payload").and_then(|v| v.as_s().ok()) {
                    if let Ok(mut json) = serde_json::from_str::<Value>(payload) {
                        json["cached"] = serde_json::json!(true);
                        json["cached_at"] = serde_json::json!(
                            chrono::DateTime::from_timestamp(cached_at, 0)
                                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                                .unwrap_or_default()
                        );
                        return Ok(Json(json));
                    }
                }
            }
        }
    }

    // Cache miss — fetch from Cost Explorer
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

    by_service.sort_by(|a, b| {
        let a_val: f64 = a["amount"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let b_val: f64 = b["amount"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        b_val.partial_cmp(&a_val).unwrap_or(std::cmp::Ordering::Equal)
    });

    let result = serde_json::json!({
        "period": { "start": start, "end": end },
        "by_service": by_service,
        "total": format!("{total:.4}"),
        "currency": currency,
        "fetched_at": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "cached": false,
    });

    // Write to cache
    let _ = s
        .ddb
        .put_item()
        .table_name(&table_name)
        .item("pk", AttributeValue::S("cache#spend".to_string()))
        .item("sk", AttributeValue::S("latest".to_string()))
        .item("payload", AttributeValue::S(result.to_string()))
        .item("cached_at", AttributeValue::N(now_secs.to_string()))
        .item("ttl", AttributeValue::N((now_secs + SPEND_CACHE_TTL_SECS).to_string()))
        .send()
        .await;

    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// Search proxy (admin-only)
// ---------------------------------------------------------------------------

async fn handler_search(
    State(s): State<DashState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let qs = raw_query.unwrap_or_default();
    let url = format!(
        "{}/api/v1/search?{qs}",
        search_service_url().trim_end_matches('/')
    );
    proxy_get(&s.http, &url, auth).await
}

// ---------------------------------------------------------------------------
// Reports proxy (admin-only CRUD)
// ---------------------------------------------------------------------------

async fn handler_reports_dashboard(
    State(s): State<DashState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let url = format!(
        "{}/api/v1/reports/dashboard",
        reporting_service_url().trim_end_matches('/')
    );
    proxy_get(&s.http, &url, auth).await
}

async fn handler_reports_list(
    State(s): State<DashState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let url = format!(
        "{}/api/v1/reports",
        reporting_service_url().trim_end_matches('/')
    );
    proxy_get(&s.http, &url, auth).await
}

async fn handler_reports_create(
    State(s): State<DashState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let url = format!(
        "{}/api/v1/reports",
        reporting_service_url().trim_end_matches('/')
    );
    proxy_post(&s.http, &url, auth, body).await
}

async fn handler_reports_update(
    State(s): State<DashState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let url = format!(
        "{}/api/v1/reports/{id}",
        reporting_service_url().trim_end_matches('/')
    );
    proxy_patch(&s.http, &url, auth, body).await
}

async fn handler_reports_delete(
    State(s): State<DashState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let url = format!(
        "{}/api/v1/reports/{id}",
        reporting_service_url().trim_end_matches('/')
    );
    proxy_delete(&s.http, &url, auth).await
}

// ---------------------------------------------------------------------------
// Observaboard proxy (admin-only)
// ---------------------------------------------------------------------------

async fn handler_observaboard_events(
    State(s): State<DashState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_admin(&headers).map_err(|e| (e, "unauthorized".to_string()))?;
    let auth = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let qs = raw_query.unwrap_or_default();
    let url = format!(
        "{}/api/events/?{qs}",
        observaboard_url().trim_end_matches('/')
    );
    proxy_get(&s.http, &url, auth).await
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

async fn gold_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/gold.html"))
}

async fn portal_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/portal.html"))
}

async fn security_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/security.html"))
}

async fn provision_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/provision.html"))
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

async fn messages_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/messages.html"))
}

async fn ai_logs_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/ai_logs.html"))
}

async fn search_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/search.html"))
}

async fn reports_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/reports.html"))
}

async fn observaboard_html() -> Html<&'static str> {
    Html(include_str!("../../dashboard/static/observaboard.html"))
}

async fn auth_js() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/javascript")],
        include_str!("../../dashboard/static/auth.js"),
    )
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
                .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
                .allow_headers([header::CONTENT_TYPE])
        }
        _ => CorsLayer::new(),
    };

    let app = Router::new()
        .route("/", get(index_html))
        .route("/bronze", get(bronze_html))
        .route("/silver", get(silver_html))
        .route("/gold", get(gold_html))
        .route("/overview", get(overview_html))
        .route("/builds", get(builds_html))
        .route("/infrastructure", get(infrastructure_html))
        .route("/spend", get(spend_html))
        .route("/messages", get(messages_html))
        .route("/ai-logs", get(ai_logs_html))
        .route("/portal", get(portal_html))
        .route("/security", get(security_html))
        .route("/provision", get(provision_html))
        .route("/search", get(search_html))
        .route("/reports", get(reports_html))
        .route("/observaboard", get(observaboard_html))
        .route("/auth.js", get(auth_js))
        .route("/api/stats", get(handler_stats))
        .route("/api/gold", get(handler_gold))
        .route("/api/bronze", get(handler_bronze))
        .route("/api/silver", get(handler_silver))
        .route("/api/overview", get(handler_overview))
        .route("/api/builds", get(handler_builds))
        .route("/api/prs", get(handler_prs))
        .route("/api/infrastructure", get(handler_infrastructure))
        .route("/api/spend", get(handler_spend))
        .route("/api/contact", post(handler_contact_submit))
        .route("/api/contacts", get(handler_contact_inbox))
        .route("/api/ai-logs", get(handler_ai_logs))
        .route("/api/security", get(handler_security))
        .route("/api/portal/projects", get(handler_portal_projects))
        .route("/api/portal/projects/{id}/milestones", get(handler_portal_milestones))
        .route("/api/portal/projects/{id}/messages", get(handler_portal_messages).post(handler_portal_send_message))
        .route("/api/portal/milestones/{id}/deliverables", get(handler_portal_deliverables))
        .route("/api/search", get(handler_search))
        .route("/api/reports/dashboard", get(handler_reports_dashboard))
        .route("/api/reports", get(handler_reports_list).post(handler_reports_create))
        .route("/api/reports/{id}", patch(handler_reports_update).delete(handler_reports_delete))
        .route("/api/observaboard/events", get(handler_observaboard_events))
        .route("/api/provision/projects", post(handler_provision_create_project))
        .route("/api/provision/projects/{id}/milestones", post(handler_provision_create_milestone))
        .route("/api/provision/milestones/{id}/deliverables", post(handler_provision_create_deliverable))
        .route("/ingest", post(handler_ingest))
        .route("/promote", post(handler_promote))
        .with_state(state)
        .layer(cors);

    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("Dashboard running at http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
