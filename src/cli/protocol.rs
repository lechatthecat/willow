//! Versioned command adapter. Semantic work belongs to willow_compiler.
use super::CliCommand;
use serde_json::{Value, json};
use std::io::{self, Write};
use willow_compiler::diagnostics::{
    Diagnostic, DiagnosticEmitter, Severity, source_map::SourceLookup,
};

pub(super) const CHECK_COMMAND: &str = "willow check . --format ndjson --protocol-version 1";
pub(super) const BUILD_COMMAND: &str = "willow build . --format ndjson --protocol-version 1";

pub(super) fn requested(args: &[String]) -> bool {
    // Existing package commands retain their own output contract.
    if matches!(
        args.first().map(String::as_str),
        Some(
            "agent"
                | "init"
                | "fetch"
                | "package"
                | "add"
                | "remove"
                | "update"
                | "deps"
                | "metadata"
        )
    ) {
        return false;
    }
    matches!(
        args.first().map(String::as_str),
        Some("impact" | "snapshot" | "risk" | "query" | "edit" | "daemon")
    ) || args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| {
            arg == "--format"
                || arg.starts_with("--format=")
                || arg == "--protocol-version"
                || arg.starts_with("--protocol-version=")
        })
}

pub(super) struct EventWriter<W> {
    writer: W,
    stream_id: String,
    seq: u64,
    errors: usize,
    failed: bool,
}

impl<W: Write> EventWriter<W> {
    fn new(writer: W) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            writer,
            stream_id: format!(
                "{}-{nanos}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            seq: 0,
            errors: 0,
            failed: false,
        }
    }

    pub(super) fn event(&mut self, event: &str, code: &str, data: Value) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "event stream previously failed",
            ));
        }
        self.failed = true;
        serde_json::to_writer(
            &mut self.writer,
            &json!({
                "schema_version": 1, "stream_id": self.stream_id, "seq": self.seq,
                "event": event, "code": code, "data": data
            }),
        )?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        self.failed = false;
        self.seq += 1;
        Ok(())
    }
}

impl<W: Write> DiagnosticEmitter for EventWriter<W> {
    fn emit(&mut self, diagnostic: &Diagnostic, sources: &dyn SourceLookup) -> io::Result<()> {
        let labels: Vec<_> = diagnostic
            .labels
            .iter()
            .map(|label| {
                json!({
                    "span": label.span, "kind": label.kind, "message": label.message,
                    "path": sources.get(label.span.file_id).map(|source| &source.path)
                })
            })
            .collect();
        let fixes: Vec<_> = diagnostic
            .fix_suggestions
            .iter()
            .map(|fix| {
                json!({
                    "span": fix.span, "replacement": fix.replacement, "message": fix.message,
                    "path": sources.get(fix.span.file_id).map(|source| &source.path)
                })
            })
            .collect();
        self.event(
            "diagnostic",
            diagnostic.code.as_str(),
            json!({
                "severity": diagnostic.severity.as_str(), "message": diagnostic.message,
                "labels": labels, "notes": diagnostic.notes, "helps": diagnostic.helps,
                "fix_suggestions": fixes
            }),
        )?;
        self.errors += usize::from(diagnostic.severity == Severity::Error);
        Ok(())
    }
}

pub(super) fn options(args: Vec<String>) -> anyhow::Result<(Vec<String>, String, Option<String>)> {
    let mut remaining = Vec::with_capacity(args.len());
    let mut format = None;
    let mut version = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            remaining.push(arg);
            remaining.extend(args);
            break;
        }
        let (target, value) = if arg == "--format" {
            (
                &mut format,
                args.next()
                    .ok_or_else(|| anyhow::anyhow!("missing --format value"))?,
            )
        } else if let Some(value) = arg.strip_prefix("--format=") {
            (&mut format, value.to_owned())
        } else if arg == "--protocol-version" {
            (
                &mut version,
                args.next()
                    .ok_or_else(|| anyhow::anyhow!("missing --protocol-version value"))?,
            )
        } else if let Some(value) = arg.strip_prefix("--protocol-version=") {
            (&mut version, value.to_owned())
        } else {
            remaining.push(arg);
            continue;
        };
        anyhow::ensure!(target.replace(value).is_none(), "duplicate protocol option");
    }
    let format = format.unwrap_or_else(|| "ndjson".into());
    anyhow::ensure!(
        matches!(format.as_str(), "human" | "ndjson"),
        "unsupported output format"
    );
    anyhow::ensure!(
        format != "human" || version.is_none(),
        "protocol version requires ndjson"
    );
    Ok((remaining, format, version))
}

pub(super) fn run(args: Vec<String>) -> anyhow::Result<i32> {
    let parsed = options(args);
    if let Ok((args, format, _)) = &parsed
        && format == "human"
    {
        CliCommand::parse(args)?.execute()?;
        return Ok(0);
    }
    let mut events = EventWriter::new(io::stdout().lock());
    events.event(
        "request.started",
        "WT0001",
        json!({"compiler_version": env!("CARGO_PKG_VERSION")}),
    )?;
    let (code, exit_code, message) = match parsed {
        Err(error) => ("WT1001", 2, error.to_string()),
        Ok((_, _, Some(version))) if version != "1" => (
            "WT1002",
            2,
            format!("unsupported protocol version {version}"),
        ),
        Ok((args, _, _)) if args.first().is_some_and(|a| a == "daemon") => {
            match super::daemon::serve(&args[1..], &mut events) {
                Ok(()) => ("WT0000", 0, String::new()),
                Err(error) => ("WT2002", 1, format!("{error:#}")),
            }
        }
        Ok((args, _, _)) => match CliCommand::parse(&args) {
            Err(error) => ("WT1001", 2, error.to_string()),
            Ok(CliCommand::Edit(command)) => match command.execute(&mut events) {
                Ok(value) => {
                    events.event("analysis.result", "WT0010", value)?;
                    ("WT0000", 0, String::new())
                }
                Err(error) => ("WT2002", 1, format!("{error:#}")),
            },
            Ok(CliCommand::Analysis(command)) => match command.execute(&mut events) {
                Ok(value) => {
                    events.event("analysis.result", "WT0010", value)?;
                    ("WT0000", 0, String::new())
                }
                Err(error) => (
                    if events.errors > 0 {
                        "WT2001"
                    } else {
                        "WT2002"
                    },
                    1,
                    format!("{error:#}"),
                ),
            },
            Ok(command) => {
                let operation = match command {
                    CliCommand::Check(command) => Some((command, false)),
                    CliCommand::Build(command) if !command.emit_hir && !command.emit_lir => {
                        Some((command, true))
                    }
                    _ => None,
                };
                match operation {
                    None => (
                        "WT1001",
                        2,
                        "ndjson supports check/build without IR dumps".into(),
                    ),
                    Some((command, build)) => match command.execute_with(&mut events, build) {
                        Ok(()) => ("WT0000", 0, String::new()),
                        Err(error) => (
                            if events.errors > 0 {
                                "WT2001"
                            } else {
                                "WT2002"
                            },
                            1,
                            format!("{error:#}"),
                        ),
                    },
                }
            }
        },
    };
    events.event("request.finished", code, json!({"status": if exit_code == 0 { "ok" } else { "error" }, "exit_code": exit_code, "message": message}))?;
    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use willow_compiler::diagnostics::{ErrorCode, FileId, FixSuggestion, Label, SourceMap, Span};

    #[derive(Default)]
    struct CountWriter {
        bytes: usize,
        writes: usize,
        flushes: usize,
    }
    impl Write for CountWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes += bytes.len();
            self.writes += 1;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }
    struct CountSources {
        source: SourceMap,
        lookups: Cell<usize>,
    }
    impl SourceLookup for CountSources {
        fn get(&self, id: FileId) -> Option<&SourceMap> {
            self.lookups.set(self.lookups.get() + 1);
            (id == self.source.file_id).then_some(&self.source)
        }
    }

    #[test]
    fn protocol_scaling_counts_labels_fixes_and_events_once() {
        for n in [16, 64, 256, 1024] {
            let sources = CountSources {
                source: SourceMap::new("x.wi", "x"),
                lookups: Cell::new(0),
            };
            let span = Span::new(0, 1, 1, 1);
            let mut diagnostic = Diagnostic::new(Severity::Error, ErrorCode::E0001, "bad");
            diagnostic.labels = vec![Label::primary(span, "bad"); n];
            diagnostic.fix_suggestions = vec![FixSuggestion::new(span, "y", "fix"); n];
            let mut writer = EventWriter::new(CountWriter::default());
            writer.stream_id = "test".into();
            writer.emit(&diagnostic, &sources).unwrap();
            assert_eq!(sources.lookups.get(), 2 * n);
            assert_eq!(writer.writer.flushes, 1);
            println!(
                "labels={n} fixes={n} lookups={} writes={} bytes={}",
                sources.lookups.get(),
                writer.writer.writes,
                writer.writer.bytes
            );
            let mut events = EventWriter::new(CountWriter::default());
            for _ in 0..n {
                events.event("event", "WT0001", json!({})).unwrap();
            }
            assert_eq!(events.seq, n as u64);
            assert_eq!(events.writer.flushes, n);
            println!(
                "events={n} flushes={} writes={}",
                events.writer.flushes, events.writer.writes
            );
        }
    }

    struct FailAfter {
        remaining: usize,
        flush_fails: bool,
    }
    impl Write for FailAfter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let n = bytes.len().min(self.remaining);
            self.remaining -= n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.flush_fails {
                Err(io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn partial_writes_and_flush_failures_poison_the_stream() {
        let mut good = EventWriter::new(Vec::new());
        good.stream_id = "test".into();
        good.event(
            "event",
            "WT0001",
            json!({"message": "quote \" newline\n日本語"}),
        )
        .unwrap();
        let parsed: Value = serde_json::from_slice(&good.writer).unwrap();
        assert_eq!(parsed["data"]["message"], "quote \" newline\n日本語");
        assert_eq!(good.writer.iter().filter(|&&b| b == b'\n').count(), 1);
        for bytes in 0..good.writer.len() {
            let mut writer = EventWriter::new(FailAfter {
                remaining: bytes,
                flush_fails: false,
            });
            writer.stream_id = "test".into();
            assert!(
                writer
                    .event("event", "WT0001", parsed["data"].clone())
                    .is_err()
            );
            assert_eq!(writer.seq, 0);
            writer.writer.remaining = usize::MAX;
            assert!(
                writer
                    .event("request.finished", "WT0000", json!({}))
                    .is_err()
            );
        }
        let mut writer = EventWriter::new(FailAfter {
            remaining: usize::MAX,
            flush_fails: true,
        });
        assert!(writer.event("event", "WT0001", json!({})).is_err());
        assert_eq!(writer.seq, 0);
        assert!(writer.failed);
    }

    #[test]
    fn app_arguments_do_not_select_protocol_mode() {
        let args = [
            "run",
            "main.wi",
            "--",
            "--format=ndjson",
            "--protocol-version=99",
        ]
        .map(str::to_owned);
        assert!(!requested(&args));
    }
}
