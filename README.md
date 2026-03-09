# DynamoDB Medallion Prototype

This prototype demonstrates an idempotent CloudTrail-style event pipeline backed by DynamoDB.

## Pipeline

- Bronze: store raw input payload
- Silver: normalized event shape
- Gold: derived risk metric used for downstream routing

Each stage is written to the same table with:

- `pk = <event_id>`
- `sk = stage#bronze|stage#silver|stage#gold`

Idempotency state is stored with:

- `pk = <event_id>`
- `sk = state`

## Requirements

- Rust toolchain
- AWS credentials configured for DynamoDB access
- Existing DynamoDB table with partition/sort keys `pk` and `sk`

## Environment Variables

- `DDB_TABLE` (default: `example_table`)
- `DDB_TTL_SECONDS` (default: `86400`)
- `EVENT_ID` (default: `event-123`)
- `EVENT_PAYLOAD` (optional JSON string)
- `DEMO_CRUD=true|1` runs the CRUD demo instead of the medallion flow
- `SPLUNK_HEC_URL` (optional; if unset, send is skipped)
- `SPLUNK_HEC_TOKEN` (optional; if unset, send is skipped)
- `SPLUNK_HEC_INDEX` (optional target index)
- `SPLUNK_HEC_TIMEOUT_SECS` (default: `5`)
- `SPLUNK_HEC_RETRIES` (default: `3`)

## Example Payloads

AWS-style payload:

```json
{"provider":"aws","event_name":"ConsoleLogin","actor":"user@example.com","source_ip":"203.0.113.10","resource":"iam:user/example","severity":"medium"}
```

GCP-style payload:

```json
{"provider":"gcp","event_name":"SetIamPolicy","actor":"service-account@project.iam.gserviceaccount.com","source_ip":"34.82.10.42","resource":"projects/sample-project","severity":"high"}
```

## Run

```bash
cargo run
```

Example with explicit payload:

```bash
EVENT_ID=evt-9001 EVENT_PAYLOAD='{"provider":"gcp","event_name":"SetIamPolicy","actor":"sa@project.iam.gserviceaccount.com","source_ip":"34.82.10.42","resource":"projects/sample-project","severity":"high"}' cargo run
```

If `SPLUNK_HEC_URL` or `SPLUNK_HEC_TOKEN` is not set, the send step is skipped and the event is treated as successful for local development.
