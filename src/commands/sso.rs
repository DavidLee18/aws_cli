use anyhow::{Context, Result};
use aws_sdk_sso::Client;

/// List AWS accounts available to the current SSO access token.
pub async fn cmd_list_accounts(client: &Client, access_token: &str) -> Result<()> {
    let resp = client
        .list_accounts()
        .access_token(access_token)
        .send()
        .await
        .context("Failed to list AWS SSO accounts")?;

    for account in resp.account_list() {
        println!(
            "{:<16} {:<40} {}",
            account.account_id().unwrap_or("N/A"),
            account.account_name().unwrap_or("N/A"),
            account.email_address().unwrap_or("N/A")
        );
    }

    Ok(())
}

/// List SSO roles available for an AWS account.
pub async fn cmd_list_account_roles(
    client: &Client,
    access_token: &str,
    account_id: &str,
) -> Result<()> {
    let resp = client
        .list_account_roles()
        .access_token(access_token)
        .account_id(account_id)
        .send()
        .await
        .context("Failed to list AWS SSO account roles")?;

    for role in resp.role_list() {
        println!(
            "{:<16} {}",
            role.account_id().unwrap_or("N/A"),
            role.role_name().unwrap_or("N/A")
        );
    }

    Ok(())
}

/// Get temporary role credentials for AWS account and role.
pub async fn cmd_get_role_credentials(
    client: &Client,
    access_token: &str,
    account_id: &str,
    role_name: &str,
) -> Result<()> {
    let resp = client
        .get_role_credentials()
        .access_token(access_token)
        .account_id(account_id)
        .role_name(role_name)
        .send()
        .await
        .context("Failed to get AWS SSO role credentials")?;

    if let Some(credentials) = resp.role_credentials() {
        println!("{:<20} {}", "AccessKeyId", credentials.access_key_id().unwrap_or("N/A"));
        println!(
            "{:<20} {}",
            "SecretAccessKey",
            credentials.secret_access_key().unwrap_or("N/A")
        );
        println!(
            "{:<20} {}",
            "SessionToken",
            credentials.session_token().unwrap_or("N/A")
        );
        println!("{:<20} {}", "Expiration", credentials.expiration());
    }

    Ok(())
}
