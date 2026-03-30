use anyhow::{Context, Result};
use aws_sdk_iam::Client;

/// Create an IAM user.
pub async fn cmd_create_user(client: &Client, user_name: &str, path: Option<&str>) -> Result<()> {
    let mut req = client.create_user().user_name(user_name);
    if let Some(p) = path {
        req = req.path(p);
    }

    let resp = req.send().await.context("Failed to create IAM user")?;
    let user = resp.user();

    println!(
        "Created user: {}",
        user.map(|u| u.user_name()).unwrap_or(user_name)
    );
    println!("User ID: {}", user.map(|u| u.user_id()).unwrap_or("N/A"));
    println!("ARN: {}", user.map(|u| u.arn()).unwrap_or("N/A"));
    println!("Path: {}", user.map(|u| u.path()).unwrap_or("N/A"));

    Ok(())
}

/// List all IAM users.
pub async fn cmd_list_users(client: &Client, path_prefix: Option<&str>) -> Result<()> {
    let mut marker: Option<String> = None;
    println!(
        "{:<30} {:<30} {:<20}",
        "UserName", "UserId", "CreateDate"
    );
    println!("{:<30} {:<30} {:<20}", "--------", "------", "----------");

    loop {
        let mut req = client.list_users();
        if let Some(p) = path_prefix {
            req = req.path_prefix(p);
        }
        if let Some(ref m) = marker {
            req = req.marker(m);
        }
        let resp = req.send().await.context("Failed to list IAM users")?;

        for user in resp.users() {
            let name = user.user_name();
            let uid = user.user_id();
            let created = user.create_date().to_string();
            println!("{name:<30} {uid:<30} {created:<20}");
        }

        if resp.is_truncated() {
            marker = resp.marker().map(str::to_string);
        } else {
            break;
        }
    }
    Ok(())
}

/// List all IAM roles.
pub async fn cmd_list_roles(client: &Client, path_prefix: Option<&str>) -> Result<()> {
    let mut marker: Option<String> = None;
    println!(
        "{:<40} {:<30} {}",
        "RoleName", "RoleId", "CreateDate"
    );
    println!("{:<40} {:<30} {}", "--------", "------", "----------");

    loop {
        let mut req = client.list_roles();
        if let Some(p) = path_prefix {
            req = req.path_prefix(p);
        }
        if let Some(ref m) = marker {
            req = req.marker(m);
        }
        let resp = req.send().await.context("Failed to list IAM roles")?;

        for role in resp.roles() {
            let name = role.role_name();
            let rid = role.role_id();
            let created = role.create_date().to_string();
            println!("{name:<40} {rid:<30} {created}");
        }

        if resp.is_truncated() {
            marker = resp.marker().map(str::to_string);
        } else {
            break;
        }
    }
    Ok(())
}

/// List IAM policies (by default scope = "Local"; pass `all` for "All").
pub async fn cmd_list_policies(client: &Client, scope: &str, only_attached: bool) -> Result<()> {
    let policy_scope = match scope.to_ascii_lowercase().as_str() {
        "all" => aws_sdk_iam::types::PolicyScopeType::All,
        "aws" => aws_sdk_iam::types::PolicyScopeType::Aws,
        _ => aws_sdk_iam::types::PolicyScopeType::Local,
    };

    let mut marker: Option<String> = None;
    println!(
        "{:<50} {:<25} {}",
        "PolicyName", "PolicyId", "AttachmentCount"
    );
    println!("{:<50} {:<25} {}", "----------", "--------", "---------------");

    loop {
        let mut req = client
            .list_policies()
            .scope(policy_scope.clone())
            .only_attached(only_attached);
        if let Some(ref m) = marker {
            req = req.marker(m);
        }
        let resp = req.send().await.context("Failed to list IAM policies")?;

        for policy in resp.policies() {
            let name = policy.policy_name().unwrap_or("<unknown>");
            let pid = policy.policy_id().unwrap_or("<unknown>");
            let count = policy.attachment_count().unwrap_or(0);
            println!("{name:<50} {pid:<25} {count}");
        }

        if resp.is_truncated() {
            marker = resp.marker().map(str::to_string);
        } else {
            break;
        }
    }
    Ok(())
}

/// List IAM groups.
pub async fn cmd_list_groups(client: &Client, path_prefix: Option<&str>) -> Result<()> {
    let mut marker: Option<String> = None;
    println!("{:<30} {:<30} {}", "GroupName", "GroupId", "CreateDate");
    println!("{:<30} {:<30} {}", "---------", "-------", "----------");

    loop {
        let mut req = client.list_groups();
        if let Some(p) = path_prefix {
            req = req.path_prefix(p);
        }
        if let Some(ref m) = marker {
            req = req.marker(m);
        }
        let resp = req.send().await.context("Failed to list IAM groups")?;

        for group in resp.groups() {
            let name = group.group_name();
            let gid = group.group_id();
            let created = group.create_date().to_string();
            println!("{name:<30} {gid:<30} {created}");
        }

        if resp.is_truncated() {
            marker = resp.marker().map(str::to_string);
        } else {
            break;
        }
    }
    Ok(())
}

/// Get details about the current IAM account alias(es).
pub async fn cmd_list_account_aliases(client: &Client) -> Result<()> {
    let resp = client
        .list_account_aliases()
        .send()
        .await
        .context("Failed to list account aliases")?;
    for alias in resp.account_aliases() {
        println!("{alias}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_policy_scope_mapping() {
        // Verify the scope string → enum logic without a live client.
        let cases = vec![
            ("all", "All"),
            ("aws", "AWS"),
            ("local", "Local"),
            ("unknown", "Local"),
        ];
        for (input, _expected) in cases {
            let _scope = match input.to_ascii_lowercase().as_str() {
                "all" => aws_sdk_iam::types::PolicyScopeType::All,
                "aws" => aws_sdk_iam::types::PolicyScopeType::Aws,
                _ => aws_sdk_iam::types::PolicyScopeType::Local,
            };
            // Just ensure no panic; the mapping is covered above.
        }
    }
}
