use anyhow::{Context, Result};
use aws_sdk_sts::Client;

/// Returns details about the IAM identity whose credentials are used to call
/// the operation.
pub async fn cmd_get_caller_identity(client: &Client) -> Result<()> {
    let resp = client
        .get_caller_identity()
        .send()
        .await
        .context("Failed to call sts get-caller-identity")?;

    let user_id = resp.user_id().unwrap_or("<unknown>");
    let account = resp.account().unwrap_or("<unknown>");
    let arn = resp.arn().unwrap_or("<unknown>");

    println!("{:<16} {}", "UserId", user_id);
    println!("{:<16} {}", "Account", account);
    println!("{:<16} {}", "Arn", arn);
    Ok(())
}

/// Assume an IAM role and return temporary security credentials.
pub async fn cmd_assume_role(
    client: &Client,
    role_arn: &str,
    role_session_name: &str,
    duration_seconds: Option<i32>,
) -> Result<()> {
    let mut req = client
        .assume_role()
        .role_arn(role_arn)
        .role_session_name(role_session_name);

    if let Some(duration) = duration_seconds {
        req = req.duration_seconds(duration);
    }

    let resp = req
        .send()
        .await
        .context("Failed to assume role")?;

    if let Some(credentials) = resp.credentials() {
        println!("{:<20} {}", "AccessKeyId", credentials.access_key_id());
        println!("{:<20} {}", "SecretAccessKey", credentials.secret_access_key());
        println!("{:<20} {}", "SessionToken", credentials.session_token());
        println!("{:<20} {}", "Expiration", credentials.expiration());
    }

    if let Some(assumed_role_user) = resp.assumed_role_user() {
        println!("\n{:<20} {}", "AssumedRoleArn", assumed_role_user.arn());
        println!("{:<20} {}", "AssumedRoleId", assumed_role_user.assumed_role_id());
    }

    Ok(())
}

/// Get temporary security credentials for the AWS account or IAM user.
pub async fn cmd_get_session_token(
    client: &Client,
    duration_seconds: Option<i32>,
) -> Result<()> {
    let mut req = client.get_session_token();

    if let Some(duration) = duration_seconds {
        req = req.duration_seconds(duration);
    }

    let resp = req
        .send()
        .await
        .context("Failed to get session token")?;

    if let Some(credentials) = resp.credentials() {
        println!("{:<20} {}", "AccessKeyId", credentials.access_key_id());
        println!("{:<20} {}", "SecretAccessKey", credentials.secret_access_key());
        println!("{:<20} {}", "SessionToken", credentials.session_token());
        println!("{:<20} {}", "Expiration", credentials.expiration());
    }

    Ok(())
}

/// Decode an authorization message returned when a request is denied by AWS.
pub async fn cmd_decode_authorization_message(
    client: &Client,
    encoded_message: &str,
) -> Result<()> {
    let resp = client
        .decode_authorization_message()
        .encoded_message(encoded_message)
        .send()
        .await
        .context("Failed to decode authorization message")?;

    if let Some(decoded) = resp.decoded_message() {
        println!("{}", decoded);
    } else {
        println!("No decoded message returned");
    }

    Ok(())
}
