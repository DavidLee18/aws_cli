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
