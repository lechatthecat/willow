use super::diagnostic::{Diagnostic, Severity};
use super::label::LabelKind;
use super::source_map::SourceMap;

/// Emits a single diagnostic to stderr in Rust-style format.
pub fn emit(diag: &Diagnostic, map: &SourceMap) {
    let color = std::env::var("NO_COLOR").is_err();
    emit_inner(diag, map, color);
}

fn emit_inner(diag: &Diagnostic, map: &SourceMap, _color: bool) {
    // header: error[E0001]: message
    let severity = diag.severity.as_str();
    let code = diag.code.as_str();
    eprintln!("{}[{}]: {}", severity, code, diag.message);

    // find primary label for the --> line
    let primary = diag.labels.iter().find(|l| l.kind == LabelKind::Primary);
    if let Some(p) = primary {
        eprintln!(" --> {}:{}:{}", map.path, p.span.line, p.span.col);
    }

    // collect all affected lines
    let mut lines: Vec<usize> = diag
        .labels
        .iter()
        .filter(|l| l.span.line > 0)
        .map(|l| l.span.line)
        .collect();
    lines.sort_unstable();
    lines.dedup();

    if lines.is_empty() {
        // no source context — still print notes/helps
    } else {
        let max_line = *lines.last().unwrap();
        let margin = digits(max_line);

        eprintln!("{} |", " ".repeat(margin));

        let mut prev: Option<usize> = None;
        for &ln in &lines {
            // print "..." gap if lines are not consecutive
            if let Some(p) = prev {
                if ln > p + 1 {
                    eprintln!("...");
                }
            }
            prev = Some(ln);

            let text = map.line_text(ln);
            eprintln!("{:>width$} | {}", ln, text, width = margin);

            // print all label underlines for this line
            for label in &diag.labels {
                if label.span.line != ln {
                    continue;
                }
                if label.span.line == 0 {
                    continue;
                }

                let col = label.span.col.saturating_sub(1); // 0-indexed visual offset
                let len = (label.span.end.saturating_sub(label.span.start)).max(1);
                let ch = if label.kind == LabelKind::Primary {
                    '^'
                } else {
                    '-'
                };
                let underline = " ".repeat(col) + &ch.to_string().repeat(len);
                let msg_part = if label.message.is_empty() {
                    String::new()
                } else {
                    format!(" {}", label.message)
                };
                eprintln!("{} | {}{}", " ".repeat(margin), underline, msg_part);
            }
        }

        eprintln!("{} |", " ".repeat(margin));
    }

    // help lines
    for help in &diag.helps {
        eprintln!("help: {}", help);
        // if help contains a code suggestion it's just printed as text for now
    }

    // notes
    for note in &diag.notes {
        let prefix = if diag.severity == Severity::Error {
            "note"
        } else {
            "note"
        };
        eprintln!("{}: {}", prefix, note);
    }
}

/// Emits all diagnostics in a slice.
pub fn emit_all(diags: &[Diagnostic], map: &SourceMap) {
    for d in diags {
        emit(d, map);
    }
}

fn digits(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut d = 0;
    let mut x = n;
    while x > 0 {
        d += 1;
        x /= 10;
    }
    d
}
