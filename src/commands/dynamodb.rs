use anyhow::{Context, Result};
use aws_sdk_dynamodb::types::{
    DeleteRequest, KeysAndAttributes, PutRequest, WriteRequest,
};
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

/// Query a DynamoDB table.
pub async fn cmd_query(
    client: &Client,
    table_name: &str,
    key_condition_expression: &str,
    expr_attr_values_json: Option<&str>,
    limit: Option<i32>,
) -> Result<()> {
    let mut req = client
        .query()
        .table_name(table_name)
        .key_condition_expression(key_condition_expression);

    if let Some(raw) = expr_attr_values_json {
        let value: Value =
            serde_json::from_str(raw).context("Failed to parse expression-attribute-values JSON")?;
        let map = json_to_attribute_value(&value)?;
        req = req.set_expression_attribute_values(Some(map));
    }

    if let Some(l) = limit {
        req = req.limit(l);
    }

    let resp = req.send().await.context("Failed to query DynamoDB table")?;

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

/// Batch get items across one or more DynamoDB tables.
pub async fn cmd_batch_get_item(client: &Client, request_items_json: &str) -> Result<()> {
    let value: Value =
        serde_json::from_str(request_items_json).context("Failed to parse request-items JSON")?;

    let mut request_items = std::collections::HashMap::new();
    let obj = value
        .as_object()
        .context("request-items JSON must be an object mapping table -> keys")?;

    for (table_name, keys_value) in obj {
        let keys_array = keys_value
            .as_array()
            .context("Each table entry must be an array of key objects")?;

        let mut keys = Vec::with_capacity(keys_array.len());
        for key_val in keys_array {
            let key_map = json_to_attribute_value(key_val)?;
            keys.push(key_map);
        }

        let kaa = KeysAndAttributes::builder()
            .set_keys(Some(keys))
            .build()?;

        request_items.insert(table_name.clone(), kaa);
    }

    let resp = client
        .batch_get_item()
        .set_request_items(Some(request_items))
        .send()
        .await
        .context("Failed to batch-get items")?;

    if let Some(responses) = resp.responses() {
        for (table, items) in responses {
            println!("Table: {}", table);
            for item in items {
                let json_item = attribute_map_to_json(item)?;
                println!("{}", serde_json::to_string_pretty(&json_item)?);
            }
        }
    }

    Ok(())
}

/// Batch write items across one or more DynamoDB tables.
pub async fn cmd_batch_write_item(client: &Client, request_items_json: &str) -> Result<()> {
    let value: Value =
        serde_json::from_str(request_items_json).context("Failed to parse request-items JSON")?;

    let obj = value
        .as_object()
        .context("request-items JSON must be an object mapping table -> {put:[...], delete:[...]}")?;

    let mut request_items = std::collections::HashMap::new();

    for (table_name, ops) in obj {
        let ops_obj = ops
            .as_object()
            .context("Each table entry must be an object with put/delete arrays")?;

        let mut writes: Vec<WriteRequest> = Vec::new();

        if let Some(puts) = ops_obj.get("put") {
            let arr = puts
                .as_array()
                .context("'put' must be an array of items")?;
            for item_val in arr {
                let item_map = json_to_attribute_value(item_val)?;
                writes.push(
                    WriteRequest::builder()
                        .put_request(
                            PutRequest::builder()
                                .set_item(Some(item_map))
                                .build()?,
                        )
                        .build(),
                );
            }
        }

        if let Some(deletes) = ops_obj.get("delete") {
            let arr = deletes
                .as_array()
                .context("'delete' must be an array of keys")?;
            for key_val in arr {
                let key_map = json_to_attribute_value(key_val)?;
                writes.push(
                    WriteRequest::builder()
                        .delete_request(
                            DeleteRequest::builder()
                                .set_key(Some(key_map))
                                .build()?,
                        )
                        .build(),
                );
            }
        }

        if !writes.is_empty() {
            request_items.insert(table_name.clone(), writes);
        }
    }

    if request_items.is_empty() {
        anyhow::bail!("No write requests were provided");
    }

    client
        .batch_write_item()
        .set_request_items(Some(request_items))
        .send()
        .await
        .context("Failed to batch-write items")?;

    println!("Batch write submitted.");

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

    let obj = value
        .as_object()
        .context("Expected JSON object of attribute names to values")?;

    for (key, val) in obj {
        let attr_val = value_to_av(val)?;
        map.insert(key.clone(), attr_val);
    }

    Ok(map)
}

fn value_to_av(val: &Value) -> Result<AttributeValue> {
    if let Some(s) = val.as_str() {
        return Ok(AttributeValue::S(s.to_owned()));
    }
    if let Some(n) = val.as_i64() {
        return Ok(AttributeValue::N(n.to_string()));
    }
    if let Some(n) = val.as_f64() {
        return Ok(AttributeValue::N(n.to_string()));
    }
    if let Some(b) = val.as_bool() {
        return Ok(AttributeValue::Bool(b));
    }
    if val.is_null() {
        return Ok(AttributeValue::Null(true));
    }

    if let Some(obj) = val.as_object() {
        if obj.len() == 1 {
            if let Some(s) = obj.get("S").and_then(|v| v.as_str()) {
                return Ok(AttributeValue::S(s.to_owned()));
            }
            if let Some(n) = obj.get("N") {
                if let Some(intv) = n.as_i64() {
                    return Ok(AttributeValue::N(intv.to_string()));
                }
                if let Some(fv) = n.as_f64() {
                    return Ok(AttributeValue::N(fv.to_string()));
                }
                if let Some(raw) = n.as_str() {
                    return Ok(AttributeValue::N(raw.to_owned()));
                }
            }
            if let Some(b) = obj.get("BOOL").and_then(|v| v.as_bool()) {
                return Ok(AttributeValue::Bool(b));
            }
            if let Some(is_null) = obj.get("NULL").and_then(|v| v.as_bool()) {
                if is_null {
                    return Ok(AttributeValue::Null(true));
                }
            }
        }
    }

    anyhow::bail!("Unsupported JSON value for AttributeValue: {val}")
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
