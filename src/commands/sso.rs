use anyhow::{anyhow, Context, Result};
use aws_sdk_sso::Client;
use aws_sdk_ssooidc::Client as SsoOidcClient;
use std::io::{self, Write};
use chrono::{TimeZone, Utc};

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
        let expiration = match Utc.timestamp_millis_opt(credentials.expiration()).single() {
            Some(ts) => ts.to_rfc3339(),
            None => credentials.expiration().to_string(),
        };
        println!("{:<20} {}", "Expiration", expiration);
    }

    Ok(())
}

/// Start AWS SSO device authorization flow and guide user through login.
pub async fn cmd_login(
    sso_oidc_client: &SsoOidcClient,
    start_url: &str,
    region: &str,
) -> Result<()> {
    let registration = sso_oidc_client
        .register_client()
        .client_name("aws-cli-rust")
        .client_type("public")
        .send()
        .await
        .context("Failed to register SSO OIDC client")?;

    let client_id = registration
        .client_id()
        .ok_or_else(|| anyhow!("SSO OIDC register-client returned no client_id"))?;
    let client_secret = registration
        .client_secret()
        .ok_or_else(|| anyhow!("SSO OIDC register-client returned no client_secret"))?;

    let auth = sso_oidc_client
        .start_device_authorization()
        .client_id(client_id)
        .client_secret(client_secret)
        .start_url(start_url)
        .send()
        .await
        .context("Failed to start device authorization")?;

    let verification_uri = auth
        .verification_uri()
        .ok_or_else(|| anyhow!("No verification URI returned from device authorization"))?;
    let user_code = auth
        .user_code()
        .ok_or_else(|| anyhow!("No user code returned from device authorization"))?;
    let device_code = auth
        .device_code()
        .ok_or_else(|| anyhow!("No device code returned from device authorization"))?;

    println!("SSO login started for region: {region}");
    println!("Open this URL in your browser and enter the code:");
    println!("{verification_uri}");
    println!("Code: {user_code}");
    print!("Press Enter after completing sign-in to continue...");
    io::stdout().flush().context("Failed to flush stdout")?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("Failed to read confirmation input")?;

    let token = sso_oidc_client
        .create_token()
        .client_id(client_id)
        .client_secret(client_secret)
        .grant_type("urn:ietf:params:oauth:grant-type:device_code")
        .device_code(device_code)
        .send()
        .await
        .context("Failed to create SSO token from device authorization")?;

    if let Some(access_token) = token.access_token() {
        println!("\nLogin successful.");
        println!("AccessToken: {access_token}");
    } else {
        println!("\nLogin flow completed but no access token was returned.");
    }

    Ok(())
}
