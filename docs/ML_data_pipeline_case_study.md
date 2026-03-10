# ML Data Pipeline Case Study: Medallion Architecture Prototype

This document describes a production-ready design and operational plan for the medallion pipeline implemented in this repository (Bronze → Silver → Gold) and steps to deploy and operate it safely in AWS.

## Goals
- Ingest raw JSON events and persist as Bronze records in DynamoDB.
- Clean and normalize events (Bronze → Bronze_Cleaned).
- Enrich and normalize records (Silver).
- Extract metrics and signals (Gold) and forward to external sinks (e.g., Splunk HEC, analytics pipelines).
- Ensure idempotency, observability, security, and cost control.

## Architecture Overview
- Data store: DynamoDB single table design with `pk` (HASH) and `sk` (RANGE). `sk` includes stage prefixes: `stage#bronze`, `stage#bronze_cleaned`, `stage#silver`, `stage#gold`.
- Processing: Rust async binaries orchestrated as Lambda container images (or ECS tasks) performing stage transitions.
- Sink: Optional Splunk HEC forwarder (configurable via environment variables).
- Dashboard: Small Axum service serving static UI and Dynamo-backed APIs for quick inspection.

## Production Improvements (recommended)
- Pagination & Querying
  - Replace scan-based reads with a GSI or targeted queries to avoid full table scans. Add an index on `sk` or a stage-specific GSI to enable efficient listing and metric queries.
  - Implement pagination in the dashboard endpoints and use `Limit` + `ExclusiveStartKey` for large result sets.

- Idempotency
  - Use deterministic `sk` values for idempotent writes where appropriate (e.g., `stage#bronze_cleaned#<event_id>` when event IDs exist).
  - Use conditional writes with `ConditionExpression` to avoid duplicate processing.

- Observability
  - Emit structured logs (JSON) with trace IDs and span IDs. Integrate with CloudWatch Logs or an external log sink.
  - Use metrics (CloudWatch / Prometheus) for counts, errors, latency, and processed bytes.
  - Add distributed tracing (X-Ray) for Lambda or OpenTelemetry to trace across ingest → processing → sink.

- Reliability & Backoff
  - Add exponential backoff and retry for transient HTTP/DynamoDB errors.
  - Use DLQs (SQS) for failed records requiring human inspection.

- Security
  - Principle of least privilege: grant each Lambda only the DynamoDB permissions required (specific table actions).
  - Store secrets (Splunk token, DB connection strings) in AWS Secrets Manager or GitHub Secrets for CI.
  - Use ECR image scanning and IAM roles for service-to-service access.

- Cost & Scaling
  - Use DynamoDB On-Demand for unpredictable loads or provisioned + Auto Scaling for steady traffic.
  - Batch writes when possible; use parallelization carefully to avoid hot partitions.

- Testing
  - Unit tests for cleaning and enrichment logic (already present in `src/processing.rs`).
  - Integration tests against a local DynamoDB (DynamoDB Local) or a test account.
  - Contract tests for sink behavior (Splunk HEC mock).

## Deployment Plan (recommended)
1. Create an ECR repository (e.g., `dynamodb-prototype`) in the target AWS account.
2. Add GitHub Secrets to the repo: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, `ECR_REGISTRY`, `ECR_REPOSITORY`.
3. Use the provided GitHub Actions workflow `.github/workflows/deploy.yml` to build and push the Lambda container image to ECR.
4. Update `template.yaml` to reference the built image URI (or parameterize ImageUri). Example placeholder: `123456789012.dkr.ecr.us-east-1.amazonaws.com/dynamodb-prototype:<SHA>`
5. Run `sam deploy` in CI or locally with proper credentials and stack name.

Quick local commands (replace placeholders):

```bash
# build image
docker build -t $ECR_REGISTRY/$ECR_REPOSITORY:latest -f Dockerfile .
# push
docker push $ECR_REGISTRY/$ECR_REPOSITORY:latest
# validate SAM and deploy (requires aws cli + sam cli configured)
sam validate
sam deploy --template-file template.yaml --stack-name dynamodb-prototype-stack --capabilities CAPABILITY_IAM
```

## CI/CD Notes
- The deploy workflow is configured for manual dispatch or push to `main`. It requires repository secrets and an ECR repo to succeed.
- For multi-environment deployments, add job matrix or separate workflows per environment, and promote images by tags.

## Operational Runbook (short)
- Recovery: On processing failures, inspect CloudWatch logs and push problematic records to the DLQ for manual reprocessing.
- Scaling: Monitor DynamoDB consumed capacity and add auto-scaling rules or switch to on-demand.
- Rollbacks: Use previous image tag in ECR and update function image to that tag.

## Checklist Before Production
- [ ] Add GSI and update dashboard queries to use queries not scans.
- [ ] Harden IAM policies and audit them.
- [ ] Add E2E tests and CI image scanning.
- [ ] Implement DLQs for failed records.
- [ ] Add alerting for error rates and throttling.

## Contacts and Ownership
- Owner: [Your team / owner name]
- On-call / Pager duty: [TBD]


