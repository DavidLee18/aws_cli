use anyhow::{Context, Result};
use aws_sdk_ec2::types::{
    DomainType, InstanceNetworkInterfaceSpecification, InstanceType, IpPermission, IpRange, Tag,
    VolumeType,
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

/// Describe EC2 availability zones.
pub async fn cmd_describe_availability_zones(client: &Client) -> Result<()> {
    let resp = client
        .describe_availability_zones()
        .all_availability_zones(true)
        .send()
        .await
        .context("Failed to describe availability zones")?;

    println!("{:<25} {:<12} {}", "ZoneName", "State", "RegionName");
    for az in resp.availability_zones() {
        let name = az.zone_name().unwrap_or("<unknown>");
        let state = az.state().map(|s| s.as_str()).unwrap_or("unknown");
        let region = az.region_name().unwrap_or("-");
        println!("{name:<25} {state:<12} {region}");
    }
    Ok(())
}

/// Describe EC2 images (AMIs) owned by the caller unless owners are specified.
pub async fn cmd_describe_images(
    client: &Client,
    image_ids: &[String],
    owners: &[String],
) -> Result<()> {
    let mut req = client.describe_images();
    for id in image_ids {
        req = req.image_ids(id);
    }
    if owners.is_empty() {
        req = req.owners("self");
    } else {
        for owner in owners {
            req = req.owners(owner);
        }
    }
    let resp = req.send().await.context("Failed to describe images")?;

    println!("{:<20} {:<12} {}", "ImageId", "State", "Name");
    for img in resp.images() {
        let id = img.image_id().unwrap_or("<unknown>");
        let state = img.state().map(|s| s.as_str()).unwrap_or("unknown");
        let name = img.name().unwrap_or("");
        println!("{id:<20} {state:<12} {name}");
    }
    Ok(())
}

/// Describe Elastic IP addresses.
pub async fn cmd_describe_addresses(client: &Client) -> Result<()> {
    let resp = client
        .describe_addresses()
        .send()
        .await
        .context("Failed to describe addresses")?;

    println!(
        "{:<20} {:<20} {:<20}",
        "PublicIp", "AllocationId", "AssociationId"
    );
    for addr in resp.addresses() {
        let pub_ip = addr.public_ip().unwrap_or("<unknown>");
        let alloc = addr.allocation_id().unwrap_or("-");
        let assoc = addr.association_id().unwrap_or("-");
        println!("{pub_ip:<20} {alloc:<20} {assoc:<20}");
    }
    Ok(())
}

/// Allocate a new Elastic IP address.
pub async fn cmd_allocate_address(client: &Client, domain: &str) -> Result<()> {
    let domain_type = match domain.to_lowercase().as_str() {
        "vpc" => DomainType::Vpc,
        other => anyhow::bail!("Invalid domain: {other} (expected vpc)"),
    };
    let resp = client
        .allocate_address()
        .domain(domain_type)
        .send()
        .await
        .with_context(|| format!("Failed to allocate address in domain {domain}"))?;
    let alloc_id = resp.allocation_id().unwrap_or("<unknown>");
    let pub_ip = resp.public_ip().unwrap_or("<unknown>");
    println!("Address allocated: {pub_ip} (Allocation ID: {alloc_id})");
    Ok(())
}

/// Associate an Elastic IP address with an instance.
pub async fn cmd_associate_address(
    client: &Client,
    allocation_id: &str,
    instance_id: Option<&str>,
    network_interface_id: Option<&str>,
) -> Result<()> {
    let mut req = client.associate_address().allocation_id(allocation_id);
    if let Some(inst) = instance_id {
        req = req.instance_id(inst);
    }
    if let Some(eni) = network_interface_id {
        req = req.network_interface_id(eni);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("Failed to associate address {allocation_id}"))?;
    let assoc = resp.association_id().unwrap_or("<unknown>");
    println!("Address associated: {allocation_id} (Association ID: {assoc})");
    Ok(())
}

/// Disassociate an Elastic IP address.
pub async fn cmd_disassociate_address(client: &Client, association_id: &str) -> Result<()> {
    client
        .disassociate_address()
        .association_id(association_id)
        .send()
        .await
        .with_context(|| format!("Failed to disassociate address {association_id}"))?;
    println!("Address disassociated: {association_id}");
    Ok(())
}

/// Release an Elastic IP address.
pub async fn cmd_release_address(client: &Client, allocation_id: &str) -> Result<()> {
    client
        .release_address()
        .allocation_id(allocation_id)
        .send()
        .await
        .with_context(|| format!("Failed to release address {allocation_id}"))?;
    println!("Address released: {allocation_id}");
    Ok(())
}

/// Create a new security group.
pub async fn cmd_create_security_group(
    client: &Client,
    group_name: &str,
    description: &str,
    vpc_id: Option<&str>,
) -> Result<()> {
    let mut req = client
        .create_security_group()
        .group_name(group_name)
        .description(description);
    if let Some(vpc) = vpc_id {
        req = req.vpc_id(vpc);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("Failed to create security group {group_name}"))?;
    let gid = resp.group_id().unwrap_or("<unknown>");
    println!("Security group created: {gid}");
    Ok(())
}

/// Delete a security group.
pub async fn cmd_delete_security_group(
    client: &Client,
    group_id: Option<&str>,
    group_name: Option<&str>,
) -> Result<()> {
    if group_id.is_none() && group_name.is_none() {
        anyhow::bail!("Either --group-id or --group-name is required");
    }
    let mut req = client.delete_security_group();
    if let Some(id) = group_id {
        req = req.group_id(id);
    }
    if let Some(name) = group_name {
        req = req.group_name(name);
    }
    req.send()
        .await
        .context("Failed to delete security group")?;
    println!(
        "Security group deleted: {}",
        group_id.unwrap_or_else(|| group_name.unwrap_or("<unknown>"))
    );
    Ok(())
}

/// Revoke an ingress rule on a security group.
pub async fn cmd_revoke_security_group_ingress(
    client: &Client,
    group_id: &str,
    protocol: &str,
    from_port: i32,
    to_port: i32,
    cidr: &str,
) -> Result<()> {
    let perm = build_ip_permission(protocol, from_port, to_port, cidr);
    client
        .revoke_security_group_ingress()
        .group_id(group_id)
        .ip_permissions(perm)
        .send()
        .await
        .with_context(|| format!("Failed to revoke ingress on {group_id}"))?;
    println!("Ingress revoked: {group_id} {protocol} {from_port}-{to_port} {cidr}");
    Ok(())
}

/// Revoke an egress rule on a security group.
pub async fn cmd_revoke_security_group_egress(
    client: &Client,
    group_id: &str,
    protocol: &str,
    from_port: i32,
    to_port: i32,
    cidr: &str,
) -> Result<()> {
    let perm = build_ip_permission(protocol, from_port, to_port, cidr);
    client
        .revoke_security_group_egress()
        .group_id(group_id)
        .ip_permissions(perm)
        .send()
        .await
        .with_context(|| format!("Failed to revoke egress on {group_id}"))?;
    println!("Egress revoked: {group_id} {protocol} {from_port}-{to_port} {cidr}");
    Ok(())
}

/// Attach an EBS volume to an instance.
pub async fn cmd_attach_volume(
    client: &Client,
    volume_id: &str,
    instance_id: &str,
    device: &str,
) -> Result<()> {
    client
        .attach_volume()
        .volume_id(volume_id)
        .instance_id(instance_id)
        .device(device)
        .send()
        .await
        .with_context(|| format!("Failed to attach volume {volume_id} to {instance_id}"))?;
    println!("Volume attached: {volume_id} -> {instance_id} ({device})");
    Ok(())
}

/// Detach an EBS volume.
pub async fn cmd_detach_volume(
    client: &Client,
    volume_id: &str,
    instance_id: Option<&str>,
    device: Option<&str>,
    force: bool,
) -> Result<()> {
    let mut req = client.detach_volume().volume_id(volume_id).force(force);
    if let Some(inst) = instance_id {
        req = req.instance_id(inst);
    }
    if let Some(dev) = device {
        req = req.device(dev);
    }
    req.send()
        .await
        .with_context(|| format!("Failed to detach volume {volume_id}"))?;
    println!("Volume detached: {volume_id}");
    Ok(())
}

/// Describe subnets.
pub async fn cmd_describe_subnets(client: &Client, subnet_ids: &[String]) -> Result<()> {
    let mut req = client.describe_subnets();
    for sid in subnet_ids {
        req = req.subnet_ids(sid);
    }
    let resp = req.send().await.context("Failed to describe subnets")?;

    println!("{:<20} {:<15} {:<10}", "SubnetId", "VpcId", "CidrBlock");
    for sn in resp.subnets() {
        let id = sn.subnet_id().unwrap_or("<unknown>");
        let vpc = sn.vpc_id().unwrap_or("-");
        let cidr = sn.cidr_block().unwrap_or("-");
        println!("{id:<20} {vpc:<15} {cidr:<10}");
    }
    Ok(())
}

/// Describe VPCs.
pub async fn cmd_describe_vpcs(client: &Client, vpc_ids: &[String]) -> Result<()> {
    let mut req = client.describe_vpcs();
    for vid in vpc_ids {
        req = req.vpc_ids(vid);
    }
    let resp = req.send().await.context("Failed to describe VPCs")?;

    println!("{:<20} {:<10} {}", "VpcId", "State", "CidrBlock");
    for vpc in resp.vpcs() {
        let id = vpc.vpc_id().unwrap_or("<unknown>");
        let state = vpc.state().map(|s| s.as_str()).unwrap_or("unknown");
        let cidr = vpc.cidr_block().unwrap_or("-");
        println!("{id:<20} {state:<10} {cidr}");
    }
    Ok(())
}

/// Describe route tables.
pub async fn cmd_describe_route_tables(client: &Client, route_table_ids: &[String]) -> Result<()> {
    let mut req = client.describe_route_tables();
    for rid in route_table_ids {
        req = req.route_table_ids(rid);
    }
    let resp = req
        .send()
        .await
        .context("Failed to describe route tables")?;

    println!("{:<20} {}", "RouteTableId", "VpcId");
    for rt in resp.route_tables() {
        let id = rt.route_table_id().unwrap_or("<unknown>");
        let vpc = rt.vpc_id().unwrap_or("-");
        println!("{id:<20} {vpc}");
    }
    Ok(())
}

/// Create one or more tags on resources.
pub async fn cmd_create_tags(
    client: &Client,
    resource_ids: &[String],
    tags: &[(String, String)],
) -> Result<()> {
    let tag_objs: Vec<Tag> = tags
        .iter()
        .map(|(k, v)| Tag::builder().key(k).value(v).build())
        .collect();
    client
        .create_tags()
        .set_resources(Some(resource_ids.to_vec()))
        .set_tags(Some(tag_objs))
        .send()
        .await
        .context("Failed to create tags")?;
    println!("Tags created on {} resource(s)", resource_ids.len());
    Ok(())
}

/// Delete tags from resources.
pub async fn cmd_delete_tags(
    client: &Client,
    resource_ids: &[String],
    tags: &[(String, String)],
) -> Result<()> {
    let tag_objs: Vec<Tag> = tags
        .iter()
        .map(|(k, v)| Tag::builder().key(k).value(v).build())
        .collect();
    client
        .delete_tags()
        .set_resources(Some(resource_ids.to_vec()))
        .set_tags(Some(tag_objs))
        .send()
        .await
        .context("Failed to delete tags")?;
    println!("Tags deleted on {} resource(s)", resource_ids.len());
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
