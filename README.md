# aws_cli

A Rust port of the official AWS CLI, providing a fast, single-binary alternative to the Python-based AWS CLI. Built with the official AWS SDK for Rust.

## Project Status

This is an **active implementation** of core AWS CLI functionality in Rust. Currently at **56.43%** feature parity with the Python AWS CLI.
Current **Phase 1 progress** is **84.07%**. Phase 1 focuses on a smaller core subset of commands, which is why its progress percentage is higher than overall parity.

### Currently Implemented (9 services, ~145 commands)

#### ✓ Configure
- [x] `configure` - Interactive configuration
- [x] `configure get` - Get configuration value
- [x] `configure list` - List all configuration

#### ✓ S3 (15/15 core commands - 100%)
- [x] `ls` - List buckets/objects
- [x] `cp` - Copy files
- [x] `sync` - Sync directories
- [x] `mv` - Move objects
- [x] `presign` - Generate presigned URLs
- [x] `rm` - Remove objects
- [x] `mb` - Make bucket
- [x] `rb` - Remove bucket
- [x] `website` - Bucket website configuration (set/disable)
- [x] `get-acl` - Get bucket/object ACL
- [x] `put-acl` - Set bucket/object ACL (canned)
- [x] `get-bucket-policy` - Get bucket policy JSON
- [x] `put-bucket-policy` - Set bucket policy JSON
- [x] `delete-bucket-policy` - Remove bucket policy
- [x] `list-object-versions` - List object versions for a prefix

#### ✓ EC2 (22/40 core commands - 55%)
- [x] `describe-instances` - List instances
- [x] `describe-regions` - List regions
- [x] `start-instances` - Start instances
- [x] `stop-instances` - Stop instances
- [x] `reboot-instances` - Reboot instances
- [x] `terminate-instances` - Terminate instances
- [x] `describe-instance-types` - List instance types
- [x] `describe-instance-status` - Show instance/system status
- [x] `describe-security-groups` - List security groups
- [x] `describe-key-pairs` - List key pairs
- [x] `create-key-pair` - Create a key pair
- [x] `delete-key-pair` - Delete a key pair
- [x] `describe-volumes` - List EBS volumes
- [x] `describe-snapshots` - List EBS snapshots
- [x] `run-instances` - Launch instances
- [x] `authorize-security-group-ingress` - Add ingress rule
- [x] `authorize-security-group-egress` - Add egress rule
- [x] `import-key-pair` - Import a public key
- [x] `create-volume` - Create an EBS volume
- [x] `delete-volume` - Delete an EBS volume
- [x] `create-snapshot` - Create an EBS snapshot
- [x] `delete-snapshot` - Delete an EBS snapshot
- [ ] Additional volume & snapshot lifecycle (attach/detach, copy, etc.)

#### ✓ IAM (50/50 core commands - 100%)
- [x] `create-role` - Create IAM roles
- [x] `create-user` - Create IAM users
- [x] `delete-user` - Delete IAM users
- [x] `get-user` - Get IAM user details
- [x] `list-users` - List users
- [x] `list-roles` - List roles
- [x] `get-role` - Get IAM role details
- [x] `delete-role` - Delete IAM role
- [x] `get-policy` - Get IAM policy details
- [x] `delete-policy` - Delete IAM policy
- [x] `create-policy` - Create IAM policy
- [x] `list-policies` - List policies
- [x] `list-groups` - List groups
- [x] `create-group` - Create IAM groups
- [x] `get-group` - Get IAM group details
- [x] `delete-group` - Delete IAM group
- [x] `list-attached-group-policies` - List attached group policies
- [x] `attach-group-policy` - Attach managed policy to group
- [x] `detach-group-policy` - Detach managed policy from group
- [x] `add-user-to-group` - Add user to group
- [x] `remove-user-from-group` - Remove user from group
- [x] `list-groups-for-user` - List groups for user
- [x] `attach-user-policy` - Attach managed policy to user
- [x] `detach-user-policy` - Detach managed policy from user
- [x] `attach-role-policy` - Attach managed policy to role
- [x] `detach-role-policy` - Detach managed policy from role
- [x] `list-attached-user-policies` - List attached policies for user
- [x] `list-attached-role-policies` - List attached policies for role
- [x] `list-user-policies` - List inline policy names for user
- [x] `list-role-policies` - List inline policy names for role
- [x] `list-group-policies` - List inline policy names for group
- [x] `get-user-policy` - Get inline policy document for user
- [x] `get-role-policy` - Get inline policy document for role
- [x] `get-group-policy` - Get inline policy document for group
- [x] `put-user-policy` - Add or update inline policy document for user
- [x] `delete-user-policy` - Delete inline policy document from user
- [x] `put-role-policy` - Add or update inline policy document for role
- [x] `delete-role-policy` - Delete inline policy document from role
- [x] `put-group-policy` - Add or update inline policy document for group
- [x] `delete-group-policy` - Delete inline policy document from group
- [x] `create-access-key` - Create an access key for an IAM user
- [x] `list-access-keys` - List access keys for an IAM user
- [x] `update-access-key` - Update an IAM access key status
- [x] `delete-access-key` - Delete an IAM access key
- [x] `create-login-profile` - Create a console login profile for an IAM user
- [x] `get-login-profile` - Get login profile metadata for an IAM user
- [x] `update-login-profile` - Update an IAM user login profile
- [x] `delete-login-profile` - Delete an IAM user login profile
- [x] `update-assume-role-policy` - Update trust policy document for an IAM role
- [x] `list-account-aliases` - List account aliases

#### ✓ STS (4/4 core commands - 100%)
- [x] `get-caller-identity` - Get caller identity
- [x] `assume-role` - Assume role
- [x] `get-session-token` - Get session token
- [x] `decode-authorization-message` - Decode error message

#### ✓ SSO (4/4 core commands - 100%)
- [x] `login`
- [x] `list-accounts`
- [x] `list-account-roles`
- [x] `get-role-credentials`

#### ✓ RDS (11/11 core commands - 100%)
- [x] `describe-db-instances`
- [x] `create-db-instance`
- [x] `delete-db-instance`
- [x] `modify-db-instance`
- [x] `start-db-instance`
- [x] `stop-db-instance`
- [x] `reboot-db-instance`
- [x] `describe-db-snapshots`
- [x] `create-db-snapshot`
- [x] `delete-db-snapshot`
- [x] `restore-db-instance-from-db-snapshot`

#### ✓ Lambda (10/10 core commands - 100%)
- [x] `create-function`
- [x] `list-functions`
- [x] `get-function`
- [x] `delete-function`
- [x] `publish-version`
- [x] `invoke`
- [x] `list-event-source-mappings`
- [x] `update-function-code`
- [x] `update-function-configuration`
- [x] `put-function-event-invoke-config`

#### ✓ DynamoDB (9/12 core commands - 75%)
- [x] `list-tables`
- [x] `describe-table`
- [x] `create-table`
- [x] `delete-table`
- [x] `update-table`
- [x] `get-item`
- [x] `put-item`
- [x] `delete-item`
- [x] `scan`

### Not Yet Implemented (42+ services)

**Critical Priority (Phase 1 - 4-5 weeks)**
- Complete S3, EC2, IAM, STS to 80%+ coverage
- [x] Include AWS SSO for common auth/account workflows

**High Priority (Phase 2 - Weeks 5-8)**
- [x] Lambda - Serverless functions (10+ commands)
- [ ] DynamoDB - NoSQL database (12+ commands)
- [ ] CloudFormation - Infrastructure as code (8+ commands)

**Medium Priority (Phase 3 - Weeks 9-12)**
- [ ] CloudWatch - Monitoring & logs (9+ commands)
- [ ] SNS - Notifications (7+ commands)
- [ ] SQS - Message queues (9+ commands)

**Additional Services (Phase 4+)**
- [ ] VPC, ELB, Route53 - Networking
- [ ] ECS - Containers
- [ ] ElastiCache - Caching
- [ ] Auto Scaling - Auto scaling
- [ ] Secrets Manager - Secret management
- [ ] Systems Manager - Operations
- [ ] And 30+ more services

## Implementation Roadmap

### Phase 1: Core Services MVP (Weeks 1-4) ← **CURRENT**
**Target:** 50% of typical AWS CLI workflows
- Complete S3, EC2, IAM, STS to 80%+ coverage
- Include AWS SSO core account/role commands
- Implement RDS basic operations
- ~40 new commands, ~100-150 hours

**Phase 1 progress snapshot (current):**
- S3: 15/15 (remaining 0)
- EC2: 22/40 (remaining 18)
- IAM: 50/50 (remaining 0)
- STS: 4/4 (remaining 0)
- SSO: 4/4 (remaining 0)

### Phase 2: Serverless & Data (Weeks 5-8)
**Target:** 70% of typical workflows
- Lambda, DynamoDB, CloudFormation
- ~30 new commands, ~80-100 hours

### Phase 3: Monitoring & Messaging (Weeks 9-12)
**Target:** 80% of typical workflows
- CloudWatch, SNS, SQS
- ~25 new commands, ~60-80 hours

### Phase 4+: Complete Parity (Weeks 13+)
**Target:** 100% feature parity
- VPC, ELB, Route53, and all remaining services
- ~150+ new commands, ~200+ hours

## Advantages Over Python AWS CLI

1. **Performance** - Faster startup time (< 1s vs 2-5s)
2. **Distribution** - Single binary, zero dependencies
3. **Memory** - Lower memory footprint (< 50MB vs 100MB+)
4. **Containers** - Ideal for containerized environments
5. **Windows** - Better native Windows experience

## Usage Examples

```bash
# Configure credentials
aws configure --aws-access-key-id AKIA... --region us-west-2

# S3 operations
aws s3 ls
aws s3 cp file.txt s3://my-bucket/
aws s3 ls s3://my-bucket/ --recursive

# EC2 operations
aws ec2 describe-instances
aws ec2 start-instances i-1234567890abcdef0
aws ec2 describe-regions

# IAM operations
aws iam list-users
aws iam list-roles --path-prefix /admin/
aws iam list-policies --scope aws --only-attached

# Verify credentials
aws sts get-caller-identity
```

## Architecture

- **Binary name:** `aws`
- **Package name:** `aws_cli`
- **Language:** Rust (2021 edition)
- **CLI framework:** clap (derive API)
- **Async runtime:** tokio
- **AWS SDK:** Official AWS SDK for Rust v1

### Project Structure

```
src/
├── main.rs           # CLI entry point, argument parsing
├── lib.rs            # Library exports
├── config.rs         # Configuration & credentials (~/.aws)
├── error.rs          # Error types
└── commands/
    ├── mod.rs        # Command module exports
    ├── s3.rs         # S3 command handlers
    ├── ec2.rs        # EC2 command handlers
    ├── iam.rs        # IAM command handlers
    ├── sts.rs        # STS command handlers
    ├── rds.rs        # RDS command handlers
    ├── lambda.rs     # Lambda command handlers
    └── dynamodb.rs   # DynamoDB command handlers
```

## Development Status

**Research Phase:** ✅ Complete
**Phase 1 Implementation:** 🚧 In Progress (Week 1)
**Test Coverage:** 🔜 Coming Soon
**Documentation:** 🔜 Coming Soon

For detailed implementation status, see:
- `QUICK_REFERENCE.md` - Quick reference card
- `IMPLEMENTATION_CHECKLIST.md` - Detailed checklist
- `RESEARCH_EXECUTIVE_SUMMARY.md` - Strategic overview

## Contributing

This is an active development project aiming for feature parity with the Python AWS CLI. Priority is being given to the most commonly used services and commands (S3, EC2, IAM, RDS, Lambda, etc.).

## License

This project uses the AWS SDK for Rust and follows AWS service conventions. Check LICENSE file for details.
