# AWS CLI Command Patterns & Common Usage Examples

## Service Command Patterns

### Pattern 1: List/Describe Operations

**Structure:**
```bash
aws <service> list-<resources> [OPTIONS]
aws <service> describe-<resources> [OPTIONS]
```

**Examples:**

```bash
# S3
aws s3 ls
aws s3 ls s3://my-bucket/
aws s3 ls s3://my-bucket/prefix --recursive

# EC2
aws ec2 describe-instances
aws ec2 describe-instances --instance-ids i-1234567890abcdef0
aws ec2 describe-regions

# IAM
aws iam list-users
aws iam list-roles
aws iam list-policies --scope local

# RDS
aws rds describe-db-instances
aws rds describe-db-clusters

# Lambda
aws lambda list-functions
aws lambda get-function --function-name my-function

# DynamoDB
aws dynamodb list-tables
aws dynamodb describe-table --table-name my-table
```

**Common Options:**
- `--filters` - Filter results
- `--query` - JMESPath query for output
- `--output json|text|table` - Output format
- `--region` - AWS region

---

### Pattern 2: Create/Update Operations

**Structure:**
```bash
aws <service> create-<resource> --required-param value [OPTIONS]
aws <service> update-<resource> --resource-id value [OPTIONS]
```

**Examples:**

```bash
# S3
aws s3 mb s3://my-new-bucket

# EC2
aws ec2 run-instances --image-id ami-12345678 --instance-type t2.micro --key-name my-key

# IAM
aws iam create-user --user-name john
aws iam create-role --role-name my-role --assume-role-policy-document file://trust-policy.json

# RDS
aws rds create-db-instance --db-instance-identifier mydbinstance --db-instance-class db.t2.micro --engine mysql

# Lambda
aws lambda create-function --function-name my-function --runtime python3.11 --role arn:aws:iam::123456789012:role/lambda-role --handler index.handler --zip-file fileb://function.zip

# DynamoDB
aws dynamodb create-table --table-name my-table --attribute-definitions AttributeName=id,AttributeType=S --key-schema AttributeName=id,KeyType=HASH --billing-mode PAY_PER_REQUEST
```

**Common Options:**
- `--dry-run` - Preview changes without executing
- `--tags` - Add tags to resources
- Various service-specific parameters

---

### Pattern 3: Delete Operations

**Structure:**
```bash
aws <service> delete-<resource> --resource-id value
```

**Examples:**

```bash
# S3
aws s3 rm s3://my-bucket/my-object
aws s3 rb s3://my-bucket --force

# EC2
aws ec2 terminate-instances --instance-ids i-1234567890abcdef0

# IAM
aws iam delete-user --user-name john
aws iam delete-role --role-name my-role

# RDS
aws rds delete-db-instance --db-instance-identifier mydbinstance --skip-final-snapshot

# Lambda
aws lambda delete-function --function-name my-function

# DynamoDB
aws dynamodb delete-table --table-name my-table
```

**Common Options:**
- `--force` - Force deletion
- `--skip-final-snapshot` - Skip backup on deletion
- Various confirmation parameters

---

### Pattern 4: State Transition Operations

**Structure:**
```bash
aws <service> <action>-<resources> --resource-ids values
```

**Examples:**

```bash
# EC2 - Instance lifecycle
aws ec2 start-instances --instance-ids i-1234567890abcdef0
aws ec2 stop-instances --instance-ids i-1234567890abcdef0
aws ec2 reboot-instances --instance-ids i-1234567890abcdef0

# RDS - Database lifecycle
aws rds start-db-instance --db-instance-identifier mydbinstance
aws rds stop-db-instance --db-instance-identifier mydbinstance
aws rds reboot-db-instance --db-instance-identifier mydbinstance

# Lambda - Publishing versions
aws lambda publish-version --function-name my-function

# DynamoDB - Stream operations
aws dynamodb enable-stream-specification --table-name my-table --stream-specification StreamEnabled=true,StreamViewType=NEW_AND_OLD_IMAGES
```

**Common Options:**
- `--force` - Force the operation
- `--query` - Filter response output

---

### Pattern 5: Attach/Detach Operations (Relationships)

**Structure:**
```bash
aws <service> attach-<resource>-<policy> --<resource>-name value --policy-arn value
aws <service> detach-<resource>-<policy> --<resource>-name value --policy-arn value
```

**Examples:**

```bash
# IAM - User policies
aws iam attach-user-policy --user-name john --policy-arn arn:aws:iam::aws:policy/ReadOnlyAccess
aws iam detach-user-policy --user-name john --policy-arn arn:aws:iam::aws:policy/ReadOnlyAccess

# IAM - Role policies
aws iam attach-role-policy --role-name my-role --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
aws iam detach-role-policy --role-name my-role --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole

# IAM - Group policies
aws iam attach-group-policy --group-name developers --policy-arn arn:aws:iam::aws:policy/DynamoDBFullAccess
aws iam detach-group-policy --group-name developers --policy-arn arn:aws:iam::aws:policy/DynamoDBFullAccess

# EC2 - Security group rules
aws ec2 authorize-security-group-ingress --group-id sg-123456 --protocol tcp --port 443 --cidr 0.0.0.0/0
aws ec2 revoke-security-group-ingress --group-id sg-123456 --protocol tcp --port 443 --cidr 0.0.0.0/0
```

**Common Options:**
- `--inline-policy-name` - For inline policies
- `--policy-document` - JSON policy document

---

### Pattern 6: Batch Operations

**Structure:**
```bash
aws <service> <action>-batch --<resources> [VALUES]
```

**Examples:**

```bash
# SQS - Batch send
aws sqs send-message-batch --queue-url https://sqs.us-east-1.amazonaws.com/123456789012/myqueue \
  --entries '[
    {
      "Id": "1",
      "MessageBody": "Message 1"
    },
    {
      "Id": "2",
      "MessageBody": "Message 2"
    }
  ]'

# SQS - Batch delete
aws sqs delete-message-batch --queue-url https://sqs.us-east-1.amazonaws.com/123456789012/myqueue \
  --entries '[
    {
      "Id": "1",
      "ReceiptHandle": "receipt-handle-1"
    }
  ]'

# DynamoDB - Batch get
aws dynamodb batch-get-item --request-items '{
  "my-table": {
    "Keys": [
      {"id": {"S": "key1"}},
      {"id": {"S": "key2"}}
    ]
  }
}'

# DynamoDB - Batch write
aws dynamodb batch-write-item --request-items '{
  "my-table": [
    {
      "PutRequest": {
        "Item": {"id": {"S": "key1"}, "data": {"S": "value1"}}
      }
    }
  ]
}'
```

**Common Options:**
- `--entries` - Array of items to process
- `--request-items` - Batch request specification

---

### Pattern 7: Query/Filter Operations

**Structure:**
```bash
aws <service> query-<resource> --table-name value --key-condition-expression expression
aws <service> scan --table-name value --filter-expression expression
```

**Examples:**

```bash
# DynamoDB - Query
aws dynamodb query --table-name my-table \
  --key-condition-expression "id = :id" \
  --expression-attribute-values '{":id":{"S":"123"}}'

# DynamoDB - Scan
aws dynamodb scan --table-name my-table \
  --filter-expression "age > :age" \
  --expression-attribute-values '{":age":{"N":"18"}}'

# CloudWatch - Metrics query
aws cloudwatch get-metric-statistics \
  --namespace AWS/EC2 \
  --metric-name CPUUtilization \
  --start-time 2024-01-01T00:00:00Z \
  --end-time 2024-01-31T23:59:59Z \
  --period 86400 \
  --statistics Average
```

**Common Options:**
- `--filter-expression` - Filter results
- `--expression-attribute-names` - Attribute name placeholders
- `--expression-attribute-values` - Value placeholders
- `--max-items` - Limit results

---

### Pattern 8: Copy/Sync Operations

**Structure:**
```bash
aws s3 cp <source> <destination>
aws s3 sync <source-directory> <destination-directory>
```

**Examples:**

```bash
# S3 - Copy file to S3
aws s3 cp local-file.txt s3://my-bucket/remote-file.txt

# S3 - Copy file from S3
aws s3 cp s3://my-bucket/remote-file.txt local-file.txt

# S3 - Copy between S3 buckets
aws s3 cp s3://source-bucket/file.txt s3://dest-bucket/file.txt

# S3 - Sync local directory to S3
aws s3 sync ./local-dir s3://my-bucket/remote-dir

# S3 - Sync S3 to local directory
aws s3 sync s3://my-bucket/remote-dir ./local-dir

# S3 - Sync with exclusions
aws s3 sync ./local-dir s3://my-bucket/ --exclude "*.log" --exclude ".git/*"

# S3 - Sync with deletion
aws s3 sync ./local-dir s3://my-bucket/ --delete
```

**Common Options:**
- `--exclude` - Exclude patterns
- `--include` - Include patterns
- `--delete` - Delete files not in source
- `--storage-class` - Storage class (STANDARD, GLACIER, etc.)
- `--recursive` - Recursive operation

---

### Pattern 9: Get/Retrieve Operations

**Structure:**
```bash
aws <service> get-<resource> --<resource>-name value
```

**Examples:**

```bash
# IAM - Get user
aws iam get-user --user-name john

# IAM - Get role
aws iam get-role --role-name my-role

# IAM - Get policy
aws iam get-policy --policy-arn arn:aws:iam::aws:policy/ReadOnlyAccess

# Lambda - Get function
aws lambda get-function --function-name my-function

# Lambda - Get function code
aws lambda get-function-code-sha256 --function-name my-function

# Secrets Manager - Get secret
aws secretsmanager get-secret-value --secret-id my-secret

# Systems Manager - Get parameter
aws ssm get-parameter --name /prod/database/password

# Systems Manager - Get parameters
aws ssm get-parameters --names /prod/database/password /prod/database/host
```

**Common Options:**
- `--query` - JMESPath query
- `--output` - Output format

---

### Pattern 10: Invoke/Execute Operations

**Structure:**
```bash
aws <service> invoke --<resource>-name value [PAYLOAD]
```

**Examples:**

```bash
# Lambda - Invoke function
aws lambda invoke --function-name my-function response.json

# Lambda - Invoke with payload
aws lambda invoke --function-name my-function \
  --payload '{"key":"value"}' \
  response.json

# Lambda - Async invocation
aws lambda invoke --function-name my-function \
  --invocation-type Event \
  response.json

# SNS - Publish message
aws sns publish --topic-arn arn:aws:sns:us-east-1:123456789012:my-topic \
  --message "Hello World"

# SQS - Send message
aws sqs send-message --queue-url https://sqs.us-east-1.amazonaws.com/123456789012/myqueue \
  --message-body "Hello World"
```

**Common Options:**
- `--invocation-type` - Sync, Async, DryRun
- `--qualifier` - Function version or alias
- `--payload` - JSON payload

---

## Most Frequently Used Full Command Examples

### Daily Operations

```bash
# Check AWS credentials
aws sts get-caller-identity

# List S3 buckets
aws s3 ls

# Describe running EC2 instances
aws ec2 describe-instances --filters "Name=instance-state-name,Values=running"

# Upload file to S3
aws s3 cp myfile.txt s3://my-bucket/myfile.txt

# Download file from S3
aws s3 cp s3://my-bucket/myfile.txt myfile.txt

# Sync local directory to S3
aws s3 sync . s3://my-bucket/ --exclude ".git/*" --exclude "*.log"

# List IAM users
aws iam list-users

# Start an EC2 instance
aws ec2 start-instances --instance-ids i-1234567890abcdef0

# Stop an EC2 instance
aws ec2 stop-instances --instance-ids i-1234567890abcdef0

# Get RDS database status
aws rds describe-db-instances --db-instance-identifier mydbinstance
```

### Deployment Operations

```bash
# Deploy Lambda function
aws lambda update-function-code --function-name my-function --zip-file fileb://function.zip

# Create CloudFormation stack
aws cloudformation create-stack --stack-name my-stack --template-body file://template.yaml

# Update CloudFormation stack
aws cloudformation update-stack --stack-name my-stack --template-body file://template.yaml

# Deploy Docker image to ECS
aws ecs update-service --cluster my-cluster --service my-service --force-new-deployment

# Update Auto Scaling group
aws autoscaling update-auto-scaling-group --auto-scaling-group-name my-asg --max-size 10
```

### Management Operations

```bash
# Create IAM user
aws iam create-user --user-name john

# Attach policy to user
aws iam attach-user-policy --user-name john --policy-arn arn:aws:iam::aws:policy/ReadOnlyAccess

# Create access key
aws iam create-access-key --user-name john

# Create RDS snapshot
aws rds create-db-snapshot --db-instance-identifier mydbinstance --db-snapshot-identifier mysnap-001

# Get CloudWatch metrics
aws cloudwatch get-metric-statistics --namespace AWS/EC2 --metric-name CPUUtilization --start-time 2024-01-01T00:00:00Z --end-time 2024-01-31T23:59:59Z --period 86400 --statistics Average

# Create security group
aws ec2 create-security-group --group-name my-sg --description "My security group"

# Open port in security group
aws ec2 authorize-security-group-ingress --group-id sg-123456 --protocol tcp --port 443 --cidr 0.0.0.0/0
```

### Monitoring Operations

```bash
# Get log events from CloudWatch
aws logs get-log-events --log-group-name /aws/lambda/my-function --log-stream-name '2024/01/15/[$LATEST]abc123'

# Describe alarms
aws cloudwatch describe-alarms --alarm-names my-alarm

# Put metric data
aws cloudwatch put-metric-data --namespace MyApp --metric-name RequestCount --value 100

# Describe auto scaling activities
aws autoscaling describe-scaling-activities --auto-scaling-group-name my-asg

# Get Lambda function logs
aws logs tail /aws/lambda/my-function --follow
```

### Troubleshooting Operations

```bash
# Check instance status
aws ec2 describe-instance-status --instance-ids i-1234567890abcdef0

# Get system log for EC2 instance
aws ec2 get-console-output --instance-id i-1234567890abcdef0

# Describe DB instance
aws rds describe-db-instances --db-instance-identifier mydbinstance

# Get Lambda function error
aws lambda get-function --function-name my-function

# Describe events in RDS
aws rds describe-events --source-identifier mydbinstance
```

---

## Command Output Examples

### JSON Output (Default)
```json
{
  "Users": [
    {
      "Path": "/",
      "UserName": "john",
      "UserId": "AIDAJ45Q7YFFAREXAMPLE",
      "Arn": "arn:aws:iam::123456789012:user/john",
      "CreateDate": "2024-01-15T10:30:45+00:00"
    }
  ]
}
```

### Table Output
```
|   Path |   UserName |            UserId |                                       Arn | CreateDate                 |
|--------+------------+-------------------+--------------------------------------------+----------------------------|
| /      | john       | AIDAJ45Q7YFFAREXAMPLE | arn:aws:iam::123456789012:user/john       | 2024-01-15T10:30:45+00:00 |
```

### Text Output
```
/       john    AIDAJ45Q7YFFAREXAMPLE    arn:aws:iam::123456789012:user/john    2024-01-15T10:30:45+00:00
```

---

## Error Handling Examples

### Common Errors

```bash
# AccessDenied - Insufficient IAM permissions
error: An error occurred (AccessDenied) when calling the StartInstances operation: User: arn:aws:iam::123456789012:user/john is not authorized to perform: ec2:StartInstances on resource...

# InvalidParameterValue - Invalid parameter
error: An error occurred (InvalidParameterValue) when calling the DescribeInstances operation: Invalid id: "i-invalid"

# ResourceNotFoundException - Resource doesn't exist
error: An error occurred (ResourceNotFoundException) when calling the GetFunction operation: The resource you requested does not exist.

# ValidationError - Validation failed
error: An error occurred (ValidationError) when calling the CreateUser operation: User already exists

# ThrottlingException - Rate limited
error: An error occurred (ThrottlingException) when calling the ListBuckets operation: Rate exceeded

# ServiceUnavailable - Service is down
error: An error occurred (ServiceUnavailable) when calling the DescribeInstances operation: Service is temporarily unavailable
```

---

## Query & Filter Examples

### Using JMESPath --query Option

```bash
# Get instance IDs
aws ec2 describe-instances --query 'Reservations[].Instances[].InstanceId' --output text

# Get running instances
aws ec2 describe-instances --query 'Reservations[?Instances[?State.Name==`running`]].Instances[].[InstanceId,InstanceType]' --output table

# Get user names
aws iam list-users --query 'Users[].UserName' --output text

# Get specific attributes
aws rds describe-db-instances --query 'DBInstances[].{Name:DBInstanceIdentifier,Engine:Engine,Status:DBInstanceStatus}' --output table
```

### Using Filters (Service-specific)

```bash
# EC2 instances by state
aws ec2 describe-instances --filters "Name=instance-state-name,Values=running"

# EC2 instances with specific tag
aws ec2 describe-instances --filters "Name=tag:Environment,Values=production"

# EC2 instances by type
aws ec2 describe-instances --filters "Name=instance-type,Values=t2.micro,t2.small"

# RDS by engine
aws rds describe-db-instances --filters "Name=engine,Values=mysql"

# IAM users with path prefix
aws iam list-users --path-prefix "/admin/"

# Lambda functions by runtime
aws lambda list-functions --query 'Functions[?Runtime==`python3.11`]' --output table
```

---

## Common Option Patterns

### Global Options (All Services)
```bash
--profile <profile-name>        # Use specific AWS profile
--region <region>               # Use specific region
--output json|text|table        # Output format
--query <jmespath-expression>   # Query output
--debug                         # Enable debug logging
--no-paginate                   # Disable pagination
--page-size <size>              # Items per page
--cli-connect-timeout <seconds> # Connection timeout
--cli-read-timeout <seconds>    # Read timeout
```

### Filtering Options (List/Describe Commands)
```bash
--filters                       # Service-specific filters
--max-items <count>             # Maximum results
--starting-token <token>        # Pagination token
--query                         # JMESPath query
```

### Dry-Run Options
```bash
--dry-run                       # Preview without executing (where supported)
```

### Tags Options
```bash
--tags                          # Add tags to resources
Key1=Value1 Key2=Value2         # Tag format
```

---

## Performance Considerations

### Operations that may take time:
- `s3 sync` - Depends on number of files and size
- `ec2 run-instances` - 30-60 seconds typically
- `rds create-db-instance` - 5-10 minutes typically
- `cloudformation create-stack` - Highly variable
- `lambda create-function` - 1-5 seconds
- `iam create-policy` - 1-2 seconds

### Operations that should be fast:
- `sts get-caller-identity` - < 1 second
- `s3 ls` - 1-5 seconds depending on bucket size
- `ec2 describe-instances` - 2-5 seconds
- `iam list-users` - 1-3 seconds
- `lambda list-functions` - 1-2 seconds

