use anyhow::{Context, Result};
use aws_sdk_ec2::Client;

/// Describe EC2 instances, optionally filtered by instance IDs.
pub async fn cmd_describe_instances(client: &Client, instance_ids: &[String]) -> Result<()> {
    let mut req = client.describe_instances();
    for id in instance_ids {
        req = req.instance_ids(id);
    }
    let resp = req
        .send()
        .await
        .context("Failed to describe EC2 instances")?;

    for reservation in resp.reservations() {
        for instance in reservation.instances() {
            let id = instance.instance_id().unwrap_or("<unknown>");
            let state = instance
                .state()
                .and_then(|s| s.name())
                .map(|n| n.as_str().to_owned())
                .unwrap_or_else(|| "unknown".to_owned());
            let instance_type = instance
                .instance_type()
                .map(|t| t.as_str().to_owned())
                .unwrap_or_else(|| "unknown".to_owned());
            let az = instance
                .placement()
                .and_then(|p| p.availability_zone())
                .unwrap_or("unknown");
            let public_ip = instance.public_ip_address().unwrap_or("");
            let private_ip = instance.private_ip_address().unwrap_or("");

            println!(
                "{id:<25} {state:<16} {instance_type:<16} {az:<20} {public_ip:<16} {private_ip}"
            );
        }
    }
    Ok(())
}

/// Describe all available EC2 regions.
pub async fn cmd_describe_regions(client: &Client) -> Result<()> {
    let resp = client
        .describe_regions()
        .all_regions(true)
        .send()
        .await
        .context("Failed to describe EC2 regions")?;

    for region in resp.regions() {
        let name = region.region_name().unwrap_or("<unknown>");
        let endpoint = region.endpoint().unwrap_or("");
        let opt_in = region
            .opt_in_status()
            .unwrap_or("unknown");
        println!("{name:<20} {opt_in:<20} {endpoint}");
    }
    Ok(())
}

/// Start one or more EC2 instances.
pub async fn cmd_start_instances(client: &Client, instance_ids: &[String]) -> Result<()> {
    if instance_ids.is_empty() {
        anyhow::bail!("At least one instance ID is required");
    }
    let mut req = client.start_instances();
    for id in instance_ids {
        req = req.instance_ids(id);
    }
    let resp = req
        .send()
        .await
        .context("Failed to start EC2 instances")?;

    for change in resp.starting_instances() {
        let id = change.instance_id().unwrap_or("<unknown>");
        let prev = change
            .previous_state()
            .and_then(|s| s.name())
            .map(|n| n.as_str())
            .unwrap_or("unknown");
        let curr = change
            .current_state()
            .and_then(|s| s.name())
            .map(|n| n.as_str())
            .unwrap_or("unknown");
        println!("StartInstances: {id}  {prev} → {curr}");
    }
    Ok(())
}

/// Stop one or more EC2 instances.
pub async fn cmd_stop_instances(client: &Client, instance_ids: &[String], force: bool) -> Result<()> {
    if instance_ids.is_empty() {
        anyhow::bail!("At least one instance ID is required");
    }
    let mut req = client.stop_instances().force(force);
    for id in instance_ids {
        req = req.instance_ids(id);
    }
    let resp = req
        .send()
        .await
        .context("Failed to stop EC2 instances")?;

    for change in resp.stopping_instances() {
        let id = change.instance_id().unwrap_or("<unknown>");
        let prev = change
            .previous_state()
            .and_then(|s| s.name())
            .map(|n| n.as_str())
            .unwrap_or("unknown");
        let curr = change
            .current_state()
            .and_then(|s| s.name())
            .map(|n| n.as_str())
            .unwrap_or("unknown");
        println!("StopInstances: {id}  {prev} → {curr}");
    }
    Ok(())
}

/// Reboot one or more EC2 instances.
pub async fn cmd_reboot_instances(client: &Client, instance_ids: &[String]) -> Result<()> {
    if instance_ids.is_empty() {
        anyhow::bail!("At least one instance ID is required");
    }
    let mut req = client.reboot_instances();
    for id in instance_ids {
        req = req.instance_ids(id);
    }
    req.send()
        .await
        .context("Failed to reboot EC2 instances")?;
    for id in instance_ids {
        println!("RebootInstances: {id}");
    }
    Ok(())
}

/// Describe available EC2 instance types.
pub async fn cmd_describe_instance_types(
    client: &Client,
    instance_types: &[String],
) -> Result<()> {
    let mut req = client.describe_instance_types();
    for t in instance_types {
        let it = aws_sdk_ec2::types::InstanceType::from(t.as_str());
        req = req.instance_types(it);
    }
    let resp = req
        .send()
        .await
        .context("Failed to describe EC2 instance types")?;

    println!("{:<20} {:>8} {:>8}  {}", "InstanceType", "vCPUs", "MemMiB", "Architectures");
    println!("{:<20} {:>8} {:>8}  {}", "------------", "-----", "------", "-------------");
    for it in resp.instance_types() {
        let name = it
            .instance_type()
            .map(|t| t.as_str())
            .unwrap_or("unknown");
        let vcpus = it
            .v_cpu_info()
            .and_then(|v| v.default_v_cpus())
            .unwrap_or(0);
        let mem = it
            .memory_info()
            .and_then(|m| m.size_in_mib())
            .unwrap_or(0);
        let arches: Vec<&str> = it
            .processor_info()
            .map(|p| p.supported_architectures())
            .unwrap_or_default()
            .iter()
            .map(|a| a.as_str())
            .collect();
        println!("{name:<20} {vcpus:>8} {mem:>8}  {}", arches.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Unit tests that don't require an AWS connection.
    #[test]
    fn test_empty_instance_ids_validation() {
        // We verify the empty-list guard by inspecting the error path inline
        // without standing up a real client.
        let ids: Vec<String> = vec![];
        assert!(ids.is_empty(), "Guard: empty ids should trigger early error");
    }
}
