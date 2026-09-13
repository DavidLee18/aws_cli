//! `help` and `--help`, rendered from the service models.
//!
//! **This is not the reference's help and does not try to be.** `aws help` renders
//! reStructuredText into a man page with `groff` and pipes it through the user's pager;
//! reproducing that byte for byte would mean shipping a roff pipeline for output nobody
//! diffs. What a reader actually needs from help is what a command is called, what it
//! does, and which options it takes — all of which the Smithy models carry, so all of it
//! is answered here, as plain text on stdout at exit 0.
//!
//! The option names are **not** re-derived. They come from the same
//! [`crate::args::flag_for_member`] the parser binds with, because a help page that lists
//! a flag the parser does not accept is worse than no help at all — and this codebase has
//! already been bitten once by two independent derivations of the same surface.
//!
//! Documentation in the models is HTML (`<p>`, `<code>`, `<ul>`), so it is converted to
//! text here rather than printed raw.

use awsc_model::shape::{Shape, StructureShape};
use awsc_model::Model;
use std::process::ExitCode;

/// Which help page was asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    /// `awsc help`
    TopLevel,
    /// `awsc <service> help`
    Service(String),
    /// `awsc <service> <operation> help`
    Operation(String, String),
}

/// Print the requested page. Always exit 0, as the reference's help does.
pub fn show(request: Request) -> Result<ExitCode, crate::Failure> {
    let page = match request {
        Request::TopLevel => top_level(),
        Request::Service(service) => service_page(&service)?,
        Request::Operation(service, operation) => operation_page(&service, &operation)?,
    };
    print!("{page}");
    Ok(crate::exit::code(crate::exit::SUCCESS))
}

fn width() -> usize {
    crate::s3::progress::terminal_width().clamp(40, 100)
}

fn top_level() -> String {
    let mut out = String::new();
    out.push_str("NAME\n    awsc - a Rust port of the AWS CLI v2\n\n");
    out.push_str("SYNOPSIS\n    awsc <command> <subcommand> [parameters]\n\n");
    out.push_str(&section(
        "DESCRIPTION",
        "Commands are derived from the AWS service models, so every operation of every \
         modelled service is available. Run `awsc <command> help` for a service's \
         operations, and `awsc <command> <subcommand> help` for one operation's options.",
    ));
    out.push_str(
        "GLOBAL OPTIONS\n\
         \x20   --region <region>            the region to call\n\
         \x20   --profile <name>             the credentials profile to use\n\
         \x20   --output <format>            json | text | table | yaml | yaml-stream | off\n\
         \x20   --query <expression>         a JMESPath expression applied to the response\n\
         \x20   --endpoint-url <url>         call this endpoint instead of the resolved one\n\
         \x20   --no-paginate                one page only, keeping the pagination token\n\
         \x20   --cli-input-json <document>  read the parameters from a JSON document\n\
         \x20   --cli-input-yaml <document>  read the parameters from a YAML document\n\
         \x20   --generate-cli-skeleton      print an empty parameter document and exit\n\
         \x20   --cli-error-format <style>   legacy | json | yaml | text | table | enhanced\n\
         \x20   --color <on|off|auto>        colour warnings and errors on stderr\n\
         \x20   --debug                      print the signed request to stderr\n\
         \x20   --version                    print the version and exit\n\n",
    );

    let services = crate::known_services();
    if services.is_empty() {
        out.push_str(
            "COMMANDS\n    (the service catalogue is not installed yet; run `awsc \
             update-models`)\n",
        );
        return out;
    }
    out.push_str("COMMANDS\n");
    out.push_str(&columns(&services.iter().map(String::as_str).collect::<Vec<_>>()));
    out.push_str("\nAlso available: s3, configure, sso, update-models.\n");
    out
}

fn service_page(service: &str) -> Result<String, crate::Failure> {
    let model = crate::load_model(service).map_err(|_| crate::unknown_service(service))?;
    let cli_service = model
        .cli_service_name()
        .map_err(|e| crate::Failure::new(crate::exit::GENERAL_ERROR, e))?;
    let table = awsc_model::command_table::build(
        &model,
        awsc_model::surface_overlays::get(),
        awsc_model::surface_overlays::custom_surface(),
    )
    .map_err(|e| crate::Failure::new(crate::exit::GENERAL_ERROR, e))?;

    let mut out = format!("NAME\n    {cli_service}\n\n");
    if let Ok(shape) = model.service() {
        if let Some(documentation) = shape.traits.documentation() {
            // The opening only. A service's documentation is the whole API preamble —
            // ec2's runs to pages of links — and the reader is here for the operation
            // list below it.
            let text = text_from_html(documentation);
            let opening: Vec<&str> = text.split("\n\n").take(2).collect();
            out.push_str(&section("DESCRIPTION", &opening.join("\n\n")));
        }
    }
    out.push_str(&format!(
        "SYNOPSIS\n    awsc {cli_service} <operation> [parameters]\n\n"
    ));

    let mut names: Vec<&str> = table.names.keys().map(String::as_str).collect();
    // Custom commands are not in the model-derived table; they are real commands all the
    // same, and a help page that omits the one the user is looking for is a dead end.
    let custom = crate::custom_commands_for(&cli_service);
    let implemented: Vec<&str> =
        custom.iter().filter(|name| crate::is_implemented(&cli_service, name)).copied().collect();
    names.extend(&implemented);
    names.sort_unstable();
    out.push_str("OPERATIONS\n");
    out.push_str(&columns(&names));

    // A name is only missing if it is neither implemented as a custom command *nor*
    // reachable as a modelled operation. Thirty-seven of the surface data's "custom
    // commands" are ordinary model-backed operations that work here already — every
    // `socialmessaging` WhatsApp call among them — and listing those as missing would be
    // a help page telling the reader that working commands do not work.
    let missing: Vec<&str> = custom
        .iter()
        .filter(|name| !crate::is_implemented(&cli_service, name))
        .filter(|name| table.resolve(name.split_whitespace().next().unwrap_or(name)).is_none())
        .copied()
        .collect();
    if !missing.is_empty() {
        out.push('\n');
        out.push_str(&section(
            "NOT IMPLEMENTED IN AWSC",
            &format!(
                "The AWS CLI also provides {} here, which no service model describes — \
                 either hand-written there, or an API the Smithy models this build \
                 derives from no longer carry: {}.",
                if missing.len() == 1 { "one command" } else { "these commands" },
                missing.join(", ")
            ),
        ));
    }
    Ok(out)
}

fn operation_page(service: &str, operation: &str) -> Result<String, crate::Failure> {
    let model = crate::load_model(service).map_err(|_| crate::unknown_service(service))?;
    let cli_service = model
        .cli_service_name()
        .map_err(|e| crate::Failure::new(crate::exit::GENERAL_ERROR, e))?;

    // A custom command has no model to describe, so its help is the argument list the
    // surface data records for it.
    if crate::custom_commands_for(&cli_service).contains(&operation) {
        return Ok(custom_command_page(&cli_service, operation));
    }

    let table = awsc_model::command_table::build(
        &model,
        awsc_model::surface_overlays::get(),
        awsc_model::surface_overlays::custom_surface(),
    )
    .map_err(|e| crate::Failure::new(crate::exit::GENERAL_ERROR, e))?;
    let wire = table
        .resolve(operation)
        .ok_or_else(|| crate::unknown_operation(operation, &table))?;
    let (_, op) = model
        .operation(wire)
        .map_err(|_| crate::unknown_operation(operation, &table))?;
    let input = model
        .operation_input(op)
        .map_err(|e| crate::Failure::new(crate::exit::GENERAL_ERROR, e))?;

    let mut out = format!("NAME\n    {operation}\n\n");
    if let Some(documentation) = op.traits.documentation() {
        out.push_str(&section("DESCRIPTION", &text_from_html(documentation)));
    }

    let options = options_of(&model, input, &cli_service, operation);
    out.push_str("SYNOPSIS\n");
    out.push_str(&format!("    awsc {cli_service} {operation}\n"));
    for option in &options {
        let line = format!("{} {}", option.flag, option.placeholder);
        out.push_str(&format!(
            "        {}\n",
            if option.required { line } else { format!("[{line}]") }
        ));
    }
    out.push('\n');

    if options.is_empty() {
        out.push_str("OPTIONS\n    This operation takes no parameters.\n\n");
    } else {
        out.push_str("OPTIONS\n");
        for option in &options {
            out.push_str(&format!(
                "    {} {}{}\n",
                option.flag,
                option.placeholder,
                if option.required { "  [required]" } else { "" }
            ));
            if !option.documentation.is_empty() {
                out.push_str(&wrap(&option.documentation, width().saturating_sub(8), 8));
            }
            if !option.values.is_empty() {
                out.push_str(&wrap(
                    &format!("Possible values: {}", option.values.join(", ")),
                    width().saturating_sub(8),
                    8,
                ));
            }
            out.push('\n');
        }
    }
    if crate::paginate::overlay().get(&cli_service, operation).is_some() {
        out.push_str(&section(
            "PAGINATION",
            "This operation paginates. Every page is fetched and the results are merged \
             unless you pass --no-paginate. --max-items stops after N items and prints a \
             NextToken to resume from, --starting-token resumes from one, and --page-size \
             changes how much each request asks for without changing the total.",
        ));
    }
    out.push_str(
        "See `awsc help` for the global options, which every operation also accepts.\n",
    );
    Ok(out)
}

fn custom_command_page(service: &str, operation: &str) -> String {
    let mut out = format!("NAME\n    {operation}\n\n");
    let implemented = crate::is_implemented(service, operation);
    let head = operation.split_whitespace().next().unwrap_or(operation);
    out.push_str(&section(
        "DESCRIPTION",
        if implemented {
            crate::custom::summary(service, head).unwrap_or(
                "A custom command: hand-written rather than derived from a service model.",
            )
        } else {
            "A command the AWS CLI provides that no service model describes. It has NOT \
             been implemented in awsc, so running it reports that rather than doing \
             anything."
        },
    ));
    let flags = crate::custom_command_flags(service, operation);
    out.push_str("SYNOPSIS\n");
    out.push_str(&format!("    awsc {service} {operation}\n"));
    for flag in &flags {
        out.push_str(&format!("        [{flag} <value>]\n"));
    }
    out
}

/// One documented option on an operation.
struct Option_ {
    flag: String,
    placeholder: String,
    required: bool,
    documentation: String,
    values: Vec<String>,
}

fn options_of(
    model: &Model,
    input: Option<&StructureShape>,
    service: &str,
    operation: &str,
) -> Vec<Option_> {
    let Some(shape) = input else { return Vec::new() };
    let mut options: Vec<Option_> = shape
        .members
        .iter()
        .map(|(name, member)| {
            let flag = crate::args::flag_for_member(service, operation, name);
            let target = model.shape(&member.target);
            let placeholder = match target {
                Some(Shape::Boolean(_)) => "| --no-...".to_string(),
                Some(other) => format!("<{}>", other.type_name()),
                None => "<value>".to_string(),
            };
            let placeholder = if matches!(target, Some(Shape::Boolean(_))) {
                format!("| --no-{}", flag.trim_start_matches("--"))
            } else {
                placeholder
            };
            let documentation = member
                .traits
                .documentation()
                .or_else(|| target.and_then(|s| s.traits().documentation()))
                .map(text_from_html)
                .unwrap_or_default();
            let values = target
                .and_then(|shape| match shape {
                    // An enum's members carry the wire values; `enum_values` reads the
                    // older trait form. Both spellings appear across the catalogue.
                    Shape::Enum(e) => Some(e.members.keys().cloned().collect::<Vec<_>>()),
                    other => other
                        .traits()
                        .enum_values()
                        .map(|v| v.into_iter().map(str::to_string).collect()),
                })
                .unwrap_or_default();
            Option_ {
                flag,
                placeholder,
                required: member.traits.is_required(),
                documentation,
                values,
            }
        })
        .collect();
    // Required first, then alphabetical: the required ones are what a reader is looking
    // for, and the model's member order is arbitrary.
    options.sort_by(|a, b| b.required.cmp(&a.required).then_with(|| a.flag.cmp(&b.flag)));
    options
}

/// A titled, wrapped block.
fn section(title: &str, body: &str) -> String {
    format!("{title}\n{}\n", wrap(body, width().saturating_sub(4), 4))
}

/// Wrap `text` to `width` columns, indenting every line by `indent` spaces. Blank lines
/// in the input are kept, so paragraphs survive.
fn wrap(text: &str, width: usize, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut out = String::new();
    for paragraph in text.split('\n') {
        if paragraph.trim().is_empty() {
            out.push('\n');
            continue;
        }
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                out.push_str(&pad);
                out.push_str(&line);
                out.push('\n');
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            out.push_str(&pad);
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// Lay names out in columns across the terminal.
fn columns(names: &[&str]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let longest = names.iter().map(|n| n.chars().count()).max().unwrap_or(1);
    let column = longest + 2;
    let per_line = ((width().saturating_sub(4)) / column).max(1);
    let mut out = String::new();
    for chunk in names.chunks(per_line) {
        out.push_str("    ");
        for (i, name) in chunk.iter().enumerate() {
            if i + 1 == chunk.len() {
                out.push_str(name);
            } else {
                out.push_str(&format!("{name:<column$}"));
            }
        }
        out.push('\n');
    }
    out
}

/// The models document everything in HTML. This is a reader, not a renderer: it keeps the
/// words and the paragraph breaks and throws away the markup.
pub fn text_from_html(html: &str) -> String {
    let mut out = String::new();
    let mut chars = html.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '<' => {
                let mut tag = String::new();
                for next in chars.by_ref() {
                    if next == '>' {
                        break;
                    }
                    tag.push(next);
                }
                let name = tag
                    .trim_start_matches('/')
                    .split([' ', '\t', '\n'])
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                match name.as_str() {
                    // Block elements become paragraph breaks; a run of them collapses.
                    "p" | "div" | "ul" | "ol" | "dl" | "note" | "important" | "br" => {
                        if !out.ends_with("\n\n") && !out.is_empty() {
                            out.push('\n');
                            if !out.ends_with("\n\n") {
                                out.push('\n');
                            }
                        }
                    }
                    "li" if !tag.starts_with('/') => {
                        if !out.ends_with('\n') && !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str("* ");
                    }
                    _ => {}
                }
            }
            '&' => {
                let mut entity = String::new();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next == ';' {
                        break;
                    }
                    entity.push(next);
                    if entity.len() > 8 {
                        break;
                    }
                }
                out.push_str(match entity.as_str() {
                    "lt" => "<",
                    "gt" => ">",
                    "amp" => "&",
                    "quot" => "\"",
                    "apos" | "#39" => "'",
                    "nbsp" => " ",
                    _ => "",
                });
            }
            _ => out.push(ch),
        }
    }
    // Collapse the runs of whitespace the markup left behind, but keep paragraph breaks.
    let paragraphs: Vec<String> = out
        .split("\n\n")
        .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|paragraph| !paragraph.is_empty())
        .collect();
    // A `<li>` whose content is wrapped in `<p>` leaves its bullet stranded on a line of
    // its own; rejoin it with the text it belongs to.
    let mut joined: Vec<String> = Vec::with_capacity(paragraphs.len());
    for paragraph in paragraphs {
        if joined.last().map(String::as_str) == Some("*") {
            let last = joined.last_mut().expect("just checked");
            *last = format!("* {paragraph}");
            continue;
        }
        joined.push(paragraph);
    }
    joined.retain(|paragraph| paragraph != "*");
    joined.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_words_out_of_html() {
        assert_eq!(
            text_from_html("<p>The <code>Id</code> of the thing.</p>"),
            "The Id of the thing."
        );
        assert_eq!(text_from_html("<p>One.</p> <p>Two.</p>"), "One.\n\nTwo.");
        assert_eq!(text_from_html("a &lt; b &amp;&amp; c &gt; d"), "a < b && c > d");
    }

    #[test]
    fn keeps_list_items_on_their_own_lines() {
        let text = text_from_html("<ul><li>first</li><li>second</li></ul>");
        assert!(text.contains("* first"), "{text}");
        assert!(text.contains("* second"), "{text}");
    }

    #[test]
    fn wraps_to_the_given_width() {
        // Width counts the text, not the indent: "aaa bbb" is exactly 7.
        assert_eq!(wrap("aaa bbb ccc ddd", 7, 2), "  aaa bbb\n  ccc ddd\n");
        assert_eq!(wrap("aaa bbb", 3, 0), "aaa\nbbb\n");
    }

    /// Every line of a column layout has to fit, or the terminal re-wraps it and the
    /// columns stop lining up.
    #[test]
    fn columns_fit_the_terminal() {
        let names = ["a", "bb", "ccc", "dddd", "eeeee", "ffffff"];
        for line in columns(&names).lines() {
            assert!(line.chars().count() <= width(), "{line:?}");
        }
    }
}
