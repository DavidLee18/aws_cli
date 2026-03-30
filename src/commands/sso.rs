use anyhow::{anyhow, Context, Result};
use aws_sdk_ssooidc::error::SdkError;
use aws_sdk_ssooidc::operation::create_token::CreateTokenError;
use aws_sdk_sso::Client;
use aws_sdk_ssooidc::Client as SsoOidcClient;
use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::io::Write;
use sha2::{Digest, Sha256};
use tokio::time::{sleep, Duration, Instant};

const MAX_BACKOFF_SECS: u64 = 10;
const HASH_START_URL_PREFIX: &str = "start_url=";
const HASH_REGION_PREFIX: &str = "\nregion=";
const SSO_CACHE_FILENAME_PREFIX: &str = "aws-cli-sso-token-";

fn ensure_min_duration(value: i32) -> u64 {
    if value < 1 {
        1
    } else {
        value as u64
    }
}

fn resolve_home_dir() -> Result<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home));
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home));
        }
    }
    if let (Ok(drive), Ok(path)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        if !drive.is_empty() && !path.is_empty() {
            return Ok(PathBuf::from(format!("{drive}{path}")));
        }
    }
    Err(anyhow!("Unable to determine user home directory"))
}

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
    sso_region: &str,
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

    println!("Open this URL in your browser and enter the code:");
    println!("{verification_uri}");
    println!("Code: {user_code}");

    // Defensive clamp: interval/expires_in should be positive, but we guard
    // against unexpected zero/negative responses to avoid invalid sleep/deadline.
    let poll_interval = ensure_min_duration(auth.interval());
    let expires_in = ensure_min_duration(auth.expires_in());
    let deadline = Instant::now() + Duration::from_secs(expires_in);
    let mut wait_secs = poll_interval;
    let mut token = None;

    while Instant::now() < deadline {
        match sso_oidc_client
            .create_token()
            .client_id(client_id)
            .client_secret(client_secret)
            .grant_type("urn:ietf:params:oauth:grant-type:device_code")
            .device_code(device_code)
            .send()
            .await
        {
            Ok(resp) => {
                token = Some(resp);
                break;
            }
            Err(err) => match &err {
                SdkError::ServiceError(service_err) => match service_err.err() {
                    CreateTokenError::AuthorizationPendingException(_) => {
                        sleep(Duration::from_secs(wait_secs)).await;
                        continue;
                    }
                    CreateTokenError::SlowDownException(_) => {
                        wait_secs = (wait_secs + 1).min(MAX_BACKOFF_SECS);
                        sleep(Duration::from_secs(wait_secs)).await;
                        continue;
                    }
                    _ => {
                        return Err(err)
                            .context("Failed to create SSO token from device authorization");
                    }
                },
                _ => {
                    return Err(err).context("Failed to create SSO token from device authorization");
                }
            },
        }
    }

    let token = token.ok_or_else(|| anyhow!("Timed out waiting for SSO authorization"))?;
    let access_token = token
        .access_token()
        .ok_or_else(|| anyhow!("Login completed but no access token was returned"))?;
    let cache_path = write_sso_token_cache(
        access_token,
        start_url,
        sso_region,
        token.expires_in(),
    )?;
    println!("\nLogin successful.");
    println!("SSO token cached at: {}", cache_path.display());

    Ok(())
}

fn write_sso_token_cache(
    access_token: &str,
    start_url: &str,
    sso_region: &str,
    expires_in: i32,
) -> Result<PathBuf> {
    let home = resolve_home_dir()?;
    let cache_dir = home.join(".aws").join("sso").join("cache");
    fs::create_dir_all(&cache_dir).context("Failed to create SSO cache directory")?;

    let mut hasher = Sha256::new();
    hasher.update(HASH_START_URL_PREFIX);
    hasher.update(start_url.as_bytes());
    hasher.update(HASH_REGION_PREFIX);
    hasher.update(sso_region.as_bytes());
    // Lowercase hex hash to align with common AWS CLI cache filename style.
    let filename = format!("{SSO_CACHE_FILENAME_PREFIX}{:x}.json", hasher.finalize());
    let cache_path = cache_dir.join(filename);
    let expires_at =
        (Utc::now() + ChronoDuration::seconds(ensure_min_duration(expires_in) as i64))
            .to_rfc3339();

    let payload = json!({
        "startUrl": start_url,
        "region": sso_region,
        "accessToken": access_token,
        "expiresIn": ensure_min_duration(expires_in),
        "expiresAt": expires_at,
    });
    let content =
        serde_json::to_vec_pretty(&payload).context("Failed to serialize SSO token cache")?;

    #[cfg(unix)]
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&cache_path)
            .context("Failed to open SSO token cache file")?;
        file.write_all(&content)
            .context("Failed to write SSO token cache file")?;
    }
    #[cfg(not(unix))]
    {
        fs::write(&cache_path, &content).context("Failed to write SSO token cache file")?;
    }

    Ok(cache_path)
}
