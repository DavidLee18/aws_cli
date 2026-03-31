use std::{
    fs,
    io::{self, BufRead, Write},
    path::PathBuf,
};

use crate::error::CliError;

/// AWS credential / config profile stored in `~/.aws/credentials`.
#[derive(Debug, Default)]
pub struct AwsCredentials {
    pub aws_access_key_id: String,
    pub aws_secret_access_key: String,
}

/// AWS configuration profile stored in `~/.aws/config`.
#[derive(Debug, Default)]
pub struct AwsConfig {
    pub region: String,
    pub output: String,
}

fn aws_dir() -> Result<PathBuf, CliError> {
    let home = dirs_next_home()
        .ok_or_else(|| CliError::Config("Could not determine home directory".to_string()))?;
    Ok(home.join(".aws"))
}

/// Resolve the user's home directory without adding an extra dependency.
fn dirs_next_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Interactive `aws configure` workflow – prompts the user for credentials and
/// region/output format, then writes them to `~/.aws/credentials` and
/// `~/.aws/config`.
pub fn run_configure(
    profile: &str,
    access_key: Option<&str>,
    secret_key: Option<&str>,
    region: Option<&str>,
    output: Option<&str>,
) -> Result<(), CliError> {
    let dir = aws_dir()?;
    fs::create_dir_all(&dir).map_err(|e| CliError::Config(format!("Cannot create ~/.aws: {e}")))?;

    let access_key_id = match access_key {
        Some(k) => k.to_string(),
        None => prompt("AWS Access Key ID", "")?,
    };
    let secret_access_key = match secret_key {
        Some(k) => k.to_string(),
        None => prompt("AWS Secret Access Key", "")?,
    };
    let region_val = match region {
        Some(r) => r.to_string(),
        None => prompt("Default region name", "us-east-1")?,
    };
    let output_val = match output {
        Some(o) => o.to_string(),
        None => prompt("Default output format", "json")?,
    };

    // Write credentials file
    let creds_path = dir.join("credentials");
    let mut creds_content = read_ini_file(&creds_path)?;
    set_ini_section(
        &mut creds_content,
        profile,
        &[
            ("aws_access_key_id", &access_key_id),
            ("aws_secret_access_key", &secret_access_key),
        ],
    );
    write_ini_file(&creds_path, &creds_content)?;

    // Write config file
    let config_path = dir.join("config");
    let config_section = if profile == "default" {
        "default".to_string()
    } else {
        format!("profile {profile}")
    };
    let mut config_content = read_ini_file(&config_path)?;
    set_ini_section(
        &mut config_content,
        &config_section,
        &[("region", &region_val), ("output", &output_val)],
    );
    write_ini_file(&config_path, &config_content)?;

    println!("Configuration saved.");
    Ok(())
}

/// Print a single configured value.
pub fn run_configure_get(key: &str, profile: &str) -> Result<(), CliError> {
    let dir = aws_dir()?;
    match key {
        "aws_access_key_id" | "aws_secret_access_key" => {
            let content = read_ini_file(&dir.join("credentials"))?;
            let val = get_ini_value(&content, profile, key).unwrap_or_default();
            println!("{val}");
        }
        "region" | "output" => {
            let config_section = if profile == "default" {
                "default".to_string()
            } else {
                format!("profile {profile}")
            };
            let content = read_ini_file(&dir.join("config"))?;
            let val = get_ini_value(&content, &config_section, key).unwrap_or_default();
            println!("{val}");
        }
        other => {
            return Err(CliError::Input(format!("Unknown config key: {other}")));
        }
    }
    Ok(())
}

/// Print every key=value pair for the profile.
pub fn run_configure_list(profile: &str) -> Result<(), CliError> {
    let dir = aws_dir()?;
    let creds = read_ini_file(&dir.join("credentials"))?;
    let config_section = if profile == "default" {
        "default".to_string()
    } else {
        format!("profile {profile}")
    };
    let config = read_ini_file(&dir.join("config"))?;

    println!("{:<30} {:<40} {}", "Name", "Value", "Type");
    println!("{:<30} {:<40} {}", "----", "-----", "----");

    let keys_creds = [
        ("aws_access_key_id", profile),
        ("aws_secret_access_key", profile),
    ];
    for (k, sec) in &keys_creds {
        let v = get_ini_value(&creds, sec, k).unwrap_or_default();
        let masked = if k.contains("secret") && !v.is_empty() {
            "****************".to_string()
        } else {
            v
        };
        println!("{:<30} {:<40} {}", k, masked, "config-file");
    }

    let keys_config = [("region", &*config_section), ("output", &*config_section)];
    for (k, sec) in &keys_config {
        let v = get_ini_value(&config, sec, k).unwrap_or_default();
        println!("{:<30} {:<40} {}", k, v, "config-file");
    }
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn prompt(label: &str, default: &str) -> Result<String, CliError> {
    let hint = if default.is_empty() {
        String::new()
    } else {
        format!(" [{default}]")
    };
    print!("{label}{hint}: ");
    io::stdout().flush().map_err(CliError::Io)?;
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(CliError::Io)?;
    let trimmed = line.trim().to_string();
    if trimmed.is_empty() && !default.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed)
    }
}

/// Simple INI representation: ordered list of (section, key, value) tuples
/// plus raw lines for sections and comments that we preserve.
type IniContent = Vec<IniLine>;

#[derive(Debug, Clone)]
enum IniLine {
    Section(String),
    KeyValue(String, String),
    Other(String),
}

fn read_ini_file(path: &PathBuf) -> Result<IniContent, CliError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(path).map_err(CliError::Io)?;
    let mut lines = Vec::new();
    for line in io::BufReader::new(file).lines() {
        let raw = line.map_err(CliError::Io)?;
        let trimmed = raw.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let name = trimmed[1..trimmed.len() - 1].to_string();
            lines.push(IniLine::Section(name));
        } else if let Some(eq) = trimmed.find('=') {
            let k = trimmed[..eq].trim().to_string();
            let v = trimmed[eq + 1..].trim().to_string();
            lines.push(IniLine::KeyValue(k, v));
        } else {
            lines.push(IniLine::Other(raw));
        }
    }
    Ok(lines)
}

fn write_ini_file(path: &PathBuf, content: &IniContent) -> Result<(), CliError> {
    let mut out = String::new();
    for line in content {
        match line {
            IniLine::Section(name) => out.push_str(&format!("[{name}]\n")),
            IniLine::KeyValue(k, v) => out.push_str(&format!("{k} = {v}\n")),
            IniLine::Other(raw) => out.push_str(&format!("{raw}\n")),
        }
    }
    fs::write(path, out).map_err(CliError::Io)
}

fn set_ini_section(content: &mut IniContent, section: &str, kvs: &[(&str, &str)]) {
    // Find the range of lines belonging to `section`.
    let mut start: Option<usize> = None;
    let mut end: Option<usize> = None;
    for (i, line) in content.iter().enumerate() {
        if let IniLine::Section(name) = line {
            if name == section {
                start = Some(i);
                end = None;
            } else if start.is_some() && end.is_none() {
                // This section header marks the end of the target section's body.
                end = Some(i);
            }
        }
    }

    if let Some(s) = start {
        let e = end.unwrap_or(content.len());
        // Update or remove old keys in the section body [s+1 .. e).
        for (k, v) in kvs {
            let mut found = false;
            for line in &mut content[s + 1..e] {
                if let IniLine::KeyValue(ek, ev) = line {
                    if ek == k {
                        *ev = v.to_string();
                        found = true;
                        break;
                    }
                }
            }
            if !found {
                content.insert(e, IniLine::KeyValue(k.to_string(), v.to_string()));
            }
        }
    } else {
        // Section doesn't exist yet – append.
        if !content.is_empty() {
            content.push(IniLine::Other(String::new()));
        }
        content.push(IniLine::Section(section.to_string()));
        for (k, v) in kvs {
            content.push(IniLine::KeyValue(k.to_string(), v.to_string()));
        }
    }
}

fn get_ini_value(content: &IniContent, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in content {
        match line {
            IniLine::Section(name) => {
                in_section = name == section;
            }
            IniLine::KeyValue(k, v) if in_section && k == key => {
                return Some(v.clone());
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ini_round_trip() {
        let mut content = Vec::new();
        set_ini_section(
            &mut content,
            "default",
            &[
                ("aws_access_key_id", "AKID"),
                ("aws_secret_access_key", "SECRET"),
            ],
        );
        assert_eq!(
            get_ini_value(&content, "default", "aws_access_key_id"),
            Some("AKID".to_string())
        );
        assert_eq!(
            get_ini_value(&content, "default", "aws_secret_access_key"),
            Some("SECRET".to_string())
        );
    }

    #[test]
    fn test_ini_update_existing() {
        let mut content = Vec::new();
        set_ini_section(&mut content, "default", &[("region", "us-east-1")]);
        set_ini_section(&mut content, "default", &[("region", "eu-west-1")]);
        assert_eq!(
            get_ini_value(&content, "default", "region"),
            Some("eu-west-1".to_string())
        );
    }

    #[test]
    fn test_ini_multiple_sections() {
        let mut content = Vec::new();
        set_ini_section(&mut content, "default", &[("region", "us-east-1")]);
        set_ini_section(&mut content, "profile dev", &[("region", "eu-central-1")]);
        assert_eq!(
            get_ini_value(&content, "default", "region"),
            Some("us-east-1".to_string())
        );
        assert_eq!(
            get_ini_value(&content, "profile dev", "region"),
            Some("eu-central-1".to_string())
        );
    }
}
