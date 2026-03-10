# GitHub OIDC setup for AWS (quick guide)

This document shows how to configure GitHub Actions OIDC trust and an IAM role that the Actions workflow can assume to push images to ECR and run deployments. Using OIDC avoids storing long-lived AWS keys in GitHub Secrets.

## Steps (AWS console or CLI)

1. Create an IAM OIDC identity provider (if not already created)
   - Console: IAM → Identity providers → Add provider
     - Provider type: OpenID Connect
     - Provider URL: `https://token.actions.githubusercontent.com`
     - Audience: `sts.amazonaws.com`

2. Create an IAM Role for GitHub Actions
   - Console: IAM → Roles → Create role
   - Select: Web identity
   - Choose provider: `token.actions.githubusercontent.com`
   - Audience: `sts.amazonaws.com`
   - Condition: set a condition for the repository and optionally the environment, e.g. using `StringLike` on `token.actions.githubusercontent.com:sub`:

Example trust relationship (replace `OWNER/REPO`):

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": { "Federated": "arn:aws:iam::ACCOUNT_ID:oidc-provider/token.actions.githubusercontent.com" },
      "Action": "sts:AssumeRoleWithWebIdentity",
      "Condition": {
        "StringLike": {
          "token.actions.githubusercontent.com:sub": "repo:rodmen07/dynamodb_prototype:*"
        },
        "StringEquals": {
          "token.actions.githubusercontent.com:aud": "sts.amazonaws.com"
        }
      }
    }
  ]
}
```

3. Attach a minimal permission policy to the role
   - For pushing images to ECR and optional SAM deploy, you can use a policy covering ECR, CloudFormation (or SAM), S3 (if packaging), and IAM pass-role where needed.

Example (start with limited set; expand as needed):

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "ecr:CreateRepository",
        "ecr:BatchCheckLayerAvailability",
        "ecr:InitiateLayerUpload",
        "ecr:UploadLayerPart",
        "ecr:CompleteLayerUpload",
        "ecr:PutImage",
        "ecr:GetAuthorizationToken"
      ],
      "Resource": "*"
    },
    {
      "Effect": "Allow",
      "Action": [
        "cloudformation:CreateStack",
        "cloudformation:UpdateStack",
        "cloudformation:DescribeStacks",
        "cloudformation:DescribeStackEvents",
        "cloudformation:GetTemplate"
      ],
      "Resource": "*"
    },
    {
      "Effect": "Allow",
      "Action": ["s3:PutObject", "s3:GetObject", "s3:ListBucket"],
      "Resource": "*"
    },
    {
      "Effect": "Allow",
      "Action": ["iam:PassRole"],
      "Resource": "*"
    }
  ]
}
```

4. Record the Role ARN
   - Example: `arn:aws:iam::123456789012:role/GitHubActionsDynamoDeploy`

5. Add the Role ARN to GitHub Secrets
   - Repository → Settings → Secrets and variables → Actions → New repository secret
   - Name: `AWS_ROLE_TO_ASSUME`
   - Value: the role ARN from step 4

6. Confirm workflow permissions
   - The workflow must request `id-token: write` permission. The `deploy.yml` workflow in this repo already sets job permissions for `id-token: write`.

7. Trigger the deploy workflow
   - From Actions → Deploy → Run workflow (or push to `main`) — the job will use the role via OIDC.

Security notes
- Narrow the `token.actions.githubusercontent.com:sub` condition to the specific repo and branch to reduce blast radius.
- Use least-privilege on the role's policy; replace `Resource: "*"` with specific ARNs where possible.
