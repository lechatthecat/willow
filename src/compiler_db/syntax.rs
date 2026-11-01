//! Revision syntax correspondence. The artifact representation is flat for
//! executable syntax, so these passes do not recurse with expression depth.
use crate::{
    diagnostics::Span,
    parser::ast::{BodyId, Expr, ExprId, Program},
};
use anyhow::Result;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

type TokenPositions = HashMap<(crate::diagnostics::FileId, usize), (usize, usize, usize)>;
#[derive(Default, Debug)]
struct TokenBoundaries {
    starts: TokenPositions,
    ends: TokenPositions,
}

#[derive(Default, Debug)]
pub(crate) struct Correspondence {
    pub spans: HashMap<Span, Span>,
    pub unchanged: HashSet<BodyId>,
    pub unchanged_initializers: HashSet<ExprId>,
    token_ranges: Vec<(Span, Span)>,
    token_boundaries: TokenBoundaries,
}

fn span(value: &Value) -> Option<Span> {
    let object = value.as_object()?;
    if object.len() != 5 {
        return None;
    }
    // This predicate runs for every wire node during canonicalization and
    // correspondence. Read the fixed schema without cloning an object tree.
    Some(Span {
        file_id: crate::diagnostics::FileId(object.get("file_id")?.as_u64()?.try_into().ok()?),
        start: object.get("start")?.as_u64()?.try_into().ok()?,
        end: object.get("end")?.as_u64()?.try_into().ok()?,
        line: object.get("line")?.as_u64()?.try_into().ok()?,
        col: object.get("col")?.as_u64()?.try_into().ok()?,
    })
}

/// Presentation coordinates in structural order, including signature-only items.
pub(crate) fn spans(value: &Value) -> Vec<Span> {
    fn visit(value: &Value, out: &mut Vec<Span>) {
        if let Some(value) = span(value) {
            out.push(value);
            return;
        }
        match value {
            Value::Object(values) => {
                for value in values.values() {
                    visit(value, out);
                }
            }
            Value::Array(values) => {
                for value in values {
                    visit(value, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    visit(value, &mut out);
    out
}

/// The flat AST wire has named IDs and tuple-expression IDs immediately after
/// a Span. Child indices and numeric literals are semantic and stay intact.
fn tuple_id(values: &[Value], index: usize) -> bool {
    index + 1 == values.len() && index > 0 && span(&values[index - 1]).is_some()
}

pub(crate) fn semantic(value: &Value) -> Value {
    canonical(value, false, false)
}

pub(crate) fn without_ids(value: &Value) -> Value {
    canonical(value, true, false)
}

pub(crate) fn without_spans(value: &Value) -> Value {
    canonical(value, false, true)
}

fn canonical(value: &Value, keep_spans: bool, keep_ids: bool) -> Value {
    if !keep_spans && span(value).is_some() {
        return Value::Null;
    }
    match value {
        Value::Object(values) => Value::Object(
            values
                .iter()
                .filter(|(key, _)| keep_ids || key.as_str() != "id")
                .map(|(key, value)| (key.clone(), canonical(value, keep_spans, keep_ids)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .enumerate()
                .map(|(i, value)| {
                    if !keep_ids && tuple_id(values, i) {
                        Value::Null
                    } else {
                        canonical(value, keep_spans, keep_ids)
                    }
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn copy_ids(old: &Value, new: &mut Value, spans: &mut HashMap<Span, Span>) {
    if let (Some(a), Some(b)) = (span(old), span(new)) {
        spans.insert(a, b);
        return;
    }
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in b {
                if let Some(previous) = a.get(key) {
                    if key == "id" {
                        *value = previous.clone();
                    } else {
                        copy_ids(previous, value, spans);
                    }
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            for (i, (previous, value)) in a.iter().zip(b).enumerate() {
                if tuple_id(a, i) {
                    *value = previous.clone();
                } else {
                    copy_ids(previous, value, spans);
                }
            }
        }
        _ => {}
    }
}

fn name(value: &Value) -> Option<String> {
    if let Some(name) = value.get("name").and_then(Value::as_str) {
        return Some(name.to_owned());
    }
    let object = value.as_object()?;
    if object.len() == 1 {
        let (kind, value) = object.iter().next()?;
        return Some(format!("{kind}:{}", value.get("name")?.as_str()?));
    }
    None
}

fn reconcile(old: &Value, new: &mut Value, result: &mut Correspondence) {
    if let (Some(a), Some(b)) = (span(old), span(new)) {
        result.spans.insert(a, b);
        return;
    }
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            if let (Some(old), Some(new)) =
                (a.get("span").and_then(span), b.get("span").and_then(span))
            {
                let old_body = a
                    .get("body")
                    .or_else(|| a.get("default_body"))
                    .and_then(|v| v.get("span"))
                    .and_then(span);
                let new_body = b
                    .get("body")
                    .or_else(|| b.get("default_body"))
                    .and_then(|v| v.get("span"))
                    .and_then(span);
                if let (Some(old_body), Some(new_body)) = (old_body, new_body) {
                    result.token_ranges.push((
                        Span {
                            end: old_body.start,
                            ..old
                        },
                        Span {
                            end: new_body.start,
                            ..new
                        },
                    ));
                }
            }
            for (key, value) in b {
                let Some(previous) = a.get(key) else { continue };
                if matches!(key.as_str(), "body" | "default_body" | "initializer") {
                    if semantic(previous) == semantic(value) {
                        if let (Some(a), Some(b)) = (
                            previous.get("span").and_then(span),
                            value.get("span").and_then(span),
                        ) {
                            result.token_ranges.push((a, b));
                        }
                        copy_ids(previous, value, &mut result.spans);
                        if let Some(id) = previous.get("id") {
                            result
                                .unchanged
                                .insert(serde_json::from_value(id.clone()).expect("body id"));
                        } else if key == "initializer" && !previous.is_null() {
                            let expr: Expr =
                                serde_json::from_value(previous.clone()).expect("initializer");
                            result.unchanged_initializers.insert(expr.id());
                        }
                    } else if let (Some(old), Some(new)) = (previous.get("id"), value.get_mut("id"))
                    {
                        // The semantic owner survives a body edit; descendants
                        // of changed syntax keep their fresh identities.
                        *new = old.clone();
                    }
                } else {
                    reconcile(previous, value, result);
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            // Named declarations match by kind/name, never by byte offset or
            // position in their containing declaration list.
            if a.iter().all(|v| name(v).is_some()) && b.iter().all(|v| name(v).is_some()) {
                let mut names = HashMap::new();
                for value in a {
                    names
                        .entry(name(value).unwrap())
                        .and_modify(|v| *v = None)
                        .or_insert(Some(value));
                }
                let mut unique = HashSet::new();
                let mut duplicates = HashSet::new();
                for value in b.iter() {
                    let key = name(value).unwrap();
                    if !unique.insert(key.clone()) {
                        duplicates.insert(key);
                    }
                }
                for value in b {
                    let key = name(value).unwrap();
                    if !duplicates.contains(&key)
                        && let Some(Some(previous)) = names.get(&key)
                    {
                        reconcile(previous, value, result);
                    }
                }
            } else {
                for (previous, value) in a.iter().zip(b) {
                    reconcile(previous, value, result);
                }
            }
        }
        _ => {}
    }
}

impl Correspondence {
    pub(crate) fn map_tokens(&mut self, old: &[(String, Span)], new: &[(String, Span)]) {
        for (a, b) in self.token_ranges.drain(..) {
            let from = old.partition_point(|(_, span)| span.start < a.start);
            let to = old.partition_point(|(_, span)| span.start < a.end);
            let next_from = new.partition_point(|(_, span)| span.start < b.start);
            let next_to = new.partition_point(|(_, span)| span.start < b.end);
            let previous = &old[from..to];
            let current = &new[next_from..next_to];
            if previous.len() == current.len()
                && previous.iter().zip(current).all(|(a, b)| a.0 == b.0)
            {
                for ((_, a), (_, b)) in previous.iter().zip(current) {
                    self.spans.insert(*a, *b);
                    self.token_boundaries
                        .ends
                        .insert((a.file_id, a.end), (b.end, b.line, b.col));
                    self.token_boundaries
                        .starts
                        .insert((a.file_id, a.start), (b.start, b.line, b.col));
                }
            }
        }
    }
    pub(crate) fn reconcile(&mut self, old: &Program, new: &mut Program) -> Result<()> {
        let old = serde_json::to_value(old)?;
        let mut value = serde_json::to_value(&*new)?;
        reconcile(&old, &mut value, self);
        *new = serde_json::from_value(value)?;
        Ok(())
    }

    /// Rebase presentation metadata after semantic reuse. The semantic result
    /// retains its stable IDs while diagnostics and AI references follow edits.
    pub(crate) fn remap<T: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        value: &T,
    ) -> Result<T> {
        self.remap_with_spans(value, &HashMap::new())
    }

    pub(crate) fn remap_with_spans<T: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        value: &T,
        local: &HashMap<Span, Span>,
    ) -> Result<T> {
        fn walk(
            value: &mut Value,
            spans: &HashMap<Span, Span>,
            local: &HashMap<Span, Span>,
            boundaries: &TokenBoundaries,
        ) {
            if let Some(old) = span(value) {
                if let Some(new) = local
                    .get(&old)
                    .copied()
                    .or_else(|| relocate_span(spans, boundaries, old))
                {
                    *value = serde_json::to_value(new).expect("span");
                }
                return;
            }
            match value {
                Value::Object(object) => object
                    .values_mut()
                    .for_each(|v| walk(v, spans, local, boundaries)),
                Value::Array(array) => array
                    .iter_mut()
                    .for_each(|v| walk(v, spans, local, boundaries)),
                _ => {}
            }
        }
        let mut value = serde_json::to_value(value)?;
        walk(&mut value, &self.spans, local, &self.token_boundaries);
        Ok(serde_json::from_value(value)?)
    }

    /// The current coordinates of one retained span, or `None` when it does
    /// not follow an unchanged token sequence.
    /// Files containing at least one relocated coordinate.
    pub(crate) fn moved_files(&self) -> HashSet<crate::diagnostics::FileId> {
        self.spans
            .iter()
            .filter(|(old, new)| old != new)
            .map(|(old, _)| old.file_id)
            .collect()
    }
    pub(crate) fn span(&self, old: Span) -> Option<Span> {
        relocate_span(&self.spans, &self.token_boundaries, old)
    }
}

fn relocate_span(
    spans: &HashMap<Span, Span>,
    boundaries: &TokenBoundaries,
    old: Span,
) -> Option<Span> {
    if let Some(new) = spans.get(&old) {
        return Some(*new);
    }
    let &(start, line, col) = boundaries
        .starts
        .get(&(old.file_id, old.start))
        .or_else(|| boundaries.ends.get(&(old.file_id, old.start)))?;
    let &(end, _, _) = boundaries
        .ends
        .get(&(old.file_id, old.end))
        .or_else(|| boundaries.starts.get(&(old.file_id, old.end)))?;
    // Analysis may narrow an AST range to several source tokens (for example
    // a member call before its first argument). Such ranges are not exact
    // AST/token spans, but both ends follow the already matched unchanged
    // token sequence.
    (start <= end).then_some(Span {
        start,
        end,
        line,
        col,
        ..old
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{
        ast::*,
        iter::{AstEvent, AstWalk},
    };

    #[test]
    fn narrowed_reference_ranges_follow_matched_token_boundaries() {
        let old = Span::new(10, 30, 1, 1);
        let new = Span::new(20, 40, 2, 1);
        let mut correspondence = Correspondence {
            token_ranges: vec![(old, new)],
            ..Default::default()
        };
        correspondence.map_tokens(
            &[
                ("member".into(), Span::new(12, 18, 1, 3)),
                ("arg".into(), Span::new(19, 22, 1, 10)),
            ],
            &[
                ("member".into(), Span::new(22, 28, 2, 3)),
                ("arg".into(), Span::new(29, 32, 2, 10)),
            ],
        );
        assert_eq!(
            correspondence.remap(&Span::new(12, 19, 1, 3)).unwrap(),
            Span::new(22, 29, 2, 3)
        );
        // An unmatched endpoint must not manufacture a source correspondence.
        assert_eq!(
            correspondence.remap(&Span::new(12, 17, 1, 3)).unwrap(),
            Span::new(12, 17, 1, 3)
        );
    }

    #[test]
    fn composite_span_end_does_not_absorb_inserted_whitespace() {
        let mut correspondence = Correspondence {
            token_ranges: vec![(Span::new(0, 3, 1, 1), Span::new(10, 15, 2, 1))],
            ..Default::default()
        };
        correspondence.map_tokens(
            &[
                ("a".into(), Span::new(0, 1, 1, 1)),
                ("b".into(), Span::new(1, 2, 1, 2)),
                ("c".into(), Span::new(2, 3, 1, 3)),
            ],
            &[
                ("a".into(), Span::new(10, 11, 2, 1)),
                ("b".into(), Span::new(11, 12, 2, 2)),
                ("c".into(), Span::new(14, 15, 2, 5)),
            ],
        );
        assert_eq!(
            correspondence.remap(&Span::new(0, 2, 1, 1)).unwrap(),
            Span::new(10, 12, 2, 1)
        );
        assert_eq!(
            correspondence.remap(&Span::new(1, 3, 1, 2)).unwrap(),
            Span::new(11, 15, 2, 2)
        );
    }

    #[test]
    fn span_probe_matches_exact_serde_schema() {
        let base = serde_json::json!({"file_id": 7, "start": 1, "end": 9, "line": 2, "col": 3});
        let mut cases = vec![base.clone(), Value::Null, serde_json::json!([])];
        for field in ["file_id", "start", "end", "line", "col"] {
            for value in [
                Value::Null,
                serde_json::json!(false),
                serde_json::json!("1"),
                serde_json::json!(-1),
                serde_json::json!(1.0),
                serde_json::json!(0),
                serde_json::json!(u32::MAX),
                serde_json::json!(u64::MAX),
            ] {
                let mut changed = base.clone();
                changed[field] = value;
                cases.push(changed);
            }
            let mut missing = base.clone();
            missing.as_object_mut().unwrap().remove(field);
            cases.push(missing);
        }
        let mut extra = base;
        extra["extra"] = Value::Null;
        cases.push(extra);
        for value in cases {
            let expected = value
                .as_object()
                .filter(|object| object.len() == 5)
                .and_then(|_| serde_json::from_value::<Span>(value.clone()).ok());
            assert_eq!(span(&value), expected, "{value}");
        }
    }

    fn parse(source: &str) -> Program {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        program
    }

    fn ids(program: &Program, name: &str) -> Vec<String> {
        let body = program
            .items
            .iter()
            .find_map(|item| match item {
                Item::Function(f) if f.name == name => Some(&f.body),
                _ => None,
            })
            .unwrap();
        AstWalk::new(AstEvent::Block(body))
            .filter_map(|event| match event {
                AstEvent::Block(b) => Some(format!("body:{:?}", b.id)),
                AstEvent::Expr(e) => Some(format!("expr:{:?}", e.id())),
                AstEvent::Pattern(p) => Some(format!("pattern:{:?}", p.id())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn whitespace_preserves_nested_identities_and_current_coordinates() {
        let old = parse(
            "fn f(x: i64) -> i64 { let g = |y: i64| { return match y { 0 => 1, _ => x }; }; return g(2); }",
        );
        let mut new = parse(
            "\n// prefix\nfn f(x: i64) -> i64 {\n let g = |y: i64| {\n return match y { 0 => 1, _ => x }; };\n return g(2); }\n",
        );
        let expected = semantic(&serde_json::to_value(&new).unwrap());
        let mut correspondence = Correspondence::default();
        correspondence.reconcile(&old, &mut new).unwrap();
        assert_eq!(ids(&old, "f"), ids(&new, "f"));
        assert_eq!(semantic(&serde_json::to_value(&new).unwrap()), expected);
        assert!(correspondence.spans.iter().any(|(a, b)| a.start != b.start));
        let Item::Function(f) = &new.items[0] else {
            panic!()
        };
        assert_eq!(f.span.line, 3);
    }

    #[test]
    fn declaration_reordering_matches_owners_and_changed_body_keeps_only_root() {
        let old = parse("fn f() -> i64 { return 1; } fn g() -> i64 { return 7; }");
        let mut new = parse("fn g() -> i64 { return 7; } fn f() -> i64 { return 2; }");
        Correspondence::default().reconcile(&old, &mut new).unwrap();
        assert_eq!(ids(&old, "g"), ids(&new, "g"));
        let before = ids(&old, "f");
        let after = ids(&new, "f");
        assert_eq!(before[0], after[0]);
        assert_ne!(before[1], after[1]);
    }
}
