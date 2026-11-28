//! Additive, independently versioned display evidence. Values are projections
//! of the *same* SAT model, never a second solve or a claim of runtime replay.
use super::*;

#[derive(Debug, Clone, Serialize)]
pub struct Presentation {
    pub schema: &'static str,
    pub evidence: &'static str,
    pub location: Option<Span>,
    pub obligation: Option<String>,
    pub inputs: Vec<DisplayInput>,
    pub result: Option<String>,
    pub incomplete: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct DisplayInput {
    pub name: String,
    pub value: String,
}
impl Default for Presentation {
    fn default() -> Self {
        Self {
            schema: "willow.verification.presentation.v1",
            evidence: "CANDIDATE",
            location: None,
            obligation: None,
            inputs: Vec::new(),
            result: None,
            incomplete: None,
        }
    }
}
impl Presentation {
    pub(super) fn vacuous(location: Option<Span>) -> Self {
        Self {
            evidence: "VACUOUS_PRECONDITION",
            location,
            obligation: Some("requires satisfiability".into()),
            ..Self::default()
        }
    }
    pub(super) fn text(&self) -> String {
        let mut text = self
            .inputs
            .iter()
            .map(|input| format!("{} = {}", input.name, input.value))
            .collect::<Vec<_>>()
            .join("; ");
        if let Some(result) = &self.result {
            if !text.is_empty() {
                text.push_str("; ");
            }
            text.push_str(&format!("result = {result}"));
        }
        if let Some(reason) = &self.incomplete {
            if !text.is_empty() {
                text.push_str("; ");
            }
            text.push_str(&format!("display incomplete: {reason}"));
        }
        text
    }
}

// Parse just the framing of a get-value response. Preserve quoted strings and
// skip nested terms iteratively, so deeply nested solver terms cannot overflow
// our stack. Aggregate values are read through typed projections below.
fn term_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if quoted {
            if byte == b'"' {
                if bytes.get(index + 1) == Some(&b'"') {
                    index += 1;
                } else {
                    quoted = false;
                    if depth == 0 {
                        return Some(index + 1);
                    }
                }
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'(' => depth += 1,
                b')' if depth == 0 => return Some(index),
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(index + 1);
                    }
                }
                b if b.is_ascii_whitespace() && depth == 0 => return Some(index),
                _ => {}
            }
        }
        index += 1;
    }
    (!quoted && depth == 0).then_some(index)
}
fn read_value(solver: &mut solver::Solver, expression: &str) -> VerificationResult<String> {
    let response = solver.exchange(&format!("(get-value ({expression}))\n"))?;
    parse_value(&response)
        .map(str::to_owned)
        .ok_or_else(|| FailureKind::Infrastructure.error("malformed display value"))
}
fn parse_value(response: &str) -> Option<&str> {
    let pair = response
        .trim()
        .strip_prefix('(')?
        .trim_start()
        .strip_prefix('(')?
        .trim_start();
    let value = pair.get(term_end(pair)?..)?.trim_start();
    let end = term_end(value)?;
    (value[end..].split_whitespace().collect::<String>() == "))").then_some(&value[..end])
}
fn number(value: &str) -> Option<i128> {
    value.parse().ok().or_else(|| {
        value
            .strip_prefix("(- ")?
            .strip_suffix(')')?
            .trim()
            .parse::<i128>()
            .ok()?
            .checked_neg()
    })
}

struct Renderer<'a, 'b> {
    solver: &'a mut solver::Solver,
    data: &'a DataTypes<'b>,
    remaining: usize,
    remaining_bytes: usize,
    non_expression_values: usize,
    limitations: &'a mut BTreeSet<String>,
}
impl Renderer<'_, '_> {
    fn value(&mut self, expression: &str, ty: &Type, depth: usize) -> VerificationResult<String> {
        if self.remaining == 0 || depth > 128 {
            return Err(FailureKind::Unsupported.error("value display budget exceeded"));
        }
        self.remaining -= 1;
        if let Some(element) = frozen_element(ty) {
            let key = type_key(ty);
            let length = read_value(self.solver, &format!("(len_{key} {expression})"))?;
            let length = number(&length)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| FailureKind::Infrastructure.error("invalid array length"))?;
            let mut values = Vec::new();
            for index in 0..length.min(32).min(self.remaining) {
                let value = self.value(
                    &format!("(select (values_{key} {expression}) {index})"),
                    element,
                    depth + 1,
                )?;
                values.push(constructor_argument(value, element));
            }
            if values.len() != length {
                self.non_expression_values += 1;
                values.push(format!("… (length = {length})"));
            }
            return Ok(format!("[{}]", values.join(", ")));
        }
        // A datatype's printed model can contain arbitrary `let` sharing.
        // Ask for its constructor index rather than decoding solver-specific
        // aggregate syntax or re-reading whole nested records at each field.
        if let Type::Named(name) = ty {
            return self.record(expression, ty, name, depth);
        }
        let raw = read_value(self.solver, expression)?;
        self.remaining_bytes = self
            .remaining_bytes
            .checked_sub(raw.len())
            .ok_or_else(|| FailureKind::Unsupported.error("value display byte budget exceeded"))?;
        if ty.integer_layout().is_some() {
            return number(&raw)
                .map(|n| n.to_string())
                .ok_or_else(|| FailureKind::Infrastructure.error("invalid integer display value"));
        }
        match ty {
            Type::Bool if raw == "true" || raw == "false" => Ok(raw),
            Type::Char => number(&raw)
                .and_then(|n| u32::try_from(n).ok())
                .and_then(char::from_u32)
                .map(|c| format!("{c:?}"))
                .ok_or_else(|| {
                    FailureKind::Infrastructure.error("invalid character display value")
                }),
            Type::String => string_value(&raw).ok_or_else(|| {
                FailureKind::Unsupported.error("string model is outside Willow UTF-8 byte strings")
            }),
            _ => Err(FailureKind::Unsupported.error("unavailable display value")),
        }
    }
    fn record(
        &mut self,
        expression: &str,
        ty: &Type,
        name: &str,
        depth: usize,
    ) -> VerificationResult<String> {
        let constructors = self
            .data
            .constructors
            .get(ty)
            .ok_or_else(|| FailureKind::Unsupported.error("unavailable data display type"))?;
        let key = type_key(ty);
        let index = if self.data.fields.contains_key(ty) {
            0
        } else {
            number(&read_value(
                self.solver,
                &format!("(tag_{key} {expression})"),
            )?)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&index| index < constructors.len())
            .ok_or_else(|| FailureKind::Infrastructure.error("invalid constructor display value"))?
        };
        let ctor = &constructors[index];
        let before = self.non_expression_values;
        let mut fields = Vec::new();
        for (field, (_, field_ty)) in ctor.fields.iter().enumerate() {
            fields.push(self.value(
                &format!("({} {expression})", selector(ty, index, field)),
                field_ty,
                depth + 1,
            )?);
        }
        if self.data.fields.contains_key(ty)
            && (!ctor.memberwise || self.non_expression_values != before)
        {
            self.non_expression_values += 1;
            let reason = if ctor.memberwise {
                "field values are not complete constructor arguments"
            } else {
                "explicit constructor has not been evaluated"
            };
            self.limitations
                .insert(format!("{name}: construction unavailable ({reason})"));
            let fields = ctor
                .fields
                .iter()
                .zip(fields)
                .map(|((name, _), value)| format!("{name}: {value}"))
                .collect::<Vec<_>>();
            return Ok(format!("{name} {{ {} }}", fields.join(", ")));
        }
        let fields = ctor
            .fields
            .iter()
            .zip(fields)
            .map(|((_, ty), value)| constructor_argument(value, ty))
            .collect::<Vec<_>>();
        if self.data.fields.contains_key(ty) {
            Ok(format!("new {name}({})", fields.join(", ")))
        } else if fields.is_empty() {
            Ok(format!("{name}::{}", ctor.name))
        } else {
            Ok(format!("{name}::{}({})", ctor.name, fields.join(", ")))
        }
    }
}
// A FrozenArray's displayed elements use array notation; when embedded in a
// constructor argument the source expression must freeze that array.
fn constructor_argument(value: String, ty: &Type) -> String {
    if frozen_element(ty).is_some() {
        format!("{value}.freeze()")
    } else {
        value
    }
}

// Match Willow's string lexer, not Rust's Debug or JSON escape vocabulary.
// Characters without a named Willow escape remain literal source characters.
fn willow_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for c in value.chars() {
        match c {
            '\0' => output.push_str("\\0"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{1b}' => output.push_str("\\e"),
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            _ => output.push(c),
        }
    }
    output.push('"');
    output
}
fn string_value(raw: &str) -> Option<String> {
    let mut chars = raw.strip_prefix('"')?.strip_suffix('"')?.chars().peekable();
    let mut bytes = Vec::new();
    while let Some(c) = chars.next() {
        let code = if c == '"' {
            if chars.next()? != '"' {
                return None;
            }
            u32::from(c)
        } else if c == '\\' && chars.peek() == Some(&'u') {
            chars.next();
            if chars.next()? != '{' {
                return None;
            }
            let mut hex = String::new();
            loop {
                let c = chars.next()?;
                if c == '}' {
                    break;
                }
                hex.push(c);
            }
            u32::from_str_radix(&hex, 16).ok()?
        } else {
            u32::from(c)
        };
        bytes.push(u8::try_from(code).ok()?);
    }
    Some(willow_string(&String::from_utf8(bytes).ok()?))
}

pub(super) fn capture(
    solver: &mut solver::Solver,
    data: &DataTypes,
    function: &FunctionDecl,
    args: &[String],
    sites: &[ProofSite],
    phase: &str,
) -> Presentation {
    let mut display = Presentation::default();
    let mut limitations = BTreeSet::new();
    let result: VerificationResult<()> = (|| {
        let id = symbol(function);
        let selection = read_value(
            solver,
            &application(&format!("f{id}_failure_{phase}"), args),
        )?;
        if let Some(site) = number(&selection)
            .and_then(|n| usize::try_from(n).ok())
            .and_then(|i| sites.get(i))
        {
            display.location = Some(site.location);
            display.obligation = Some(site.kind.into());
        }
        let mut renderer = Renderer {
            solver,
            data,
            remaining: 256,
            remaining_bytes: 16_384,
            non_expression_values: 0,
            limitations: &mut limitations,
        };
        for (param, arg) in function.params.iter().zip(args) {
            display.inputs.push(DisplayInput {
                name: param.name.clone(),
                value: renderer.value(arg, &param.ty, 0)?,
            });
        }
        // Only ensures truth has already passed all safety obligations. Earlier
        // failures must not display a totalized SMT result as an executed value.
        if phase == "post" {
            display.result = Some(renderer.value(
                &application(&format!("f{id}_value"), args),
                &function.return_type,
                0,
            )?);
        }
        Ok(())
    })();
    if let Err(error) = result {
        limitations.insert(error.message.into_owned());
    }
    if !limitations.is_empty() {
        display.incomplete = Some(limitations.into_iter().collect::<Vec<_>>().join("; "));
    }
    display
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_diagnostics_string_literal_round_trip() {
        use crate::lexer::{Lexer, token::TokenKind};
        // Every ASCII/C1 control, every named escape, quotes, backslashes and
        // multibyte scalars must survive the actual Willow lexer unchanged.
        let mut values: Vec<String> = (0..=0x9f)
            .map(|code| format!("a{}z", char::from_u32(code).unwrap()))
            .collect();
        values.extend([
            "あ😀".into(),
            r"\u{1b} \x1b \e".into(),
            "Unknown Counterexample VacuousPrecondition".into(),
        ]);
        for value in values {
            let literal = willow_string(&value);
            let tokens = Lexer::new(&literal).tokenize().unwrap();
            assert_eq!(
                tokens[0].kind,
                TokenKind::StringLiteral(value),
                "{literal:?}"
            );
            assert_eq!(tokens.len(), 2);
        }
        assert_eq!(willow_string("\0\n\r\t\u{1b}\\\""), r#""\0\n\r\t\e\\\"""#);
        assert_eq!(string_value(r#""\u{1b}""#).as_deref(), Some(r#""\e""#));
        for size in [128, 256, 512, 1024] {
            assert_eq!(willow_string(&"\u{1b}".repeat(size)).len(), size * 2 + 2);
        }
    }
    #[test]
    fn source_diagnostics_model_framing_and_strings() {
        for (raw, expected) in [
            ("((x (- 1)))", Some("(- 1)")),
            ("(((select xs 0) true))", Some("true")),
            ("((x \"a\"\"b\"))", Some("\"a\"\"b\"")),
            ("((x \"unterminated))", None),
            ("((x 1)) junk", None),
        ] {
            assert_eq!(parse_value(raw), expected);
        }
        assert_eq!(
            number("(- 9223372036854775808)"),
            Some(i128::from(i64::MIN))
        );
        assert_eq!(string_value(r#""a""b""#), Some(r#""a\"b""#.into()));
        assert_eq!(
            string_value(r#""\u{e3}\u{81}\u{82}""#),
            Some("\"あ\"".into())
        );
        assert_eq!(string_value(r#""\u{100}""#), None);
    }
}
