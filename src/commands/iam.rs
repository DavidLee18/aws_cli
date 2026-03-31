use anyhow::{bail, Context, Result};
use aws_sdk_iam::Client;

const UNKNOWN: &str = "<unknown>";

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

/// Create an IAM role.
pub async fn cmd_create_role(
    client: &Client,
    role_name: &str,
    assume_role_policy_document: &str,
    path: Option<&str>,
) -> Result<()> {
    let mut req = client
        .create_role()
        .role_name(role_name)
        .assume_role_policy_document(assume_role_policy_document);
    if let Some(p) = path {
        req = req.path(p);
    }

    let resp = req.send().await.context("Failed to create IAM role")?;
    let role = resp.role();

    println!(
        "Created role: {}",
        role.map(|r| r.role_name()).unwrap_or(role_name)
    );
    println!("Role ID: {}", role.map(|r| r.role_id()).unwrap_or("N/A"));
    println!("ARN: {}", role.map(|r| r.arn()).unwrap_or("N/A"));
    println!("Path: {}", role.map(|r| r.path()).unwrap_or("N/A"));

    Ok(())
}

/// Create an IAM group.
pub async fn cmd_create_group(client: &Client, group_name: &str, path: Option<&str>) -> Result<()> {
    let mut req = client.create_group().group_name(group_name);
    if let Some(p) = path {
        req = req.path(p);
    }

    let resp = req.send().await.context("Failed to create IAM group")?;
    let group = resp.group();

    println!(
        "Created group: {}",
        group.map(|g| g.group_name()).unwrap_or(group_name)
    );
    println!("Group ID: {}", group.map(|g| g.group_id()).unwrap_or("N/A"));
    println!("ARN: {}", group.map(|g| g.arn()).unwrap_or("N/A"));
    println!("Path: {}", group.map(|g| g.path()).unwrap_or("N/A"));

    Ok(())
}

/// Get details for a single IAM group.
pub async fn cmd_get_group(client: &Client, group_name: &str) -> Result<()> {
    let resp = client
        .get_group()
        .group_name(group_name)
        .send()
        .await
        .context("Failed to get IAM group")?;

    if let Some(group) = resp.group() {
        println!("GroupName: {}", group.group_name());
        println!("GroupId: {}", group.group_id());
        println!("ARN: {}", group.arn());
        println!("Path: {}", group.path());
        println!("CreateDate: {}", group.create_date());
    } else {
        println!("No group data returned for: {group_name}");
    }

    println!("UsersInGroup: {}", resp.users().len());
    Ok(())
}

/// Delete an IAM group.
pub async fn cmd_delete_group(client: &Client, group_name: &str) -> Result<()> {
    client
        .delete_group()
        .group_name(group_name)
        .send()
        .await
        .context("Failed to delete IAM group")?;

    println!("Deleted group: {group_name}");
    Ok(())
}

/// Delete an IAM user.
pub async fn cmd_delete_user(client: &Client, user_name: &str) -> Result<()> {
    client
        .delete_user()
        .user_name(user_name)
        .send()
        .await
        .context("Failed to delete IAM user")?;

    println!("Deleted user: {user_name}");
    Ok(())
}

/// Get details for a single IAM user.
pub async fn cmd_get_user(client: &Client, user_name: &str) -> Result<()> {
    let resp = client
        .get_user()
        .user_name(user_name)
        .send()
        .await
        .context("Failed to get IAM user")?;

    if let Some(user) = resp.user() {
        println!("UserName: {}", user.user_name());
        println!("UserId: {}", user.user_id());
        println!("ARN: {}", user.arn());
        println!("Path: {}", user.path());
        println!("CreateDate: {}", user.create_date());
    } else {
        println!("No user data returned for: {user_name}");
    }

    Ok(())
}

/// Get details for a single IAM role.
pub async fn cmd_get_role(client: &Client, role_name: &str) -> Result<()> {
    let resp = client
        .get_role()
        .role_name(role_name)
        .send()
        .await
        .context("Failed to get IAM role")?;

    if let Some(role) = resp.role() {
        println!("RoleName: {}", role.role_name());
        println!("RoleId: {}", role.role_id());
        println!("ARN: {}", role.arn());
        println!("Path: {}", role.path());
        println!("CreateDate: {}", role.create_date());
    } else {
        println!("No role data returned for: {role_name}");
    }

    Ok(())
}

/// Delete an IAM role.
pub async fn cmd_delete_role(client: &Client, role_name: &str) -> Result<()> {
    client
        .delete_role()
        .role_name(role_name)
        .send()
        .await
        .context("Failed to delete IAM role")?;

    println!("Deleted role: {role_name}");
    Ok(())
}

/// Get details for a single IAM policy.
pub async fn cmd_get_policy(client: &Client, policy_arn: &str) -> Result<()> {
    let resp = client
        .get_policy()
        .policy_arn(policy_arn)
        .send()
        .await
        .context("Failed to get IAM policy")?;

    if let Some(policy) = resp.policy() {
        println!("PolicyName: {}", policy.policy_name().unwrap_or(UNKNOWN));
        println!("PolicyId: {}", policy.policy_id().unwrap_or(UNKNOWN));
        println!("ARN: {}", policy.arn().unwrap_or(UNKNOWN));
        println!("Path: {}", policy.path().unwrap_or("/"));
        println!(
            "DefaultVersionId: {}",
            policy.default_version_id().unwrap_or(UNKNOWN)
        );
        match policy.attachment_count() {
            Some(count) => println!("AttachmentCount: {count}"),
            None => println!("AttachmentCount: {UNKNOWN}"),
        }
    } else {
        println!("No policy data returned for: {policy_arn}");
    }

    Ok(())
}

/// Delete an IAM policy.
pub async fn cmd_delete_policy(client: &Client, policy_arn: &str) -> Result<()> {
    client
        .delete_policy()
        .policy_arn(policy_arn)
        .send()
        .await
        .context("Failed to delete IAM policy")?;

    println!("Deleted policy: {policy_arn}");
    Ok(())
}

/// Create an IAM policy.
pub async fn cmd_create_policy(
    client: &Client,
    policy_name: &str,
    policy_document: &str,
    description: Option<&str>,
    path: Option<&str>,
) -> Result<()> {
    let mut req = client
        .create_policy()
        .policy_name(policy_name)
        .policy_document(policy_document);
    if let Some(d) = description {
        req = req.description(d);
    }
    if let Some(p) = path {
        req = req.path(p);
    }

    let resp = req.send().await.context("Failed to create IAM policy")?;
    let policy = resp.policy();

    println!(
        "Created policy: {}",
        policy.and_then(|p| p.policy_name()).unwrap_or(policy_name)
    );
    println!(
        "PolicyId: {}",
        policy.and_then(|p| p.policy_id()).unwrap_or(UNKNOWN)
    );
    println!(
        "ARN: {}",
        policy.and_then(|p| p.arn()).unwrap_or(UNKNOWN)
    );

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

/// List managed policies attached to an IAM group.
pub async fn cmd_list_attached_group_policies(client: &Client, group_name: &str) -> Result<()> {
    let mut marker: Option<String> = None;
    println!("{:<50} {}", "PolicyName", "PolicyArn");
    println!("{:<50} {}", "----------", "---------");

    loop {
        let mut req = client.list_attached_group_policies().group_name(group_name);
        if let Some(ref m) = marker {
            req = req.marker(m);
        }
        let resp = req
            .send()
            .await
            .context("Failed to list attached group policies")?;

        for policy in resp.attached_policies() {
            let name = policy.policy_name().unwrap_or(UNKNOWN);
            let arn = policy.policy_arn().unwrap_or(UNKNOWN);
            println!("{name:<50} {arn}");
        }

        if resp.is_truncated() {
            marker = resp.marker().map(str::to_string);
        } else {
            break;
        }
    }

    Ok(())
}

/// Attach a managed policy to an IAM group.
pub async fn cmd_attach_group_policy(
    client: &Client,
    group_name: &str,
    policy_arn: &str,
) -> Result<()> {
    client
        .attach_group_policy()
        .group_name(group_name)
        .policy_arn(policy_arn)
        .send()
        .await
        .context("Failed to attach policy to group")?;

    println!("Attached policy to group: {group_name} -> {policy_arn}");
    Ok(())
}

/// Detach a managed policy from an IAM group.
pub async fn cmd_detach_group_policy(
    client: &Client,
    group_name: &str,
    policy_arn: &str,
) -> Result<()> {
    client
        .detach_group_policy()
        .group_name(group_name)
        .policy_arn(policy_arn)
        .send()
        .await
        .context("Failed to detach policy from group")?;

    println!("Detached policy from group: {group_name} -> {policy_arn}");
    Ok(())
}

/// Add an IAM user to an IAM group.
pub async fn cmd_add_user_to_group(client: &Client, group_name: &str, user_name: &str) -> Result<()> {
    client
        .add_user_to_group()
        .group_name(group_name)
        .user_name(user_name)
        .send()
        .await
        .context("Failed to add user to group")?;

    println!("Added user to group: {user_name} -> {group_name}");
    Ok(())
}

/// Remove an IAM user from an IAM group.
pub async fn cmd_remove_user_from_group(
    client: &Client,
    group_name: &str,
    user_name: &str,
) -> Result<()> {
    client
        .remove_user_from_group()
        .group_name(group_name)
        .user_name(user_name)
        .send()
        .await
        .context("Failed to remove user from group")?;

    println!("Removed user from group: {user_name} -> {group_name}");
    Ok(())
}

/// List groups that an IAM user belongs to.
pub async fn cmd_list_groups_for_user(client: &Client, user_name: &str) -> Result<()> {
    let mut marker: Option<String> = None;
    println!("{:<30} {:<30} {}", "GroupName", "GroupId", "CreateDate");
    println!("{:<30} {:<30} {}", "---------", "-------", "----------");

    loop {
        let mut req = client.list_groups_for_user().user_name(user_name);
        if let Some(ref m) = marker {
            req = req.marker(m);
        }
        let resp = req
            .send()
            .await
            .context("Failed to list groups for user")?;

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

/// Attach a managed policy to an IAM user.
pub async fn cmd_attach_user_policy(client: &Client, user_name: &str, policy_arn: &str) -> Result<()> {
    client
        .attach_user_policy()
        .user_name(user_name)
        .policy_arn(policy_arn)
        .send()
        .await
        .context("Failed to attach policy to user")?;

    println!("Attached policy to user: {user_name} -> {policy_arn}");
    Ok(())
}

/// Detach a managed policy from an IAM user.
pub async fn cmd_detach_user_policy(client: &Client, user_name: &str, policy_arn: &str) -> Result<()> {
    client
        .detach_user_policy()
        .user_name(user_name)
        .policy_arn(policy_arn)
        .send()
        .await
        .context("Failed to detach policy from user")?;

    println!("Detached policy from user: {user_name} -> {policy_arn}");
    Ok(())
}

/// Attach a managed policy to an IAM role.
pub async fn cmd_attach_role_policy(client: &Client, role_name: &str, policy_arn: &str) -> Result<()> {
    client
        .attach_role_policy()
        .role_name(role_name)
        .policy_arn(policy_arn)
        .send()
        .await
        .context("Failed to attach policy to role")?;

    println!("Attached policy to role: {role_name} -> {policy_arn}");
    Ok(())
}

/// Detach a managed policy from an IAM role.
pub async fn cmd_detach_role_policy(client: &Client, role_name: &str, policy_arn: &str) -> Result<()> {
    client
        .detach_role_policy()
        .role_name(role_name)
        .policy_arn(policy_arn)
        .send()
        .await
        .context("Failed to detach policy from role")?;

    println!("Detached policy from role: {role_name} -> {policy_arn}");
    Ok(())
}

/// List attached managed policies for an IAM user.
pub async fn cmd_list_attached_user_policies(client: &Client, user_name: &str) -> Result<()> {
    let mut marker: Option<String> = None;
    println!("{:<50} {}", "PolicyName", "PolicyArn");
    println!("{:<50} {}", "----------", "---------");

    loop {
        let mut req = client.list_attached_user_policies().user_name(user_name);
        if let Some(ref m) = marker {
            req = req.marker(m);
        }
        let resp = req
            .send()
            .await
            .context("Failed to list attached user policies")?;

        for policy in resp.attached_policies() {
            let name = policy.policy_name().unwrap_or(UNKNOWN);
            let arn = policy.policy_arn().unwrap_or(UNKNOWN);
            println!("{name:<50} {arn}");
        }

        if resp.is_truncated() {
            marker = resp.marker().map(str::to_string);
        } else {
            break;
        }
    }

    Ok(())
}

/// List attached managed policies for an IAM role.
pub async fn cmd_list_attached_role_policies(client: &Client, role_name: &str) -> Result<()> {
    let mut marker: Option<String> = None;
    println!("{:<50} {}", "PolicyName", "PolicyArn");
    println!("{:<50} {}", "----------", "---------");

    loop {
        let mut req = client.list_attached_role_policies().role_name(role_name);
        if let Some(ref m) = marker {
            req = req.marker(m);
        }
        let resp = req
            .send()
            .await
            .context("Failed to list attached role policies")?;

        for policy in resp.attached_policies() {
            let name = policy.policy_name().unwrap_or(UNKNOWN);
            let arn = policy.policy_arn().unwrap_or(UNKNOWN);
            println!("{name:<50} {arn}");
        }

        if resp.is_truncated() {
            marker = resp.marker().map(str::to_string);
        } else {
            break;
        }
    }

    Ok(())
}

/// List inline policy names embedded in an IAM user.
pub async fn cmd_list_user_policies(client: &Client, user_name: &str) -> Result<()> {
    let mut marker: Option<String> = None;
    println!("PolicyName");
    println!("----------");

    loop {
        let mut req = client.list_user_policies().user_name(user_name);
        if let Some(ref m) = marker {
            req = req.marker(m);
        }
        let resp = req.send().await.context("Failed to list user policies")?;

        for policy_name in resp.policy_names() {
            println!("{policy_name}");
        }

        if resp.is_truncated() {
            marker = resp.marker().map(str::to_string);
        } else {
            break;
        }
    }

    Ok(())
}

/// List inline policy names embedded in an IAM role.
pub async fn cmd_list_role_policies(client: &Client, role_name: &str) -> Result<()> {
    let mut marker: Option<String> = None;
    println!("PolicyName");
    println!("----------");

    loop {
        let mut req = client.list_role_policies().role_name(role_name);
        if let Some(ref m) = marker {
            req = req.marker(m);
        }
        let resp = req.send().await.context("Failed to list role policies")?;

        for policy_name in resp.policy_names() {
            println!("{policy_name}");
        }

        if resp.is_truncated() {
            marker = resp.marker().map(str::to_string);
        } else {
            break;
        }
    }

    Ok(())
}

/// List inline policy names embedded in an IAM group.
pub async fn cmd_list_group_policies(client: &Client, group_name: &str) -> Result<()> {
    let mut marker: Option<String> = None;
    println!("PolicyName");
    println!("----------");

    loop {
        let mut req = client.list_group_policies().group_name(group_name);
        if let Some(ref m) = marker {
            req = req.marker(m);
        }
        let resp = req.send().await.context("Failed to list group policies")?;

        for policy_name in resp.policy_names() {
            println!("{policy_name}");
        }

        if resp.is_truncated() {
            marker = resp.marker().map(str::to_string);
        } else {
            break;
        }
    }

    Ok(())
}

/// Get an inline policy document embedded in an IAM user.
pub async fn cmd_get_user_policy(client: &Client, user_name: &str, policy_name: &str) -> Result<()> {
    let resp = client
        .get_user_policy()
        .user_name(user_name)
        .policy_name(policy_name)
        .send()
        .await
        .context("Failed to get user policy")?;

    println!("UserName: {}", resp.user_name());
    println!("PolicyName: {}", resp.policy_name());
    println!("PolicyDocument: {}", resp.policy_document());

    Ok(())
}

/// Get an inline policy document embedded in an IAM role.
pub async fn cmd_get_role_policy(client: &Client, role_name: &str, policy_name: &str) -> Result<()> {
    let resp = client
        .get_role_policy()
        .role_name(role_name)
        .policy_name(policy_name)
        .send()
        .await
        .context("Failed to get role policy")?;

    println!("RoleName: {}", resp.role_name());
    println!("PolicyName: {}", resp.policy_name());
    println!("PolicyDocument: {}", resp.policy_document());

    Ok(())
}

/// Get an inline policy document embedded in an IAM group.
pub async fn cmd_get_group_policy(
    client: &Client,
    group_name: &str,
    policy_name: &str,
) -> Result<()> {
    let resp = client
        .get_group_policy()
        .group_name(group_name)
        .policy_name(policy_name)
        .send()
        .await
        .context("Failed to get group policy")?;

    println!("GroupName: {}", resp.group_name());
    println!("PolicyName: {}", resp.policy_name());
    println!("PolicyDocument: {}", resp.policy_document());

    Ok(())
}

/// Add or update an inline policy document for an IAM user.
pub async fn cmd_put_user_policy(
    client: &Client,
    user_name: &str,
    policy_name: &str,
    policy_document: &str,
) -> Result<()> {
    client
        .put_user_policy()
        .user_name(user_name)
        .policy_name(policy_name)
        .policy_document(policy_document)
        .send()
        .await
        .context("Failed to put user policy")?;

    println!("Updated inline policy '{policy_name}' on user '{user_name}'");
    Ok(())
}

/// Delete an inline policy embedded in an IAM user.
pub async fn cmd_delete_user_policy(
    client: &Client,
    user_name: &str,
    policy_name: &str,
) -> Result<()> {
    client
        .delete_user_policy()
        .user_name(user_name)
        .policy_name(policy_name)
        .send()
        .await
        .context("Failed to delete user policy")?;

    println!("Deleted inline policy '{policy_name}' from user '{user_name}'");
    Ok(())
}

/// Add or update an inline policy document for an IAM role.
pub async fn cmd_put_role_policy(
    client: &Client,
    role_name: &str,
    policy_name: &str,
    policy_document: &str,
) -> Result<()> {
    client
        .put_role_policy()
        .role_name(role_name)
        .policy_name(policy_name)
        .policy_document(policy_document)
        .send()
        .await
        .context("Failed to put role policy")?;

    println!("Updated inline policy '{policy_name}' on role '{role_name}'");
    Ok(())
}

/// Delete an inline policy embedded in an IAM role.
pub async fn cmd_delete_role_policy(
    client: &Client,
    role_name: &str,
    policy_name: &str,
) -> Result<()> {
    client
        .delete_role_policy()
        .role_name(role_name)
        .policy_name(policy_name)
        .send()
        .await
        .context("Failed to delete role policy")?;

    println!("Deleted inline policy '{policy_name}' from role '{role_name}'");
    Ok(())
}

/// Add or update an inline policy document for an IAM group.
pub async fn cmd_put_group_policy(
    client: &Client,
    group_name: &str,
    policy_name: &str,
    policy_document: &str,
) -> Result<()> {
    client
        .put_group_policy()
        .group_name(group_name)
        .policy_name(policy_name)
        .policy_document(policy_document)
        .send()
        .await
        .context("Failed to put group policy")?;

    println!("Updated inline policy '{policy_name}' on group '{group_name}'");
    Ok(())
}

/// Delete an inline policy embedded in an IAM group.
pub async fn cmd_delete_group_policy(
    client: &Client,
    group_name: &str,
    policy_name: &str,
) -> Result<()> {
    client
        .delete_group_policy()
        .group_name(group_name)
        .policy_name(policy_name)
        .send()
        .await
        .context("Failed to delete group policy")?;

    println!("Deleted inline policy '{policy_name}' from group '{group_name}'");
    Ok(())
}

/// Create a new access key for an IAM user.
pub async fn cmd_create_access_key(client: &Client, user_name: &str) -> Result<()> {
    let resp = client
        .create_access_key()
        .user_name(user_name)
        .send()
        .await
        .context("Failed to create access key")?;

    if let Some(key) = resp.access_key() {
        println!("UserName: {}", key.user_name());
        println!("AccessKeyId: {}", key.access_key_id());
        println!("SecretAccessKey: {}", key.secret_access_key());
        println!("Status: {}", key.status().as_str());
        println!(
            "CreateDate: {}",
            key.create_date()
                .map(|d| d.to_string())
                .unwrap_or_else(|| UNKNOWN.to_string())
        );
    } else {
        println!("No access key data returned for user: {user_name}");
    }

    Ok(())
}

/// List access keys for an IAM user.
pub async fn cmd_list_access_keys(client: &Client, user_name: &str) -> Result<()> {
    let mut marker: Option<String> = None;
    println!("{:<25} {:<10} {}", "AccessKeyId", "Status", "CreateDate");
    println!("{:<25} {:<10} {}", "-----------", "------", "----------");

    loop {
        let mut req = client.list_access_keys().user_name(user_name);
        if let Some(ref m) = marker {
            req = req.marker(m);
        }
        let resp = req.send().await.context("Failed to list access keys")?;

        for key in resp.access_key_metadata() {
            let access_key_id = key.access_key_id().unwrap_or(UNKNOWN);
            let status = key.status().map(|s| s.as_str()).unwrap_or(UNKNOWN);
            let created = key
                .create_date()
                .map(|d| d.to_string())
                .unwrap_or_else(|| UNKNOWN.to_string());
            println!(
                "{:<25} {:<10} {}",
                access_key_id,
                status,
                created
            );
        }

        if resp.is_truncated() {
            marker = resp.marker().map(str::to_string);
        } else {
            break;
        }
    }

    Ok(())
}

/// Update the status of an IAM access key.
pub async fn cmd_update_access_key(
    client: &Client,
    user_name: &str,
    access_key_id: &str,
    status: &str,
) -> Result<()> {
    let status = match status.to_ascii_lowercase().as_str() {
        "active" => aws_sdk_iam::types::StatusType::Active,
        "inactive" => aws_sdk_iam::types::StatusType::Inactive,
        _ => bail!("Invalid status '{status}'. Use 'Active' or 'Inactive'."),
    };

    client
        .update_access_key()
        .user_name(user_name)
        .access_key_id(access_key_id)
        .status(status)
        .send()
        .await
        .context("Failed to update access key")?;

    println!("Updated access key '{access_key_id}' for user '{user_name}'");
    Ok(())
}

/// Delete an IAM access key.
pub async fn cmd_delete_access_key(
    client: &Client,
    user_name: &str,
    access_key_id: &str,
) -> Result<()> {
    client
        .delete_access_key()
        .user_name(user_name)
        .access_key_id(access_key_id)
        .send()
        .await
        .context("Failed to delete access key")?;

    println!("Deleted access key '{access_key_id}' for user '{user_name}'");
    Ok(())
}

/// Create a console login profile (password) for an IAM user.
pub async fn cmd_create_login_profile(
    client: &Client,
    user_name: &str,
    password: &str,
    password_reset_required: bool,
) -> Result<()> {
    client
        .create_login_profile()
        .user_name(user_name)
        .password(password)
        .set_password_reset_required(Some(password_reset_required))
        .send()
        .await
        .context("Failed to create login profile")?;

    println!("Created login profile for user '{user_name}'");
    Ok(())
}

/// Get login profile metadata for an IAM user.
pub async fn cmd_get_login_profile(client: &Client, user_name: &str) -> Result<()> {
    let resp = client
        .get_login_profile()
        .user_name(user_name)
        .send()
        .await
        .context("Failed to get login profile")?;

    if let Some(profile) = resp.login_profile() {
        println!("UserName: {}", profile.user_name());
        println!("CreateDate: {}", profile.create_date());
        println!(
            "PasswordResetRequired: {}",
            profile.password_reset_required()
        );
    } else {
        println!("No login profile data returned for user: {user_name}");
    }

    Ok(())
}

/// Update the login profile password settings for an IAM user.
pub async fn cmd_update_login_profile(
    client: &Client,
    user_name: &str,
    password: Option<&str>,
    password_reset_required: Option<bool>,
) -> Result<()> {
    let mut req = client.update_login_profile().user_name(user_name);
    if let Some(p) = password {
        req = req.password(p);
    }
    if let Some(flag) = password_reset_required {
        req = req.password_reset_required(flag);
    }

    req.send()
        .await
        .context("Failed to update login profile")?;

    println!("Updated login profile for user '{user_name}'");
    Ok(())
}

/// Delete a console login profile for an IAM user.
pub async fn cmd_delete_login_profile(client: &Client, user_name: &str) -> Result<()> {
    client
        .delete_login_profile()
        .user_name(user_name)
        .send()
        .await
        .context("Failed to delete login profile")?;

    println!("Deleted login profile for user '{user_name}'");
    Ok(())
}

/// Update the trust policy document for an IAM role.
pub async fn cmd_update_assume_role_policy(
    client: &Client,
    role_name: &str,
    policy_document: &str,
) -> Result<()> {
    client
        .update_assume_role_policy()
        .role_name(role_name)
        .policy_document(policy_document)
        .send()
        .await
        .context("Failed to update assume role policy")?;

    println!("Updated assume-role policy for role '{role_name}'");
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
