use std::io::{self, Write};

use super::FileId;
use super::diagnostic::Diagnostic;
use super::label::{Label, LabelKind};
use super::source_map::{SourceLookup, SourceMap, SourceMaps};

/// Emits a diagnostic against one source file.
pub fn emit(diag: &Diagnostic, map: &SourceMap) {
    let mut stderr = io::stderr().lock();
    // Preserve the legacy infallible API; request-local callers use emit_with.
    emit_single(diag, map, &mut stderr).expect("failed to write diagnostic");
}

fn emit_single(diag: &Diagnostic, map: &SourceMap, writer: &mut dyn Write) -> io::Result<()> {
    emit_header(diag, writer)?;
    render_file_labels(diag, map, &diag.labels, writer)?;
    render_footer(
        diag,
        |file_id| (file_id == map.file_id).then_some(map),
        writer,
    )
}

/// Emits a diagnostic whose labels may refer to multiple files.
pub fn emit_multi(diag: &Diagnostic, maps: &SourceMaps) {
    emit_with(diag, maps, &mut io::stderr().lock()).expect("failed to write diagnostic");
}

/// Render to a caller-owned destination, stopping on the first write failure.
/// The caller owns buffering and flushing of the destination.
pub fn emit_with(
    diag: &Diagnostic,
    maps: &dyn SourceLookup,
    writer: &mut dyn Write,
) -> io::Result<()> {
    emit_header(diag, writer)?;

    let mut file_ids: Vec<FileId> = diag
        .labels
        .iter()
        .filter(|label| label.span.line > 0)
        .map(|label| label.span.file_id)
        .collect();
    file_ids.sort_unstable();
    file_ids.dedup();

    for file_id in file_ids {
        if let Some(map) = maps.get(file_id) {
            render_file_labels(diag, map, &diag.labels, writer)?;
        }
    }
    render_footer(diag, |file_id| maps.get(file_id), writer)
}

fn emit_header(diag: &Diagnostic, writer: &mut dyn Write) -> io::Result<()> {
    writeln!(
        writer,
        "{}[{}]: {}",
        diag.severity.as_str(),
        diag.code.as_str(),
        diag.message
    )?;
    Ok(())
}

fn render_file_labels(
    diag: &Diagnostic,
    map: &SourceMap,
    labels: &[Label],
    writer: &mut dyn Write,
) -> io::Result<()> {
    let labels: Vec<&Label> = labels
        .iter()
        .filter(|label| label.span.file_id == map.file_id)
        .collect();
    if labels.is_empty() {
        return Ok(());
    }

    let location = labels
        .iter()
        .find(|label| label.kind == LabelKind::Primary)
        .copied()
        .unwrap_or(labels[0]);
    let marker = if location.kind == LabelKind::Primary {
        "-->"
    } else {
        ":::"
    };
    writeln!(
        writer,
        " {marker} {}:{}:{}",
        map.path, location.span.line, location.span.col
    )?;

    let mut lines: Vec<usize> = labels
        .iter()
        .filter(|label| label.span.line > 0)
        .map(|label| label.span.line)
        .collect();
    lines.sort_unstable();
    lines.dedup();
    if lines.is_empty() {
        return Ok(());
    }

    let margin = digits(*lines.last().unwrap());
    writeln!(writer, "{} |", " ".repeat(margin))?;
    let mut previous = None;
    for line in lines {
        if previous.is_some_and(|previous| line > previous + 1) {
            writeln!(writer, "...")?;
        }
        previous = Some(line);
        writeln!(
            writer,
            "{:>width$} | {}",
            line,
            map.line_text(line),
            width = margin
        )?;

        for label in &labels {
            if label.span.line != line {
                continue;
            }
            let col = label.span.col.saturating_sub(1);
            let len = label.span.end.saturating_sub(label.span.start).max(1);
            let character = if label.kind == LabelKind::Primary {
                '^'
            } else {
                '-'
            };
            let underline = " ".repeat(col) + &character.to_string().repeat(len);
            let message = if label.message.is_empty() {
                String::new()
            } else {
                format!(" {}", label.message)
            };
            writeln!(writer, "{} | {}{}", " ".repeat(margin), underline, message)?;
        }
    }
    writeln!(writer, "{} |", " ".repeat(margin))?;

    // Keep this parameter in the signature so callers cannot accidentally
    // render labels without the diagnostic that owns them.
    let _ = diag;
    Ok(())
}

fn render_footer<'a>(
    diag: &Diagnostic,
    mut map_for: impl FnMut(FileId) -> Option<&'a SourceMap>,
    writer: &mut dyn Write,
) -> io::Result<()> {
    for note in &diag.notes {
        writeln!(writer, "note: {note}")?;
    }
    for help in &diag.helps {
        writeln!(writer, "help: {help}")?;
    }

    for fix in &diag.fix_suggestions {
        if fix.span.line == 0 {
            continue;
        }
        let Some(map) = map_for(fix.span.file_id) else {
            continue;
        };
        let line = fix.span.line;
        let margin = digits(line);
        writeln!(writer, "{} |", " ".repeat(margin))?;

        let original = map.line_text(line);
        let line_start = fix
            .span
            .start
            .saturating_sub(map.line_start(line))
            .min(original.len());
        let line_end = fix
            .span
            .end
            .saturating_sub(map.line_start(line))
            .min(original.len())
            .max(line_start);
        let fixed_line = format!(
            "{}{}{}",
            &original[..line_start],
            fix.replacement,
            &original[line_end..]
        );
        writeln!(writer, "{:>width$} | {}", line, fixed_line, width = margin)?;

        let col = fix.span.col.saturating_sub(1);
        let old_len = line_end.saturating_sub(line_start);
        let new_len = fix.replacement.len();
        let markers = if new_len >= old_len {
            " ".repeat(col) + &"+".repeat(new_len.max(1))
        } else {
            " ".repeat(col) + &"~".repeat(old_len.max(1))
        };
        writeln!(writer, "{} | {}", " ".repeat(margin), markers)?;
    }
    Ok(())
}

pub fn emit_all(diags: &[Diagnostic], map: &SourceMap) {
    for diagnostic in diags {
        emit(diagnostic, map);
    }
}

pub fn emit_all_multi(diags: &[Diagnostic], maps: &SourceMaps) {
    for diagnostic in diags {
        emit_multi(diagnostic, maps);
    }
}

fn digits(mut number: usize) -> usize {
    if number == 0 {
        return 1;
    }
    let mut digits = 0;
    while number > 0 {
        digits += 1;
        number /= 10;
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{ErrorCode, Label, Severity, Span};

    fn rendered_fixture() -> (Diagnostic, SourceMaps) {
        let mut maps = SourceMaps::new(SourceMap::new("entry.wi", "bad"));
        maps.insert(SourceMap::with_file_id(FileId(1), "module.wi", "old"));
        let diagnostic = Diagnostic::new(Severity::Error, ErrorCode::E0001, "example")
            .with_label(Label::primary(Span::new(0, 3, 1, 1), "primary"))
            .with_label(Label::secondary(
                Span::in_file(FileId(1), 0, 3, 1, 1),
                "related",
            ))
            .with_note("a note")
            .with_help("a help")
            .with_fix(crate::diagnostics::FixSuggestion::new(
                Span::in_file(FileId(1), 0, 3, 1, 1),
                "new",
                "replace",
            ));
        (diagnostic, maps)
    }

    #[test]
    fn writable_renderer_preserves_human_output() {
        let (diagnostic, maps) = rendered_fixture();
        let mut output = Vec::new();
        emit_with(&diagnostic, &maps, &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            concat!(
                "error[E0001]: example\n",
                " --> entry.wi:1:1\n",
                "  |\n1 | bad\n  | ^^^ primary\n  |\n",
                " ::: module.wi:1:1\n",
                "  |\n1 | old\n  | --- related\n  |\n",
                "note: a note\nhelp: a help\n",
                "  |\n1 | new\n  | +++\n",
            )
        );
    }

    #[test]
    fn every_output_boundary_propagates_failure_without_retrying() {
        struct Failing {
            remaining: usize,
            failures: usize,
        }
        impl Write for Failing {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.remaining == 0 {
                    self.failures += 1;
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                let written = bytes.len().min(1);
                self.remaining -= written;
                Ok(written)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (diagnostic, maps) = rendered_fixture();
        let mut expected = Vec::new();
        emit_with(&diagnostic, &maps, &mut expected).unwrap();
        for remaining in 0..expected.len() {
            let mut output = Failing {
                remaining,
                failures: 0,
            };
            assert_eq!(
                emit_with(&diagnostic, &maps, &mut output)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::BrokenPipe
            );
            assert_eq!(output.failures, 1);
        }
        for n in [16, 64, 256] {
            let mut output = Failing {
                remaining: n * expected.len(),
                failures: 0,
            };
            for _ in 0..n {
                emit_with(&diagnostic, &maps, &mut output).unwrap();
            }
            assert_eq!(output.remaining, 0);
            assert_eq!(output.failures, 0);
            eprintln!("renderer-count n={n} bytes={}", n * expected.len());
        }
    }

    #[test]
    fn source_maps_select_files_by_span_identity() {
        let mut maps = SourceMaps::new(SourceMap::new("entry.wi", "fn main() {}"));
        maps.insert(SourceMap::with_file_id(
            FileId(1),
            "module.wi",
            "fn helper() {}",
        ));
        let diagnostic = Diagnostic::new(Severity::Error, ErrorCode::E0001, "cross-file")
            .with_label(Label::primary(Span::new(0, 2, 1, 1), "entry"))
            .with_label(Label::secondary(
                Span::in_file(FileId(1), 0, 2, 1, 1),
                "module",
            ));
        assert_eq!(diagnostic.labels[0].span.file_id, FileId::ENTRY);
        assert_eq!(diagnostic.labels[1].span.file_id, FileId(1));
        assert_eq!(
            maps.get(diagnostic.labels[1].span.file_id).unwrap().path,
            "module.wi"
        );
    }
}
