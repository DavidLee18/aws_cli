# aws_cli

Rust CLI named `aws` using the AWS SDK for Rust, porting core workflows from
the original Python AWS CLI.

## Implemented command groups

- `aws configure` (`get`, `list`, and interactive configure flow)
- `aws s3` (`ls`, `cp`, `rm`, `mb`, `rb`)
- `aws ec2` (`describe-instances`, `describe-regions`, `start-instances`,
  `stop-instances`, `reboot-instances`, `describe-instance-types`)
- `aws iam` (`list-users`, `list-roles`, `list-policies`, `list-groups`,
  `list-account-aliases`)
- `aws sts` (`get-caller-identity`)

## Scope note

The upstream Python AWS CLI supports hundreds of services and commands. This
repository currently ports a focused subset of frequently used commands and is
not yet feature-complete with upstream.
