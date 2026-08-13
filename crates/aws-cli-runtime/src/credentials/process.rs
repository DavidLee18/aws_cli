//! The `credential_process` provider.
//!
//! Runs an external command that prints credentials as JSON on stdout. Widely used by
//! brokers such as Granted/Common Fate and aws-vault.

use super::{CredentialError, Credentials};
use serde::Deserialize;
use std::process::Command;

/// The documented output schema. `Version` must be 1.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ProcessOutput {
    version: u32,
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    /// RFC 3339. Absent means the credentials do not expire.
    expiration: Option<String>,
}

/// Execute a `credential_process` command and parse its output.
///
/// The command string is split like a shell would for the simple cases (respecting
/// single and double quotes) but is NOT passed to a shell: no globbing, no pipelines, no
/// substitution. That matches botocore, which uses `shlex.split` plus a direct exec, and
/// avoids handing arbitrary profile text to `sh -c`.
pub fn resolve(command: &str, profile: &str) -> Result<Credentials, CredentialError> {
    let argv = split_command(command).ok_or_else(|| CredentialError::Process {
        profile: profile.to_string(),
        message: "credential_process has unbalanced quotes".to_string(),
    })?;
    let Some((program, args)) = argv.split_first() else {
        return Err(CredentialError::Process {
            profile: profile.to_string(),
            message: "credential_process is empty".to_string(),
        });
    };

    let output = Command::new(program).args(args).output().map_err(|e| {
        CredentialError::Process {
            profile: profile.to_string(),
            message: format!("failed to run `{program}`: {e}"),
        }
    })?;

    if !output.status.success() {
        // The broker's own stderr is the useful diagnostic ("run `granted login`"),
        // so surface it rather than a generic failure.
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(CredentialError::Process {
            profile: profile.to_string(),
            message: if stderr.is_empty() {
                format!("`{program}` exited with {}", output.status)
            } else {
                stderr
            },
        });
    }

    let parsed: ProcessOutput = serde_json::from_slice(&output.stdout).map_err(|e| {
        CredentialError::Process {
            profile: profile.to_string(),
            message: format!("output was not valid credential JSON: {e}"),
        }
    })?;

    if parsed.version != 1 {
        return Err(CredentialError::Process {
            profile: profile.to_string(),
            message: format!("unsupported credential_process Version {}", parsed.version),
        });
    }

    Ok(Credentials {
        access_key_id: parsed.access_key_id,
        secret_access_key: parsed.secret_access_key,
        session_token: parsed.session_token,
        expires_at: parsed.expiration,
    })
}

/// Split a command line on whitespace, honouring single and double quotes.
///
/// Returns `None` on an unterminated quote rather than guessing at the intent.
fn split_command(input: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;

    for c in input.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                has_token = true;
            }
            None if c.is_whitespace() => {
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            None => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if has_token {
        args.push(current);
    }
    Some(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_plain_and_quoted_commands() {
        assert_eq!(split_command("aws-vault exec prod").unwrap(), ["aws-vault", "exec", "prod"]);
        assert_eq!(
            split_command(r#"/usr/bin/broker --profile "my profile" -q"#).unwrap(),
            ["/usr/bin/broker", "--profile", "my profile", "-q"]
        );
        assert_eq!(split_command("cmd  --a   --b").unwrap(), ["cmd", "--a", "--b"]);
        // An empty quoted argument is a real argument.
        assert_eq!(split_command(r#"cmd """#).unwrap(), ["cmd", ""]);
    }

    #[test]
    fn rejects_unbalanced_quotes() {
        assert!(split_command(r#"cmd "unterminated"#).is_none());
    }

    #[test]
    fn runs_a_command_and_parses_credentials() {
        let json = r#"{"Version":1,"AccessKeyId":"AKIA","SecretAccessKey":"secret","SessionToken":"tok","Expiration":"2030-01-01T00:00:00Z"}"#;
        let creds = resolve(&format!("printf %s '{json}'"), "test");
        // `printf` is not a real broker but exercises the exec + parse path.
        let creds = creds.expect("should parse");
        assert_eq!(creds.access_key_id, "AKIA");
        assert_eq!(creds.session_token.as_deref(), Some("tok"));
        assert_eq!(creds.expires_at.as_deref(), Some("2030-01-01T00:00:00Z"));
    }

    #[test]
    fn surfaces_command_failure_stderr() {
        let err = resolve("sh -c 'echo need-login >&2; exit 1'", "p").unwrap_err();
        assert!(format!("{err}").contains("need-login"), "got: {err}");
    }

    #[test]
    fn rejects_wrong_version_and_bad_json() {
        let err = resolve(r#"printf %s {"Version":2,"AccessKeyId":"a","SecretAccessKey":"b"}"#, "p");
        assert!(err.is_err());
        assert!(resolve("printf %s not-json", "p").is_err());
    }
}
