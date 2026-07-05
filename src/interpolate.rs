//! Format-string interpolation spec parsing (willow-csax), shared by the type
//! checker (placeholder counting + argument type validation) and the Cranelift
//! backend (message assembly), so the two can never drift.
//!
//! Grammar: `{}` interpolates the next argument by its type; the f64 precision
//! forms `{:.17g}`, `{:.16f}`, `{:.6f}` interpolate an f64 with fixed
//! formatting; `{{` and `}}` are literal braces.

/// One piece of a parsed format string.
#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    /// Literal text (brace escapes already resolved).
    Literal(String),
    /// `{}` — display the next argument by its type.
    Display,
    /// `{:.17g}` / `{:.16f}` / `{:.6f}` — f64-only precision formats.
    F64(F64Format),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum F64Format {
    G17,
    F16,
    F6,
}

impl F64Format {
    pub fn runtime_symbol(self) -> &'static str {
        match self {
            F64Format::G17 => "willow_format_f64_17g",
            F64Format::F16 => "willow_format_f64_16f",
            F64Format::F6 => "willow_format_f64_6f",
        }
    }
}

/// Parse a format spec into segments. Errors describe the malformed piece.
pub fn parse_spec(spec: &str) -> Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut chars = spec.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '{' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    literal.push('{');
                    continue;
                }
                // Collect up to the closing `}`.
                let mut inner = String::new();
                let mut closed = false;
                for ic in chars.by_ref() {
                    if ic == '}' {
                        closed = true;
                        break;
                    }
                    inner.push(ic);
                }
                if !closed {
                    return Err(
                        "unterminated `{` placeholder (use `{{` for a literal brace)".to_string(),
                    );
                }
                if !literal.is_empty() {
                    segments.push(Segment::Literal(std::mem::take(&mut literal)));
                }
                match inner.as_str() {
                    "" => segments.push(Segment::Display),
                    ":.17g" => segments.push(Segment::F64(F64Format::G17)),
                    ":.16f" => segments.push(Segment::F64(F64Format::F16)),
                    ":.6f" => segments.push(Segment::F64(F64Format::F6)),
                    other => {
                        return Err(format!(
                            "unsupported placeholder `{{{other}}}` (supported: `{{}}`, \
                             `{{:.17g}}`, `{{:.16f}}`, `{{:.6f}}`)"
                        ));
                    }
                }
            }
            '}' => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                    literal.push('}');
                } else {
                    return Err("stray `}` (use `}}` for a literal closing brace)".to_string());
                }
            }
            other => literal.push(other),
        }
    }
    if !literal.is_empty() {
        segments.push(Segment::Literal(literal));
    }
    Ok(segments)
}

/// Number of argument-consuming placeholders in parsed segments.
pub fn placeholder_count(segments: &[Segment]) -> usize {
    segments
        .iter()
        .filter(|s| !matches!(s, Segment::Literal(_)))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 10 spec-parser perspectives (the checker/codegen tests cover usage).
    #[test]
    fn s01_plain_literal() {
        assert_eq!(
            parse_spec("hello").unwrap(),
            vec![Segment::Literal("hello".into())]
        );
    }

    #[test]
    fn s02_single_display() {
        assert_eq!(
            parse_spec("x = {}").unwrap(),
            vec![Segment::Literal("x = ".into()), Segment::Display]
        );
    }

    #[test]
    fn s03_multiple_displays_with_tail() {
        let s = parse_spec("{} + {} = ?").unwrap();
        assert_eq!(placeholder_count(&s), 2);
        assert_eq!(s.last(), Some(&Segment::Literal(" = ?".into())));
    }

    #[test]
    fn s04_f64_forms() {
        assert_eq!(
            parse_spec("{:.17g}{:.16f}{:.6f}").unwrap(),
            vec![
                Segment::F64(F64Format::G17),
                Segment::F64(F64Format::F16),
                Segment::F64(F64Format::F6)
            ]
        );
    }

    #[test]
    fn s05_brace_escapes() {
        assert_eq!(
            parse_spec("{{}} and {{x}}").unwrap(),
            vec![Segment::Literal("{} and {x}".into())]
        );
    }

    #[test]
    fn s06_escape_adjacent_to_placeholder() {
        let s = parse_spec("{{{}}}").unwrap();
        assert_eq!(
            s,
            vec![
                Segment::Literal("{".into()),
                Segment::Display,
                Segment::Literal("}".into())
            ]
        );
    }

    #[test]
    fn s07_unknown_placeholder_errors() {
        assert!(parse_spec("{:x}").is_err());
    }

    #[test]
    fn s08_unterminated_errors() {
        assert!(parse_spec("oops {").is_err());
    }

    #[test]
    fn s09_stray_close_errors() {
        assert!(parse_spec("oops }").is_err());
    }

    #[test]
    fn s10_empty_spec() {
        assert_eq!(parse_spec("").unwrap(), Vec::<Segment>::new());
    }
}
