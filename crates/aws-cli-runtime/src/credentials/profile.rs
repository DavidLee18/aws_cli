//! Parsing and resolution of `~/.aws/config` and `~/.aws/credentials`.
//!
//! The two files differ in one important way: profiles in `config` are written
//! `[profile name]` (except `default`), while in `credentials` they are bare `[name]`.
//! `config` also holds non-profile sections such as `[sso-session NAME]`, which profiles
//! reference by name.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// One parsed section: lowercased keys to raw values.
pub type Section = BTreeMap<String, String>;

/// The two files are kept SEPARATE, not merged, because the provider chain treats them
/// as distinct providers at different priorities: static keys from `~/.aws/credentials`
/// outrank `credential_process`, which in turn outranks static keys from `~/.aws/config`
/// (botocore `create_credential_resolver`, positions 5/7/8). A single merged view cannot
/// express that. [`Config::profile`] gives the merged view for everything else.
#[derive(Debug, Default)]
pub struct Config {
    /// Profiles from `~/.aws/config`.
    pub config_profiles: BTreeMap<String, Section>,
    /// Profiles from `~/.aws/credentials`.
    pub credentials_profiles: BTreeMap<String, Section>,
    /// `[sso-session NAME]` sections, keyed by NAME.
    pub sso_sessions: BTreeMap<String, Section>,
    /// `[services NAME]` sections, kept for completeness.
    pub services: BTreeMap<String, Section>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io { path: String, source: std::io::Error },
}

impl Config {
    /// Load both files, honouring `AWS_CONFIG_FILE` / `AWS_SHARED_CREDENTIALS_FILE`.
    pub fn load() -> Result<Self, ConfigError> {
        let mut config = Config::default();

        if let Some(path) = config_file_path() {
            for (raw_name, section) in parse_ini(&path)? {
                config.insert_config_section(&raw_name, section);
            }
        }
        // The credentials file holds only profiles, named bare.
        if let Some(path) = credentials_file_path() {
            for (name, section) in parse_ini(&path)? {
                config.credentials_profiles.insert(name, section);
            }
        }
        Ok(config)
    }

    fn insert_config_section(&mut self, raw_name: &str, section: Section) {
        let mut parts = raw_name.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or_default();
        let tail = parts.next().map(str::trim);

        match (head, tail) {
            ("profile", Some(name)) => {
                self.config_profiles.insert(name.to_string(), section);
            }
            ("sso-session", Some(name)) => {
                self.sso_sessions.insert(name.to_string(), section);
            }
            ("services", Some(name)) => {
                self.services.insert(name.to_string(), section);
            }
            // A bare section in `config` is only a profile when it is `default`;
            // anything else unprefixed is ignored by botocore too.
            ("default", None) => {
                self.config_profiles.insert("default".to_string(), section);
            }
            _ => {}
        }
    }

    /// The merged view: credentials-file keys layered over config-file keys, as botocore
    /// builds `full_config`. Use this for settings (region, `role_arn`, `sso_*`); use the
    /// per-file maps where provider priority depends on which file a key came from.
    pub fn profile(&self, name: &str) -> Option<Section> {
        let config = self.config_profiles.get(name);
        let credentials = self.credentials_profiles.get(name);
        if config.is_none() && credentials.is_none() {
            return None;
        }
        let mut merged = config.cloned().unwrap_or_default();
        if let Some(c) = credentials {
            merged.extend(c.clone());
        }
        Some(merged)
    }

    pub fn profile_exists(&self, name: &str) -> bool {
        self.config_profiles.contains_key(name) || self.credentials_profiles.contains_key(name)
    }

    pub fn sso_session(&self, name: &str) -> Option<&Section> {
        self.sso_sessions.get(name)
    }
}

/// The profile to use: explicit flag, then `AWS_PROFILE`, then `default`.
/// The `region` key of the active profile, if the config file sets one.
///
/// Without this the region falls back to the service's global pseudo-region, which for
/// most services is a `NoRegion` error but for S3 and STS *silently* resolves to the
/// legacy global endpoint — a different host from the one the reference uses.
pub fn profile_region(explicit_profile: Option<&str>) -> Option<String> {
    let config = Config::load().ok()?;
    let section = config.profile(&profile_name(explicit_profile))?;
    section.get("region").filter(|r| !r.is_empty()).cloned()
}

pub fn profile_name(explicit: Option<&str>) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| std::env::var("AWS_PROFILE").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "default".to_string())
}

pub fn config_file_path() -> Option<PathBuf> {
    env_path("AWS_CONFIG_FILE", ".aws/config")
}

pub fn credentials_file_path() -> Option<PathBuf> {
    env_path("AWS_SHARED_CREDENTIALS_FILE", ".aws/credentials")
}

fn env_path(var: &str, default_suffix: &str) -> Option<PathBuf> {
    if let Ok(v) = std::env::var(var) {
        return if v.is_empty() { None } else { Some(PathBuf::from(v)) };
    }
    home().map(|h| h.join(default_suffix))
}

pub fn home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Minimal INI reader for the AWS config dialect.
///
/// Section headers keep their raw text (`profile foo`, `sso-session bar`) so the caller
/// can classify them. Indented continuation lines — used by nested settings such as
/// `s3 =\n  addressing_style = path` — are flattened away rather than mis-parsed as
/// top-level keys.
fn parse_ini(path: &PathBuf) -> Result<Vec<(String, Section)>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        // A missing file is normal: the chain simply moves on.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ConfigError::Io { path: path.display().to_string(), source: e }),
    };

    let mut out: Vec<(String, Section)> = Vec::new();
    let mut current: Option<(String, Section)> = None;
    let mut in_nested = false;

    for raw_line in text.lines() {
        let is_indented = raw_line.starts_with([' ', '\t']);
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some(name) = line.strip_prefix('[').and_then(|l| l.split(']').next()) {
            if let Some(section) = current.take() {
                out.push(section);
            }
            current = Some((name.trim().to_string(), Section::new()));
            in_nested = false;
            continue;
        }

        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim().to_string());

        // `foo =` with nothing after it opens a nested block whose indented children we
        // skip; a subsequent unindented key closes it.
        if is_indented && in_nested {
            continue;
        }
        in_nested = value.is_empty();

        if let Some((_, section)) = current.as_mut() {
            section.insert(key, value);
        }
    }
    if let Some(section) = current.take() {
        out.push(section);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(name: &str, contents: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("awsc-{name}-{}.ini", std::process::id()));
        std::fs::File::create(&path).unwrap().write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn classifies_config_section_headers() {
        let path = temp(
            "cfg",
            "[default]\nregion = us-east-1\n\
             [profile work]\nsso_session = corp\nsso_account_id = 1\n\
             [sso-session corp]\nsso_start_url = https://example.awsapps.com/start\n\
             sso_region = us-east-1\n",
        );
        let mut config = Config::default();
        for (name, section) in parse_ini(&path).unwrap() {
            config.insert_config_section(&name, section);
        }

        assert_eq!(config.profile("default").unwrap()["region"], "us-east-1");
        assert_eq!(config.profile("work").unwrap()["sso_session"], "corp");
        assert_eq!(config.sso_session("corp").unwrap()["sso_region"], "us-east-1");
        // `[profile work]` must not also appear under the literal name "profile work".
        assert!(config.profile("profile work").is_none());
        std::fs::remove_file(&path).ok();
    }

    /// The credentials file layers over the config file in the merged view, but the two
    /// stay separately addressable because provider priority depends on the source.
    #[test]
    fn keeps_files_separate_and_merges_on_demand() {
        let mut config = Config::default();
        config
            .config_profiles
            .insert("p".into(), section(&[("region", "us-east-1"), ("aws_access_key_id", "FROM_CONFIG")]));
        config
            .credentials_profiles
            .insert("p".into(), section(&[("aws_access_key_id", "FROM_CREDS")]));

        let merged = config.profile("p").unwrap();
        assert_eq!(merged["aws_access_key_id"], "FROM_CREDS");
        assert_eq!(merged["region"], "us-east-1");
        assert_eq!(config.config_profiles["p"]["aws_access_key_id"], "FROM_CONFIG");
    }

    fn section(pairs: &[(&str, &str)]) -> Section {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn skips_nested_setting_blocks() {
        let path = temp(
            "nested",
            "[profile p]\ns3 =\n  addressing_style = path\n  use_accelerate_endpoint = true\nregion = eu-west-1\n",
        );
        let sections = parse_ini(&path).unwrap();
        let (_, s) = &sections[0];
        assert_eq!(s["region"], "eu-west-1");
        // The indented children must not become top-level keys.
        assert!(!s.contains_key("addressing_style"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_file_is_empty_not_an_error() {
        assert!(parse_ini(&PathBuf::from("/nonexistent/awsc.ini")).unwrap().is_empty());
    }

    #[test]
    fn tolerates_comments_and_blank_lines() {
        let path = temp("comments", "# c\n\n[default]\n; another\nregion = ap-south-1\n");
        let sections = parse_ini(&path).unwrap();
        assert_eq!(sections[0].1["region"], "ap-south-1");
        std::fs::remove_file(&path).ok();
    }
}
