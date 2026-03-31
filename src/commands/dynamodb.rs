use anyhow::{Context, Result};
use aws_sdk_dynamodb::Client;

/// List all DynamoDB tables.
pub async fn cmd_list_tables(client: &Client) -> Result<()> {
    let resp = client
        .list_tables()
        .send()
        .await
        .context("Failed to list DynamoDB tables")?;

    let table_names = resp.table_names();
    if table_names.is_empty() {
        println!("No tables found.");
    } else {
        for table_name in table_names {
            println!("{}", table_name);
        }
    }

    Ok(())
}

/// Describe a DynamoDB table.
pub async fn cmd_describe_table(client: &Client, table_name: &str) -> Result<()> {
    let resp = client
        .describe_table()
        .table_name(table_name)
        .send()
        .await
        .context("Failed to describe DynamoDB table")?;

    if let Some(table) = resp.table() {
        println!("Table Name:        {}", table.table_name().unwrap_or("N/A"));
        println!("Table ARN:         {}", table.table_arn().unwrap_or("N/A"));
        println!(
            "Table Status:      {}",
            table.table_status().map(|s| s.as_str()).unwrap_or("N/A")
        );
        println!("Item Count:        {}", table.item_count().unwrap_or(0));
        println!(
            "Table Size (bytes): {}",
            table.table_size_bytes().unwrap_or(0)
        );

        if let Some(created) = table.creation_date_time() {
            println!("Created:           {}", created);
        }

        let key_schema = table.key_schema();
        if !key_schema.is_empty() {
            println!("\nKey Schema:");
            for key in key_schema {
                println!("  {} ({})", key.attribute_name(), key.key_type().as_str());
            }
        }

        let attrs = table.attribute_definitions();
        if !attrs.is_empty() {
            println!("\nAttribute Definitions:");
            for attr in attrs {
                println!(
                    "  {} ({})",
                    attr.attribute_name(),
                    attr.attribute_type().as_str()
                );
            }
        }
    }

    Ok(())
}

/// Create a DynamoDB table.
pub async fn cmd_create_table(
    client: &Client,
    table_name: &str,
    partition_key: &str,
    partition_key_type: &str,
    sort_key: Option<&str>,
    sort_key_type: Option<&str>,
) -> Result<()> {
    use aws_sdk_dynamodb::types::{
        AttributeDefinition, KeySchemaElement, KeyType, ProvisionedThroughput, ScalarAttributeType,
    };

    let mut attribute_definitions = vec![AttributeDefinition::builder()
        .attribute_name(partition_key)
        .attribute_type(match partition_key_type {
            "S" => ScalarAttributeType::S,
            "N" => ScalarAttributeType::N,
            "B" => ScalarAttributeType::B,
            _ => anyhow::bail!("Invalid partition key type. Use S, N, or B"),
        })
        .build()?];

    let mut key_schema = vec![KeySchemaElement::builder()
        .attribute_name(partition_key)
        .key_type(KeyType::Hash)
        .build()?];

    if let (Some(sk), Some(skt)) = (sort_key, sort_key_type) {
        attribute_definitions.push(
            AttributeDefinition::builder()
                .attribute_name(sk)
                .attribute_type(match skt {
                    "S" => ScalarAttributeType::S,
                    "N" => ScalarAttributeType::N,
                    "B" => ScalarAttributeType::B,
                    _ => anyhow::bail!("Invalid sort key type. Use S, N, or B"),
                })
                .build()?,
        );

        key_schema.push(
            KeySchemaElement::builder()
                .attribute_name(sk)
                .key_type(KeyType::Range)
                .build()?,
        );
    }

    let provisioned_throughput = ProvisionedThroughput::builder()
        .read_capacity_units(5)
        .write_capacity_units(5)
        .build()?;

    let resp = client
        .create_table()
        .table_name(table_name)
        .set_attribute_definitions(Some(attribute_definitions))
        .set_key_schema(Some(key_schema))
        .provisioned_throughput(provisioned_throughput)
        .send()
        .await
        .context("Failed to create DynamoDB table")?;

    if let Some(table) = resp.table_description() {
        println!("Created table: {}", table.table_name().unwrap_or("N/A"));
        println!(
            "Status: {}",
            table.table_status().map(|s| s.as_str()).unwrap_or("N/A")
        );
    }

    Ok(())
}

/// Delete a DynamoDB table.
pub async fn cmd_delete_table(client: &Client, table_name: &str) -> Result<()> {
    let resp = client
        .delete_table()
        .table_name(table_name)
        .send()
        .await
        .context("Failed to delete DynamoDB table")?;

    if let Some(table) = resp.table_description() {
        println!("Deleting table: {}", table.table_name().unwrap_or("N/A"));
        println!(
            "Status: {}",
            table.table_status().map(|s| s.as_str()).unwrap_or("N/A")
        );
    }

    Ok(())
}

/// Update a DynamoDB table (provisioned throughput).
pub async fn cmd_update_table(
    client: &Client,
    table_name: &str,
    read_capacity: Option<i64>,
    write_capacity: Option<i64>,
) -> Result<()> {
    use aws_sdk_dynamodb::types::ProvisionedThroughput;

    if read_capacity.is_none() && write_capacity.is_none() {
        anyhow::bail!("Must provide at least one of --read-capacity or --write-capacity");
    }

    let mut req = client.update_table().table_name(table_name);

    if let (Some(read), Some(write)) = (read_capacity, write_capacity) {
        let throughput = ProvisionedThroughput::builder()
            .read_capacity_units(read)
            .write_capacity_units(write)
            .build()?;
        req = req.provisioned_throughput(throughput);
    }

    let resp = req
        .send()
        .await
        .context("Failed to update DynamoDB table")?;

    if let Some(table) = resp.table_description() {
        println!("Updated table: {}", table.table_name().unwrap_or("N/A"));
        println!(
            "Status: {}",
            table.table_status().map(|s| s.as_str()).unwrap_or("N/A")
        );
    }

    Ok(())
}

/// Get an item from a DynamoDB table.
pub async fn cmd_get_item(client: &Client, table_name: &str, key_json: &str) -> Result<()> {
    use serde_json::Value;

    let key_value: Value = serde_json::from_str(key_json).context("Failed to parse key JSON")?;

    let key = json_to_attribute_value(&key_value)?;

    let resp = client
        .get_item()
        .table_name(table_name)
        .set_key(Some(key))
        .send()
        .await
        .context("Failed to get item from DynamoDB")?;

    if let Some(item) = resp.item() {
        let json_item = attribute_map_to_json(item)?;
        println!("{}", serde_json::to_string_pretty(&json_item)?);
    } else {
        println!("Item not found.");
    }

    Ok(())
}

/// Put an item into a DynamoDB table.
pub async fn cmd_put_item(client: &Client, table_name: &str, item_json: &str) -> Result<()> {
    use serde_json::Value;

    let item_value: Value = serde_json::from_str(item_json).context("Failed to parse item JSON")?;

    let item = json_to_attribute_value(&item_value)?;

    client
        .put_item()
        .table_name(table_name)
        .set_item(Some(item))
        .send()
        .await
        .context("Failed to put item into DynamoDB")?;

    println!("Item added successfully.");

    Ok(())
}

/// Delete an item from a DynamoDB table.
pub async fn cmd_delete_item(client: &Client, table_name: &str, key_json: &str) -> Result<()> {
    use serde_json::Value;

    let key_value: Value = serde_json::from_str(key_json).context("Failed to parse key JSON")?;

    let key = json_to_attribute_value(&key_value)?;

    client
        .delete_item()
        .table_name(table_name)
        .set_key(Some(key))
        .send()
        .await
        .context("Failed to delete item from DynamoDB")?;

    println!("Item deleted successfully.");

    Ok(())
}

/// Scan a DynamoDB table.
pub async fn cmd_scan(client: &Client, table_name: &str, limit: Option<i32>) -> Result<()> {
    let mut req = client.scan().table_name(table_name);

    if let Some(l) = limit {
        req = req.limit(l);
    }

    let resp = req.send().await.context("Failed to scan DynamoDB table")?;

    let items = resp.items();
    if items.is_empty() {
        println!("No items found.");
    } else {
        println!("Found {} items:", items.len());
        for item in items {
            let json_item = attribute_map_to_json(item)?;
            println!("{}", serde_json::to_string_pretty(&json_item)?);
        }
    }

    Ok(())
}

// Helper functions to convert between JSON and DynamoDB AttributeValue
use aws_sdk_dynamodb::types::AttributeValue;
use serde_json::Value;
use std::collections::HashMap;

fn json_to_attribute_value(value: &Value) -> Result<HashMap<String, AttributeValue>> {
    let mut map = HashMap::new();

    if let Value::Object(obj) = value {
        for (key, val) in obj {
            let attr_val = match val {
                Value::String(s) => AttributeValue::S(s.clone()),
                Value::Number(n) => AttributeValue::N(n.to_string()),
                Value::Bool(b) => AttributeValue::Bool(*b),
                Value::Null => AttributeValue::Null(true),
                _ => anyhow::bail!("Unsupported JSON type for key {}", key),
            };
            map.insert(key.clone(), attr_val);
        }
    } else {
        anyhow::bail!("Expected JSON object");
    }

    Ok(map)
}

fn attribute_map_to_json(map: &HashMap<String, AttributeValue>) -> Result<Value> {
    let mut json_map = serde_json::Map::new();

    for (key, val) in map {
        let json_val = match val {
            AttributeValue::S(s) => Value::String(s.clone()),
            AttributeValue::N(n) => {
                // Try to parse as number
                if let Ok(num) = n.parse::<i64>() {
                    Value::Number(num.into())
                } else if let Ok(num) = n.parse::<f64>() {
                    Value::Number(serde_json::Number::from_f64(num).unwrap_or(0.into()))
                } else {
                    Value::String(n.clone())
                }
            }
            AttributeValue::Bool(b) => Value::Bool(*b),
            AttributeValue::Null(_) => Value::Null,
            _ => Value::String(format!("{:?}", val)),
        };
        json_map.insert(key.clone(), json_val);
    }

    Ok(Value::Object(json_map))
}
