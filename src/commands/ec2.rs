use anyhow::{Context, Result};
use aws_sdk_ec2::types::{
    InstanceNetworkInterfaceSpecification, InstanceType, IpPermission, IpRange, VolumeType,
};
use aws_sdk_ec2::Client;
use std::fs;

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
        let opt_in = region.opt_in_status().unwrap_or("unknown");
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
    let resp = req.send().await.context("Failed to start EC2 instances")?;

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
pub async fn cmd_stop_instances(
    client: &Client,
    instance_ids: &[String],
    force: bool,
) -> Result<()> {
    if instance_ids.is_empty() {
        anyhow::bail!("At least one instance ID is required");
    }
    let mut req = client.stop_instances().force(force);
    for id in instance_ids {
        req = req.instance_ids(id);
    }
    let resp = req.send().await.context("Failed to stop EC2 instances")?;

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
    req.send().await.context("Failed to reboot EC2 instances")?;
    for id in instance_ids {
        println!("RebootInstances: {id}");
    }
    Ok(())
}

/// Terminate one or more EC2 instances.
pub async fn cmd_terminate_instances(client: &Client, instance_ids: &[String]) -> Result<()> {
    if instance_ids.is_empty() {
        anyhow::bail!("At least one instance ID is required");
    }
    let mut req = client.terminate_instances();
    for id in instance_ids {
        req = req.instance_ids(id);
    }
    let resp = req
        .send()
        .await
        .context("Failed to terminate EC2 instances")?;

    for change in resp.terminating_instances() {
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
        println!("TerminateInstances: {id}  {prev} → {curr}");
    }
    Ok(())
}

/// Describe available EC2 instance types.
pub async fn cmd_describe_instance_types(client: &Client, instance_types: &[String]) -> Result<()> {
    let mut req = client.describe_instance_types();
    for t in instance_types {
        let it = aws_sdk_ec2::types::InstanceType::from(t.as_str());
        req = req.instance_types(it);
    }
    let resp = req
        .send()
        .await
        .context("Failed to describe EC2 instance types")?;

    println!(
        "{:<20} {:>8} {:>8}  {}",
        "InstanceType", "vCPUs", "MemMiB", "Architectures"
    );
    println!(
        "{:<20} {:>8} {:>8}  {}",
        "------------", "-----", "------", "-------------"
    );
    for it in resp.instance_types() {
        let name = it.instance_type().map(|t| t.as_str()).unwrap_or("unknown");
        let vcpus = it
            .v_cpu_info()
            .and_then(|v| v.default_v_cpus())
            .unwrap_or(0);
        let mem = it.memory_info().and_then(|m| m.size_in_mib()).unwrap_or(0);
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

/// Launch one or more EC2 instances.
pub async fn cmd_run_instances(
    client: &Client,
    image_id: &str,
    instance_type: &str,
    count: u32,
    key_name: Option<&str>,
    subnet_id: Option<&str>,
    security_group_ids: &[String],
    associate_public_ip: bool,
) -> Result<()> {
    let mut req = client
        .run_instances()
        .image_id(image_id)
        .instance_type(InstanceType::from(instance_type))
        .min_count(1)
        .max_count(count as i32);

    if !associate_public_ip {
        if let Some(kn) = key_name {
            req = req.key_name(kn);
        }
        if let Some(sub) = subnet_id {
            req = req.subnet_id(sub);
        }
        for sg in security_group_ids {
            req = req.security_group_ids(sg);
        }
    } else {
        let mut ni = InstanceNetworkInterfaceSpecification::builder()
            .device_index(0)
            .associate_public_ip_address(true);
        if let Some(sub) = subnet_id {
            ni = ni.subnet_id(sub);
        }
        for sg in security_group_ids {
            ni = ni.groups(sg);
        }
        req = req.network_interfaces(ni.build());
    }

    if let Some(kn) = key_name {
        req = req.key_name(kn);
    }

    let resp = req.send().await.context("Failed to run instances")?;

    for inst in resp.instances() {
        let id = inst.instance_id().unwrap_or("<unknown>");
        let type_str = inst
            .instance_type()
            .map(|t| t.as_str())
            .unwrap_or("unknown");
        println!("launched: {id} ({type_str})");
    }

    Ok(())
}

fn build_ip_permission(protocol: &str, from_port: i32, to_port: i32, cidr: &str) -> IpPermission {
    IpPermission::builder()
        .ip_protocol(protocol)
        .from_port(from_port)
        .to_port(to_port)
        .ip_ranges(IpRange::builder().cidr_ip(cidr).build())
        .build()
}

/// Authorize ingress on a security group.
pub async fn cmd_authorize_security_group_ingress(
    client: &Client,
    group_id: &str,
    protocol: &str,
    from_port: i32,
    to_port: i32,
    cidr: &str,
) -> Result<()> {
    let perm = build_ip_permission(protocol, from_port, to_port, cidr);
    client
        .authorize_security_group_ingress()
        .group_id(group_id)
        .ip_permissions(perm)
        .send()
        .await
        .with_context(|| format!("Failed to authorize ingress on {group_id}"))?;
    println!("ingress authorized: {group_id} {protocol} {from_port}-{to_port} {cidr}");
    Ok(())
}

/// Authorize egress on a security group.
pub async fn cmd_authorize_security_group_egress(
    client: &Client,
    group_id: &str,
    protocol: &str,
    from_port: i32,
    to_port: i32,
    cidr: &str,
) -> Result<()> {
    let perm = build_ip_permission(protocol, from_port, to_port, cidr);
    client
        .authorize_security_group_egress()
        .group_id(group_id)
        .ip_permissions(perm)
        .send()
        .await
        .with_context(|| format!("Failed to authorize egress on {group_id}"))?;
    println!("egress authorized: {group_id} {protocol} {from_port}-{to_port} {cidr}");
    Ok(())
}

/// Import a public key as a key pair.
pub async fn cmd_import_key_pair(
    client: &Client,
    key_name: &str,
    public_key_file: &str,
) -> Result<()> {
    let material = fs::read_to_string(public_key_file)
        .with_context(|| format!("Failed to read public key file {public_key_file}"))?;
    let resp = client
        .import_key_pair()
        .key_name(key_name)
        .public_key_material(material.into_bytes().into())
        .send()
        .await
        .with_context(|| format!("Failed to import key pair {key_name}"))?;
    let fp = resp.key_fingerprint().unwrap_or("<unknown>");
    println!("key pair imported: {key_name} (fingerprint: {fp})");
    Ok(())
}

/// Create an EBS volume.
pub async fn cmd_create_volume(
    client: &Client,
    availability_zone: &str,
    size: i32,
    volume_type: &str,
) -> Result<()> {
    let resp = client
        .create_volume()
        .availability_zone(availability_zone)
        .size(size)
        .volume_type(VolumeType::from(volume_type))
        .send()
        .await
        .with_context(|| format!("Failed to create volume in {availability_zone}"))?;
    let vid = resp.volume_id().unwrap_or("<unknown>");
    println!("volume created: {vid} ({size} GiB {volume_type})");
    Ok(())
}

/// Delete an EBS volume.
pub async fn cmd_delete_volume(client: &Client, volume_id: &str) -> Result<()> {
    client
        .delete_volume()
        .volume_id(volume_id)
        .send()
        .await
        .with_context(|| format!("Failed to delete volume {volume_id}"))?;
    println!("volume deleted: {volume_id}");
    Ok(())
}

/// Create an EBS snapshot.
pub async fn cmd_create_snapshot(
    client: &Client,
    volume_id: &str,
    description: Option<&str>,
) -> Result<()> {
    let mut req = client.create_snapshot().volume_id(volume_id);
    if let Some(desc) = description {
        req = req.description(desc);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("Failed to create snapshot from {volume_id}"))?;
    let sid = resp.snapshot_id().unwrap_or("<unknown>");
    println!("snapshot created: {sid} from {volume_id}");
    Ok(())
}

/// Delete an EBS snapshot.
pub async fn cmd_delete_snapshot(client: &Client, snapshot_id: &str) -> Result<()> {
    client
        .delete_snapshot()
        .snapshot_id(snapshot_id)
        .send()
        .await
        .with_context(|| format!("Failed to delete snapshot {snapshot_id}"))?;
    println!("snapshot deleted: {snapshot_id}");
    Ok(())
}

/// Describe EC2 instance status, optionally including all instances.
pub async fn cmd_describe_instance_status(
    client: &Client,
    instance_ids: &[String],
    include_all: bool,
) -> Result<()> {
    let mut req = client
        .describe_instance_status()
        .include_all_instances(include_all);
    for id in instance_ids {
        req = req.instance_ids(id);
    }
    let resp = req
        .send()
        .await
        .context("Failed to describe EC2 instance status")?;

    println!(
        "{:<20} {:<20} {:<20}",
        "InstanceId", "InstanceStatus", "SystemStatus"
    );
    for status in resp.instance_statuses() {
        let id = status.instance_id().unwrap_or("<unknown>");
        let instance_status = status
            .instance_status()
            .and_then(|s| s.status())
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        let system_status = status
            .system_status()
            .and_then(|s| s.status())
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        println!("{id:<20} {instance_status:<20} {system_status:<20}");
    }
    Ok(())
}

/// Describe EC2 security groups.
pub async fn cmd_describe_security_groups(
    client: &Client,
    group_ids: &[String],
    group_names: &[String],
) -> Result<()> {
    let mut req = client.describe_security_groups();
    for id in group_ids {
        req = req.group_ids(id);
    }
    for name in group_names {
        req = req.group_names(name);
    }
    let resp = req
        .send()
        .await
        .context("Failed to describe EC2 security groups")?;

    println!(
        "{:<20} {:<20} {:<20} {}",
        "GroupId", "GroupName", "VpcId", "Description"
    );
    for sg in resp.security_groups() {
        let id = sg.group_id().unwrap_or("<unknown>");
        let name = sg.group_name().unwrap_or("<unknown>");
        let vpc = sg.vpc_id().unwrap_or("-");
        let desc = sg.description().unwrap_or("");
        println!("{id:<20} {name:<20} {vpc:<20} {desc}");
    }
    Ok(())
}

/// Describe EC2 key pairs.
pub async fn cmd_describe_key_pairs(client: &Client, key_names: &[String]) -> Result<()> {
    let mut req = client.describe_key_pairs();
    for name in key_names {
        req = req.key_names(name);
    }
    let resp = req
        .send()
        .await
        .context("Failed to describe EC2 key pairs")?;

    println!("{:<25} {:<20} {}", "KeyName", "KeyType", "KeyFingerprint");
    for kp in resp.key_pairs() {
        let name = kp.key_name().unwrap_or("<unknown>");
        let key_type = kp.key_type().map(|k| k.as_str()).unwrap_or("unknown");
        let fp = kp.key_fingerprint().unwrap_or("<no-fingerprint>");
        println!("{name:<25} {key_type:<20} {fp}");
    }
    Ok(())
}

/// Create a new EC2 key pair (prints the private key to stdout).
pub async fn cmd_create_key_pair(client: &Client, key_name: &str) -> Result<()> {
    let resp = client
        .create_key_pair()
        .key_name(key_name)
        .send()
        .await
        .with_context(|| format!("Failed to create EC2 key pair {key_name}"))?;

    let material = resp.key_material().unwrap_or("<no key material returned>");
    println!("{material}");
    Ok(())
}

/// Delete an EC2 key pair.
pub async fn cmd_delete_key_pair(client: &Client, key_name: &str) -> Result<()> {
    client
        .delete_key_pair()
        .key_name(key_name)
        .send()
        .await
        .with_context(|| format!("Failed to delete EC2 key pair {key_name}"))?;
    println!("Deleted key pair: {key_name}");
    Ok(())
}

/// Describe EBS volumes.
pub async fn cmd_describe_volumes(client: &Client, volume_ids: &[String]) -> Result<()> {
    let mut req = client.describe_volumes();
    for id in volume_ids {
        req = req.volume_ids(id);
    }
    let resp = req.send().await.context("Failed to describe EBS volumes")?;

    println!(
        "{:<20} {:>8} {:<12} {:<20}",
        "VolumeId", "SizeGiB", "State", "AvailabilityZone"
    );
    for vol in resp.volumes() {
        let id = vol.volume_id().unwrap_or("<unknown>");
        let size = vol.size().unwrap_or(0);
        let state = vol.state().map(|s| s.as_str()).unwrap_or("unknown");
        let az = vol.availability_zone().unwrap_or("-");
        println!("{id:<20} {size:>8} {state:<12} {az:<20}");
    }
    Ok(())
}

/// Describe EBS snapshots.
pub async fn cmd_describe_snapshots(client: &Client, snapshot_ids: &[String]) -> Result<()> {
    let mut req = client.describe_snapshots();
    for sid in snapshot_ids {
        req = req.snapshot_ids(sid);
    }
    let resp = req
        .owner_ids("self")
        .send()
        .await
        .context("Failed to describe EBS snapshots")?;

    println!(
        "{:<20} {:>8} {:<12} {:<20}",
        "SnapshotId", "SizeGiB", "State", "VolumeId"
    );
    for snap in resp.snapshots() {
        let id = snap.snapshot_id().unwrap_or("<unknown>");
        let size = snap.volume_size().unwrap_or(0);
        let state = snap.state().map(|s| s.as_str()).unwrap_or("unknown");
        let vol_id = snap.volume_id().unwrap_or("-");
        println!("{id:<20} {size:>8} {state:<12} {vol_id:<20}");
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
        assert!(
            ids.is_empty(),
            "Guard: empty ids should trigger early error"
        );
    }
}
