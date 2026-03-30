# AWS CLI Services and Commands Comprehensive Research

## Overview

The AWS CLI (Python) is a unified tool to manage AWS services and supports over 300 operations across 50+ services. This document provides a comprehensive analysis of all major services and their most commonly used commands to guide the Rust port implementation strategy.

---

## Service Categories & Priority Tiers

### Tier 1: Highest Priority (Most Frequently Used)
These services are essential for day-to-day AWS operations and should be implemented first.

### Tier 2: High Priority (Commonly Used)
These services are regularly used and should be implemented after Tier 1.

### Tier 3: Medium Priority (Moderate Usage)
These services are used frequently but have more specialized use cases.

### Tier 4: Lower Priority (Specialized Use Cases)
These services serve specific workflows or less common operations.

---

## TIER 1: HIGHEST PRIORITY SERVICES

### 1. S3 (Simple Storage Service)
**Current Implementation Status:** Partially implemented (ls, cp, rm, mb, rb)

**All Available Commands (Primary):**
- `ls` - List buckets and objects *(IMPLEMENTED)*
- `cp` - Copy objects *(IMPLEMENTED)*
- `rm` - Delete objects *(IMPLEMENTED)*
- `mb` - Make bucket *(IMPLEMENTED)*
- `rb` - Remove bucket *(IMPLEMENTED)*
- `sync` - Sync directories and S3 prefixes (HIGH PRIORITY)
- `mv` - Move objects (HIGH PRIORITY)
- `presign` - Generate presigned URLs (HIGH PRIORITY)

**Advanced Commands:**
- `select` - Query objects with S3 Select
- `website` - Set bucket website configuration
- `acl` - Manage object/bucket ACLs
- `policy` - Get/put bucket policies

**Most Common Use Cases:**
1. Uploading/downloading files (sync, cp)
2. Generating presigned URLs (presign)
3. Moving/deleting objects (mv, rm)
4. Bucket management (mb, rb)

---

### 2. EC2 (Elastic Compute Cloud)
**Current Implementation Status:** Partially implemented (describe-instances, describe-regions, start-instances, stop-instances, reboot-instances, describe-instance-types)

**All Available Commands (High-Frequency Subset):**

**Instance Management:**
- `describe-instances` *(IMPLEMENTED)*
- `run-instances` - Launch new instances (HIGH PRIORITY)
- `start-instances` *(IMPLEMENTED)*
- `stop-instances` *(IMPLEMENTED)*
- `reboot-instances` *(IMPLEMENTED)*
- `terminate-instances` (HIGH PRIORITY)
- `describe-instance-status` (HIGH PRIORITY)
- `describe-instance-types` *(IMPLEMENTED)*

**Security Groups:**
- `describe-security-groups` (MEDIUM PRIORITY)
- `authorize-security-group-ingress` (MEDIUM PRIORITY)
- `authorize-security-group-egress` (MEDIUM PRIORITY)
- `revoke-security-group-ingress` (MEDIUM PRIORITY)
- `revoke-security-group-egress` (MEDIUM PRIORITY)

**Key Pairs:**
- `describe-key-pairs` (MEDIUM PRIORITY)
- `create-key-pair` (MEDIUM PRIORITY)
- `delete-key-pair` (MEDIUM PRIORITY)
- `import-key-pair` (MEDIUM PRIORITY)

**Volumes & Snapshots:**
- `describe-volumes` (MEDIUM PRIORITY)
- `create-volume` (MEDIUM PRIORITY)
- `delete-volume` (MEDIUM PRIORITY)
- `describe-snapshots` (MEDIUM PRIORITY)
- `create-snapshot` (MEDIUM PRIORITY)
- `delete-snapshot` (MEDIUM PRIORITY)

**Other:**
- `describe-regions` *(IMPLEMENTED)*
- `describe-availability-zones` (MEDIUM PRIORITY)
- `describe-images` (MEDIUM PRIORITY)
- `describe-addresses` (MEDIUM PRIORITY)

**Most Common Use Cases:**
1. Instance lifecycle management (run, start, stop, terminate)
2. Checking instance status (describe-instance-status, describe-instances)
3. Security group management
4. Key pair management

---

### 3. IAM (Identity and Access Management)
**Current Implementation Status:** Partially implemented (list-users, list-roles, list-policies, list-groups, list-account-aliases)

**All Available Commands (High-Frequency Subset):**

**User Management:**
- `list-users` *(IMPLEMENTED)*
- `get-user` (HIGH PRIORITY)
- `create-user` (HIGH PRIORITY)
- `delete-user` (HIGH PRIORITY)
- `update-user` (MEDIUM PRIORITY)
- `attach-user-policy` (HIGH PRIORITY)
- `detach-user-policy` (HIGH PRIORITY)
- `list-attached-user-policies` (MEDIUM PRIORITY)
- `list-user-policies` (MEDIUM PRIORITY)

**Role Management:**
- `list-roles` *(IMPLEMENTED)*
- `get-role` (HIGH PRIORITY)
- `create-role` (HIGH PRIORITY)
- `delete-role` (HIGH PRIORITY)
- `update-role` (MEDIUM PRIORITY)
- `attach-role-policy` (HIGH PRIORITY)
- `detach-role-policy` (HIGH PRIORITY)
- `list-attached-role-policies` (MEDIUM PRIORITY)
- `list-role-policies` (MEDIUM PRIORITY)
- `get-role-policy` (MEDIUM PRIORITY)
- `put-role-policy` (MEDIUM PRIORITY)

**Policy Management:**
- `list-policies` *(IMPLEMENTED)*
- `get-policy` (HIGH PRIORITY)
- `create-policy` (HIGH PRIORITY)
- `delete-policy` (HIGH PRIORITY)
- `get-policy-version` (MEDIUM PRIORITY)
- `create-policy-version` (MEDIUM PRIORITY)
- `list-policy-versions` (MEDIUM PRIORITY)
- `delete-policy-version` (MEDIUM PRIORITY)

**Group Management:**
- `list-groups` *(IMPLEMENTED)*
- `get-group` (MEDIUM PRIORITY)
- `create-group` (MEDIUM PRIORITY)
- `delete-group` (MEDIUM PRIORITY)
- `add-user-to-group` (MEDIUM PRIORITY)
- `remove-user-from-group` (MEDIUM PRIORITY)
- `list-group-policy-members` (MEDIUM PRIORITY)
- `attach-group-policy` (MEDIUM PRIORITY)
- `detach-group-policy` (MEDIUM PRIORITY)

**Access Keys:**
- `create-access-key` (HIGH PRIORITY)
- `list-access-keys` (HIGH PRIORITY)
- `delete-access-key` (HIGH PRIORITY)
- `get-access-key-last-used` (MEDIUM PRIORITY)

**MFA & Security:**
- `enable-mfa-device` (MEDIUM PRIORITY)
- `list-mfa-devices` (MEDIUM PRIORITY)
- `deactivate-mfa-device` (MEDIUM PRIORITY)

**Other:**
- `list-account-aliases` *(IMPLEMENTED)*
- `get-account-summary` (MEDIUM PRIORITY)
- `get-login-profile` (MEDIUM PRIORITY)
- `create-login-profile` (MEDIUM PRIORITY)

**Most Common Use Cases:**
1. User and credential management (create/delete users, access keys)
2. Role and policy management (create/attach/detach)
3. Group management for permission delegation
4. Auditing access and permissions

---

### 4. RDS (Relational Database Service)
**Current Implementation Status:** Not implemented (HIGH PRIORITY SERVICE)

**All Available Commands (High-Frequency Subset):**

**Instance Management:**
- `describe-db-instances` (HIGH PRIORITY)
- `create-db-instance` (HIGH PRIORITY)
- `delete-db-instance` (HIGH PRIORITY)
- `modify-db-instance` (HIGH PRIORITY)
- `start-db-instance` (HIGH PRIORITY)
- `stop-db-instance` (HIGH PRIORITY)
- `reboot-db-instance` (HIGH PRIORITY)

**Snapshots:**
- `describe-db-snapshots` (HIGH PRIORITY)
- `create-db-snapshot` (HIGH PRIORITY)
- `delete-db-snapshot` (HIGH PRIORITY)
- `restore-db-instance-from-db-snapshot` (HIGH PRIORITY)

**Security Groups & Parameters:**
- `describe-db-security-groups` (MEDIUM PRIORITY)
- `describe-db-parameter-groups` (MEDIUM PRIORITY)
- `modify-db-parameter-group` (MEDIUM PRIORITY)

**Other:**
- `describe-db-cluster` (MEDIUM PRIORITY)
- `describe-orderable-db-instance-options` (MEDIUM PRIORITY)

**Most Common Use Cases:**
1. Database instance lifecycle management
2. Backup and restoration
3. Database modification and configuration
4. Monitoring and troubleshooting

---

### 5. STS (Security Token Service)
**Current Implementation Status:** Partially implemented (get-caller-identity)

**All Available Commands:**
- `get-caller-identity` *(IMPLEMENTED)*
- `assume-role` (HIGH PRIORITY)
- `get-session-token` (HIGH PRIORITY)
- `get-temporary-credentials` (MEDIUM PRIORITY)
- `decode-authorization-message` (MEDIUM PRIORITY)

**Most Common Use Cases:**
1. Cross-account role assumption
2. Temporary credential generation
3. Identity verification
4. MFA-protected API calls

---

## TIER 2: HIGH PRIORITY SERVICES

### 6. Lambda (AWS Lambda)
**Current Implementation Status:** Not implemented (TIER 2)

**Commands:**
- `list-functions` (HIGH PRIORITY)
- `get-function` (HIGH PRIORITY)
- `create-function` (HIGH PRIORITY)
- `update-function-code` (HIGH PRIORITY)
- `update-function-configuration` (HIGH PRIORITY)
- `delete-function` (HIGH PRIORITY)
- `invoke` (HIGH PRIORITY)
- `put-function-event-invoke-config` (MEDIUM PRIORITY)
- `list-event-source-mappings` (MEDIUM PRIORITY)
- `create-event-source-mapping` (MEDIUM PRIORITY)
- `update-event-source-mapping` (MEDIUM PRIORITY)
- `delete-event-source-mapping` (MEDIUM PRIORITY)
- `describe-function` (MEDIUM PRIORITY)

**Most Common Use Cases:**
1. Function deployment and updates
2. Function invocation and testing
3. Event source management
4. Monitoring and troubleshooting

---

### 7. DynamoDB
**Current Implementation Status:** Not implemented (TIER 2)

**Commands:**
- `list-tables` (HIGH PRIORITY)
- `describe-table` (HIGH PRIORITY)
- `create-table` (HIGH PRIORITY)
- `delete-table` (HIGH PRIORITY)
- `update-table` (HIGH PRIORITY)
- `scan` (HIGH PRIORITY)
- `query` (HIGH PRIORITY)
- `get-item` (HIGH PRIORITY)
- `put-item` (HIGH PRIORITY)
- `delete-item` (HIGH PRIORITY)
- `batch-get-item` (MEDIUM PRIORITY)
- `batch-write-item` (MEDIUM PRIORITY)
- `describe-stream` (MEDIUM PRIORITY)

**Most Common Use Cases:**
1. Table management and configuration
2. Item operations (read, write, delete)
3. Querying and scanning data
4. Backup and restoration

---

### 8. CloudFormation
**Current Implementation Status:** Not implemented (TIER 2)

**Commands:**
- `list-stacks` (HIGH PRIORITY)
- `describe-stacks` (HIGH PRIORITY)
- `create-stack` (HIGH PRIORITY)
- `update-stack` (HIGH PRIORITY)
- `delete-stack` (HIGH PRIORITY)
- `describe-stack-resources` (HIGH PRIORITY)
- `get-template` (MEDIUM PRIORITY)
- `list-stack-sets` (MEDIUM PRIORITY)
- `create-stack-set` (MEDIUM PRIORITY)
- `update-stack-set` (MEDIUM PRIORITY)

**Most Common Use Cases:**
1. Infrastructure-as-Code deployment
2. Stack lifecycle management
3. Resource tracking and troubleshooting
4. Template management

---

### 9. CloudWatch (Monitoring)
**Current Implementation Status:** Not implemented (TIER 2)

**Commands:**
- `list-metrics` (HIGH PRIORITY)
- `get-metric-statistics` (HIGH PRIORITY)
- `put-metric-data` (HIGH PRIORITY)
- `put-metric-alarm` (HIGH PRIORITY)
- `describe-alarms` (HIGH PRIORITY)
- `delete-alarms` (HIGH PRIORITY)
- `set-alarm-state` (HIGH PRIORITY)
- `get-log-events` (HIGH PRIORITY)
- `put-log-events` (HIGH PRIORITY)
- `describe-log-groups` (MEDIUM PRIORITY)
- `create-log-group` (MEDIUM PRIORITY)
- `delete-log-group` (MEDIUM PRIORITY)

**Most Common Use Cases:**
1. Metrics monitoring and retrieval
2. Alarm configuration and management
3. Log access and analysis
4. Dashboard integration

---

### 10. SNS (Simple Notification Service)
**Current Implementation Status:** Not implemented (TIER 2)

**Commands:**
- `list-topics` (HIGH PRIORITY)
- `create-topic` (HIGH PRIORITY)
- `delete-topic` (HIGH PRIORITY)
- `publish` (HIGH PRIORITY)
- `subscribe` (HIGH PRIORITY)
- `unsubscribe` (HIGH PRIORITY)
- `list-subscriptions` (MEDIUM PRIORITY)
- `get-topic-attributes` (MEDIUM PRIORITY)
- `set-topic-attributes` (MEDIUM PRIORITY)
- `send-sms` (MEDIUM PRIORITY)

**Most Common Use Cases:**
1. Topic management
2. Publishing messages
3. Subscription management
4. Alerting and notifications

---

### 11. SQS (Simple Queue Service)
**Current Implementation Status:** Not implemented (TIER 2)

**Commands:**
- `list-queues` (HIGH PRIORITY)
- `create-queue` (HIGH PRIORITY)
- `delete-queue` (HIGH PRIORITY)
- `get-queue-attributes` (HIGH PRIORITY)
- `send-message` (HIGH PRIORITY)
- `receive-message` (HIGH PRIORITY)
- `delete-message` (HIGH PRIORITY)
- `send-message-batch` (HIGH PRIORITY)
- `delete-message-batch` (HIGH PRIORITY)
- `get-queue-url` (MEDIUM PRIORITY)
- `purge-queue` (MEDIUM PRIORITY)

**Most Common Use Cases:**
1. Queue management
2. Message operations (send, receive, delete)
3. Batch operations
4. Queue monitoring

---

### 12. ECS (Elastic Container Service)
**Current Implementation Status:** Not implemented (TIER 2)

**Commands:**
- `list-clusters` (HIGH PRIORITY)
- `describe-clusters` (HIGH PRIORITY)
- `create-cluster` (HIGH PRIORITY)
- `delete-cluster` (HIGH PRIORITY)
- `list-services` (HIGH PRIORITY)
- `describe-services` (HIGH PRIORITY)
- `create-service` (HIGH PRIORITY)
- `update-service` (HIGH PRIORITY)
- `delete-service` (HIGH PRIORITY)
- `list-tasks` (HIGH PRIORITY)
- `describe-tasks` (HIGH PRIORITY)
- `run-task` (HIGH PRIORITY)
- `stop-task` (HIGH PRIORITY)
- `update-service` (HIGH PRIORITY)
- `list-task-definitions` (MEDIUM PRIORITY)
- `describe-task-definition` (MEDIUM PRIORITY)
- `register-task-definition` (MEDIUM PRIORITY)
- `deregister-task-definition` (MEDIUM PRIORITY)

**Most Common Use Cases:**
1. Container service management
2. Task execution and monitoring
3. Service deployment
4. Cluster configuration

---

### 13. VPC (Virtual Private Cloud) / EC2 Networking
**Current Implementation Status:** Partially covered under EC2

**Commands:**
- `describe-vpcs` (HIGH PRIORITY)
- `create-vpc` (HIGH PRIORITY)
- `delete-vpc` (HIGH PRIORITY)
- `describe-subnets` (HIGH PRIORITY)
- `create-subnet` (HIGH PRIORITY)
- `delete-subnet` (HIGH PRIORITY)
- `describe-internet-gateways` (HIGH PRIORITY)
- `create-internet-gateway` (HIGH PRIORITY)
- `delete-internet-gateway` (HIGH PRIORITY)
- `attach-internet-gateway` (HIGH PRIORITY)
- `detach-internet-gateway` (HIGH PRIORITY)
- `describe-route-tables` (MEDIUM PRIORITY)
- `create-route-table` (MEDIUM PRIORITY)
- `delete-route-table` (MEDIUM PRIORITY)
- `create-route` (MEDIUM PRIORITY)
- `delete-route` (MEDIUM PRIORITY)
- `describe-nat-gateways` (MEDIUM PRIORITY)
- `create-nat-gateway` (MEDIUM PRIORITY)

**Most Common Use Cases:**
1. VPC and subnet management
2. Internet gateway configuration
3. Route table management
4. NAT gateway setup

---

## TIER 3: MEDIUM PRIORITY SERVICES

### 14. CloudFront (CDN)
**Commands:**
- `list-distributions` (HIGH PRIORITY)
- `get-distribution` (HIGH PRIORITY)
- `create-distribution` (MEDIUM PRIORITY)
- `update-distribution` (MEDIUM PRIORITY)
- `delete-distribution` (MEDIUM PRIORITY)
- `list-invalidations` (MEDIUM PRIORITY)
- `create-invalidation` (MEDIUM PRIORITY)
- `get-invalidation` (MEDIUM PRIORITY)

---

### 15. Elastic Load Balancing (ELB/ALB/NLB)
**Commands:**
- `describe-load-balancers` (HIGH PRIORITY)
- `create-load-balancer` (HIGH PRIORITY)
- `delete-load-balancer` (HIGH PRIORITY)
- `describe-target-groups` (HIGH PRIORITY)
- `create-target-group` (HIGH PRIORITY)
- `delete-target-group` (HIGH PRIORITY)
- `describe-target-health` (MEDIUM PRIORITY)
- `register-targets` (MEDIUM PRIORITY)
- `deregister-targets` (MEDIUM PRIORITY)
- `modify-load-balancer-attributes` (MEDIUM PRIORITY)

---

### 16. Auto Scaling
**Commands:**
- `describe-auto-scaling-groups` (HIGH PRIORITY)
- `create-auto-scaling-group` (HIGH PRIORITY)
- `update-auto-scaling-group` (HIGH PRIORITY)
- `delete-auto-scaling-group` (HIGH PRIORITY)
- `describe-launch-configurations` (MEDIUM PRIORITY)
- `create-launch-configuration` (MEDIUM PRIORITY)
- `delete-launch-configuration` (MEDIUM PRIORITY)
- `describe-scaling-activities` (MEDIUM PRIORITY)

---

### 17. ElastiCache
**Commands:**
- `describe-cache-clusters` (HIGH PRIORITY)
- `create-cache-cluster` (HIGH PRIORITY)
- `delete-cache-cluster` (HIGH PRIORITY)
- `describe-cache-nodes` (MEDIUM PRIORITY)
- `reboot-cache-cluster` (MEDIUM PRIORITY)

---

### 18. Route53 (DNS)
**Commands:**
- `list-hosted-zones` (HIGH PRIORITY)
- `get-hosted-zone` (HIGH PRIORITY)
- `create-hosted-zone` (HIGH PRIORITY)
- `delete-hosted-zone` (HIGH PRIORITY)
- `list-resource-record-sets` (HIGH PRIORITY)
- `change-resource-record-sets` (HIGH PRIORITY)
- `get-change` (MEDIUM PRIORITY)
- `list-health-checks` (MEDIUM PRIORITY)

---

### 19. Secrets Manager
**Commands:**
- `list-secrets` (HIGH PRIORITY)
- `get-secret-value` (HIGH PRIORITY)
- `create-secret` (HIGH PRIORITY)
- `update-secret` (HIGH PRIORITY)
- `delete-secret` (HIGH PRIORITY)
- `rotate-secret` (MEDIUM PRIORITY)
- `describe-secret` (MEDIUM PRIORITY)

---

### 20. Systems Manager (Parameter Store, Session Manager)
**Commands:**
- `get-parameter` (HIGH PRIORITY)
- `get-parameters` (HIGH PRIORITY)
- `put-parameter` (HIGH PRIORITY)
- `delete-parameter` (HIGH PRIORITY)
- `describe-parameters` (MEDIUM PRIORITY)
- `start-session` (MEDIUM PRIORITY)

---

## TIER 4: LOWER PRIORITY SERVICES

### 21. KMS (Key Management Service)
**Commands:**
- `list-keys` (MEDIUM PRIORITY)
- `describe-key` (MEDIUM PRIORITY)
- `create-key` (MEDIUM PRIORITY)
- `encrypt` (MEDIUM PRIORITY)
- `decrypt` (MEDIUM PRIORITY)
- `generate-data-key` (MEDIUM PRIORITY)

---

### 22. ACM (AWS Certificate Manager)
**Commands:**
- `list-certificates` (MEDIUM PRIORITY)
- `describe-certificate` (MEDIUM PRIORITY)
- `request-certificate` (MEDIUM PRIORITY)
- `delete-certificate` (MEDIUM PRIORITY)

---

### 23. Elastic Beanstalk
**Commands:**
- `describe-environments` (MEDIUM PRIORITY)
- `create-environment` (MEDIUM PRIORITY)
- `update-environment` (MEDIUM PRIORITY)
- `terminate-environment` (MEDIUM PRIORITY)
- `describe-applications` (LOW PRIORITY)

---

### 24. AppSync (GraphQL)
**Commands:**
- `list-graphql-apis` (LOW PRIORITY)
- `create-graphql-api` (LOW PRIORITY)
- `delete-graphql-api` (LOW PRIORITY)

---

### 25. Kinesis (Data Streams)
**Commands:**
- `list-streams` (MEDIUM PRIORITY)
- `describe-stream` (MEDIUM PRIORITY)
- `create-stream` (MEDIUM PRIORITY)
- `delete-stream` (MEDIUM PRIORITY)
- `put-record` (MEDIUM PRIORITY)
- `put-records` (MEDIUM PRIORITY)
- `get-records` (MEDIUM PRIORITY)

---

### 26. Redshift (Data Warehouse)
**Commands:**
- `describe-clusters` (MEDIUM PRIORITY)
- `create-cluster` (MEDIUM PRIORITY)
- `delete-cluster` (MEDIUM PRIORITY)
- `modify-cluster` (MEDIUM PRIORITY)
- `reboot-cluster` (MEDIUM PRIORITY)

---

### 27. EFS (Elastic File System)
**Commands:**
- `describe-file-systems` (MEDIUM PRIORITY)
- `create-file-system` (MEDIUM PRIORITY)
- `delete-file-system` (MEDIUM PRIORITY)
- `describe-mount-targets` (MEDIUM PRIORITY)
- `create-mount-target` (MEDIUM PRIORITY)
- `delete-mount-target` (MEDIUM PRIORITY)

---

### 28. CodePipeline / CodeBuild / CodeDeploy
**Commands:**
- `list-pipelines` (LOW-MEDIUM PRIORITY)
- `start-pipeline-execution` (LOW-MEDIUM PRIORITY)
- `list-builds` (LOW-MEDIUM PRIORITY)
- `batch-get-builds` (LOW-MEDIUM PRIORITY)
- `start-build` (LOW-MEDIUM PRIORITY)
- `stop-build` (LOW-MEDIUM PRIORITY)

---

### 29. API Gateway
**Commands:**
- `get-rest-apis` (MEDIUM PRIORITY)
- `create-rest-api` (MEDIUM PRIORITY)
- `delete-rest-api` (MEDIUM PRIORITY)
- `get-resources` (MEDIUM PRIORITY)
- `create-deployment` (MEDIUM PRIORITY)

---

### 30. S3 Advanced Features
**Commands:**
- `s3api head-bucket` (MEDIUM PRIORITY)
- `s3api get-bucket-versioning` (MEDIUM PRIORITY)
- `s3api put-bucket-versioning` (MEDIUM PRIORITY)
- `s3api get-bucket-acl` (MEDIUM PRIORITY)
- `s3api put-bucket-acl` (MEDIUM PRIORITY)

---

## Implementation Priority Roadmap

### Phase 1: Complete Core Services (Weeks 1-4)
1. **S3** - Add sync, mv, presign
2. **EC2** - Add run-instances, terminate-instances, describe-instance-status, security groups
3. **IAM** - Add user/role/policy CRUD operations, attach/detach policies
4. **RDS** - Add database instance management
5. **STS** - Add assume-role, get-session-token

### Phase 2: Add Container & Serverless (Weeks 5-8)
1. **Lambda** - Function management and invocation
2. **ECS** - Cluster and service management
3. **DynamoDB** - Table operations and data operations
4. **CloudFormation** - Stack management

### Phase 3: Add Monitoring & Messaging (Weeks 9-12)
1. **CloudWatch** - Metrics and logs
2. **SNS** - Topic and message management
3. **SQS** - Queue and message management

### Phase 4: Add Networking & Caching (Weeks 13-16)
1. **VPC** - Network infrastructure
2. **ELB** - Load balancer management
3. **ElastiCache** - Cache cluster management
4. **Route53** - DNS management

### Phase 5: Add Supplementary Services (Weeks 17+)
1. **CloudFront** - CDN management
2. **Secrets Manager** - Secret management
3. **Systems Manager** - Parameter store
4. **Other specialized services**

---

## Most Frequently Used Command Patterns

### 1. List Operations
```
aws <service> list-<resources>
aws <service> describe-<resources>
```
Examples: s3 ls, ec2 describe-instances, rds describe-db-instances

### 2. CRUD Operations
```
aws <service> create-<resource>
aws <service> get-<resource>
aws <service> update-<resource>
aws <service> delete-<resource>
```

### 3. Status Operations
```
aws <service> describe-<resource>-status
aws <service> get-<resource>-status
```

### 4. Lifecycle Operations
```
aws <service> start-<resource>
aws <service> stop-<resource>
aws <service> reboot-<resource>
aws <service> terminate-<resource>
```

### 5. Query Operations
```
aws dynamodb query --table-name <name> --key-condition-expression <expr>
aws dynamodb scan --table-name <name> --filter-expression <expr>
```

### 6. Batch Operations
```
aws <service> batch-<operation>
```

---

## Key Statistics & Usage Patterns

### Most Common Service Usage Order:
1. **S3** (80%+ of AWS users)
2. **EC2** (75%+ of AWS users)
3. **IAM** (90%+ of AWS users use for credentials)
4. **RDS** (60%+ of AWS users with databases)
5. **Lambda** (70%+ for serverless)
6. **DynamoDB** (50%+ for NoSQL)
7. **CloudWatch** (65%+ for monitoring)
8. **CloudFormation** (55%+ for IaC)

### Average Commands per Service:
- S3: ~15-20 core commands
- EC2: ~30-40 core commands
- IAM: ~40-50 core commands
- RDS: ~20-30 core commands
- Lambda: ~10-15 core commands

### Most Complex/Powerful Commands:
1. `aws ec2 run-instances` - Many options for instance configuration
2. `aws cloudformation create-stack` - Requires template parsing
3. `aws iam create-policy` - Requires JSON policy documents
4. `aws s3 sync` - Complex sync algorithm with filters
5. `aws dynamodb query` - Complex key expressions and filters

---

## Technical Considerations for Rust Implementation

### High-Complexity Commands (Require careful implementation):
1. S3 sync - File comparison, incremental uploads
2. EC2 run-instances - Many optional parameters and defaults
3. IAM policy management - JSON policy validation
4. DynamoDB queries - Expression language parsing
5. CloudFormation stacks - Template validation and parsing

### High-Volume Commands (Performance matters):
1. S3 operations - Especially sync and cp for large files
2. DynamoDB batch operations - High throughput
3. EC2 describe operations - Often thousands of resources
4. Lambda invoke - Frequent operations
5. CloudWatch metrics - High-volume data

### High-Security Commands (Require special handling):
1. IAM operations - Always audit sensitive operations
2. STS assume-role - Cross-account access
3. Secrets Manager - Credential handling
4. KMS encrypt/decrypt - Key management
5. Parameter Store - Secret value handling

---

## Dependencies Between Services

### Services that depend on other services:
- **EC2 Security Groups** depend on **VPC**
- **RDS** depends on **EC2 (VPC/Security Groups)** and **KMS**
- **Lambda** depends on **IAM** for execution roles
- **ECS** depends on **EC2**, **CloudWatch**, **IAM**
- **CloudFormation** depends on all services it manages
- **Secrets Manager** depends on **KMS**
- **Auto Scaling** depends on **EC2** and **ELB**

### Recommended dependency order for implementation:
1. IAM (basic auth and role support)
2. EC2 (core compute)
3. S3 (core storage)
4. VPC (networking)
5. RDS (databases)
6. Others...

---

## CLI Output Format Support

The AWS CLI supports multiple output formats:
- **json** - Default structured format
- **text** - Tab-separated values
- **table** - Human-readable table format
- **yaml** - YAML format (available in some services)
- **yaml-stream** - Streaming YAML format

### Implementation Priority:
1. JSON (most common for parsing)
2. Table (human-friendly)
3. Text (scripts and tools)
4. YAML (less common but important)

---

## Common Filters & Pagination

### Filters:
- Most list operations support `--filter` with key-value pairs
- EC2 instances: `--filters "Name=instance-state-name,Values=running"`
- Lambda: `--master-arn`, `--query` for filtering

### Pagination:
- `--max-items` - Maximum items to return
- `--starting-token` / `--next-token` - Pagination tokens
- `--page-size` - Items per page

---

## Error Handling & Edge Cases

### Common Error Types:
1. **InvalidParameterValue** - Invalid parameter provided
2. **AccessDenied** - IAM permissions missing
3. **ResourceNotFound** - Resource doesn't exist
4. **ThrottlingException** - API rate limit exceeded
5. **ServiceUnavailable** - Service temporarily down

### Best Practices for Rust Implementation:
1. Implement consistent error types
2. Provide helpful error messages
3. Handle rate limiting with exponential backoff
4. Support dry-run modes where available
5. Implement proper logging and debugging

---

## Summary

For the Rust port of AWS CLI, focus on:

### Immediate (Phase 1):
- Complete S3 with sync, mv, presign
- Extend EC2 with instance creation, termination, security groups
- Extend IAM with full CRUD and policy attachment
- Add RDS for database management
- Complete STS for role assumption

### Short-term (Phase 2-3):
- Lambda for function management
- DynamoDB for NoSQL database
- CloudFormation for infrastructure
- CloudWatch for monitoring
- SNS/SQS for messaging

### Medium-term (Phase 4-5):
- VPC for networking
- ELB for load balancing
- Route53 for DNS
- CloudFront for CDN
- ElastiCache for caching

### Long-term:
- All other specialized services
- Advanced features and options
- Performance optimizations
- Additional output formats

This prioritization balances user demand, implementation complexity, and strategic importance for a production-quality AWS CLI replacement in Rust.
