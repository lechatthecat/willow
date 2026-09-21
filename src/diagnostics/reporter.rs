use std::collections::HashMap;
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
    if let Some(labels) = group_labels(&diag.labels).get(&map.file_id) {
        render_file_labels(map, labels, writer)?;
    }
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

    let mut files: Vec<_> = group_labels(&diag.labels).into_iter().collect();
    files.sort_unstable_by_key(|(file_id, _)| *file_id);
    for (file_id, labels) in files {
        // Legacy multi-file output omits files with only line-zero labels.
        if !labels.lines.is_empty()
            && let Some(map) = maps.get(file_id)
        {
            render_file_labels(map, &labels, writer)?;
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

struct FileLabels<'a> {
    location: &'a Label,
    lines: HashMap<usize, Vec<&'a Label>>,
}

fn group_labels(labels: &[Label]) -> HashMap<FileId, FileLabels<'_>> {
    let mut files = HashMap::<FileId, FileLabels<'_>>::new();
    for label in labels {
        #[cfg(test)]
        LABEL_PROBES.with(|count| count.set(count.get() + 1));
        let file = files
            .entry(label.span.file_id)
            .or_insert_with(|| FileLabels {
                location: label,
                lines: HashMap::new(),
            });
        // First primary wins, including line-zero labels used for the location.
        if file.location.kind != LabelKind::Primary && label.kind == LabelKind::Primary {
            file.location = label;
        }
        if label.span.line > 0 {
            file.lines.entry(label.span.line).or_default().push(label);
        }
    }
    files
}

#[cfg(test)]
thread_local! {
    static LABEL_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn render_file_labels(
    map: &SourceMap,
    labels: &FileLabels<'_>,
    writer: &mut dyn Write,
) -> io::Result<()> {
    let location = labels.location;
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

    let mut lines: Vec<_> = labels.lines.iter().collect();
    lines.sort_unstable_by_key(|(line, _)| **line);
    let Some((last_line, _)) = lines.last() else {
        return Ok(());
    };
    let margin = digits(**last_line);
    writeln!(writer, "{} |", " ".repeat(margin))?;
    let mut previous = None;
    for (&line, line_labels) in lines {
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

        for label in line_labels {
            #[cfg(test)]
            LABEL_PROBES.with(|count| count.set(count.get() + 1));
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
    fn label_grouping_preserves_order_and_zero_line_locations() {
        let mut maps = SourceMaps::new(SourceMap::new("entry.wi", "a\nb\nc"));
        maps.insert(SourceMap::with_file_id(FileId(1), "other.wi", "z"));
        let diagnostic = Diagnostic::new(Severity::Error, ErrorCode::E0001, "order")
            .with_label(Label::secondary(
                Span::in_file(FileId(1), 0, 1, 0, 2),
                "hidden",
            ))
            .with_label(Label::secondary(Span::new(4, 5, 3, 1), "third"))
            .with_label(Label::primary(Span::new(0, 0, 0, 2), "location"))
            .with_label(Label::secondary(Span::new(0, 1, 1, 1), "first"))
            .with_label(Label::primary(Span::new(0, 1, 1, 1), "second"))
            .with_label(Label::primary(
                Span::in_file(FileId(99), 0, 1, 1, 1),
                "missing",
            ));
        let mut output = Vec::new();
        emit_with(&diagnostic, &maps, &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            concat!(
                "error[E0001]: order\n --> entry.wi:0:2\n",
                "  |\n1 | a\n  | - first\n  | ^ second\n...\n",
                "3 | c\n  | - third\n  |\n",
            )
        );
        let mut single = Vec::new();
        emit_single(&diagnostic, maps.get(FileId(1)).unwrap(), &mut single).unwrap();
        assert_eq!(
            String::from_utf8(single).unwrap(),
            "error[E0001]: order\n ::: other.wi:0:2\n"
        );
    }

    #[test]
    fn label_probes_scale_linearly() {
        for shape in ["files", "lines", "same-line"] {
            for n in [16, 64, 256, 1024] {
                let mut maps = SourceMaps::new(SourceMap::new("entry.wi", "x\n".repeat(n)));
                let mut diagnostic = Diagnostic::new(Severity::Error, ErrorCode::E0001, "scale");
                for i in (0..n).rev() {
                    let file = if shape == "files" {
                        FileId(i as u32)
                    } else {
                        FileId::ENTRY
                    };
                    if shape == "files" {
                        maps.insert(SourceMap::with_file_id(file, "file.wi", "x"));
                    }
                    let line = if shape == "lines" { i + 1 } else { 1 };
                    diagnostic
                        .labels
                        .push(Label::primary(Span::in_file(file, 0, 1, line, 1), ""));
                }
                LABEL_PROBES.with(|count| count.set(0));
                emit_with(&diagnostic, &maps, &mut io::sink()).unwrap();
                let probes = LABEL_PROBES.with(|count| count.get());
                assert_eq!(probes, 2 * n);
                eprintln!("label-probes shape={shape} n={n} probes={probes}");
            }
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
