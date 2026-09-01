//! `aws configure sso` and `aws configure sso-session`: the interactive setup for IAM
//! Identity Center.
//!
//! Both walk the user through the same session settings and write them to
//! `~/.aws/config`; `configure sso` then logs in and additionally picks an account and a
//! role, so the resulting profile can source credentials.
//!
//! The prompting here is line-based. The reference uses `prompt_toolkit`, which redraws
//! the terminal, offers tab completion against existing values, and picks an account from
//! a full-screen arrow-key menu. Reproducing that interaction byte for byte is neither
//! achievable nor useful; what matters is that the *questions*, their defaults and the
//! *configuration written* are the same, and those are.

use crate::args::Parsed;
use crate::configure::writer::{self, Setting, Update};
use crate::exit;
use crate::Failure;
use aws_cli_runtime::credentials::profile::{Config, Section};
use aws_cli_runtime::credentials::sso_login::{self, LoginRequest};
use aws_cli_runtime::RuntimeError;
use std::io::Write;
use std::process::ExitCode;

/// The scope every session gets unless the user says otherwise.
const DEFAULT_SSO_SCOPE: &str = "sso:account:access";

// ---------------------------------------------------------------------------
// Prompting
// ---------------------------------------------------------------------------

/// Ask a question, offering the current value in brackets.
///
/// Enter keeps the current value, which is what makes re-running the command a way to
/// change one setting without retyping the rest. `None` renders as `[None]`, because the
/// reference interpolates Python's `None` into the same slot.
fn ask(prompt_text: &str, current: Option<&str>) -> Result<Option<String>, Failure> {
    let rendered = match current {
        Some(value) => format!("{prompt_text} [{value}]: "),
        None => format!("{prompt_text} [None]: "),
    };
    ask_raw(&rendered, current)
}

/// What came back from a prompt.
///
/// `EndOfInput` is separate from an empty line on purpose: an empty line means "keep the
/// default" and a required prompt asks again, while a closed stdin can never produce an
/// answer and asking again would spin forever.
enum Answer {
    Value(String),
    Empty,
    EndOfInput,
}

/// Ask with an explicit prompt string, for the cases the reference words differently.
fn ask_raw(prompt: &str, current: Option<&str>) -> Result<Option<String>, Failure> {
    Ok(match read_answer(prompt)? {
        Answer::Value(value) => Some(value),
        Answer::Empty | Answer::EndOfInput => current.map(str::to_string),
    })
}

fn read_answer(prompt: &str) -> Result<Answer, Failure> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => Ok(Answer::EndOfInput),
        Ok(_) => {
            let trimmed = line.trim();
            match trimmed.is_empty() {
                true => Ok(Answer::Empty),
                false => Ok(Answer::Value(trimmed.to_string())),
            }
        }
        Err(e) => Err(Failure::new(exit::GENERAL_ERROR, format!("reading input: {e}"))),
    }
}

/// Ask until an answer is given, for the values that have no usable default.
fn ask_required(prompt: &str, current: Option<&str>) -> Result<String, Failure> {
    loop {
        match read_answer(prompt)? {
            Answer::Value(value) => return Ok(value),
            Answer::Empty => match current {
                Some(value) => return Ok(value.to_string()),
                // No default to fall back on, so ask again.
                None => continue,
            },
            Answer::EndOfInput => {
                return Err(Failure::new(
                    exit::GENERAL_ERROR,
                    "a value is required and standard input has ended",
                ))
            }
        }
    }
}

/// The session settings, in the order they are written.
struct SessionAnswers {
    name: String,
    start_url: String,
    region: String,
    scopes: String,
}

/// Walk the shared session questions. `existing` seeds the defaults when the session is
/// already configured, so re-running changes only what the user retypes.
fn prompt_for_session(
    config: &Config,
    initial_name: Option<String>,
) -> Result<SessionAnswers, Failure> {
    // The name is asked for without a bracketed default when there is none yet, and the
    // answer then decides which existing settings become the defaults for the rest.
    let name = match initial_name {
        Some(name) => name,
        None => ask_required("SSO session name: ", None)?,
    };
    let existing: Section = config.sso_sessions.get(&name).cloned().unwrap_or_default();

    let start_url = ask_required(
        &format!(
            "SSO start URL [{}]: ",
            existing.get("sso_start_url").map(String::as_str).unwrap_or("None")
        ),
        existing.get("sso_start_url").map(String::as_str),
    )?;

    // An AWS-owned start URL tells us nothing about the region, so it is asked for. The
    // reference would first try to resolve a *vanity* URL by fetching it; we always ask,
    // which is the same path it takes when that resolution fails.
    let region = ask_required(
        &format!(
            "SSO region [{}]: ",
            existing.get("sso_region").map(String::as_str).unwrap_or("None")
        ),
        existing.get("sso_region").map(String::as_str),
    )?;

    let current_scopes =
        existing.get("sso_registration_scopes").cloned().unwrap_or_else(|| DEFAULT_SSO_SCOPE.to_string());
    let scopes = ask("SSO registration scopes", Some(&current_scopes))?
        .unwrap_or(current_scopes);

    Ok(SessionAnswers { name, start_url, region, scopes })
}

/// Write `[sso-session NAME]`, in the reference's key order.
fn write_session(answers: &SessionAnswers) -> Result<(), Failure> {
    let update = Update {
        section: format!("sso-session {}", super::quote_section_name(&answers.name)),
        values: vec![
            ("sso_start_url".to_string(), Setting::Value(answers.start_url.clone())),
            ("sso_region".to_string(), Setting::Value(answers.region.clone())),
            (
                "sso_registration_scopes".to_string(),
                Setting::Value(answers.scopes.clone()),
            ),
        ],
    };
    writer::update_config(&update, &super::config_path())
        .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))
}

// ---------------------------------------------------------------------------
// configure sso-session
// ---------------------------------------------------------------------------

pub fn sso_session(_parsed: &Parsed) -> Result<ExitCode, Failure> {
    let config = Config::load().map_err(|e| Failure::new(exit::CONFIGURATION, e))?;
    let answers = prompt_for_session(&config, None)?;
    write_session(&answers)?;

    print!(
        "\nCompleted configuring SSO session: {}\nRun the following to login and refresh \
         access token for this session:\n\naws sso login --sso-session {}\n",
        answers.name, answers.name
    );
    Ok(exit::code(exit::SUCCESS))
}

// ---------------------------------------------------------------------------
// configure sso
// ---------------------------------------------------------------------------

pub fn sso(parsed: &Parsed) -> Result<ExitCode, Failure> {
    let config = Config::load().map_err(|e| Failure::new(exit::CONFIGURATION, e))?;
    let no_browser = parsed.parameters.contains_key("--no-browser");
    let profile_flag = parsed.profile.clone();
    let profile_config: Section = profile_flag
        .as_deref()
        .and_then(|name| config.profile(name))
        .unwrap_or_default();

    // A session name is recommended but not required; declining it selects the legacy
    // format, where the SSO settings live directly in the profile.
    let session_name = ask_raw(
        "SSO session name (Recommended): ",
        profile_config.get("sso_session").map(String::as_str),
    )?;

    let Some(session_name) = session_name.filter(|s| !s.is_empty()) else {
        return Err(Failure::new(
            exit::GENERAL_ERROR,
            "configuring SSO in the legacy format (without a session) is not implemented; \
             supply a session name, or write sso_start_url and sso_region with \
             `aws configure set`",
        ));
    };

    let answers = prompt_for_session(&config, Some(session_name.clone()))?;

    // Log in first: the account and role lists come from the token.
    let request = LoginRequest {
        start_url: answers.start_url.clone(),
        sso_region: answers.region.clone(),
        session_name: Some(answers.name.clone()),
        scopes: answers.scopes.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        no_browser,
    };
    sso_login::device_login(&request).map_err(crate::sso::login_failure)?;

    // The session is written before the profile, so a failure picking an account still
    // leaves something usable behind -- `aws sso login --sso-session X` will work.
    write_session(&answers)?;

    let token = sso_login::cached_access_token(&answers.name).ok_or_else(|| {
        Failure::new(
            exit::CONFIGURATION,
            RuntimeError::Configuration("the login did not produce an access token".to_string()),
        )
    })?;

    let account_id = choose_account(&answers.region, &token)?;
    let role_name = choose_role(&answers.region, &token, &account_id)?;

    let region = ask("Default client Region", profile_config.get("region").map(String::as_str))?;
    let output = ask(
        "CLI default output format (json if not specified)",
        profile_config.get("output").map(String::as_str),
    )?;

    let default_profile = format!("{role_name}-{account_id}");
    let profile = match profile_flag {
        Some(name) => name,
        None => ask("Profile name", Some(&default_profile))?.unwrap_or(default_profile),
    };

    let mut values = vec![
        ("sso_session".to_string(), Setting::Value(answers.name.clone())),
        ("sso_account_id".to_string(), Setting::Value(account_id)),
        ("sso_role_name".to_string(), Setting::Value(role_name)),
    ];
    if let Some(region) = region.filter(|s| !s.is_empty()) {
        values.push(("region".to_string(), Setting::Value(region)));
    }
    if let Some(output) = output.filter(|s| !s.is_empty()) {
        values.push(("output".to_string(), Setting::Value(output)));
    }

    let section = if profile == "default" {
        "default".to_string()
    } else {
        format!("profile {}", super::quote_section_name(&profile))
    };
    writer::update_config(&Update { section, values }, &super::config_path())
        .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;

    if profile.eq_ignore_ascii_case("default") {
        print!(
            "The AWS CLI is now configured to use the default profile.\nRun the following \
             command to verify your configuration:\n\naws sts get-caller-identity\n"
        );
    } else {
        print!(
            "To use this profile, specify the profile name using --profile, as shown:\n\n\
             aws sts get-caller-identity --profile {profile}\n"
        );
    }
    Ok(exit::code(exit::SUCCESS))
}

/// List the accounts the token can see and pick one.
fn choose_account(region: &str, token: &str) -> Result<String, Failure> {
    let accounts = sso_login::list_accounts(region, token).map_err(crate::sso::login_failure)?;

    if accounts.is_empty() {
        return Err(Failure::new(exit::GENERAL_ERROR, "No AWS accounts are available to you."));
    }
    if accounts.len() == 1 {
        let id = accounts[0].account_id.clone();
        println!("The only AWS account available to you is: {id}");
        println!("Using the account ID {id}");
        return Ok(id);
    }

    println!("There are {} AWS accounts available to you.", accounts.len());
    let mut sorted = accounts;
    sorted.sort_by_key(sso_login::Account::sort_key);
    let labels: Vec<String> = sorted.iter().map(sso_login::Account::display).collect();
    let index = choose(&labels)?;
    let id = sorted[index].account_id.clone();
    println!("Using the account ID {id}");
    Ok(id)
}

fn choose_role(region: &str, token: &str, account_id: &str) -> Result<String, Failure> {
    let mut roles =
        sso_login::list_account_roles(region, token, account_id).map_err(crate::sso::login_failure)?;

    if roles.is_empty() {
        return Err(Failure::new(
            exit::GENERAL_ERROR,
            format!("No roles are available for the account {account_id}"),
        ));
    }
    if roles.len() == 1 {
        let name = roles.remove(0);
        println!("The only role available to you is: {name}");
        println!("Using the role name \"{name}\"");
        return Ok(name);
    }

    println!("There are {} roles available to you.", roles.len());
    roles.sort_by_key(|r| r.to_lowercase());
    let index = choose(&roles)?;
    let name = roles[index].clone();
    println!("Using the role name \"{name}\"");
    Ok(name)
}

/// Pick one of several options.
///
/// A numbered list, where the reference draws an arrow-key menu. The selection is the same
/// and the ordering is the same; only the drawing differs, and a numbered list is the one
/// form that also works when stdin is a pipe.
fn choose(options: &[String]) -> Result<usize, Failure> {
    for (i, option) in options.iter().enumerate() {
        println!("{:>3}. {option}", i + 1);
    }
    loop {
        let answer = ask_raw(&format!("Select 1-{}: ", options.len()), None)?;
        let Some(answer) = answer else {
            return Err(Failure::new(exit::GENERAL_ERROR, "no selection was made"));
        };
        match answer.parse::<usize>() {
            Ok(n) if n >= 1 && n <= options.len() => return Ok(n - 1),
            _ => println!("Enter a number between 1 and {}.", options.len()),
        }
    }
}
