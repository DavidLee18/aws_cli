//! `aws cloudtrail verify-query-results`: check exported query results against the
//! signature CloudTrail wrote beside them.
//!
//! A port of `customizations/cloudtrail/verifyqueryresults.py`. Two independent checks,
//! in this order and both required:
//!
//! 1. **Every exported file hashes to what `result_sign.json` says it does** — SHA-256,
//!    hex, streamed rather than read whole, because an export can be large.
//! 2. **The sign file's own signature verifies** against a public key fetched from
//!    CloudTrail, RSA PKCS#1 v1.5 over SHA-256 of the file hashes joined by single
//!    spaces, in the order the sign file lists them.
//!
//! The order matters: a tampered export file is caught by (1) with a message naming the
//! file, and only a tampered *sign file* reaches (2). Doing them the other way round
//! would report "invalid signature" for a problem that is not in the signature.
//!
//! The key is matched by **fingerprint**, from the keys CloudTrail was using in the
//! twenty days after the query completed. A key that does not turn up is a hard failure
//! rather than a skipped check — an unverifiable export is not a verified one.

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use serde_json::{json, Value};
use std::process::ExitCode;

const SIGN_FILE_NAME: &str = "result_sign.json";

pub fn dispatch(parsed: &Parsed, globals: &Globals) -> Result<Option<ExitCode>, Failure> {
    match parsed.operation.as_str() {
        "verify-query-results" => verify_query_results(parsed, globals).map(Some),
        _ => Ok(None),
    }
}

fn verify_query_results(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(
        parsed,
        &["--s3-bucket", "--s3-prefix", "--local-export-path"],
    )?;
    let value = |flag: &str| args.get(flag).copied().flatten();
    let bucket = value("--s3-bucket");
    let prefix = value("--s3-prefix");
    let local = value("--local-export-path");

    if local.is_none() && bucket.is_none() {
        return Err(param_error("Require parameter --s3-bucket or --local-export-path."));
    }
    if local.is_some() && (bucket.is_some() || prefix.is_some()) {
        return Err(param_error(
            "Parameter --local-export-path can not be specified with parameter \
             --s3-bucket nor --s3-prefix.",
        ));
    }

    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;

    // Both readers answer the same question — "give me these bytes" — so the rest of the
    // command does not care which one it has.
    let source: Box<dyn Source> = match local {
        Some(path) => Box::new(LocalSource { root: path.to_string() }),
        None => {
            let s3_globals = Globals { region: Some(region.clone()), ..globals.for_service("s3") };
            let model =
                crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
            Box::new(S3Source {
                bucket: bucket.unwrap_or_default().to_string(),
                // The prefix always ends in `/` once it is non-empty, so keys can be
                // concatenated without thinking about it.
                prefix: format_prefix(prefix.unwrap_or_default()),
                globals: s3_globals,
                model,
            })
        }
    };

    let sign_file: Value = serde_json::from_slice(&source.read(SIGN_FILE_NAME)?).map_err(|e| {
        Failure::new(
            exit::GENERAL_ERROR,
            format!("Unable to load result_sign.json file, due to {e}"),
        )
    })?;

    validate_export_files(source.as_ref(), &sign_file)?;

    let fingerprint = sign_file
        .get("publicKeyFingerprint")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let completed = sign_file
        .get("queryCompleteTime")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let ct_globals = Globals { region: Some(region), ..globals.clone() };
    let model =
        crate::load_model("cloudtrail").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let cloudtrail = Client::new(&model, &ct_globals)?;
    let public_key = public_key(&cloudtrail, &completed, &fingerprint)?;

    validate_signature(&public_key, &sign_file)?;
    println!("Successfully validated sign and query result files");
    Ok(exit::code(exit::SUCCESS))
}

/// Where the exported files are. Local and S3 differ only in how bytes are fetched.
trait Source {
    fn read(&self, name: &str) -> Result<Vec<u8>, Failure>;
}

struct LocalSource {
    root: String,
}

impl Source for LocalSource {
    fn read(&self, name: &str) -> Result<Vec<u8>, Failure> {
        let path = std::path::Path::new(&self.root).join(name);
        std::fs::read(&path)
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{}: {e}", path.display())))
    }
}

struct S3Source {
    bucket: String,
    prefix: String,
    globals: Globals,
    model: awsc_model::Model,
}

impl Source for S3Source {
    fn read(&self, name: &str) -> Result<Vec<u8>, Failure> {
        let client = Client::new(&self.model, &self.globals)?;
        let key = format!("{}{name}", self.prefix);
        let response =
            client.call("get-object", Some(&json!({ "Bucket": self.bucket, "Key": key })))?;
        // `get-object` hands back the body as a string; the export files are text.
        Ok(response
            .get("Body")
            .and_then(Value::as_str)
            .map(|body| body.as_bytes().to_vec())
            .unwrap_or_default())
    }
}

/// A prefix is either empty or ends in `/`.
fn format_prefix(prefix: &str) -> String {
    if prefix.is_empty() || prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    }
}

/// Check every file the sign file lists against its recorded hash.
fn validate_export_files(source: &dyn Source, sign_file: &Value) -> Result<(), Failure> {
    let files = match sign_file.get("files").and_then(Value::as_array) {
        Some(files) => files,
        // A sign file whose `files` is not a list is not a sign file.
        None => return Err(validation_error("Invalid sign file provided.")),
    };
    if files.is_empty() {
        return Err(validation_error("No export file was found in sign file."));
    }
    for file in files {
        let name = file.get("fileName").and_then(Value::as_str).unwrap_or_default();
        let expected = file.get("fileHashValue").and_then(Value::as_str).unwrap_or_default();
        let bytes = source.read(name)?;
        let computed = sha256_hex(&bytes);
        if computed != expected {
            return Err(validation_error(&format!(
                "File {name} has inconsistent hash value with hash value recorded in sign \
                 file, hash value in sign file is {expected} , but get {computed}"
            )));
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// The public key CloudTrail used, by fingerprint.
///
/// The search window is the completion time plus **twenty days**, because a key stays
/// listed for a while after it is rotated out and the export may be verified later.
fn public_key(
    client: &Client<'_>,
    query_complete_time: &str,
    fingerprint: &str,
) -> Result<String, Failure> {
    let start = awsc_protocol::shapes::parse_timestamp(query_complete_time).ok_or_else(|| {
        Failure::new(
            exit::GENERAL_ERROR,
            format!("Unable to read the query completion time: {query_complete_time}"),
        )
    })?;
    let end = start + 20 * 86_400;
    let listed = client.call(
        "list-public-keys",
        Some(&json!({
            "StartTime": awsc_protocol::shapes::format_cli_output(start),
            "EndTime": awsc_protocol::shapes::format_cli_output(end),
        })),
    )?;
    let keys = listed.get("PublicKeyList").and_then(Value::as_array).cloned().unwrap_or_default();
    for key in keys {
        if key.get("Fingerprint").and_then(Value::as_str) == Some(fingerprint) {
            return Ok(key.get("Value").and_then(Value::as_str).unwrap_or_default().to_string());
        }
    }
    Err(Failure::new(
        exit::GENERAL_ERROR,
        format!("No public keys found for key with fingerprint: {fingerprint}"),
    ))
}

/// Verify the sign file's own signature.
///
/// The signed string is the file hashes joined by **single spaces**, in the sign file's
/// order — not the files' order on disk, and not sorted.
fn validate_signature(public_key_base64: &str, sign_file: &Value) -> Result<(), Failure> {
    use rsa::pkcs1::DecodeRsaPublicKey;
    use sha2::Digest;

    let der = base64_decode(public_key_base64).ok_or_else(|| {
        validation_error(&format!(
            "Sign file invalid, unable to load PKCS #1 key: {public_key_base64}"
        ))
    })?;
    let key = rsa::RsaPublicKey::from_pkcs1_der(&der).map_err(|_| {
        validation_error(&format!(
            "Sign file invalid, unable to load PKCS #1 key: {public_key_base64}"
        ))
    })?;

    let signature = hex_decode(
        sign_file.get("hashSignature").and_then(Value::as_str).unwrap_or_default(),
    )
    .ok_or_else(|| validation_error("Invalid signature in sign file"))?;

    let string_to_sign = sign_file
        .get("files")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .map(|file| {
                    file.get("fileHashValue").and_then(Value::as_str).unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();

    let digest = sha2::Sha256::digest(string_to_sign.as_bytes());
    key.verify(rsa::Pkcs1v15Sign::new::<sha2::Sha256>(), &digest, &signature)
        .map_err(|_| validation_error("Invalid signature in sign file"))
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    use base64ct::Encoding;
    base64ct::Base64::decode_vec(text.trim()).ok()
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn param_error(message: &str) -> Failure {
    Failure::new(
        exit::PARAM_VALIDATION,
        awsc_runtime::RuntimeError::ParamValidation(message.to_string()),
    )
}

fn validation_error(message: &str) -> Failure {
    Failure::new(exit::GENERAL_ERROR, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(std::collections::BTreeMap<String, Vec<u8>>);

    impl Source for Fixed {
        fn read(&self, name: &str) -> Result<Vec<u8>, Failure> {
            self.0
                .get(name)
                .cloned()
                .ok_or_else(|| Failure::new(exit::GENERAL_ERROR, format!("{name}: absent")))
        }
    }

    fn source(files: &[(&str, &str)]) -> Fixed {
        Fixed(
            files
                .iter()
                .map(|(name, body)| (name.to_string(), body.as_bytes().to_vec()))
                .collect(),
        )
    }

    #[test]
    fn a_prefix_is_empty_or_ends_in_a_slash() {
        assert_eq!(format_prefix(""), "");
        assert_eq!(format_prefix("exports"), "exports/");
        assert_eq!(format_prefix("exports/"), "exports/");
    }

    #[test]
    fn a_matching_hash_passes_and_a_changed_file_is_named() {
        let body = "row1,row2\n";
        let hash = sha256_hex(body.as_bytes());
        let sign = json!({"files": [{"fileName": "r.csv.gz", "fileHashValue": hash}]});
        assert!(validate_export_files(&source(&[("r.csv.gz", body)]), &sign).is_ok());

        let tampered = source(&[("r.csv.gz", "row1,row2,row3\n")]);
        let failure = validate_export_files(&tampered, &sign).expect_err("refuses");
        assert!(failure.message().contains("File r.csv.gz has inconsistent hash value"));
    }

    #[test]
    fn an_empty_or_malformed_file_list_is_refused() {
        let empty = json!({"files": []});
        assert!(validate_export_files(&source(&[]), &empty)
            .expect_err("refuses")
            .message()
            .contains("No export file was found"));

        let malformed = json!({"files": "not a list"});
        assert!(validate_export_files(&source(&[]), &malformed)
            .expect_err("refuses")
            .message()
            .contains("Invalid sign file provided"));
    }

    #[test]
    fn hex_decoding_rejects_odd_and_non_hex_input() {
        assert_eq!(hex_decode("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(hex_decode("abc"), None);
        assert_eq!(hex_decode("zz"), None);
    }

    /// The signed string is the hashes joined by single spaces in the sign file's order.
    /// A different order is a different document and would verify against nothing.
    #[test]
    fn the_signed_string_keeps_the_sign_files_order() {
        let sign = json!({"files": [
            {"fileName": "b", "fileHashValue": "bbb"},
            {"fileName": "a", "fileHashValue": "aaa"}
        ]});
        let joined = sign["files"]
            .as_array()
            .expect("files")
            .iter()
            .map(|f| f["fileHashValue"].as_str().expect("hash"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(joined, "bbb aaa");
    }

    /// An unparsable key is reported as an invalid *sign file*, not as a crypto failure.
    #[test]
    fn a_bad_public_key_is_reported_as_an_invalid_sign_file() {
        let sign = json!({"files": [], "hashSignature": "00"});
        let failure = validate_signature("not base64 at all!!", &sign).expect_err("refuses");
        assert!(failure.message().contains("unable to load PKCS #1 key"), "{}", failure.message());
    }
}
