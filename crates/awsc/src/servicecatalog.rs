//! `aws servicecatalog generate product` and `generate provisioning-artifact`.
//!
//! A port of `customizations/servicecatalog/`. Both do the same two things: upload a
//! CloudFormation template to S3, then create something in Service Catalog that points at
//! it by URL.
//!
//! Three details are easy to get wrong and all three are visible in the result:
//!
//! - **The S3 key is the file's basename**, not its path. `generate product --file-path
//!   deep/nested/template.yaml` uploads to `s3://bucket/template.yaml`, so two templates
//!   with the same name in different directories overwrite each other.
//! - **The URL is path-style and hardcoded**, not the endpoint the ruleset would resolve:
//!   `https://s3.amazonaws.com/<bucket>/<key>` in `us-east-1` and
//!   `https://s3-<region>.amazonaws.com/...` everywhere else. That is the old
//!   `s3-<region>` spelling with a hyphen, not `s3.<region>`.
//! - **The response is printed with `json.dumps(indent=2)` and no trailing newline**,
//!   not through the output formatter — so `--output text` does nothing here, and the
//!   two-space indent differs from the four the formatter uses everywhere else.

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use serde_json::{json, Value};
use std::process::ExitCode;

const PRODUCT_FLAGS: &[&str] = &[
    "--product-name",
    "--product-owner",
    "--product-type",
    "--product-description",
    "--product-distributor",
    "--tags",
    "--file-path",
    "--bucket-name",
    "--support-description",
    "--support-email",
    "--provisioning-artifact-name",
    "--provisioning-artifact-description",
    "--provisioning-artifact-type",
];

const ARTIFACT_FLAGS: &[&str] = &[
    "--file-path",
    "--bucket-name",
    "--provisioning-artifact-name",
    "--provisioning-artifact-description",
    "--provisioning-artifact-type",
    "--product-id",
];

const PRODUCT_TYPES: &[&str] = &["CLOUD_FORMATION_TEMPLATE", "MARKETPLACE"];
const ARTIFACT_TYPES: &[&str] =
    &["CLOUD_FORMATION_TEMPLATE", "MARKETPLACE_AMI", "MARKETPLACE_CAR"];

pub fn dispatch(parsed: &Parsed, globals: &Globals) -> Result<Option<ExitCode>, Failure> {
    if parsed.operation != "generate" {
        return Ok(None);
    }
    // `generate` is a command *tree*: the real command is the positional after it.
    match parsed.positionals.first().map(String::as_str) {
        Some("product") => generate_product(parsed, globals).map(Some),
        Some("provisioning-artifact") => generate_artifact(parsed, globals).map(Some),
        Some(other) => Err(Failure::after_usage(awsc_runtime::RuntimeError::ParamValidation(
            format!(
                "argument operation: Invalid choice: '{other}', maybe you meant:\n\n  \
                 * product\n  * provisioning-artifact"
            ),
        ))),
        None => Err(Failure::after_usage(awsc_runtime::RuntimeError::ParamValidation(
            "the following arguments are required: operation".to_string(),
        ))),
    }
}

fn generate_product(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(parsed, PRODUCT_FLAGS)?;
    let required = [
        "--product-name",
        "--product-owner",
        "--product-type",
        "--file-path",
        "--bucket-name",
        "--provisioning-artifact-name",
        "--provisioning-artifact-description",
        "--provisioning-artifact-type",
    ];
    let missing: Vec<&str> =
        required.into_iter().filter(|flag| !args.contains_key(flag)).collect();
    if !missing.is_empty() {
        return Err(crate::custom::missing_required(&missing));
    }
    let value = |flag: &str| args.get(flag).copied().flatten().unwrap_or_default();
    check_choice("--product-type", value("--product-type"), PRODUCT_TYPES)?;
    check_choice(
        "--provisioning-artifact-type",
        value("--provisioning-artifact-type"),
        ARTIFACT_TYPES,
    )?;

    let (region, url) = upload(parsed, globals, value("--bucket-name"), value("--file-path"))?;

    let mut input = json!({
        "Name": value("--product-name"),
        "Owner": value("--product-owner"),
        "ProductType": value("--product-type"),
        "Tags": tags(args.get("--tags").copied().flatten()),
        "ProvisioningArtifactParameters": {
            "Name": value("--provisioning-artifact-name"),
            "Description": value("--provisioning-artifact-description"),
            "Info": { "LoadTemplateFromURL": url },
            "Type": value("--provisioning-artifact-type"),
        },
    });
    // Optional members are added only when given: sending `"Description": ""` is not the
    // same request as omitting it.
    for (flag, member) in [
        ("--support-description", "SupportDescription"),
        ("--product-description", "Description"),
        ("--support-email", "SupportEmail"),
        ("--product-distributor", "Distributor"),
    ] {
        if let Some(Some(text)) = args.get(flag).copied() {
            if !text.is_empty() {
                input[member] = Value::String(text.to_string());
            }
        }
    }

    call_and_print(globals, &region, "create-product", &input)
}

fn generate_artifact(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(parsed, ARTIFACT_FLAGS)?;
    let missing: Vec<&str> =
        ARTIFACT_FLAGS.iter().copied().filter(|flag| !args.contains_key(flag)).collect();
    if !missing.is_empty() {
        return Err(crate::custom::missing_required(&missing));
    }
    let value = |flag: &str| args.get(flag).copied().flatten().unwrap_or_default();
    check_choice(
        "--provisioning-artifact-type",
        value("--provisioning-artifact-type"),
        ARTIFACT_TYPES,
    )?;

    let (region, url) = upload(parsed, globals, value("--bucket-name"), value("--file-path"))?;

    let input = json!({
        "ProductId": value("--product-id"),
        "Parameters": {
            "Name": value("--provisioning-artifact-name"),
            "Description": value("--provisioning-artifact-description"),
            "Info": { "LoadTemplateFromURL": url },
            "Type": value("--provisioning-artifact-type"),
        },
    });
    call_and_print(globals, &region, "create-provisioning-artifact", &input)
}

/// Put the file in the bucket and return the region plus the URL to hand Service Catalog.
///
/// The upload is unconditional — the reference builds its uploader with
/// `force_upload=True`, so an object already at that key is replaced without a check.
fn upload(
    parsed: &Parsed,
    globals: &Globals,
    bucket: &str,
    file_path: &str,
) -> Result<(String, String), Failure> {
    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    let key = s3_key(file_path);
    // Checked before the call so a missing file reports the reference's message rather
    // than a protocol-layer complaint about an unreadable payload.
    if !std::path::Path::new(file_path).is_file() {
        return Err(Failure::new(exit::GENERAL_ERROR, format!("{file_path} cannot be found")));
    }

    let s3_globals = Globals { region: Some(region.clone()), ..globals.clone() };
    let model = crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let s3 = Client::new(&model, &s3_globals)?;
    // `Body` on a streaming blob is the *path*: the request layer sends the file as a
    // handle rather than reading it into memory, so a large template costs nothing here.
    s3.call("put-object", Some(&json!({ "Bucket": bucket, "Key": key, "Body": file_path })))?;
    let _ = parsed;
    Ok((region.clone(), make_url(&region, bucket, &key)))
}

/// The S3 key for an uploaded file: its basename, path discarded.
fn s3_key(file_path: &str) -> String {
    std::path::Path::new(file_path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_path.to_string())
}

/// The path-style URL the reference hands to Service Catalog.
fn make_url(region: &str, bucket: &str, key: &str) -> String {
    let base = if region.is_empty() || region == "us-east-1" {
        "https://s3.amazonaws.com".to_string()
    } else {
        // `s3-<region>`, with a hyphen: the old path-style host, not `s3.<region>`.
        format!("https://s3-{region}.amazonaws.com")
    };
    format!("{base}/{bucket}/{key}")
}

/// `--tags Key=k1,Value=v1 Key=k2,Value=v2` — each token is its own tag, and each
/// comma-separated pair inside it becomes one entry of that tag's map.
fn tags(raw: Option<&str>) -> Value {
    let Some(raw) = raw.filter(|text| !text.is_empty()) else { return json!([]) };
    let parsed: Vec<Value> = raw
        .split_whitespace()
        .map(|token| {
            let mut map = serde_json::Map::new();
            for pair in token.split(',') {
                if let Some((key, value)) = pair.split_once('=') {
                    map.insert(key.to_string(), Value::String(value.to_string()));
                }
            }
            Value::Object(map)
        })
        .collect();
    Value::Array(parsed)
}

fn check_choice(flag: &str, given: &str, choices: &[&str]) -> Result<(), Failure> {
    if choices.contains(&given) {
        return Ok(());
    }
    Err(Failure::after_usage(awsc_runtime::RuntimeError::ParamValidation(format!(
        "argument {flag}: Invalid choice, valid choices are:\n\n{}",
        choices.join(" | ")
    ))))
}

/// Call Service Catalog and print the response as the reference does: two-space JSON,
/// `ResponseMetadata` removed, and **no trailing newline**.
fn call_and_print(
    globals: &Globals,
    region: &str,
    operation: &str,
    input: &Value,
) -> Result<ExitCode, Failure> {
    let sc_globals = Globals { region: Some(region.to_string()), ..globals.clone() };
    let model =
        crate::load_model("servicecatalog").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let client = Client::new(&model, &sc_globals)?;
    let mut response = client.call(operation, Some(input))?;
    if let Some(fields) = response.as_object_mut() {
        fields.remove("ResponseMetadata");
    }
    print!("{}", two_space_json(&response));
    Ok(exit::code(exit::SUCCESS))
}

fn two_space_json(value: &Value) -> String {
    let mut buffer = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"  ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, formatter);
    serde::Serialize::serialize(value, &mut serializer).expect("a JSON value serializes");
    String::from_utf8(buffer).expect("serde_json emits UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key is the basename, so two templates with the same name collide in S3.
    #[test]
    fn the_key_is_the_files_basename() {
        assert_eq!(s3_key("deep/nested/template.yaml"), "template.yaml");
        assert_eq!(s3_key("template.yaml"), "template.yaml");
    }

    /// `s3-<region>`, with a hyphen — the old path-style host.
    #[test]
    fn the_url_is_path_style_with_a_hyphenated_host() {
        assert_eq!(
            make_url("us-east-1", "b", "t.yaml"),
            "https://s3.amazonaws.com/b/t.yaml"
        );
        assert_eq!(
            make_url("eu-west-1", "b", "t.yaml"),
            "https://s3-eu-west-1.amazonaws.com/b/t.yaml"
        );
    }

    #[test]
    fn tags_split_on_spaces_then_commas() {
        assert_eq!(
            tags(Some("Key=k1,Value=v1 Key=k2,Value=v2")),
            json!([{"Key": "k1", "Value": "v1"}, {"Key": "k2", "Value": "v2"}])
        );
        assert_eq!(tags(None), json!([]));
        assert_eq!(tags(Some("")), json!([]));
    }

    /// Two spaces, not the formatter's four, and no trailing newline.
    #[test]
    fn the_response_is_two_space_json_without_a_newline() {
        let text = two_space_json(&json!({"ProductViewDetail": {"Status": "CREATED"}}));
        assert_eq!(
            text,
            "{\n  \"ProductViewDetail\": {\n    \"Status\": \"CREATED\"\n  }\n}"
        );
        assert!(!text.ends_with('\n'));
    }

    #[test]
    fn a_bad_choice_names_the_valid_ones() {
        let failure = check_choice("--product-type", "NOPE", PRODUCT_TYPES).expect_err("rejects");
        assert!(failure.message().contains("CLOUD_FORMATION_TEMPLATE | MARKETPLACE"));
    }
}
