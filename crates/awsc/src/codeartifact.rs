//! `aws codeartifact login`: point a package manager at a CodeArtifact repository.
//!
//! A port of `customizations/codeartifact/login.py`. Two API calls
//! (`GetAuthorizationToken`, `GetRepositoryEndpoint`) and then whatever the tool needs,
//! which is different for all six of them:
//!
//! | tool | what actually happens |
//! |---|---|
//! | `npm` | writes `_authToken` into `~/.npmrc`, then runs two `npm config set` |
//! | `pip` | one `pip config set global.index-url`, token **in the URL** |
//! | `twine` | writes `~/.pypirc` directly; runs nothing |
//! | `nuget` / `dotnet` | lists existing sources, then adds *or updates* one |
//! | `swift` | writes `~/.netrc` (except on macOS, which uses the Keychain) |
//!
//! **Every path here handles a live credential**, and that shapes three decisions the
//! reference makes and this follows: files are created `0600` and `chmod`ed even when
//! they already existed, a failed subprocess has the token redacted out of its error
//! before the error is printed, and `--dry-run` deliberately prints the token unredacted
//! because that is the point of showing the command.

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use serde_json::{json, Value};
use std::process::ExitCode;

const FLAGS: &[&str] = &[
    "--tool",
    "--domain",
    "--domain-owner",
    "--namespace",
    "--duration-seconds",
    "--repository",
    "--endpoint-type",
    "--dry-run",
];

/// tool -> (package format, whether `--namespace` is accepted).
const TOOLS: &[(&str, &str, bool)] = &[
    ("swift", "swift", true),
    ("nuget", "nuget", false),
    ("dotnet", "nuget", false),
    ("npm", "npm", true),
    ("pip", "pypi", false),
    ("twine", "pypi", false),
];

pub fn dispatch(parsed: &Parsed, globals: &Globals) -> Result<Option<ExitCode>, Failure> {
    match parsed.operation.as_str() {
        "login" => login(parsed, globals).map(Some),
        _ => Ok(None),
    }
}

fn login(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(parsed, FLAGS)?;
    let missing: Vec<&str> = ["--tool", "--domain", "--repository"]
        .into_iter()
        .filter(|flag| !args.contains_key(flag))
        .collect();
    if !missing.is_empty() {
        return Err(crate::custom::missing_required(&missing));
    }
    let value = |flag: &str| args.get(flag).copied().flatten().unwrap_or_default();
    // `parsed_args.tool.lower()`, so `--tool NPM` works.
    let tool = value("--tool").to_ascii_lowercase();
    let Some((_, package_format, namespace_support)) =
        TOOLS.iter().find(|(name, _, _)| *name == tool)
    else {
        return Err(Failure::after_usage(awsc_runtime::RuntimeError::ParamValidation(format!(
            "argument --tool: Invalid choice, valid choices are:\n\n{}",
            TOOLS.iter().map(|(name, _, _)| *name).collect::<Vec<_>>().join(" | ")
        ))));
    };
    let domain = value("--domain");
    let repository = value("--repository");
    let namespace = args.get("--namespace").copied().flatten().filter(|n| !n.is_empty());
    if namespace.is_some() && !namespace_support {
        return Err(Failure::new(
            exit::GENERAL_ERROR,
            format!("Argument --namespace is not supported for {tool}"),
        ));
    }
    let dry_run = args.contains_key("--dry-run");

    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    let client_globals = Globals { region: Some(region), ..globals.clone() };
    let model =
        crate::load_model("codeartifact").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let client = Client::new(&model, &client_globals)?;

    let mut token_input = json!({ "domain": domain });
    if let Some(owner) = args.get("--domain-owner").copied().flatten() {
        token_input["domainOwner"] = Value::String(owner.to_string());
    }
    if let Some(seconds) = args.get("--duration-seconds").copied().flatten() {
        let parsed: i64 = seconds.parse().map_err(|_| {
            Failure::new(
                exit::GENERAL_ERROR,
                format!("invalid literal for int() with base 10: '{seconds}'"),
            )
        })?;
        token_input["durationSeconds"] = Value::from(parsed);
    }
    let token_response = client.call("get-authorization-token", Some(&token_input))?;
    let auth_token = token_response
        .get("authorizationToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let expiration = token_response
        .get("expiration")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut endpoint_input =
        json!({ "domain": domain, "repository": repository, "format": package_format });
    if let Some(kind) = args.get("--endpoint-type").copied().flatten() {
        endpoint_input["endpointType"] = Value::String(kind.to_string());
    }
    if let Some(owner) = args.get("--domain-owner").copied().flatten() {
        endpoint_input["domainOwner"] = Value::String(owner.to_string());
    }
    let endpoint = client
        .call("get-repository-endpoint", Some(&endpoint_input))?
        .get("repositoryEndpoint")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let scope = match &namespace {
        Some(namespace) => Some(scope_name(namespace)?),
        None => None,
    };
    let source_name = format!("{domain}/{repository}");
    let commands = commands_for(&tool, &endpoint, &auth_token, scope.as_deref(), &source_name);

    // Twine writes a file and runs nothing, so it is handled apart from the command loop.
    if tool == "twine" {
        let path = home().join(".pypirc");
        let contents = pypirc(&path, &endpoint, &auth_token)?;
        if dry_run {
            println!("Dryrun mode is enabled, not writing to pypirc.");
            println!("{} would have been set to the following:\n", path.display());
            println!("{contents}");
            return Ok(exit::code(exit::SUCCESS));
        }
        write_private(&path, &contents)?;
        success_message("twine", &endpoint, &expiration);
        return Ok(exit::code(exit::SUCCESS));
    }

    // Swift keeps its credential in `~/.netrc` everywhere but macOS, where the token
    // rides on the command line into the Keychain instead.
    if tool == "swift" && !cfg!(target_os = "macos") {
        let path = home().join(".netrc");
        let hostname = split_endpoint(&endpoint)
            .0
            .trim_start_matches("//")
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string();
        let entry = format!("machine {hostname} login token password {auth_token}");
        if dry_run {
            println!("Dryrun mode is enabled, not writing to netrc.");
            println!("The following line would have been written to {}:\n", path.display());
            println!("{entry}\n");
            println!("And would have run the following commands:\n");
        } else {
            write_private(&path, &netrc_with(&read_or_empty(&path), &hostname, &entry))?;
        }
    }

    if dry_run {
        // The token is printed unredacted on purpose: the point of a dry run is a command
        // the reader can paste, and the reference documents that in the flag's help.
        for command in &commands {
            println!("{}\n", command.join(" "));
        }
        return Ok(exit::code(exit::SUCCESS));
    }

    // npm's token does not go through `npm config`: it is written straight into the
    // npmrc, because `npm config set` would log it.
    if tool == "npm" {
        let path = npmrc_path();
        let (host_and_path, _) = split_endpoint(&endpoint);
        let key = format!("{host_and_path}:_authToken");
        let updated = npmrc_with(&read_or_empty(&path), &key, &auth_token);
        write_private(&path, &updated)?;
    }

    // nuget and dotnet decide between `add` and `update` from what is already configured,
    // which means listing first — and the listing is also how a missing tool is noticed.
    if tool == "nuget" || tool == "dotnet" {
        let index_url = format!("{endpoint}v3/index.json");
        let existing = list_nuget_sources(&tool)?;
        let (name, exists) = nuget_source_name(&index_url, &source_name, &existing);
        let operation = if exists { "update" } else { "add" };
        let command = nuget_command(&tool, operation, &index_url, &name, &auth_token);
        run(&tool, &command, &auth_token)?;
        if exists {
            println!("Updated source {name} in the NuGet.Config");
        } else {
            println!("Added source {name} to the user level NuGet.Config");
        }
        success_message("nuget", &endpoint, &expiration);
        return Ok(exit::code(exit::SUCCESS));
    }

    for command in &commands {
        run(&tool, command, &auth_token)?;
    }
    success_message(&tool, &endpoint, &expiration);
    Ok(exit::code(exit::SUCCESS))
}

/// The configured sources, as `nuget sources list -format detailed` prints them:
/// a numbered name line followed by the URL on the next line.
fn list_nuget_sources(tool: &str) -> Result<Vec<(String, String)>, Failure> {
    let command: Vec<String> = if tool == "nuget" {
        ["nuget", "sources", "list", "-format", "detailed"].iter().map(|s| s.to_string()).collect()
    } else {
        ["dotnet", "nuget", "list", "source", "--format", "detailed"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    };
    let output = std::process::Command::new(&command[0]).args(&command[1..]).output();
    let output = match output {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Failure::new(
                exit::GENERAL_ERROR,
                format!("{tool} was not found. Please verify installation."),
            ))
        }
        Err(e) => return Err(Failure::new(exit::GENERAL_ERROR, e)),
    };
    Ok(parse_nuget_sources(&String::from_utf8_lossy(&output.stdout)))
}

/// Parse the listing. Header and footer lines are ignored; a source is a line like
/// `1.  nuget.org [Enabled]` whose URL is the line after it. The bracketed word is
/// localised (`[Activé]`), so only its brackets can be relied on.
fn parse_nuget_sources(text: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let mut sources = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some((number, rest)) = line.split_once(". ") else { continue };
        if !number.chars().all(|c| c.is_ascii_digit()) || number.is_empty() {
            continue;
        }
        let Some(open) = rest.rfind(" [") else { continue };
        if !rest.ends_with(']') {
            continue;
        }
        let name = rest[..open].trim().to_string();
        if let Some(url) = lines.get(i + 1) {
            sources.push((name, url.to_string()));
        }
    }
    sources
}

/// The name to configure, and whether it is already there.
///
/// A source already pointing at this URL keeps its name whatever it is called; otherwise
/// the default `domain/repository` is used, and updated rather than added if that name is
/// taken by something else.
fn nuget_source_name(
    index_url: &str,
    default_name: &str,
    existing: &[(String, String)],
) -> (String, bool) {
    if let Some((name, _)) = existing.iter().find(|(_, url)| url == index_url) {
        return (name.clone(), true);
    }
    if existing.iter().any(|(name, _)| name == default_name) {
        return (default_name.to_string(), true);
    }
    (default_name.to_string(), false)
}

fn nuget_command(
    tool: &str,
    operation: &str,
    index_url: &str,
    source_name: &str,
    auth_token: &str,
) -> Vec<String> {
    let owned = |parts: Vec<&str>| parts.into_iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if tool == "nuget" {
        return owned(vec![
            "nuget", "sources", operation, "-name", source_name, "-source", index_url,
            "-username", "aws", "-password", auth_token,
        ]);
    }
    // `dotnet nuget add source <url> --name <name>` but
    // `dotnet nuget update source <name> --source <url>` — the positional swaps.
    let mut command = owned(vec!["dotnet", "nuget", operation, "source"]);
    if operation == "add" {
        command.push(index_url.to_string());
        command.extend(owned(vec!["--name", source_name]));
    } else {
        command.push(source_name.to_string());
        command.extend(owned(vec!["--source", index_url]));
    }
    command.extend(owned(vec!["--username", "aws", "--password", auth_token]));
    if !cfg!(target_os = "windows") {
        command.push("--store-password-in-clear-text".to_string());
    }
    command
}

/// `~/.npmrc` with `key=value` set: the existing line replaced in place if there is one,
/// otherwise appended. Everything else in the file is left alone.
fn npmrc_with(contents: &str, key: &str, value: &str) -> String {
    let entry = format!("{key}={value}");
    let prefix = format!("{key}=");
    if contents.lines().any(|line| line.starts_with(&prefix)) {
        let mut out: Vec<String> = Vec::new();
        for line in contents.lines() {
            out.push(if line.starts_with(&prefix) { entry.clone() } else { line.to_string() });
        }
        let mut text = out.join("\n");
        if contents.ends_with('\n') {
            text.push('\n');
        }
        return text;
    }
    if contents.is_empty() {
        return format!("{entry}\n");
    }
    if contents.ends_with('\n') {
        format!("{contents}{entry}\n")
    } else {
        format!("{contents}\n{entry}\n")
    }
}

/// `~/.netrc` with this machine's password replaced, or the entry appended.
fn netrc_with(contents: &str, hostname: &str, entry: &str) -> String {
    let marker = format!("machine {hostname} login ");
    if contents.contains(&marker) {
        let mut out: Vec<String> = Vec::new();
        for line in contents.lines() {
            out.push(if line.trim_start().starts_with(&marker) {
                entry.to_string()
            } else {
                line.to_string()
            });
        }
        let mut text = out.join("\n");
        if contents.ends_with('\n') {
            text.push('\n');
        }
        return text;
    }
    if contents.is_empty() {
        format!("{entry}\n")
    } else if contents.ends_with('\n') {
        format!("{contents}{entry}\n")
    } else {
        format!("{contents}\n{entry}\n")
    }
}

/// The `.pypirc` to write: the existing file with a `codeartifact` server added, or a
/// fresh one. `index-servers` is a newline-separated list and `codeartifact` joins it.
fn pypirc(path: &std::path::Path, endpoint: &str, auth_token: &str) -> Result<String, Failure> {
    let existing = read_or_empty(path);
    if existing.trim().is_empty() {
        return Ok(format!(
            "[distutils]\nindex-servers = \n\tpypi\n\tcodeartifact\n\n\
             [codeartifact]\nrepository = {endpoint}\nusername = aws\npassword = {auth_token}\n\n"
        ));
    }
    let mut ini = Ini::parse(&existing);
    let mut servers: Vec<String> = ini
        .get("distutils", "index-servers")
        .map(|value| {
            value.lines().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
        })
        .unwrap_or_default();
    if !servers.iter().any(|s| s == "codeartifact") {
        servers.push("codeartifact".to_string());
    }
    ini.set("distutils", "index-servers", &format!("\n{}", servers.join("\n")));
    ini.set("codeartifact", "repository", endpoint);
    ini.set("codeartifact", "username", "aws");
    ini.set("codeartifact", "password", auth_token);
    Ok(ini.render())
}

/// The little INI model `.pypirc` needs: sections of key/value pairs, with continuation
/// lines folded into the value above them, which is how `index-servers` is written.
struct Ini {
    sections: Vec<(String, Vec<(String, String)>)>,
}

impl Ini {
    fn parse(text: &str) -> Ini {
        let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                sections.push((trimmed[1..trimmed.len() - 1].to_string(), Vec::new()));
                continue;
            }
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                continue;
            }
            let Some((_, entries)) = sections.last_mut() else { continue };
            // An indented line continues the value above it.
            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some((_, value)) = entries.last_mut() {
                    value.push('\n');
                    value.push_str(trimmed);
                    continue;
                }
            }
            if let Some((key, value)) = trimmed.split_once(['=', ':']) {
                entries.push((key.trim().to_string(), value.trim().to_string()));
            }
        }
        Ini { sections }
    }

    fn get(&self, section: &str, key: &str) -> Option<&str> {
        let (_, entries) = self.sections.iter().find(|(name, _)| name == section)?;
        entries.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
    }

    fn set(&mut self, section: &str, key: &str, value: &str) {
        let entries = match self.sections.iter_mut().find(|(name, _)| name == section) {
            Some((_, entries)) => entries,
            None => {
                self.sections.push((section.to_string(), Vec::new()));
                &mut self.sections.last_mut().expect("just pushed").1
            }
        };
        match entries.iter_mut().find(|(name, _)| name == key) {
            Some((_, existing)) => *existing = value.to_string(),
            None => entries.push((key.to_string(), value.to_string())),
        }
    }

    /// `RawConfigParser.write`: `key = value`, a blank line after each section, and a
    /// continuation line indented by a tab.
    fn render(&self) -> String {
        let mut out = String::new();
        for (name, entries) in &self.sections {
            out.push_str(&format!("[{name}]\n"));
            for (key, value) in entries {
                let value = value.replace('\n', "\n\t");
                out.push_str(&format!("{key} = {value}\n"));
            }
            out.push('\n');
        }
        out
    }
}

/// Run one command, with the token kept out of any error it produces.
fn run(tool: &str, command: &[String], auth_token: &str) -> Result<(), Failure> {
    let output = std::process::Command::new(&command[0]).args(&command[1..]).output();
    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            // `CommandFailedError` replaces the token with `******` before printing —
            // the token is on the command line of half these tools.
            let stderr = String::from_utf8_lossy(&output.stderr).replace(auth_token, "******");
            Err(Failure::new(
                exit::GENERAL_ERROR,
                format!(
                    "Command '{}' returned non-zero exit status {}.\nStderr from command:\n{stderr}",
                    command.join(" ").replace(auth_token, "******"),
                    output.status.code().unwrap_or(-1),
                ),
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Failure::new(
            exit::GENERAL_ERROR,
            format!("{tool} was not found. Please verify installation."),
        )),
        Err(e) => Err(Failure::new(exit::GENERAL_ERROR, e)),
    }
}

fn home() -> std::path::PathBuf {
    std::env::var("HOME").map(std::path::PathBuf::from).unwrap_or_default()
}

/// `NPM_CONFIG_USERCONFIG` wins over `~/.npmrc`, as npm itself resolves it.
fn npmrc_path() -> std::path::PathBuf {
    match std::env::var("NPM_CONFIG_USERCONFIG") {
        Ok(custom) if !custom.is_empty() => std::path::PathBuf::from(custom),
        _ => home().join(".npmrc"),
    }
}

fn read_or_empty(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Write a credential-bearing file as `0600`, creating parents, and set the mode even
/// when the file already existed — the reference `chmod`s on top of the open for exactly
/// that case.
fn write_private(path: &std::path::Path, contents: &str) -> Result<(), Failure> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    std::fs::write(path, contents)
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            eprintln!("Unable to set file permissions for {}: {e}", path.display());
        }
    }
    Ok(())
}

fn success_message(tool: &str, endpoint: &str, expiration: &str) {
    let remaining = awsc_protocol::shapes::parse_timestamp(expiration)
        .map(|at| at - crate::now_unix())
        .unwrap_or(0);
    println!("Successfully configured {tool} to use AWS CodeArtifact repository {endpoint} ");
    println!("Login expires in {} at {expiration}", relative_expiration(remaining));
}

/// `@scope`, with the `@` added if it is missing, validated as npm validates it.
fn scope_name(namespace: &str) -> Result<String, Failure> {
    let scope =
        if namespace.starts_with('@') { namespace.to_string() } else { format!("@{namespace}") };
    // `^(@[a-z0-9-~][a-z0-9-._~]*)`: no leading dot or underscore, lowercase only.
    let mut chars = scope.chars().skip(1);
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '~');
    let rest_ok = chars.all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_' | '~')
    });
    if first_ok && rest_ok {
        Ok(scope)
    } else {
        Err(Failure::new(
            exit::GENERAL_ERROR,
            "Invalid scope name, scope must contain URL-safe characters, no leading dots or \
             underscores",
        ))
    }
}

/// The commands a tool's login runs, in order.
///
/// `twine` runs none — it writes `~/.pypirc` itself — and `nuget`/`dotnet` decide between
/// `add` and `update` only after listing what is already configured, which needs the tool
/// present. What is returned here is the `add` form, which is what a dry run shows for a
/// source that is not yet there.
fn commands_for(
    tool: &str,
    endpoint: &str,
    auth_token: &str,
    scope: Option<&str>,
    source_name: &str,
) -> Vec<Vec<String>> {
    let owned = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (host_and_path, scheme) = split_endpoint(endpoint);
    match tool {
        "npm" => {
            let registry =
                scope.map(|scope| format!("{scope}:registry")).unwrap_or("registry".to_string());
            vec![
                owned(&["npm", "config", "set", &registry, endpoint]),
                owned(&["npm", "config", "set", &format!("{host_and_path}:always-auth"), "true"]),
            ]
        }
        // The token goes *into the URL*, which is why `pip config` writes a credential
        // into a plainly readable file.
        "pip" => vec![owned(&[
            "pip",
            "config",
            "set",
            "global.index-url",
            &format!("{scheme}://aws:{auth_token}@{}simple/", host_and_path.trim_start_matches("//")),
        ])],
        "swift" => {
            let mut set = owned(&["swift", "package-registry", "set", endpoint]);
            if let Some(scope) = scope {
                set.push("--scope".to_string());
                set.push(scope.to_string());
            }
            let mut login = owned(&["swift", "package-registry", "login", &format!("{endpoint}login")]);
            if cfg!(target_os = "macos") {
                // macOS stores the token in the Keychain, so it is passed on the command
                // line instead of written to `~/.netrc`.
                login.push("--token".to_string());
                login.push(auth_token.to_string());
            }
            vec![set, login]
        }
        "nuget" => vec![owned(&[
            "nuget",
            "sources",
            "add",
            "-name",
            source_name,
            "-source",
            &format!("{endpoint}v3/index.json"),
            "-username",
            "aws",
            "-password",
            auth_token,
        ])],
        "dotnet" => {
            let mut command = owned(&[
                "dotnet",
                "nuget",
                "add",
                "source",
                &format!("{endpoint}v3/index.json"),
                "--name",
                source_name,
                "--username",
                "aws",
                "--password",
                auth_token,
            ]);
            if !cfg!(target_os = "windows") {
                // Encryption is Windows-only, so everywhere else the password is stored
                // in clear text and the tool insists you say so.
                command.push("--store-password-in-clear-text".to_string());
            }
            vec![command]
        }
        _ => Vec::new(),
    }
}

/// `//host/path` and the scheme, as `urlsplit` gives them.
fn split_endpoint(endpoint: &str) -> (String, String) {
    match endpoint.split_once("://") {
        Some((scheme, rest)) => (format!("//{rest}"), scheme.to_string()),
        None => (endpoint.to_string(), "https".to_string()),
    }
}

/// `12 hours`, `1 hour and 5 minutes` — the reference's own phrasing, which stops after
/// the first two non-zero units and singularises a value of one.
pub fn relative_expiration(mut seconds: i64) -> String {
    // 30 seconds of slack, so 11h59m31s reads as "12 hours" rather than "11 hours".
    seconds += 30;
    let units: [(&str, i64); 3] =
        [("day", 86_400), ("hour", 3_600), ("minute", 60)];
    let mut parts: Vec<String> = Vec::new();
    let mut started = false;
    for (name, size) in units {
        let value = seconds / size;
        seconds %= size;
        if value > 0 {
            if started {
                parts.push("and".to_string());
            }
            parts.push(value.to_string());
            parts.push(if value == 1 { name.to_string() } else { format!("{name}s") });
        }
        if started {
            break;
        }
        started = value > 0;
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npmrc_replaces_an_existing_line_and_keeps_the_rest() {
        let existing = "registry=https://old\n//host/npm/repo/:_authToken=OLD\nother=1\n";
        let updated = npmrc_with(existing, "//host/npm/repo/:_authToken", "NEW");
        assert!(updated.contains("//host/npm/repo/:_authToken=NEW"));
        assert!(!updated.contains("OLD"));
        assert!(updated.contains("registry=https://old"), "{updated}");
        assert!(updated.contains("other=1"));
    }

    #[test]
    fn npmrc_appends_when_absent_and_creates_when_empty() {
        assert_eq!(npmrc_with("", "k", "v"), "k=v\n");
        assert_eq!(npmrc_with("a=1\n", "k", "v"), "a=1\nk=v\n");
        // A file with no trailing newline gets one before the new entry.
        assert_eq!(npmrc_with("a=1", "k", "v"), "a=1\nk=v\n");
    }

    #[test]
    fn netrc_replaces_the_entry_for_this_machine_only() {
        let existing = "machine other.com login token password X\nmachine host login token password OLD\n";
        let updated = netrc_with(existing, "host", "machine host login token password NEW");
        assert!(updated.contains("machine host login token password NEW"));
        assert!(updated.contains("machine other.com login token password X"));
        assert!(!updated.contains("OLD"));
    }

    /// The listing is localised in the bracketed word, so only the brackets are matched.
    #[test]
    fn nuget_sources_parse_out_of_the_listing() {
        let listing = "Registered Sources:\n\n  1.  nuget.org [Enabled]\n      https://api.nuget.org/v3/index.json\n  100. My Source [Activé]\n       https://example.com/v3/index.json\n";
        let sources = parse_nuget_sources(listing);
        assert_eq!(
            sources,
            vec![
                ("nuget.org".to_string(), "https://api.nuget.org/v3/index.json".to_string()),
                ("My Source".to_string(), "https://example.com/v3/index.json".to_string()),
            ]
        );
    }

    /// A source already pointing at this URL keeps whatever name it has.
    #[test]
    fn an_existing_url_keeps_its_source_name() {
        let existing = vec![("Someone Elses Name".to_string(), "https://e/v3/index.json".to_string())];
        assert_eq!(
            nuget_source_name("https://e/v3/index.json", "domain/repo", &existing),
            ("Someone Elses Name".to_string(), true)
        );
        // A free name is added rather than updated.
        assert_eq!(
            nuget_source_name("https://new/v3/index.json", "domain/repo", &existing),
            ("domain/repo".to_string(), false)
        );
        // The default name taken by something else is updated in place.
        let taken = vec![("domain/repo".to_string(), "https://other".to_string())];
        assert_eq!(
            nuget_source_name("https://new/v3/index.json", "domain/repo", &taken),
            ("domain/repo".to_string(), true)
        );
    }

    /// `dotnet` swaps the positional between add and update, which is easy to miss.
    #[test]
    fn dotnet_orders_add_and_update_differently() {
        let add = nuget_command("dotnet", "add", "https://u", "name", "tok");
        assert_eq!(add[2..5], ["add", "source", "https://u"]);
        let update = nuget_command("dotnet", "update", "https://u", "name", "tok");
        assert_eq!(update[2..5], ["update", "source", "name"]);
    }

    #[test]
    fn pypirc_adds_the_server_without_disturbing_the_rest() {
        let dir = std::env::temp_dir().join(format!("awsc-pypirc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(".pypirc");
        std::fs::write(
            &path,
            "[distutils]\nindex-servers =\n\tpypi\n\n[pypi]\nusername = me\n",
        )
        .expect("write");
        let rendered = pypirc(&path, "https://endpoint/", "TOKEN").expect("renders");
        assert!(rendered.contains("[codeartifact]"), "{rendered}");
        assert!(rendered.contains("password = TOKEN"));
        // The existing server survives, and so does the unrelated section.
        assert!(rendered.contains("pypi"));
        assert!(rendered.contains("username = me"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn npm_sets_the_registry_and_always_auth() {
        let commands =
            commands_for("npm", "https://d-1.codeartifact.eu-west-1.amazonaws.com/npm/repo/", "tok", None, "d/r");
        assert_eq!(commands[0][3], "registry");
        assert_eq!(
            commands[1][3],
            "//d-1.codeartifact.eu-west-1.amazonaws.com/npm/repo/:always-auth"
        );
    }

    /// A namespace becomes an npm scope, and the registry key is scoped with it.
    #[test]
    fn a_namespace_scopes_the_npm_registry() {
        let commands = commands_for("npm", "https://host/npm/repo/", "tok", Some("@acme"), "d/r");
        assert_eq!(commands[0][3], "@acme:registry");
    }

    #[test]
    fn a_scope_gets_its_at_sign_and_is_validated() {
        assert_eq!(scope_name("acme").expect("valid"), "@acme");
        assert_eq!(scope_name("@acme").expect("valid"), "@acme");
        assert!(scope_name(".leading-dot").is_err());
        assert!(scope_name("_leading-underscore").is_err());
        assert!(scope_name("UPPER").is_err());
    }

    /// pip puts the token in the URL — worth seeing in a test, because it means
    /// `pip config` writes a live credential into a world-readable file.
    #[test]
    fn pip_puts_the_token_in_the_index_url() {
        let commands = commands_for("pip", "https://host/pypi/repo/", "SECRET", None, "d/r");
        assert_eq!(commands[0][4], "https://aws:SECRET@host/pypi/repo/simple/");
    }

    #[test]
    fn twine_runs_no_commands() {
        assert!(commands_for("twine", "https://host/pypi/repo/", "tok", None, "d/r").is_empty());
    }

    #[test]
    fn nuget_points_at_the_v3_index() {
        let commands = commands_for("nuget", "https://host/nuget/repo/", "tok", None, "d/r");
        assert!(commands[0].contains(&"https://host/nuget/repo/v3/index.json".to_string()));
        assert!(commands[0].contains(&"aws".to_string()));
    }

    /// Two non-zero units at most, joined with "and", and singular at one.
    #[test]
    fn the_expiry_reads_the_way_the_reference_phrases_it() {
        assert_eq!(relative_expiration(12 * 3600), "12 hours");
        // 11h59m31s + 30s of slack rounds up to 12 hours exactly.
        assert_eq!(relative_expiration(11 * 3600 + 59 * 60 + 31), "12 hours");
        assert_eq!(relative_expiration(3600 + 5 * 60), "1 hour and 5 minutes");
        assert_eq!(relative_expiration(60), "1 minute");
        assert_eq!(relative_expiration(0), "");
    }
}
