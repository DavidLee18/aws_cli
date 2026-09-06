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
    /// Profile names in the order the files declare them -- config file first, then any
    /// credentials-file profile not already seen.
    ///
    /// The maps above are sorted, which is right for lookup and wrong for `configure
    /// list-profiles`: botocore lists `full_config['profiles']` in dict order, so the
    /// output follows the file rather than the alphabet.
    pub profile_order: Vec<String>,
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
                config.remember_profile(&name);
                config.credentials_profiles.insert(name, section);
            }
        }
        Ok(config)
    }

    fn remember_profile(&mut self, name: &str) {
        if !self.profile_order.iter().any(|n| n == name) {
            self.profile_order.push(name.to_string());
        }
    }

    fn insert_config_section(&mut self, raw_name: &str, section: Section) {
        // botocore splits the header with `shlex.split` and keeps it only when the result
        // is EXACTLY two words (`configloader._parse_section`). Two consequences that a
        // plain whitespace split gets wrong in opposite directions:
        //
        //   [profile 'my dev']  is the profile `my dev` -- the quotes are shell quoting,
        //                       not part of the name, and `aws configure set` writes them
        //   [profile my dev]    is not a profile at all; it splits into three words and is
        //                       dropped, so treating it as `my dev` would invent a profile
        //                       the reference cannot see
        let words = shlex_split(raw_name);
        let (head, tail) = match words.len() {
            1 => (words[0].as_str(), None),
            2 => (words[0].as_str(), Some(words[1].as_str())),
            _ => return,
        };

        match (head, tail) {
            ("profile", Some(name)) => {
                self.remember_profile(name);
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
                self.remember_profile("default");
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
/// Split a section header the way `shlex.split` does, which is all the quoting the config
/// format has: single and double quotes group words, and a backslash escapes the next
/// character outside single quotes.
fn shlex_split(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = text.chars();

    while let Some(c) = chars.next() {
        match quote {
            Some('\'') if c == '\'' => quote = None,
            Some('"') if c == '"' => quote = None,
            Some('"') if c == '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            None if c == '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                    started = true;
                }
            }
            None if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(current);
    }
    words
}

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
    // The key that opened the current nested block, so its children can be named after it.
    let mut nested_parent: Option<String> = None;

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
            nested_parent = None;
            continue;
        }

        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim().to_string());

        // `foo =` with nothing after it opens a nested block. Its indented children are
        // kept under a dotted name -- `s3.endpoint_url` -- rather than dropped, which is
        // how `configure get s3.endpoint_url` reaches them. A flat key never contains a
        // dot, so the two namespaces cannot collide. A subsequent unindented key closes
        // the block.
        if is_indented {
            if let (true, Some(parent), Some((_, section))) =
                (in_nested, nested_parent.as_ref(), current.as_mut())
            {
                section.insert(format!("{parent}.{key}"), value);
            }
            continue;
        }
        in_nested = value.is_empty();
        nested_parent = if in_nested { Some(key.clone()) } else { None };

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
    /// A quoted header names the profile inside the quotes; an unquoted multi-word header
    /// is not a profile at all. Both come straight from `shlex.split` + "exactly two
    /// words", and getting either wrong is silent: the first makes a profile written by
    /// `aws configure set --profile "my dev"` unreadable, the second invents one the
    /// reference cannot see.
    #[test]
    fn a_section_header_is_split_the_way_shlex_does() {
        assert_eq!(shlex_split("profile dev"), vec!["profile", "dev"]);
        assert_eq!(shlex_split("profile 'my dev'"), vec!["profile", "my dev"]);
        assert_eq!(shlex_split("profile \"my dev\""), vec!["profile", "my dev"]);
        assert_eq!(shlex_split("profile my dev").len(), 3, "three words, so not a profile");
        assert_eq!(shlex_split("sso-session 'a b'"), vec!["sso-session", "a b"]);
    }

    #[test]
    fn a_quoted_profile_is_reachable_by_its_unquoted_name() {
        let mut config = Config::default();
        config.insert_config_section("profile 'my dev'", section(&[("region", "eu-north-1")]));
        assert!(config.profile_exists("my dev"));
        assert_eq!(config.profile("my dev").unwrap().get("region").unwrap(), "eu-north-1");

        // Three words: dropped, as botocore drops it.
        let mut config = Config::default();
        config.insert_config_section("profile my dev", section(&[("region", "x")]));
        assert!(!config.profile_exists("my dev"));
    }

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
