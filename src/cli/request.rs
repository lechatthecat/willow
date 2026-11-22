//! The argument stream is forward-only. Each family consumes its own syntax;
//! protocol options are removed lazily, including between a flag and its value.
use super::*;
use protocol::ArgumentError;
use willow_compiler::package::CommandErrorFamily;

pub(super) type Args<'i, 'a> = &'i mut dyn Iterator<Item = &'a String>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum OutputFormat {
    #[default]
    Human,
    Json,
    Ndjson,
}

impl OutputFormat {
    pub(super) fn parse(value: &str) -> Option<Self> {
        match value {
            "human" => Some(Self::Human),
            "json" => Some(Self::Json),
            "ndjson" => Some(Self::Ndjson),
            _ => None,
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Json => "json",
            Self::Ndjson => "ndjson",
        }
    }
}

pub(super) fn split_option(arg: &str) -> (&str, Option<&str>) {
    if !arg.starts_with('-') {
        return (arg, None);
    }
    arg.split_once('=')
        .map_or((arg, None), |(k, v)| (k, Some(v)))
}

// A malformed request still has an output contract, including when the
// offending argument precedes the output option or consumes it as a value.
#[derive(Default)]
struct ErrorOutput {
    pending_format: bool,
    first_human: Option<bool>,
    machine: bool,
    capped: bool,
}

impl ErrorOutput {
    fn observe(&mut self, arg: &str) {
        let (key, inline) = split_option(arg);
        if self.pending_format {
            self.format(arg);
        }
        self.pending_format = key == "--format" && inline.is_none();
        if key == "--format"
            && let Some(value) = inline
        {
            self.format(value);
        }
        self.capped |= key == "--max-chars";
    }

    fn format(&mut self, value: &str) {
        let format = OutputFormat::parse(value);
        self.first_human
            .get_or_insert(format == Some(OutputFormat::Human));
        self.machine |= matches!(format, Some(OutputFormat::Json | OutputFormat::Ndjson));
    }

    fn human(&self) -> bool {
        self.first_human.unwrap_or(true)
    }
}

pub(super) enum Request<'a> {
    Help(String),
    Human(Result<CliCommand>),
    ProtocolHuman(Result<CliCommand>),
    Machine(CommandErrorFamily, Result<CliCommand>),
    Protocol(Result<CliCommand, ArgumentError>),
    Direct {
        name: &'a str,
        parsed: Result<direct::Options>,
        human_errors: bool,
    },
    Overview {
        parsed: Result<overview::Options>,
        human_errors: bool,
    },
    VerifyEdit(Result<semantic_diff::VerifyEdit>),
}

impl<'a> Request<'a> {
    pub(super) fn parse(argv: &'a [String]) -> Self {
        Self::parse_iter(argv.iter())
    }

    fn parse_iter(mut argv: impl Iterator<Item = &'a String>) -> Self {
        let Some(name) = argv.next() else {
            return Self::Human(CliCommand::parse(&mut argv));
        };
        let mut tail = argv.peekable();
        let second = tail.peek().map(|s| s.as_str());
        match name.as_str() {
            "verify-edit" => {
                if matches!(second, Some("-h" | "--help")) {
                    let first = tail.next();
                    if tail.peek().is_none() {
                        return Self::Help(semantic_diff::HELP.into());
                    }
                    return Self::VerifyEdit(semantic_diff::parse(
                        &mut first.into_iter().chain(tail),
                    ));
                }
                return Self::VerifyEdit(semantic_diff::parse(&mut tail));
            }
            "help" => return Self::Help(help_text(second.filter(|s| *s != "--"))),
            "--help" | "-h" => return Self::Help(usage()),
            _ => {}
        }

        let mut help = false;
        let mut own = name != "--";
        let mut args = tail
            .take_while(|arg| {
                own &= *arg != "--";
                help = own && matches!(arg.as_str(), "-h" | "--help");
                !help
            })
            .fuse();
        let request = match name.as_str() {
            "overview" => {
                let mut output = ErrorOutput::default();
                let mut tokens = args.by_ref().inspect(|arg| output.observe(arg));
                let parsed = overview::Options::parse(&mut tokens);
                tokens.for_each(drop);
                Self::Overview {
                    parsed,
                    human_errors: output.human(),
                }
            }
            "refs" | "references" | "symbol" | "type" | "effects" | "rename" => {
                Self::direct(name, &mut args, false)
            }
            "impact"
                if !second.is_some_and(|arg| {
                    arg.ends_with(".wi")
                        || matches!(arg, "." | "..")
                        || std::path::Path::new(arg).is_dir()
                }) =>
            {
                Self::direct(name, &mut args, true)
            }
            "init" | "agent" => Self::Human(CliCommand::parse(
                &mut std::iter::once(name).chain(&mut args),
            )),
            "add" | "remove" | "update" | "deps" | "metadata" | "tidy" | "outdated" | "fetch"
            | "package" => Self::package(name, &mut args, CommandErrorFamily::Package),
            "rust" => Self::package(name, &mut args, CommandErrorFamily::Rust),
            "doctor" => Self::package(name, &mut args, CommandErrorFamily::Doctor),
            "query" if second == Some("rust-bridge") => {
                Self::package(name, &mut args, CommandErrorFamily::Query)
            }
            _ => Self::protocol(
                &mut std::iter::once(name).chain(&mut args),
                matches!(name.as_str(), "impact" | "query" | "edit"),
            ),
        };
        // Help can override even a command that failed on its first argument.
        args.for_each(drop);
        if help {
            Self::Help(help_text((!name.starts_with('-')).then_some(name)))
        } else {
            request
        }
    }

    fn package(name: &'a String, args: Args<'_, 'a>, family: CommandErrorFamily) -> Self {
        let mut output = ErrorOutput::default();
        let mut tokens = args.inspect(|arg| output.observe(arg));
        let parsed = CliCommand::parse(&mut std::iter::once(name).chain(&mut tokens));
        tokens.for_each(drop);
        let machine = match &parsed {
            Ok(CliCommand::Fetch(c) | CliCommand::Verify(c)) => c.format != OutputFormat::Human,
            Ok(CliCommand::Rust(c)) => c.format != OutputFormat::Human,
            Ok(CliCommand::Package(c)) => c.format != OutputFormat::Human,
            _ => output.machine,
        };
        if machine {
            Self::Machine(family, parsed)
        } else {
            Self::Human(parsed)
        }
    }

    fn direct(name: &'a String, args: Args<'_, 'a>, ambiguous: bool) -> Self {
        let mut parser = Ok(direct::Parser::new(name));
        let mut output = ErrorOutput::default();
        let mut legacy = false;
        let mut tokens = args.inspect(|arg| {
            if ambiguous
                && matches!(
                    split_option(arg).0,
                    "--function" | "--revision" | "--file" | "--byte" | "--protocol-version"
                )
            {
                legacy = true;
            }
            if !legacy {
                output.observe(arg);
                if let Ok(state) = &mut parser
                    && let Err(error) = state.push(arg)
                {
                    parser = Err(error);
                }
            }
        });
        // A trailing legacy selector selects the analysis grammar for the
        // entire invocation. Carry both interpretations of the ambiguous
        // prefix in bounded parser state, without buffering/replaying it.
        let analysis =
            ambiguous.then(|| Self::protocol(&mut std::iter::once(name).chain(&mut tokens), true));
        tokens.for_each(drop);
        if legacy {
            return analysis.expect("legacy selectors only apply to impact");
        }
        Self::Direct {
            name,
            parsed: parser.and_then(direct::Parser::finish),
            human_errors: output.human() || output.capped,
        }
    }

    fn protocol(args: Args<'_, 'a>, implicit: bool) -> Self {
        let mut format = None;
        let mut version = None;
        let mut options = true;
        let mut error = None;
        let mut tokens = std::iter::from_fn(|| {
            while let Some(arg) = args.next() {
                options &= arg != "--";
                if !options {
                    return Some(arg);
                }
                let (key, inline) = split_option(arg);
                let slot = match key {
                    "--format" => &mut format,
                    "--protocol-version" => &mut version,
                    _ => return Some(arg),
                };
                let Some(value) = inline.or_else(|| args.next().map(String::as_str)) else {
                    error.get_or_insert_with(|| anyhow::anyhow!("missing {key} value"));
                    return None;
                };
                if slot.replace(value).is_some() {
                    error.get_or_insert_with(|| anyhow::anyhow!("duplicate protocol option"));
                }
            }
            None
        });
        let parsed = CliCommand::parse(&mut tokens);
        tokens.for_each(drop);
        if let Some(error) = error {
            return Self::Protocol(Err(ArgumentError::Invalid(error)));
        }
        if !implicit && format.is_none() && version.is_none() {
            return Self::Human(parsed);
        }
        match OutputFormat::parse(format.unwrap_or("ndjson")) {
            Some(OutputFormat::Human) if version.is_none() => Self::ProtocolHuman(parsed),
            Some(OutputFormat::Human) => Self::Protocol(Err(ArgumentError::Invalid(
                anyhow::anyhow!("protocol version requires ndjson"),
            ))),
            Some(OutputFormat::Ndjson) => match version {
                Some(v) if v != "1" => Self::Protocol(Err(ArgumentError::Version(v.into()))),
                _ => Self::Protocol(parsed.map_err(ArgumentError::Invalid)),
            },
            _ => Self::Protocol(Err(ArgumentError::Invalid(anyhow::anyhow!(
                "unsupported output format"
            )))),
        }
    }

    pub(super) fn execute(self) -> Result<i32> {
        match self {
            Self::Help(text) => {
                println!("{text}");
                Ok(0)
            }
            Self::Human(parsed) => human_result(parsed.and_then(CliCommand::execute)),
            Self::ProtocolHuman(parsed) => parsed.and_then(CliCommand::execute),
            Self::Machine(family, parsed) => match parsed.and_then(CliCommand::execute) {
                Ok(code) => Ok(code),
                Err(error) => {
                    write_machine_output(&family.render(&error))?;
                    Ok(1)
                }
            },
            Self::Protocol(parsed) => protocol::run(parsed),
            Self::Direct {
                name,
                parsed,
                human_errors,
            } => direct::run(name, parsed, human_errors),
            Self::Overview {
                parsed,
                human_errors,
            } => overview::run(parsed, human_errors),
            Self::VerifyEdit(parsed) => semantic_diff::run(parsed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn argv(words: &str) -> Vec<String> {
        words.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn request_parse_errors_use_family_output_and_precedence() {
        for (input, kind) in [
            ("metadata --bad --format=json", "package.error"),
            ("fetch --format=human --format ndjson", "package.error"),
            ("package unknown --format json", "package.error"),
            ("rust unknown --format ndjson", "rust.error"),
            ("doctor --bad --format=json", "doctor.error"),
            ("query rust-bridge --bad --format=json", "query.error"),
        ] {
            let args = argv(input);
            let Request::Machine(family, Err(error)) = Request::parse(&args) else {
                panic!("{input}");
            };
            assert_eq!(family.render(&error)["kind"], kind);
        }
        for (input, human) in [
            ("refs --bad --format=invalid", false),
            ("references --bad --format json --max-chars=3", true),
            ("symbol --format=human --format=ndjson", true),
            ("type --format=json --format=human", false),
            ("effects --bad --format", true),
        ] {
            let args = argv(input);
            assert!(
                matches!(Request::parse(&args), Request::Direct { parsed: Err(_), human_errors, .. } if human_errors == human),
                "{input}"
            );
        }
        for (input, expected) in [
            (
                "query --bad --direction=invalid",
                "direction must be callers or callees",
            ),
            ("check --bad --format=invalid", "unsupported output format"),
            (
                "edit --bad --protocol-version=1 --protocol-version=2",
                "duplicate protocol option",
            ),
        ] {
            let args = argv(input);
            let Request::Protocol(Err(ArgumentError::Invalid(e))) = Request::parse(&args) else {
                panic!("{input}")
            };
            assert_eq!(e.to_string(), expected);
        }
    }

    #[test]
    fn request_reads_each_argument_once_for_late_routing() {
        for n in [1, 16, 256, 1024] {
            for (prefix, repeated, suffix) in [
                ("run --", "--format=json", "--help"),
                ("check --bad", "--debug", "--protocol-version=3"),
                ("impact symbol", "--all", "--function=id"),
                ("fetch --bad", "dir", "--format=json"),
                ("impact foo --format=json", "--all", "--max-chars=1"),
                ("query --bad", "--function=f", "--byte=invalid"),
            ] {
                let args: Vec<_> = argv(prefix)
                    .into_iter()
                    .chain(std::iter::repeat_n(repeated.to_string(), n))
                    .chain(argv(suffix))
                    .collect();
                let count = Cell::new(0);
                let request =
                    Request::parse_iter(args.iter().inspect(|_| count.set(count.get() + 1)));
                assert_eq!(count.get(), args.len());
                match prefix {
                    "run --" => assert!(
                        matches!(request, Request::Human(Ok(CliCommand::Run(run))) if run.program_args.len() == n+1)
                    ),
                    "check --bad" => assert!(
                        matches!(request, Request::Protocol(Err(ArgumentError::Version(v))) if v == "3")
                    ),
                    "impact symbol" | "query --bad" => assert!(matches!(
                        request,
                        Request::Protocol(Err(ArgumentError::Invalid(_)))
                    )),
                    "fetch --bad" => assert!(matches!(request, Request::Machine(_, Err(_)))),
                    _ => assert!(matches!(
                        request,
                        Request::Direct {
                            human_errors: true,
                            ..
                        }
                    )),
                }
            }
        }
    }

    #[test]
    fn request_failures_return_to_caller() {
        for (input, code) in [
            ("version extra", 1),
            ("package verify missing-package-dir --format=json", 1),
            ("verify-edit --base", 2),
            ("refs --bad", 2),
            ("overview --depth=9", 2),
            ("build --protocol-version=9", 2),
            ("-V", 0),
        ] {
            assert_eq!(super::super::run(argv(input)), code, "{input}");
        }
    }
}
