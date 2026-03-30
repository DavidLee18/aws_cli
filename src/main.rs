use anyhow::Result;
use aws_config::meta::region::RegionProviderChain;
use clap::{Parser, Subcommand};

use aws_cli::commands::{ec2 as ec2_cmd, iam as iam_cmd, s3 as s3_cmd};
use aws_cli::config as cfg;

// ── Top-level CLI ─────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "aws",
    version = env!("CARGO_PKG_VERSION"),
    about = "AWS CLI – Rust port of the official Python AWS CLI",
    long_about = None,
)]
struct Cli {
    /// AWS profile to use (default: "default").
    #[arg(long, global = true, default_value = "default")]
    profile: String,

    /// AWS region to use (overrides config / environment).
    #[arg(long, short = 'r', global = true)]
    region: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Amazon S3 commands.
    S3 {
        #[command(subcommand)]
        subcommand: S3Commands,
    },
    /// Amazon EC2 commands.
    Ec2 {
        #[command(subcommand)]
        subcommand: Ec2Commands,
    },
    /// AWS IAM commands.
    Iam {
        #[command(subcommand)]
        subcommand: IamCommands,
    },
    /// Configure AWS credentials and settings.
    Configure {
        #[command(subcommand)]
        subcommand: Option<ConfigureCommands>,
        /// AWS Access Key ID.
        #[arg(long)]
        aws_access_key_id: Option<String>,
        /// AWS Secret Access Key.
        #[arg(long)]
        aws_secret_access_key: Option<String>,
        /// Default region name.
        #[arg(long)]
        region: Option<String>,
        /// Default output format (json, text, table).
        #[arg(long)]
        output: Option<String>,
    },
}

// ── S3 sub-commands ───────────────────────────────────────────────────────────

#[derive(Subcommand)]
enum S3Commands {
    /// List S3 buckets or objects.
    Ls {
        /// S3 URI (s3://bucket[/prefix]).  Omit to list all buckets.
        uri: Option<String>,
        /// Recursively list all objects under the prefix.
        #[arg(long, short = 'r')]
        recursive: bool,
    },
    /// Copy a file to/from/within S3.
    Cp {
        /// Source path (local or s3://).
        src: String,
        /// Destination path (local or s3://).
        dst: String,
    },
    /// Remove an S3 object.
    Rm {
        /// S3 URI of the object to remove (s3://bucket/key).
        uri: String,
    },
    /// Create a new S3 bucket.
    Mb {
        /// S3 URI of the bucket to create (s3://bucket-name).
        uri: String,
        /// AWS region for the new bucket (default: us-east-1).
        #[arg(long, default_value = "us-east-1")]
        region: String,
    },
    /// Remove an S3 bucket.
    Rb {
        /// S3 URI of the bucket to remove (s3://bucket-name).
        uri: String,
        /// Delete all objects before removing the bucket.
        #[arg(long)]
        force: bool,
    },
}

// ── EC2 sub-commands ──────────────────────────────────────────────────────────

#[derive(Subcommand)]
enum Ec2Commands {
    /// Describe one or more EC2 instances.
    DescribeInstances {
        /// Instance IDs to describe (omit for all).
        #[arg(value_name = "INSTANCE_ID")]
        instance_ids: Vec<String>,
    },
    /// List available EC2 regions.
    DescribeRegions,
    /// Start one or more EC2 instances.
    StartInstances {
        /// Instance IDs to start.
        #[arg(value_name = "INSTANCE_ID", required = true)]
        instance_ids: Vec<String>,
    },
    /// Stop one or more EC2 instances.
    StopInstances {
        /// Instance IDs to stop.
        #[arg(value_name = "INSTANCE_ID", required = true)]
        instance_ids: Vec<String>,
        /// Force stop (equivalent to cutting power).
        #[arg(long)]
        force: bool,
    },
    /// Reboot one or more EC2 instances.
    RebootInstances {
        /// Instance IDs to reboot.
        #[arg(value_name = "INSTANCE_ID", required = true)]
        instance_ids: Vec<String>,
    },
    /// Describe EC2 instance types.
    DescribeInstanceTypes {
        /// Specific instance types to describe (omit for all).
        #[arg(value_name = "INSTANCE_TYPE")]
        instance_types: Vec<String>,
    },
}

// ── IAM sub-commands ──────────────────────────────────────────────────────────

#[derive(Subcommand)]
enum IamCommands {
    /// List IAM users.
    ListUsers {
        /// Path prefix filter (e.g. /division_abc/).
        #[arg(long)]
        path_prefix: Option<String>,
    },
    /// List IAM roles.
    ListRoles {
        /// Path prefix filter.
        #[arg(long)]
        path_prefix: Option<String>,
    },
    /// List IAM policies.
    ListPolicies {
        /// Scope: local (default), aws, or all.
        #[arg(long, default_value = "local")]
        scope: String,
        /// Only return attached policies.
        #[arg(long)]
        only_attached: bool,
    },
    /// List IAM groups.
    ListGroups {
        /// Path prefix filter.
        #[arg(long)]
        path_prefix: Option<String>,
    },
    /// List account aliases.
    ListAccountAliases,
}

// ── Configure sub-commands ────────────────────────────────────────────────────

#[derive(Subcommand)]
enum ConfigureCommands {
    /// Print a single configuration value.
    Get {
        /// Configuration key (e.g. region, aws_access_key_id).
        key: String,
    },
    /// Print all configuration values for the current profile.
    List,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Configure {
            subcommand,
            aws_access_key_id,
            aws_secret_access_key,
            region,
            output,
        } => {
            match subcommand {
                Some(ConfigureCommands::Get { key }) => {
                    cfg::run_configure_get(&key, &cli.profile)
                        .map_err(anyhow::Error::from)?;
                }
                Some(ConfigureCommands::List) => {
                    cfg::run_configure_list(&cli.profile)
                        .map_err(anyhow::Error::from)?;
                }
                None => {
                    cfg::run_configure(
                        &cli.profile,
                        aws_access_key_id.as_deref(),
                        aws_secret_access_key.as_deref(),
                        region.as_deref(),
                        output.as_deref(),
                    )
                    .map_err(anyhow::Error::from)?;
                }
            }
        }

        service_command => {
            // Build an AWS config for all service commands.
            let aws_cfg = build_aws_config(&cli.profile, cli.region.as_deref()).await?;

            match service_command {
                Commands::S3 { subcommand } => {
                    let client = aws_sdk_s3::Client::new(&aws_cfg);
                    match subcommand {
                        S3Commands::Ls { uri, recursive } => {
                            s3_cmd::cmd_ls(&client, uri.as_deref(), recursive).await?
                        }
                        S3Commands::Cp { src, dst } => {
                            s3_cmd::cmd_cp(&client, &src, &dst).await?
                        }
                        S3Commands::Rm { uri } => s3_cmd::cmd_rm(&client, &uri).await?,
                        S3Commands::Mb { uri, region } => {
                            s3_cmd::cmd_mb(&client, &uri, &region).await?
                        }
                        S3Commands::Rb { uri, force } => {
                            s3_cmd::cmd_rb(&client, &uri, force).await?
                        }
                    }
                }

                Commands::Ec2 { subcommand } => {
                    let client = aws_sdk_ec2::Client::new(&aws_cfg);
                    match subcommand {
                        Ec2Commands::DescribeInstances { instance_ids } => {
                            ec2_cmd::cmd_describe_instances(&client, &instance_ids).await?
                        }
                        Ec2Commands::DescribeRegions => {
                            ec2_cmd::cmd_describe_regions(&client).await?
                        }
                        Ec2Commands::StartInstances { instance_ids } => {
                            ec2_cmd::cmd_start_instances(&client, &instance_ids).await?
                        }
                        Ec2Commands::StopInstances {
                            instance_ids,
                            force,
                        } => ec2_cmd::cmd_stop_instances(&client, &instance_ids, force).await?,
                        Ec2Commands::RebootInstances { instance_ids } => {
                            ec2_cmd::cmd_reboot_instances(&client, &instance_ids).await?
                        }
                        Ec2Commands::DescribeInstanceTypes { instance_types } => {
                            ec2_cmd::cmd_describe_instance_types(&client, &instance_types).await?
                        }
                    }
                }

                Commands::Iam { subcommand } => {
                    let client = aws_sdk_iam::Client::new(&aws_cfg);
                    match subcommand {
                        IamCommands::ListUsers { path_prefix } => {
                            iam_cmd::cmd_list_users(&client, path_prefix.as_deref()).await?
                        }
                        IamCommands::ListRoles { path_prefix } => {
                            iam_cmd::cmd_list_roles(&client, path_prefix.as_deref()).await?
                        }
                        IamCommands::ListPolicies {
                            scope,
                            only_attached,
                        } => iam_cmd::cmd_list_policies(&client, &scope, only_attached).await?,
                        IamCommands::ListGroups { path_prefix } => {
                            iam_cmd::cmd_list_groups(&client, path_prefix.as_deref()).await?
                        }
                        IamCommands::ListAccountAliases => {
                            iam_cmd::cmd_list_account_aliases(&client).await?
                        }
                    }
                }

                // Already matched above; this branch satisfies the exhaustiveness check.
                Commands::Configure { .. } => unreachable!(),
            }
        }
    }

    Ok(())
}

/// Build the AWS SDK configuration from the named profile and optional region
/// override.
async fn build_aws_config(
    profile: &str,
    region_override: Option<&str>,
) -> Result<aws_config::SdkConfig> {
    let region_provider = if let Some(r) = region_override {
        RegionProviderChain::first_try(aws_config::Region::new(r.to_owned()))
    } else {
        RegionProviderChain::default_provider()
    };

    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .profile_name(profile)
        .region(region_provider)
        .load()
        .await;

    Ok(config)
}
