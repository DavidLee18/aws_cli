//! The `aws configure` command tree.
//!
//! Like `aws s3` this has no model: each subcommand is hand-written and writes plain text
//! to stdout rather than going through `--output`. Unlike `s3`, most of what it does is
//! edit `~/.aws/config` and `~/.aws/credentials`, so the interesting part is
//! [`writer`] -- which edits the file as lines rather than re-serialising it.
//!
//! Implemented: `list`, `get`, `set`, `list-profiles`. Not implemented, and refused by
//! name rather than approximated: `sso`, `sso-session`, `mfa-login`, `wizard`, `import`,
//! `add-model`, `export-credentials`, `agent-toolkit`, and the bare interactive
//! `aws configure` prompt.

pub mod writer;

use crate::args::{Arity, Parsed};
use crate::exit;
use crate::Failure;
use aws_cli_runtime::credentials::profile::Config;
use std::collections::BTreeMap;
use std::process::ExitCode;
use writer::{Setting, Update};

/// `<not set>`, the reference's placeholder in `configure list`.
const NOT_SET: &str = "<not set>";

/// Variables that go to `~/.aws/credentials` rather than `~/.aws/config`
/// (`ConfigureSetCommand._WRITE_TO_CREDS_FILE`).
const WRITE_TO_CREDENTIALS: &[&str] = &[
    "aws_access_key_id",
    "aws_secret_access_key",
    "aws_session_token",
    "aws_security_token",
];

/// Sections in the config file that are not profiles, so a `plugins.foo` key is written
/// to `[plugins]` and not to `[profile plugins]`.
const PREDEFINED_SECTIONS: &[&str] = &["plugins"];

/// How many values each `configure` flag takes, so the generic splitter does not let
/// `--sso-session` swallow the positional after it.
pub fn flag_arity(flag: &str) -> Arity {
    match flag {
        "--sso-session" | "--services" | "--format" | "--csv" | "--profile-prefix"
        | "--service-model" | "--service-name" | "--serial-number" | "--duration-seconds" => {
            Arity::One
        }
        _ => Arity::None,
    }
}

pub fn dispatch(parsed: &Parsed) -> Result<ExitCode, Failure> {
    match parsed.operation.as_str() {
        "list" => list(parsed),
        "get" => get(parsed),
        "set" => set(parsed),
        "list-profiles" => list_profiles(parsed),
        // Everything else the reference offers. Named individually so the error says the
        // command exists and we have not built it, rather than "invalid choice" -- which
        // would suggest a typo.
        other @ ("sso" | "sso-session" | "mfa-login" | "wizard" | "import" | "add-model"
        | "export-credentials" | "agent-toolkit") => Err(Failure::new(
            exit::GENERAL_ERROR,
            format!("configure {other} is not implemented yet"),
        )),
        "" => Err(Failure::new(
            exit::GENERAL_ERROR,
            "the interactive `aws configure` prompt is not implemented yet; \
             use `aws configure set` to write individual values",
        )),
        other => Err(Failure::new(
            exit::PARAM_VALIDATION,
            aws_cli_runtime::RuntimeError::ParamValidation(format!(
                "argument subcommand: Invalid choice: '{other}'"
            )),
        )),
    }
}

/// The profile these commands act on, and whether it was named explicitly.
///
/// `configure list` distinguishes the two: an explicit `--profile` reports type `manual`,
/// while `AWS_PROFILE` reports type `env`.
fn profile_source(parsed: &Parsed) -> (String, Option<(&'static str, &'static str)>) {
    if let Some(name) = &parsed.profile {
        return (name.clone(), Some(("manual", "--profile")));
    }
    for variable in ["AWS_PROFILE", "AWS_DEFAULT_PROFILE"] {
        if let Ok(value) = std::env::var(variable) {
            if !value.is_empty() {
                return (value, Some(("env", "['AWS_PROFILE', 'AWS_DEFAULT_PROFILE']")));
            }
        }
    }
    ("default".to_string(), None)
}

fn load_config() -> Result<Config, Failure> {
    Config::load().map_err(|e| Failure::new(exit::CONFIGURATION, e))
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// `****************ABCD` -- sixteen stars and the last four characters, however long the
/// value is. Not a truncation of the real length, deliberately.
fn mask(value: &str) -> String {
    let tail: String = value.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{}{}", "*".repeat(16), tail)
}

/// One row of `configure list`: value, where it came from, and the name of the thing it
/// came from.
fn row(name: &str, value: &str, kind: &str, location: &str) -> String {
    format!("{name:<10} : {value:<24} : {kind:<16} : {location}\n")
}

fn list(parsed: &Parsed) -> Result<ExitCode, Failure> {
    let mut out = row("NAME", "VALUE", "TYPE", "LOCATION");

    let (profile, source) = profile_source(parsed);
    out.push_str(&match source {
        Some((kind, location)) => row("profile", &profile, kind, location),
        None => row("profile", NOT_SET, "None", "None"),
    });

    // Credentials always report the *provider* that supplied them, never where the value
    // was looked up -- which is why the LOCATION column is empty for them, and why
    // `Credentials` has to carry its own method.
    match aws_cli_runtime::credentials::resolve(parsed.profile.as_deref(), None) {
        Ok(credentials) => {
            out.push_str(&row(
                "access_key",
                &mask(&credentials.access_key_id),
                credentials.method,
                "",
            ));
            out.push_str(&row(
                "secret_key",
                &mask(&credentials.secret_access_key),
                credentials.method,
                "",
            ));
        }
        // A profile that does not exist is fatal, and reported as such -- the command is
        // being asked about a profile the user believes in, so answering "<not set>" for
        // every row would confirm a setup that is not there. Everything printed so far
        // still goes out, matching the reference, which has already written those rows by
        // the time the lookup raises.
        Err(e @ aws_cli_runtime::credentials::CredentialError::UnknownProfile(_)) => {
            print!("{out}");
            return Err(Failure::new(exit::GENERAL_ERROR, e));
        }
        Err(_) => {
            // Every other failure is not an error: `configure list` reports what is
            // missing and still exits 0, which is what makes it usable for diagnosing a
            // setup that has no credentials yet.
            out.push_str(&row("access_key", NOT_SET, "None", "None"));
            out.push_str(&row("secret_key", NOT_SET, "None", "None"));
        }
    }

    out.push_str(&region_row(&profile)?);
    print!("{out}");
    Ok(exit::code(exit::SUCCESS))
}

fn region_row(profile: &str) -> Result<String, Failure> {
    // `AWS_REGION` beats `AWS_DEFAULT_REGION`, and the reference prints the whole pair as
    // the location because that is the list its variable map holds.
    for variable in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Ok(value) = std::env::var(variable) {
            if !value.is_empty() {
                return Ok(row("region", &value, "env", "['AWS_REGION', 'AWS_DEFAULT_REGION']"));
            }
        }
    }
    let config = load_config()?;
    match config.profile(profile).and_then(|p| p.get("region").cloned()) {
        Some(region) => Ok(row("region", &region, "config-file", "~/.aws/config")),
        None => Ok(row("region", NOT_SET, "None", "None")),
    }
}

// ---------------------------------------------------------------------------
// get
// ---------------------------------------------------------------------------

/// `configure get` prints the value and exits 0, or prints nothing and exits **1** when
/// there is none. Scripts branch on that exit code, so a missing value must not be an
/// error line.
fn get(parsed: &Parsed) -> Result<ExitCode, Failure> {
    let varname = parsed
        .positionals
        .first()
        .ok_or_else(|| missing_positionals(&["varname"]))?
        .clone();
    let config = load_config()?;
    let (profile, source) = profile_source(parsed);
    let explicit = source.is_some();

    if let Some(section) = subsection(parsed)? {
        let value = subsection_value(&config, &section, &varname);
        return Ok(print_or_absent(value));
    }

    let value = match varname.split_once('.') {
        None => {
            // Only the unqualified path resolves through the *scoped* config, and that is
            // what raises for a profile that does not exist. A dotted name reads the whole
            // config instead and simply finds nothing, so `configure get
            // profile.nosuch.region --profile alsonosuch` exits 1 rather than erroring --
            // an asymmetry in the reference, kept because scripts branch on the code.
            if explicit && !config.profile_exists(&profile) {
                return Err(Failure::new(
                    exit::GENERAL_ERROR,
                    aws_cli_runtime::credentials::CredentialError::UnknownProfile(profile),
                ));
            }
            config.profile(&profile).and_then(|p| p.get(&varname).cloned())
        }
        Some(_) => dotted_value(&config, &profile, &varname),
    };
    Ok(print_or_absent(value))
}

fn print_or_absent(value: Option<String>) -> ExitCode {
    match value {
        Some(value) => {
            println!("{value}");
            exit::code(exit::SUCCESS)
        }
        None => exit::code(1),
    }
}

/// `profile.dev.region`, `default.output`, `dev.region`, or a bare `region` scoped to the
/// active profile. The leading token decides which, and a name that happens to match an
/// existing profile is read as that profile.
fn dotted_value(config: &Config, active: &str, varname: &str) -> Option<String> {
    let parts: Vec<&str> = varname.split('.').collect();
    let (profile, key) = match parts.as_slice() {
        ["profile", profile, key, ..] => ((*profile).to_string(), (*key).to_string()),
        [head, key, ..] if *head == "default" || config.profile_exists(head) => {
            ((*head).to_string(), (*key).to_string())
        }
        [key, ..] => (active.to_string(), (*key).to_string()),
        [] => return None,
    };
    config.profile(&profile).and_then(|p| p.get(&key).cloned())
}

/// `--sso-session NAME` / `--services NAME`, which are mutually exclusive.
fn subsection(parsed: &Parsed) -> Result<Option<(&'static str, String)>, Failure> {
    let sso = parsed.parameters.get("--sso-session").and_then(Clone::clone);
    let services = parsed.parameters.get("--services").and_then(Clone::clone);
    match (sso, services) {
        (Some(_), Some(_)) => Err(Failure::new(
            exit::PARAM_VALIDATION,
            aws_cli_runtime::RuntimeError::ParamValidation(
                "The key \"services\" cannot be specified when one of the following keys \
                 are also specified: sso_session"
                    .to_string(),
            ),
        )),
        (Some(name), None) => Ok(Some(("sso-session", name))),
        (None, Some(name)) => Ok(Some(("services", name))),
        (None, None) => Ok(None),
    }
}

fn subsection_value(config: &Config, section: &(&'static str, String), varname: &str) -> Option<String> {
    let (kind, name) = section;
    let sections = match *kind {
        "sso-session" => &config.sso_sessions,
        _ => &config.services,
    };
    sections.get(name)?.get(varname).cloned()
}

// ---------------------------------------------------------------------------
// set
// ---------------------------------------------------------------------------

fn set(parsed: &Parsed) -> Result<ExitCode, Failure> {
    // Both are reported at once, not just the first: argparse collects every missing
    // positional before it complains.
    let missing: Vec<&str> = ["varname", "value"]
        .iter()
        .enumerate()
        .filter(|(i, _)| parsed.positionals.get(*i).is_none())
        .map(|(_, name)| *name)
        .collect();
    if !missing.is_empty() {
        return Err(missing_positionals(&missing));
    }
    let varname = parsed.positionals[0].clone();
    let value = parsed.positionals[1].clone();

    let (profile, _) = profile_source(parsed);

    // A sub-section is written to the config file under its own header, and the profile
    // plays no part.
    if let Some((kind, name)) = subsection(parsed)? {
        let (key, setting) = nest_strict(&varname, value)?;
        return write(
            &Update {
                section: format!("{kind} {}", quote_section_name(&name)),
                values: vec![(key, setting)],
            },
            &config_path(),
        );
    }

    let (section_profile, key, setting) = resolve_target(&profile, &varname, value)?;

    // The credential variables always go to `~/.aws/credentials`, where the section is
    // the bare profile name -- that file has no `[profile x]` spelling.
    if WRITE_TO_CREDENTIALS.contains(&key.as_str()) {
        let path = credentials_path();
        let result = write(
            &Update { section: section_profile, values: vec![(key, setting)] },
            &path,
        );
        warn_if_permissive(&path);
        return result;
    }

    let section = if section_profile == "default" || PREDEFINED_SECTIONS.contains(&section_profile.as_str()) {
        section_profile
    } else {
        format!("profile {}", quote_section_name(&section_profile))
    };
    write(&Update { section, values: vec![(key, setting)] }, &config_path())
}

/// Work out which profile and key a `varname` names.
///
/// `region` is the active profile's; `default.region` and `profile.dev.region` name one
/// explicitly; `plugins.x` is the predefined section rather than a profile; and a
/// two-part name that is none of those is a *nested* key within the active profile.
fn resolve_target(
    active: &str,
    varname: &str,
    value: String,
) -> Result<(String, String, Setting), Failure> {
    let parts: Vec<&str> = varname.split('.').collect();
    match parts.as_slice() {
        [_] => {
            let (key, setting) = nest(varname, value);
            Ok((active.to_string(), key, setting))
        }
        ["default", rest @ ..] => {
            let (key, setting) = nest(&rest.join("."), value);
            Ok(("default".to_string(), key, setting))
        }
        ["profile", profile, rest @ ..] => {
            let (key, setting) = nest(&rest.join("."), value);
            Ok(((*profile).to_string(), key, setting))
        }
        [head, key] if PREDEFINED_SECTIONS.contains(head) => {
            Ok(((*head).to_string(), (*key).to_string(), Setting::Value(value)))
        }
        _ => {
            let (key, setting) = nest(varname, value);
            Ok((active.to_string(), key, setting))
        }
    }
}

/// A two-part key sets a nested value: `s3.max_concurrent_requests 10` writes an indented
/// block under `s3 =`.
///
/// Three or more parts is NOT an error on the profile path. The reference takes the first
/// part as the key and silently drops the rest, and matching that matters more than
/// improving on it -- a script that has been getting away with `a.b.c` would start failing
/// against a drop-in replacement that was stricter.
fn nest(varname: &str, value: String) -> (String, Setting) {
    let parts: Vec<&str> = varname.split('.').collect();
    match parts.as_slice() {
        [key, sub] => {
            let mut map = BTreeMap::new();
            map.insert((*sub).to_string(), value);
            ((*key).to_string(), Setting::Nested(map))
        }
        [key, ..] => ((*key).to_string(), Setting::Value(value)),
        [] => (varname.to_string(), Setting::Value(value)),
    }
}

/// The sub-section path, where deep nesting IS rejected -- `_set_subsection_property`
/// raises rather than dropping the extra parts.
fn nest_strict(varname: &str, value: String) -> Result<(String, Setting), Failure> {
    if varname.split('.').count() > 2 {
        return Err(Failure::new(
            exit::PARAM_VALIDATION,
            aws_cli_runtime::RuntimeError::ParamValidation(
                "Found more than two parts in the property to set. \
                 Deep nesting of properties is not supported."
                    .to_string(),
            ),
        ));
    }
    Ok(nest(varname, value))
}

fn write(update: &Update, path: &std::path::Path) -> Result<ExitCode, Failure> {
    writer::update_config(update, path)
        .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    Ok(exit::code(exit::SUCCESS))
}

/// Quote a section name that contains whitespace, the way `get_section_header` does.
///
/// The reference runs it through `shlex.quote`, so `my dev` is written
/// `[profile 'my dev']` -- with SINGLE quotes, which is not the double-quoted spelling
/// botocore's parser is usually shown with. Only whitespace triggers it, so an ordinary
/// name is untouched.
fn quote_section_name(name: &str) -> String {
    if !name.contains(' ') && !name.contains('\t') {
        return name.to_string();
    }
    // `shlex.quote`: wrap in single quotes, and end/reopen the quoting around any single
    // quote in the name.
    format!("'{}'", name.replace('\'', "'\"'\"'"))
}

/// Warn when the credentials file can be read by anyone but its owner.
///
/// Only for the credentials file, and only after a successful write: this is the one
/// place the CLI puts a long-lived secret on disk, and a 0644 file there is a real
/// exposure that nothing else in the tool would tell you about. Any group or other bit
/// counts as too permissive (`is_overly_permissive(mode, 0o700)`).
fn warn_if_permissive(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(metadata) = std::fs::metadata(path) else { return };
        if !metadata.is_file() || metadata.mode() & 0o077 == 0 {
            return;
        }
        let path = path.display();
        eprintln!(
            "\naws: [WARNING]: The file '{path}' is accessible by other users. \
             Consider running 'chmod 600 {path}' to restrict access to only your user."
        );
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn config_path() -> std::path::PathBuf {
    aws_cli_runtime::credentials::profile::config_file_path()
        .unwrap_or_else(|| std::path::PathBuf::from("config"))
}

fn credentials_path() -> std::path::PathBuf {
    aws_cli_runtime::credentials::profile::credentials_file_path()
        .unwrap_or_else(|| std::path::PathBuf::from("credentials"))
}

// ---------------------------------------------------------------------------
// list-profiles
// ---------------------------------------------------------------------------

/// Every profile from both files, in the order the files declare them.
///
/// Not sorted: botocore lists `full_config['profiles']` in dict order, so the config
/// file's order comes first and credentials-only profiles follow. Sorting looks tidier
/// and is a visible difference from the reference.
fn list_profiles(_parsed: &Parsed) -> Result<ExitCode, Failure> {
    for name in load_config()?.profile_order {
        println!("{name}");
    }
    Ok(exit::code(exit::SUCCESS))
}

/// A missing positional. Note the ordering: these commands print the message FIRST and
/// the usage block after it, which is the opposite of the modelled path, where argparse
/// writes usage first. `Failure::after_usage` produces the other order.
fn missing_positionals(names: &[&str]) -> Failure {
    Failure::new(
        exit::PARAM_VALIDATION,
        format!(
            "{}\n\n{}",
            aws_cli_runtime::RuntimeError::ParamValidation(format!(
                "the following arguments are required: {}",
                names.join(", ")
            )),
            crate::USAGE_HINT
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whitespace in a profile name is single-quoted, and nothing else is touched.
    #[test]
    fn a_section_name_is_quoted_only_when_it_has_whitespace() {
        assert_eq!(quote_section_name("dev"), "dev");
        assert_eq!(quote_section_name("my-dev.2"), "my-dev.2");
        assert_eq!(quote_section_name("my dev"), "'my dev'");
        assert_eq!(quote_section_name("it's here"), "'it'\"'\"'s here'");
    }

    #[test]
    fn masking_keeps_the_last_four_characters_behind_a_fixed_width_prefix() {
        assert_eq!(mask("AKIAIOSFODNN7EXAMPLE"), "****************MPLE");
        // The star count is fixed, so a short value is not a shorter mask -- the mask
        // must not leak the length of the secret.
        assert_eq!(mask("abcd"), "****************abcd");
        assert_eq!(mask("ab"), "****************ab");
    }

    /// The column layout is what makes the output greppable, and it is fixed-width until
    /// a field overflows -- at which point the reference lets it run rather than truncate.
    #[test]
    fn the_row_layout_matches_the_reference() {
        assert_eq!(
            row("region", "us-west-2", "config-file", "~/.aws/config"),
            "region     : us-west-2                : config-file      : ~/.aws/config\n"
        );
        assert!(row("access_key", "x", "shared-credentials-file", "")
            .starts_with("access_key : x                        : shared-credentials-file : "));
    }

    #[test]
    fn a_two_part_name_becomes_a_nested_block() {
        let (key, setting) = nest("s3.max_concurrent_requests", "10".into());
        assert_eq!(key, "s3");
        match setting {
            Setting::Nested(map) => assert_eq!(map.get("max_concurrent_requests").unwrap(), "10"),
            other => panic!("expected a nested value, got {other:?}"),
        }
    }

    /// Three parts is refused in a sub-section and *silently truncated* on the profile
    /// path. Both halves are the reference's behaviour, and the asymmetry is deliberate.
    #[test]
    fn deep_nesting_is_refused_only_in_a_subsection() {
        assert!(nest_strict("a.b.c", "v".into()).is_err());
        let (key, setting) = nest("a.b.c", "v".into());
        assert_eq!(key, "a");
        assert!(matches!(setting, Setting::Value(v) if v == "v"));
    }

    /// The profile a bare name is scoped to, and the explicit spellings that override it.
    #[test]
    fn a_varname_resolves_to_the_right_profile_and_key() {
        let (profile, key, _) = resolve_target("dev", "region", "x".into()).expect("resolves");
        assert_eq!((profile.as_str(), key.as_str()), ("dev", "region"));

        let (profile, key, _) =
            resolve_target("dev", "default.region", "x".into()).expect("resolves");
        assert_eq!((profile.as_str(), key.as_str()), ("default", "region"));

        let (profile, key, _) =
            resolve_target("dev", "profile.other.region", "x".into()).expect("resolves");
        assert_eq!((profile.as_str(), key.as_str()), ("other", "region"));

        // `plugins` is a section in its own right, not a profile named plugins.
        let (profile, key, _) = resolve_target("dev", "plugins.cwlogs", "x".into()).expect("resolves");
        assert_eq!((profile.as_str(), key.as_str()), ("plugins", "cwlogs"));
    }
}
