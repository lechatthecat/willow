//! Format-string interpolation spec parsing, shared by the type
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

/// Lowering retains byte ranges in the checked template until a literal is emitted.
pub(crate) enum SourceSegment {
    Literal(std::ops::Range<usize>),
    Display,
    F64(F64Format),
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
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

#[cfg(test)]
std::thread_local! {
    pub(crate) static DECODED_LITERAL_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static RENDERED_TEMPLATE_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static SPEC_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Parse a format spec into segments. Errors describe the malformed piece.
pub fn parse_spec(spec: &str) -> Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    visit_ranges(spec, |segment| {
        segments.push(match segment {
            SourceSegment::Literal(range) => Segment::Literal(decode_literal(&spec[range])),
            SourceSegment::Display => Segment::Display,
            SourceSegment::F64(format) => Segment::F64(format),
        })
    })
    .map_err(FormatError::message)?;
    Ok(segments)
}

pub(crate) fn decode_literal(source: &str) -> String {
    let mut literal = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(i) = rest.find(['{', '}']) {
        // The parser checked that every brace in this range is doubled.
        literal.push_str(&rest[..=i]);
        rest = &rest[i + 2..];
    }
    literal.push_str(rest);
    #[cfg(test)]
    DECODED_LITERAL_BYTES.set(DECODED_LITERAL_BYTES.get() + literal.len());
    literal
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SourceFormat {
    Display,
    Precision(F64Format),
}

// Display is distinguished by its closing brace. In the three precision
// spellings the fourth byte after the opening brace is unique (7, 6, f).
fn instruction(source: &str) -> Option<(&'static str, SourceFormat)> {
    let bytes = source.as_bytes();
    if bytes.get(1) == Some(&b'}') {
        return Some(("{}", SourceFormat::Display));
    }
    Some(match bytes.get(4)? {
        b'7' => ("{:.17g}", SourceFormat::Precision(F64Format::G17)),
        b'6' => ("{:.16f}", SourceFormat::Precision(F64Format::F16)),
        b'f' => ("{:.6f}", SourceFormat::Precision(F64Format::F6)),
        _ => return None,
    })
}

/// An immutable template accepted by the grammar. Literal decoding can consume
/// the checked spelling directly, without building another segment table.
#[derive(Clone, Copy)]
pub(crate) struct CheckedTemplate<'a>(&'a str);

impl<'a> CheckedTemplate<'a> {
    pub(crate) fn as_str(self) -> &'a str {
        self.0
    }

    /// Materialize just the literal before the next operand. The cursor stops
    /// there so an operand that diverges never allocates its suffix.
    pub(crate) fn render_piece(&mut self) -> (String, Option<SourceFormat>) {
        let mut text = String::new();
        let mut rest = self.0;
        #[cfg(test)]
        let before = rest.len();
        loop {
            let Some(i) = rest.find(['{', '}']) else {
                text.push_str(rest);
                self.0 = "";
                break;
            };
            let brace = &rest[i..];
            if brace.as_bytes()[0] != brace.as_bytes()[1] {
                text.push_str(&rest[..i]);
                self.0 = brace;
                break;
            }
            text.push_str(&rest[..=i]);
            rest = &brace[2..];
        }
        #[cfg(test)]
        DECODED_LITERAL_BYTES.set(DECODED_LITERAL_BYTES.get() + text.len());
        let operand = if self.0.is_empty() {
            None
        } else {
            let (spelling, op) =
                instruction(self.0).expect("a checked operand has a format instruction");
            self.0 = &self.0[spelling.len()..];
            Some(op)
        };
        #[cfg(test)]
        RENDERED_TEMPLATE_BYTES.set(RENDERED_TEMPLATE_BYTES.get() + before - self.0.len());
        (text, operand)
    }
}

pub(crate) enum FormatError<'a> {
    StrayClose,
    Unterminated,
    Unsupported(&'a str),
}

impl FormatError<'_> {
    fn message(self) -> String {
        match self {
            Self::StrayClose => "stray `}` (use `}}` for a literal closing brace)".to_owned(),
            Self::Unterminated => {
                "unterminated `{` placeholder (use `{{` for a literal brace)".to_owned()
            }
            Self::Unsupported(other) => format!(
                "unsupported placeholder `{{{other}}}` (supported: `{{}}`, \
                 `{{:.17g}}`, `{{:.16f}}`, `{{:.6f}}`)"
            ),
        }
    }
}

pub(crate) fn visit_ranges(
    spec: &str,
    mut visit: impl FnMut(SourceSegment),
) -> Result<CheckedTemplate<'_>, FormatError<'_>> {
    #[cfg(test)]
    SPEC_PARSES.set(SPEC_PARSES.get() + 1);
    let mut start = 0;
    let mut pos = 0;
    while let Some(offset) = spec[pos..].find(['{', '}']) {
        let brace = pos + offset;
        let byte = spec.as_bytes()[brace];
        pos = brace + 1;
        if spec.as_bytes().get(pos) == Some(&byte) {
            pos += 1;
            continue;
        }
        if byte == b'}' {
            return Err(FormatError::StrayClose);
        }
        let source = &spec[brace..];
        let Some((spelling, op)) =
            instruction(source).filter(|(spelling, _)| source.starts_with(spelling))
        else {
            let Some(end) = spec[pos..].find('}') else {
                return Err(FormatError::Unterminated);
            };
            return Err(FormatError::Unsupported(&spec[pos..pos + end]));
        };
        if start < brace {
            visit(SourceSegment::Literal(start..brace));
        }
        visit(match op {
            SourceFormat::Display => SourceSegment::Display,
            SourceFormat::Precision(format) => SourceSegment::F64(format),
        });
        pos = brace + spelling.len();
        start = pos;
    }
    if start < spec.len() {
        visit(SourceSegment::Literal(start..spec.len()));
    }
    Ok(CheckedTemplate(spec))
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
        use Segment::{Display, Literal};
        assert_eq!(
            parse_spec("x = {}").unwrap(),
            vec![Literal("x = ".into()), Display]
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
