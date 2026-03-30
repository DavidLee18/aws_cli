use anyhow::Result;
use aws_sdk_rds::Client;

/// List RDS database instances.
pub async fn cmd_describe_db_instances(
    client: &Client,
    db_instance_ids: &[String],
) -> Result<()> {
    let mut req = client.describe_db_instances();

    if !db_instance_ids.is_empty() {
        // Filter by specific instance IDs
        for id in db_instance_ids {
            req = req.db_instance_identifier(id);
        }
    }

    let resp = req.send().await?;

    for instance in resp.db_instances() {
        let id = instance.db_instance_identifier().unwrap_or("N/A");
        let status = instance.db_instance_status().unwrap_or("N/A");
        let engine = instance.engine().unwrap_or("N/A");
        let instance_class = instance.db_instance_class().unwrap_or("N/A");

        let endpoint = match instance.endpoint() {
            Some(ep) => ep.address().unwrap_or("N/A"),
            None => "N/A",
        };

        let port = match instance.endpoint() {
            Some(ep) => ep.port().map(|p| p.to_string()).unwrap_or_else(|| "N/A".to_string()),
            None => "N/A".to_string(),
        };

        println!(
            "DBInstanceIdentifier: {}\n  Status: {}\n  Engine: {}\n  InstanceClass: {}\n  Endpoint: {}:{}\n",
            id, status, engine, instance_class, endpoint, port
        );
    }

    Ok(())
}

/// Create a new RDS database instance.
pub async fn cmd_create_db_instance(
    client: &Client,
    db_instance_identifier: &str,
    db_instance_class: &str,
    engine: &str,
    master_username: &str,
    master_user_password: &str,
    allocated_storage: i32,
) -> Result<()> {
    let resp = client
        .create_db_instance()
        .db_instance_identifier(db_instance_identifier)
        .db_instance_class(db_instance_class)
        .engine(engine)
        .master_username(master_username)
        .master_user_password(master_user_password)
        .allocated_storage(allocated_storage)
        .send()
        .await?;

    if let Some(instance) = resp.db_instance() {
        let id = instance.db_instance_identifier().unwrap_or("N/A");
        let status = instance.db_instance_status().unwrap_or("N/A");
        println!("Created DB instance: {} (Status: {})", id, status);
    }

    Ok(())
}

/// Delete an RDS database instance.
pub async fn cmd_delete_db_instance(
    client: &Client,
    db_instance_identifier: &str,
    skip_final_snapshot: bool,
    final_db_snapshot_identifier: Option<&str>,
) -> Result<()> {
    let mut req = client
        .delete_db_instance()
        .db_instance_identifier(db_instance_identifier)
        .skip_final_snapshot(skip_final_snapshot);

    if let Some(snapshot_id) = final_db_snapshot_identifier {
        req = req.final_db_snapshot_identifier(snapshot_id);
    }

    let resp = req.send().await?;

    if let Some(instance) = resp.db_instance() {
        let id = instance.db_instance_identifier().unwrap_or("N/A");
        let status = instance.db_instance_status().unwrap_or("N/A");
        println!("Deleting DB instance: {} (Status: {})", id, status);
    }

    Ok(())
}

/// Modify an RDS database instance.
pub async fn cmd_modify_db_instance(
    client: &Client,
    db_instance_identifier: &str,
    db_instance_class: Option<&str>,
    allocated_storage: Option<i32>,
    apply_immediately: bool,
) -> Result<()> {
    let mut req = client
        .modify_db_instance()
        .db_instance_identifier(db_instance_identifier)
        .apply_immediately(apply_immediately);

    if let Some(class) = db_instance_class {
        req = req.db_instance_class(class);
    }

    if let Some(storage) = allocated_storage {
        req = req.allocated_storage(storage);
    }

    let resp = req.send().await?;

    if let Some(instance) = resp.db_instance() {
        let id = instance.db_instance_identifier().unwrap_or("N/A");
        let status = instance.db_instance_status().unwrap_or("N/A");
        println!("Modified DB instance: {} (Status: {})", id, status);
    }

    Ok(())
}

/// Start an RDS database instance.
pub async fn cmd_start_db_instance(
    client: &Client,
    db_instance_identifier: &str,
) -> Result<()> {
    let resp = client
        .start_db_instance()
        .db_instance_identifier(db_instance_identifier)
        .send()
        .await?;

    if let Some(instance) = resp.db_instance() {
        let id = instance.db_instance_identifier().unwrap_or("N/A");
        let status = instance.db_instance_status().unwrap_or("N/A");
        println!("Starting DB instance: {} (Status: {})", id, status);
    }

    Ok(())
}

/// Stop an RDS database instance.
pub async fn cmd_stop_db_instance(
    client: &Client,
    db_instance_identifier: &str,
) -> Result<()> {
    let resp = client
        .stop_db_instance()
        .db_instance_identifier(db_instance_identifier)
        .send()
        .await?;

    if let Some(instance) = resp.db_instance() {
        let id = instance.db_instance_identifier().unwrap_or("N/A");
        let status = instance.db_instance_status().unwrap_or("N/A");
        println!("Stopping DB instance: {} (Status: {})", id, status);
    }

    Ok(())
}

/// Reboot an RDS database instance.
pub async fn cmd_reboot_db_instance(
    client: &Client,
    db_instance_identifier: &str,
) -> Result<()> {
    let resp = client
        .reboot_db_instance()
        .db_instance_identifier(db_instance_identifier)
        .send()
        .await?;

    if let Some(instance) = resp.db_instance() {
        let id = instance.db_instance_identifier().unwrap_or("N/A");
        let status = instance.db_instance_status().unwrap_or("N/A");
        println!("Rebooting DB instance: {} (Status: {})", id, status);
    }

    Ok(())
}

/// List RDS database snapshots.
pub async fn cmd_describe_db_snapshots(
    client: &Client,
    db_instance_identifier: Option<&str>,
    db_snapshot_identifier: Option<&str>,
) -> Result<()> {
    let mut req = client.describe_db_snapshots();

    if let Some(instance_id) = db_instance_identifier {
        req = req.db_instance_identifier(instance_id);
    }

    if let Some(snapshot_id) = db_snapshot_identifier {
        req = req.db_snapshot_identifier(snapshot_id);
    }

    let resp = req.send().await?;

    for snapshot in resp.db_snapshots() {
        let id = snapshot.db_snapshot_identifier().unwrap_or("N/A");
        let instance_id = snapshot.db_instance_identifier().unwrap_or("N/A");
        let status = snapshot.status().unwrap_or("N/A");
        let engine = snapshot.engine().unwrap_or("N/A");

        println!(
            "SnapshotIdentifier: {}\n  DBInstanceIdentifier: {}\n  Status: {}\n  Engine: {}\n",
            id, instance_id, status, engine
        );
    }

    Ok(())
}

/// Create an RDS database snapshot.
pub async fn cmd_create_db_snapshot(
    client: &Client,
    db_snapshot_identifier: &str,
    db_instance_identifier: &str,
) -> Result<()> {
    let resp = client
        .create_db_snapshot()
        .db_snapshot_identifier(db_snapshot_identifier)
        .db_instance_identifier(db_instance_identifier)
        .send()
        .await?;

    if let Some(snapshot) = resp.db_snapshot() {
        let id = snapshot.db_snapshot_identifier().unwrap_or("N/A");
        let status = snapshot.status().unwrap_or("N/A");
        println!("Creating DB snapshot: {} (Status: {})", id, status);
    }

    Ok(())
}

/// Delete an RDS database snapshot.
pub async fn cmd_delete_db_snapshot(
    client: &Client,
    db_snapshot_identifier: &str,
) -> Result<()> {
    let resp = client
        .delete_db_snapshot()
        .db_snapshot_identifier(db_snapshot_identifier)
        .send()
        .await?;

    if let Some(snapshot) = resp.db_snapshot() {
        let id = snapshot.db_snapshot_identifier().unwrap_or("N/A");
        let status = snapshot.status().unwrap_or("N/A");
        println!("Deleting DB snapshot: {} (Status: {})", id, status);
    }

    Ok(())
}

/// Restore an RDS database instance from a snapshot.
pub async fn cmd_restore_db_instance_from_snapshot(
    client: &Client,
    db_instance_identifier: &str,
    db_snapshot_identifier: &str,
) -> Result<()> {
    let resp = client
        .restore_db_instance_from_db_snapshot()
        .db_instance_identifier(db_instance_identifier)
        .db_snapshot_identifier(db_snapshot_identifier)
        .send()
        .await?;

    if let Some(instance) = resp.db_instance() {
        let id = instance.db_instance_identifier().unwrap_or("N/A");
        let status = instance.db_instance_status().unwrap_or("N/A");
        println!("Restoring DB instance: {} (Status: {})", id, status);
    }

    Ok(())
}
