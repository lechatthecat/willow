use super::diagnostic::Diagnostic;
use super::label::LabelKind;
use super::source_map::SourceMap;

/// Emits a single diagnostic to stderr in Rust-style format.
pub fn emit(diag: &Diagnostic, map: &SourceMap) {
    emit_inner(diag, map);
}

fn emit_inner(diag: &Diagnostic, map: &SourceMap) {
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

    if !lines.is_empty() {
        let max_line = *lines.last().unwrap();
        let margin = digits(max_line);

        eprintln!("{} |", " ".repeat(margin));

        let mut prev: Option<usize> = None;
        for &ln in &lines {
            if let Some(p) = prev {
                if ln > p + 1 {
                    eprintln!("...");
                }
            }
            prev = Some(ln);

            let text = map.line_text(ln);
            eprintln!("{:>width$} | {}", ln, text, width = margin);

            // underlines for every label on this line
            for label in &diag.labels {
                if label.span.line != ln || label.span.line == 0 {
                    continue;
                }
                let col = label.span.col.saturating_sub(1);
                let len = (label.span.end.saturating_sub(label.span.start)).max(1);
                let ch = if label.kind == LabelKind::Primary { '^' } else { '-' };
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

    // notes
    for note in &diag.notes {
        eprintln!("note: {}", note);
    }

    // help lines followed by optional fix-suggestion code block
    for help in &diag.helps {
        eprintln!("help: {}", help);
    }

    // fix suggestions — rendered as a code diff block
    for fix in &diag.fix_suggestions {
        if fix.span.line == 0 {
            continue;
        }
        let ln = fix.span.line;
        let margin = digits(ln);
        eprintln!("{} |", " ".repeat(margin));

        let original = map.line_text(ln);
        // Build the fixed line by splicing `replacement` into the original text.
        let line_start = fix.span.start.saturating_sub(
            // span.start is byte-absolute; compute offset within the line
            map.line_start(ln),
        );
        let line_end = fix.span.end.saturating_sub(map.line_start(ln));
        let line_end = line_end.min(original.len());
        let line_start = line_start.min(line_end);

        let fixed_line = format!(
            "{}{}{}",
            &original[..line_start],
            fix.replacement,
            &original[line_end..]
        );
        eprintln!("{:>width$} | {}", ln, fixed_line, width = margin);

        // diff markers: `+` under inserted/replaced text, `-` under removed
        let col = fix.span.col.saturating_sub(1);
        let old_len = line_end.saturating_sub(line_start);
        let new_len = fix.replacement.len();
        let markers: String = if new_len >= old_len {
            // net insertion — show `+` for the new characters
            " ".repeat(col) + &"+".repeat(new_len.max(1))
        } else {
            // net deletion — show `~` over the old extent
            " ".repeat(col) + &"~".repeat(old_len.max(1))
        };
        eprintln!("{} | {}", " ".repeat(margin), markers);
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
