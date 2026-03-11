# Deploying the DynamoDB prototype dashboard (container)

Build the Docker image locally (from `dynamodb_prototype`):

```bash
docker build -t YOUR_REGISTRY/dynamodb-dashboard:latest .
```

Push to a container registry (example: Google Container Registry):

```bash
docker tag YOUR_REGISTRY/dynamodb-dashboard:latest gcr.io/PROJECT_ID/dynamodb-dashboard:latest
docker push gcr.io/PROJECT_ID/dynamodb-dashboard:latest
```

Deploy to Cloud Run:

```bash
gcloud run deploy dynamodb-dashboard --image gcr.io/PROJECT_ID/dynamodb-dashboard:latest \
  --platform managed --region us-central1 --allow-unauthenticated \
  --set-env-vars DDB_TABLE=your-table-name
```

Or push to AWS ECR and deploy to ECS / Fargate. The container listens on port `8080`.
