use anyhow::{Context, Result};
use aws_sdk_lambda::Client;

/// Create a Lambda function from a ZIP file.
pub async fn cmd_create_function(
    client: &Client,
    function_name: &str,
    runtime: &str,
    role: &str,
    handler: &str,
    zip_file: &str,
    timeout: Option<i32>,
    memory_size: Option<i32>,
) -> Result<()> {
    let contents = std::fs::read(zip_file)
        .with_context(|| format!("Failed to read ZIP file: {}", zip_file))?;

    let mut req = client
        .create_function()
        .function_name(function_name)
        .runtime(aws_sdk_lambda::types::Runtime::from(runtime))
        .role(role)
        .handler(handler)
        .code(
            aws_sdk_lambda::types::FunctionCode::builder()
                .zip_file(aws_sdk_lambda::primitives::Blob::new(contents))
                .build(),
        );

    if let Some(t) = timeout {
        req = req.timeout(t);
    }

    if let Some(m) = memory_size {
        req = req.memory_size(m);
    }

    let resp = req
        .send()
        .await
        .context("Failed to create Lambda function")?;

    println!("Created function: {}", resp.function_name().unwrap_or("N/A"));
    println!("Function ARN: {}", resp.function_arn().unwrap_or("N/A"));
    println!(
        "Runtime: {}",
        resp.runtime().map(|r| r.as_str()).unwrap_or("N/A")
    );
    println!("Version: {}", resp.version().unwrap_or("N/A"));

    Ok(())
}

/// List Lambda functions.
pub async fn cmd_list_functions(client: &Client) -> Result<()> {
    let resp = client
        .list_functions()
        .send()
        .await
        .context("Failed to list Lambda functions")?;

    for func in resp.functions() {
        let name = func.function_name().unwrap_or("N/A");
        let runtime = func
            .runtime()
            .map(|r| r.as_str())
            .unwrap_or("N/A");
        let handler = func.handler().unwrap_or("N/A");
        let role = func.role().unwrap_or("N/A");

        println!("{:<40} {:<20} {:<30}", name, runtime, handler);
        println!("  Role: {}", role);
        println!();
    }

    Ok(())
}

/// Get details about a specific Lambda function.
pub async fn cmd_get_function(client: &Client, function_name: &str) -> Result<()> {
    let resp = client
        .get_function()
        .function_name(function_name)
        .send()
        .await
        .context("Failed to get Lambda function")?;

    if let Some(config) = resp.configuration() {
        println!("Function Name:  {}", config.function_name().unwrap_or("N/A"));
        println!("Function ARN:   {}", config.function_arn().unwrap_or("N/A"));
        println!("Runtime:        {}", config.runtime().map(|r| r.as_str()).unwrap_or("N/A"));
        println!("Handler:        {}", config.handler().unwrap_or("N/A"));
        println!("Role:           {}", config.role().unwrap_or("N/A"));
        println!("Code Size:      {} bytes", config.code_size());
        println!("Memory Size:    {} MB", config.memory_size().unwrap_or(0));
        println!("Timeout:        {} seconds", config.timeout().unwrap_or(0));
        println!("Last Modified:  {}", config.last_modified().unwrap_or("N/A"));
    }

    if let Some(code) = resp.code() {
        if let Some(location) = code.location() {
            println!("Code Location:  {}", location);
        }
    }

    Ok(())
}

/// Delete a Lambda function.
pub async fn cmd_delete_function(client: &Client, function_name: &str) -> Result<()> {
    client
        .delete_function()
        .function_name(function_name)
        .send()
        .await
        .context("Failed to delete Lambda function")?;

    println!("Deleted function: {}", function_name);
    Ok(())
}

/// Invoke a Lambda function synchronously.
pub async fn cmd_invoke(
    client: &Client,
    function_name: &str,
    payload: Option<&str>,
    log_type: Option<&str>,
) -> Result<()> {
    let mut req = client.invoke().function_name(function_name);

    if let Some(p) = payload {
        req = req.payload(aws_sdk_lambda::primitives::Blob::new(p.as_bytes()));
    }

    if let Some(lt) = log_type {
        req = req.log_type(
            aws_sdk_lambda::types::LogType::from(lt)
        );
    }

    let resp = req
        .send()
        .await
        .context("Failed to invoke Lambda function")?;

    println!("Status Code: {}", resp.status_code());

    if let Some(payload) = resp.payload() {
        let payload_str = String::from_utf8_lossy(payload.as_ref());
        println!("Response:\n{}", payload_str);
    }

    if let Some(log_result) = resp.log_result() {
        // Use base64 engine
        use base64::{Engine as _, engine::general_purpose};
        let decoded = match general_purpose::STANDARD.decode(log_result) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(_) => "Unable to decode logs".to_string(),
        };
        println!("\nLogs:\n{}", decoded);
    }

    if let Some(error) = resp.function_error() {
        println!("Function Error: {}", error);
    }

    Ok(())
}

/// List event source mappings for a Lambda function.
pub async fn cmd_list_event_source_mappings(
    client: &Client,
    function_name: Option<&str>,
) -> Result<()> {
    let mut req = client.list_event_source_mappings();

    if let Some(name) = function_name {
        req = req.function_name(name);
    }

    let resp = req
        .send()
        .await
        .context("Failed to list event source mappings")?;

    for mapping in resp.event_source_mappings() {
        println!("UUID:           {}", mapping.uuid().unwrap_or("N/A"));
        println!("Function ARN:   {}", mapping.function_arn().unwrap_or("N/A"));
        println!("Event Source:   {}", mapping.event_source_arn().unwrap_or("N/A"));
        println!("State:          {}", mapping.state().unwrap_or("N/A"));
        println!("Batch Size:     {}", mapping.batch_size().unwrap_or(0));
        println!();
    }

    Ok(())
}

/// Update Lambda function code from a ZIP file.
pub async fn cmd_update_function_code(
    client: &Client,
    function_name: &str,
    zip_file: Option<&str>,
    s3_bucket: Option<&str>,
    s3_key: Option<&str>,
) -> Result<()> {
    let mut req = client
        .update_function_code()
        .function_name(function_name);

    if let Some(zip_path) = zip_file {
        let contents = std::fs::read(zip_path)
            .with_context(|| format!("Failed to read ZIP file: {}", zip_path))?;
        req = req.zip_file(aws_sdk_lambda::primitives::Blob::new(contents));
    } else if let (Some(bucket), Some(key)) = (s3_bucket, s3_key) {
        req = req.s3_bucket(bucket).s3_key(key);
    } else {
        anyhow::bail!("Must provide either --zip-file or both --s3-bucket and --s3-key");
    }

    let resp = req
        .send()
        .await
        .context("Failed to update Lambda function code")?;

    println!("Updated function: {}", resp.function_name().unwrap_or("N/A"));
    println!("Code SHA256: {}", resp.code_sha256().unwrap_or("N/A"));
    println!("Version: {}", resp.version().unwrap_or("N/A"));

    Ok(())
}

/// Update Lambda function configuration.
pub async fn cmd_update_function_configuration(
    client: &Client,
    function_name: &str,
    memory_size: Option<i32>,
    timeout: Option<i32>,
    handler: Option<&str>,
) -> Result<()> {
    let mut req = client
        .update_function_configuration()
        .function_name(function_name);

    if let Some(mem) = memory_size {
        req = req.memory_size(mem);
    }

    if let Some(t) = timeout {
        req = req.timeout(t);
    }

    if let Some(h) = handler {
        req = req.handler(h);
    }

    let resp = req
        .send()
        .await
        .context("Failed to update Lambda function configuration")?;

    println!("Updated function configuration:");
    println!("  Memory Size: {} MB", resp.memory_size().unwrap_or(0));
    println!("  Timeout:     {} seconds", resp.timeout().unwrap_or(0));
    println!("  Handler:     {}", resp.handler().unwrap_or("N/A"));

    Ok(())
}
