use super::FileId;
use super::diagnostic::Diagnostic;
use super::label::{Label, LabelKind};
use super::source_map::{SourceMap, SourceMaps};

/// Emits a diagnostic against one source file.
pub fn emit(diag: &Diagnostic, map: &SourceMap) {
    emit_header(diag);
    render_file_labels(diag, map, &diag.labels);
    render_footer(diag, |file_id| (file_id == map.file_id).then_some(map));
}

/// Emits a diagnostic whose labels may refer to multiple files.
pub fn emit_multi(diag: &Diagnostic, maps: &SourceMaps) {
    emit_header(diag);

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
            render_file_labels(diag, map, &diag.labels);
        }
    }
    render_footer(diag, |file_id| maps.get(file_id));
}

fn emit_header(diag: &Diagnostic) {
    eprintln!(
        "{}[{}]: {}",
        diag.severity.as_str(),
        diag.code.as_str(),
        diag.message
    );
}

fn render_file_labels(diag: &Diagnostic, map: &SourceMap, labels: &[Label]) {
    let labels: Vec<&Label> = labels
        .iter()
        .filter(|label| label.span.file_id == map.file_id)
        .collect();
    if labels.is_empty() {
        return;
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
    eprintln!(
        " {marker} {}:{}:{}",
        map.path, location.span.line, location.span.col
    );

    let mut lines: Vec<usize> = labels
        .iter()
        .filter(|label| label.span.line > 0)
        .map(|label| label.span.line)
        .collect();
    lines.sort_unstable();
    lines.dedup();
    if lines.is_empty() {
        return;
    }

    let margin = digits(*lines.last().unwrap());
    eprintln!("{} |", " ".repeat(margin));
    let mut previous = None;
    for line in lines {
        if previous.is_some_and(|previous| line > previous + 1) {
            eprintln!("...");
        }
        previous = Some(line);
        eprintln!("{:>width$} | {}", line, map.line_text(line), width = margin);

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
            eprintln!("{} | {}{}", " ".repeat(margin), underline, message);
        }
    }
    eprintln!("{} |", " ".repeat(margin));

    // Keep this parameter in the signature so callers cannot accidentally
    // render labels without the diagnostic that owns them.
    let _ = diag;
}

fn render_footer<'a>(diag: &Diagnostic, mut map_for: impl FnMut(FileId) -> Option<&'a SourceMap>) {
    for note in &diag.notes {
        eprintln!("note: {note}");
    }
    for help in &diag.helps {
        eprintln!("help: {help}");
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
        eprintln!("{} |", " ".repeat(margin));

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
        eprintln!("{:>width$} | {}", line, fixed_line, width = margin);

        let col = fix.span.col.saturating_sub(1);
        let old_len = line_end.saturating_sub(line_start);
        let new_len = fix.replacement.len();
        let markers = if new_len >= old_len {
            " ".repeat(col) + &"+".repeat(new_len.max(1))
        } else {
            " ".repeat(col) + &"~".repeat(old_len.max(1))
        };
        eprintln!("{} | {}", " ".repeat(margin), markers);
    }
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
