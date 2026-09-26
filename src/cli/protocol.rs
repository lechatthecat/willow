//! Versioned command adapter. Semantic work belongs to willow_compiler.
use super::CliCommand;
use serde_json::{Value, json};
use std::io::{self, Write};
use willow_compiler::diagnostics::{
    Diagnostic, DiagnosticEmitter, FileId, Severity, source_map::SourceLookup,
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
    /// Diagnostics of the current batch, held until the next other event so
    /// roots can be written before the cascades they cause.
    pending: Vec<Pending>,
}

struct Pending {
    code: &'static str,
    error: bool,
    /// File and line of the first label.
    at: Option<(FileId, usize)>,
    data: Value,
}

/// Lexer (E005x) and parser (E010x) codes: recovery after the first one in a
/// file is unreliable, so later errors in that file are likely consequences.
fn syntax(code: &str) -> bool {
    code.starts_with("E005") || code.starts_with("E010")
}

/// For each diagnostic, the index of the likely root it cascades from.
/// Heuristic: later errors in a file with a syntax error cascade from that
/// file's first syntax error; a type error (E02xx) on the line of an earlier
/// name-resolution error (E035x) cascades from it. Warnings are never cascades.
fn causes(pending: &[Pending]) -> Vec<Option<usize>> {
    let mut syntax_roots = std::collections::HashMap::new();
    let mut unresolved = std::collections::HashMap::new();
    let mut causes = Vec::with_capacity(pending.len());
    for (index, item) in pending.iter().enumerate() {
        let file = item.at.map(|(file, _)| file);
        let cause = if !item.error {
            None
        } else if let Some(&root) = syntax_roots.get(&file) {
            Some(root)
        } else if syntax(item.code) {
            syntax_roots.insert(file, index);
            None
        } else if item.code.starts_with("E02") {
            item.at.and_then(|at| unresolved.get(&at).copied())
        } else {
            if item.code.starts_with("E035")
                && let Some(at) = item.at
            {
                unresolved.entry(at).or_insert(index);
            }
            None
        };
        causes.push(cause);
    }
    causes
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
            pending: Vec::new(),
        }
    }

    pub(super) fn event(&mut self, event: &str, code: &str, data: Value) -> io::Result<()> {
        self.flush_diagnostics()?;
        self.write(event, code, data)
    }

    /// Writes buffered diagnostics, roots first, marking each cascade with the
    /// `seq` of its root.
    pub(super) fn flush_diagnostics(&mut self) -> io::Result<()> {
        let pending = std::mem::take(&mut self.pending);
        let causes = causes(&pending);
        let mut seqs = vec![None; pending.len()];
        let (roots, cascades): (Vec<_>, Vec<_>) = pending
            .into_iter()
            .enumerate()
            .partition(|(index, _)| causes[*index].is_none());
        for (index, mut item) in roots.into_iter().chain(cascades) {
            let root = causes[index].and_then(|root| seqs[root]);
            item.data["cascade"] = json!(root.is_some());
            item.data["root_cause"] = json!(root);
            seqs[index] = Some(self.seq);
            self.write("diagnostic", item.code, item.data)?;
        }
        Ok(())
    }

    fn write(&mut self, event: &str, code: &str, data: Value) -> io::Result<()> {
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
        self.pending.push(Pending {
            code: diagnostic.code.as_str(),
            error: diagnostic.severity == Severity::Error,
            at: diagnostic
                .labels
                .first()
                .map(|label| (label.span.file_id, label.span.line)),
            data: json!({
                "severity": diagnostic.severity.as_str(), "message": diagnostic.message,
                "labels": labels, "notes": diagnostic.notes, "helps": diagnostic.helps,
                "fix_suggestions": fixes
            }),
        });
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
    let mut location = None;
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
                Err(error) => {
                    location = error
                        .downcast_ref::<willow_compiler::ai::edit::Rejection>()
                        .map(|rejection| rejection.location.clone());
                    ("WT2002", 1, format!("{error:#}"))
                }
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
    let mut data = json!({"status": if exit_code == 0 { "ok" } else { "error" }, "exit_code": exit_code, "message": message});
    if let Some(location) = location {
        data["location"] = serde_json::to_value(location)?;
    }
    events.event("request.finished", code, data)?;
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
            writer.flush_diagnostics().unwrap();
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

    fn pending(code: ErrorCode, severity: Severity, file: u32, line: usize) -> Diagnostic {
        let mut diagnostic = Diagnostic::new(severity, code, "x");
        let mut span = Span::new(0, 1, line, 1);
        span.file_id = FileId(file);
        diagnostic.labels = vec![Label::primary(span, "x")];
        diagnostic
    }

    fn written(diagnostics: &[Diagnostic]) -> Vec<Value> {
        let sources = CountSources {
            source: SourceMap::new("x.wi", "x"),
            lookups: Cell::new(0),
        };
        let mut writer = EventWriter::new(Vec::new());
        for diagnostic in diagnostics {
            writer.emit(diagnostic, &sources).unwrap();
        }
        assert!(writer.writer.is_empty(), "diagnostics wait for the batch");
        writer
            .event("request.finished", "WT2001", json!({}))
            .unwrap();
        String::from_utf8(writer.writer)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn cascades_are_written_after_roots_and_name_their_root_seq() {
        use ErrorCode::*;
        let events = written(&[
            pending(E0101, Severity::Error, 0, 3),
            pending(E0102, Severity::Error, 0, 3),
            pending(E0350, Severity::Error, 1, 1),
            pending(E0201, Severity::Error, 1, 1),
            pending(E0201, Severity::Error, 1, 2),
            pending(E0205, Severity::Error, 0, 9),
            pending(E0051, Severity::Error, 2, 4),
            pending(E0101, Severity::Error, 2, 5),
            pending(E0350, Severity::Warning, 0, 1),
        ]);
        let summary: Vec<_> = events
            .iter()
            .map(|v| {
                (
                    v["seq"].as_u64().unwrap(),
                    v["code"].as_str().unwrap(),
                    v["data"]["cascade"].as_bool(),
                    v["data"]["root_cause"].as_u64(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (0, "E0101", Some(false), None),
                (1, "E0350", Some(false), None),
                (2, "E0201", Some(false), None),
                (3, "E0051", Some(false), None),
                (4, "E0350", Some(false), None),
                (5, "E0102", Some(true), Some(0)),
                (6, "E0201", Some(true), Some(1)),
                (7, "E0205", Some(true), Some(0)),
                (8, "E0101", Some(true), Some(3)),
                (9, "WT2001", None, None),
            ]
        );
    }

    #[test]
    fn a_batch_ends_at_each_other_event() {
        let sources = CountSources {
            source: SourceMap::new("x.wi", "x"),
            lookups: Cell::new(0),
        };
        let mut writer = EventWriter::new(Vec::new());
        writer
            .emit(&pending(ErrorCode::E0101, Severity::Error, 0, 1), &sources)
            .unwrap();
        writer
            .event("analysis.result", "WT0010", json!({}))
            .unwrap();
        writer
            .emit(&pending(ErrorCode::E0102, Severity::Error, 0, 1), &sources)
            .unwrap();
        writer
            .event("analysis.result", "WT0010", json!({}))
            .unwrap();
        assert_eq!(writer.errors, 2);
        let events: Vec<Value> = String::from_utf8(writer.writer)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        // A new batch (e.g. a daemon refresh) starts with fresh roots.
        assert_eq!(events[2]["code"], "E0102");
        assert_eq!(events[2]["data"]["cascade"], false);
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
