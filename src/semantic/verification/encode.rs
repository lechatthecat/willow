use super::*;
use crate::parser::iter::{AstEvent, AstWalk};
use crate::qualified_name::QualifiedName;
use error::FailureKind::Unsupported;

#[derive(Clone, Debug)]
pub(super) struct Value {
    pub text: String,
    pub boolean: bool,
    pub ty: Type,
}
impl Value {
    fn literal(text: impl Into<String>, boolean: bool) -> Self {
        Self {
            text: text.into(),
            boolean,
            ty: if boolean { Type::Bool } else { Type::I64 },
        }
    }
}
/// Escape every UTF-8 byte, including quotes, backslashes and NUL. Encoding
/// literals as byte strings makes str.len agree with Willow's byte length.
fn byte_string(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(value.len().saturating_mul(6).saturating_add(2));
    out.push('"');
    for byte in value.bytes() {
        write!(out, "\\u{{{byte:02x}}}").expect("write to String");
    }
    out.push('"');
    out
}
pub(super) fn symbol(f: &FunctionDecl) -> String {
    symbol_name(&f.name)
}
pub(super) fn symbol_name(name: &str) -> String {
    format!("{:x}", crate::hex::Hex(name.as_bytes()))
}
pub(super) fn integer(n: impl Into<i128>) -> String {
    let n = n.into();
    if n < 0 {
        format!("(- {})", n.unsigned_abs())
    } else {
        n.to_string()
    }
}
pub(super) fn bounds(v: &str) -> String {
    integer_bounds(v, Type::<String>::I64.integer_layout().unwrap())
}
pub(super) fn integer_bounds(v: &str, layout: crate::parser::ast::IntegerLayout) -> String {
    format!(
        "(and (<= {} {v}) (<= {v} {}))",
        integer(layout.min()),
        integer(layout.max())
    )
}
pub(super) fn has(f: &FunctionDecl, k: EffectContractKind) -> bool {
    f.effect_contracts.iter().any(|c| c.kind == k)
}
pub(super) struct Summary {
    #[cfg(test)]
    pub assembly_bytes: usize,
    pub contract_dependencies: BTreeSet<usize>,
    pub data_digest: String,
    pub definitions: String,
    pub contract_definitions: String,
    pub nodes: usize,
    pub dependencies: BTreeSet<usize>,
    pub loop_symbols: std::collections::BTreeMap<String, String>,
    pub inferred_loop_annotations: Vec<InferredLoopAnnotations>,
}
// Every assignment and return carries its reachability guard. Branches can
// update one symbolic environment sequentially without copying or re-merging
// all modified variables at each join. Persistent snapshots retain inputs.
#[derive(Clone)]
struct State {
    env: im::HashMap<String, Value>,
    alive: Value,
    result: Value,
    returned: Value,
}
/// Shape of one verified loop. `while` loops have no counter; `for` loops
/// name their counter as `step`, and array iteration also binds `element`.
struct LoopShape<'a> {
    cond: &'a Expr,
    invariants: &'a [Expr],
    variant: Option<&'a Expr>,
    body: &'a Block,
    span: Span,
    step: Option<&'a str>,
    element: Option<(&'a str, &'a Value)>,
    bounds: Option<(&'a Value, &'a Value)>,
}
struct Encoder<'a> {
    data: &'a DataTypes<'a>,
    functions: &'a [&'a FunctionDecl],
    names: &'a HashMap<&'a str, usize>,
    nodes: Vec<(Value, String)>,
    safety: Vec<String>,
    assumptions: Vec<String>,
    dependencies: BTreeSet<usize>,
    depth: usize,
    expected: Option<Type>,
    function_id: String,
    havoc: Vec<Value>,
    loop_symbols: std::collections::BTreeMap<String, String>,
    loop_writes: HashMap<BodyId, BTreeSet<String>>,
    verified: bool,
    jumps: Vec<(Value, Value)>,
    next_quantifier: usize,
    scoped_nodes: usize,
    fragments: Vec<String>,
    inferred_loop_annotations: Vec<InferredLoopAnnotations>,
}
impl<'a> Encoder<'a> {
    fn new(
        function_id: usize,
        functions: &'a [&'a FunctionDecl],
        names: &'a HashMap<&'a str, usize>,
        data: &'a DataTypes<'a>,
    ) -> Self {
        Self {
            data,
            functions,
            names,
            nodes: Vec::new(),
            safety: Vec::new(),
            assumptions: Vec::new(),
            dependencies: BTreeSet::new(),
            depth: 0,
            expected: None,
            function_id: format!(
                "{}{}",
                data.symbol_namespace,
                symbol(functions[function_id])
            ),
            havoc: Vec::new(),
            loop_symbols: std::collections::BTreeMap::new(),
            loop_writes: HashMap::new(),
            verified: has(functions[function_id], EffectContractKind::Verified),
            jumps: Vec::new(),
            next_quantifier: 0,
            scoped_nodes: 0,
            fragments: Vec::new(),
            inferred_loop_annotations: Vec::new(),
        }
    }

    fn typed_node(&mut self, expr: String, ty: &Type) -> VerificationResult<Value> {
        self.data.check(ty)?;
        let mut value = self.node(expr, *ty == Type::Bool)?;
        value.ty = ty.clone();
        self.nodes.last_mut().unwrap().0.ty = ty.clone();
        Ok(value)
    }

    fn node(&mut self, expr: String, boolean: bool) -> VerificationResult<Value> {
        if self.nodes.len() + self.scoped_nodes >= 100_000 {
            return Err(Unsupported.error("VC node budget exceeded"));
        }
        let value = Value::literal(format!("v{}", self.nodes.len()), boolean);
        self.nodes.push((value.clone(), expr));
        Ok(value)
    }
    fn and(&mut self, a: &Value, b: &Value) -> VerificationResult<Value> {
        self.node(format!("(and {} {})", a.text, b.text), true)
    }
    fn guard(&mut self, gate: &Value, condition: String) -> VerificationResult<()> {
        let v = self.node(format!("(=> {} {condition})", gate.text), true)?;
        self.safety.push(v.text);
        Ok(())
    }
    fn expr(&mut self, expr: &Expr, state: &State, gate: &Value) -> VerificationResult<Value> {
        self.depth += 1;
        if self.depth > 256 {
            return Err(Unsupported.error("expression depth budget exceeded"));
        }
        let expected = self.expected.take();
        let result = self.expr_inner(expr, state, gate, expected.as_ref());
        self.depth -= 1;
        result
    }
    fn expr_expecting(
        &mut self,
        expr: &Expr,
        state: &State,
        gate: &Value,
        expected: Option<&Type>,
    ) -> VerificationResult<Value> {
        let previous = self.expected.take();
        self.expected = expected.cloned();
        let result = self.expr(expr, state, gate);
        self.expected = previous;
        result
    }
    fn binary_expr(
        &mut self,
        b: &BinaryExpr,
        state: &State,
        gate: &Value,
        expected: Option<&Type>,
    ) -> VerificationResult<Value> {
        let context = expected.filter(|ty| ty.integer_layout().is_some());
        let is_integer_literal = |expr: &Expr| {
            matches!(expr, Expr::Integer(..))
                || matches!(expr, Expr::Unary(unary) if unary.op == UnaryOp::Neg
                    && matches!(unary.expr, Expr::Integer(..)))
        };
        let (l, r) =
            if context.is_none() && is_integer_literal(&b.lhs) && !is_integer_literal(&b.rhs) {
                let r = self.expr(&b.rhs, state, gate)?;
                let l = self.expr_expecting(&b.lhs, state, gate, Some(&r.ty))?;
                (l, r)
            } else {
                let l = self.expr_expecting(&b.lhs, state, gate, context)?;
                let rhs_gate = match b.op {
                    BinOp::And if l.boolean => self.and(gate, &l)?,
                    BinOp::Or if l.boolean => {
                        self.node(format!("(and {} (not {}))", gate.text, l.text), true)?
                    }
                    _ => gate.clone(),
                };
                let r = self.expr_expecting(&b.rhs, state, &rhs_gate, Some(&l.ty))?;
                (l, r)
            };
        let (op, boolean) = match b.op {
            BinOp::Add => ("+", false),
            BinOp::Sub => ("-", false),
            BinOp::Mul => ("*", false),
            BinOp::Div => ("wdiv", false),
            BinOp::Rem => ("wrem", false),
            BinOp::Eq => ("=", true),
            BinOp::Ne => ("distinct", true),
            BinOp::Lt => ("<", true),
            BinOp::Le => ("<=", true),
            BinOp::Gt => (">", true),
            BinOp::Ge => (">=", true),
            BinOp::And => ("and", true),
            BinOp::Or => ("or", true),
            _ => return Err(Unsupported.error("unsupported binary operation")),
        };
        let valid = match b.op {
            BinOp::Eq | BinOp::Ne => {
                l.ty == r.ty
                    && matches!(
                        l.ty,
                        Type::I8
                            | Type::I16
                            | Type::I32
                            | Type::U16
                            | Type::U32
                            | Type::U64
                            | Type::Char
                            | Type::U8
                            | Type::I64
                            | Type::Bool
                            | Type::String
                    )
            }
            BinOp::And | BinOp::Or => l.boolean && r.boolean,
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                l.ty.scalar_layout().is_some() && l.ty == r.ty
            }
            _ => l.ty.integer_layout().is_some() && l.ty == r.ty,
        };
        if !valid {
            return Err(Unsupported.error("invalid operand types"));
        }
        if matches!(b.op, BinOp::Div | BinOp::Rem) {
            let layout = l.ty.integer_layout().unwrap();
            let safe = if layout.signed {
                format!(
                    "(and (distinct {} 0) (not (and (= {} {}) (= {} (- 1)))))",
                    r.text,
                    l.text,
                    integer(layout.min()),
                    r.text
                )
            } else {
                format!("(distinct {} 0)", r.text)
            };
            self.guard(gate, safe)?;
        }
        let result_ty = if boolean { Type::Bool } else { l.ty.clone() };
        let value = self.typed_node(format!("({op} {} {})", l.text, r.text), &result_ty)?;
        if !boolean {
            self.guard(gate, data::domain(&value.ty, &value.text))?;
        }
        Ok(value)
    }
    fn expr_inner(
        &mut self,
        expr: &Expr,
        state: &State,
        gate: &Value,
        expected: Option<&Type>,
    ) -> VerificationResult<Value> {
        match expr {
            Expr::Integer(n, ..) => {
                let mut value = Value::literal(integer(*n), false);
                if let Some(ty) = expected.filter(|ty| ty.integer_layout().is_some()) {
                    value.ty = ty.clone();
                }
                Ok(value)
            }
            Expr::Char(c, ..) => Ok(Value {
                text: integer(u32::from(*c)),
                boolean: false,
                ty: Type::Char,
            }),
            Expr::Bool(b, ..) => Ok(Value::literal(b.to_string(), true)),
            Expr::String(s, ..) => self.typed_node(byte_string(s), &Type::String),
            Expr::Var(n, ..) => {
                if let Some(v) = state.env.get(n) {
                    return Ok(v.clone());
                }
                let resolved = self
                    .data
                    .targets
                    .and_then(|t| t.get(&expr.id()))
                    .map(String::as_str)
                    .unwrap_or(n);
                match self
                    .names
                    .get(resolved)
                    .and_then(|i| self.functions[*i].constant.as_ref())
                {
                    Some(ConstValue::Integer(number)) => {
                        let mut value = Value::literal(integer(*number), false);
                        value.ty = self.functions[self.names[resolved]].return_type.clone();
                        Ok(value)
                    }
                    Some(ConstValue::Char(c)) => Ok(Value {
                        text: integer(u32::from(*c)),
                        boolean: false,
                        ty: Type::Char,
                    }),
                    Some(ConstValue::Bool(b)) => Ok(Value::literal(b.to_string(), true)),
                    _ => Err(Unsupported.error(format!("unknown scalar binding `{n}`"))),
                }
            }
            Expr::Index(array, index, ..) => {
                let array = self.expr(array, state, gate)?;
                let index = self.expr(index, state, gate)?;
                let element = frozen_element(&array.ty)
                    .ok_or_else(|| Unsupported.error("index requires immutable FrozenArray"))?
                    .clone();
                if index.ty != Type::I64 {
                    return Err(Unsupported.error("array index must be i64"));
                }
                let key = type_key(&array.ty);
                self.guard(
                    gate,
                    format!(
                        "(and (<= 0 {}) (< {} (len_{key} {})))",
                        index.text, index.text, array.text
                    ),
                )?;
                self.typed_node(
                    format!("(select (values_{key} {}) {})", array.text, index.text),
                    &element,
                )
            }
            Expr::FieldAccess(object, field, ..) => {
                let object = self.expr(object, state, gate)?;
                let fields = self.data.fields.get(&object.ty).ok_or_else(|| {
                    Unsupported.error("field access requires an immutable record")
                })?;
                let (index, ty) = fields
                    .get(field)
                    .ok_or_else(|| Unsupported.error("unknown record field"))?;
                let index = *index;
                let ty = ty.clone();
                self.typed_node(
                    format!("({} {})", selector(&object.ty, 0, index), object.text),
                    &ty,
                )
            }
            Expr::MethodCall(c) if c.args.is_empty() => {
                use crate::semantic::intrinsics::{Intrinsic, MethodName};
                // Check resolved intrinsic identity before encoding a length.
                // Unchecked comparison/test ASTs additionally check the receiver type.
                let intrinsic = self.data.calls.map(|calls| calls.get(&c.id).copied());
                let object = self.expr(&c.object, state, gate)?;
                if c.method.kind() == MethodName::Len {
                    if object.ty == Type::String
                        && intrinsic.is_none_or(|i| i == Some(Intrinsic::StringLen))
                    {
                        return self.node(format!("(str.len {})", object.text), false);
                    }
                    if frozen_element(&object.ty).is_some()
                        && intrinsic.is_none_or(|i| i == Some(Intrinsic::FrozenArrayLen))
                    {
                        return self.node(
                            format!("(len_{} {})", type_key(&object.ty), object.text),
                            false,
                        );
                    }
                    return Err(Unsupported.error("len requires immutable FrozenArray or String"));
                }
                Err(Unsupported.error("unsupported verification method"))
            }
            Expr::Match(m) => self.match_expr(m, state, gate, expected),
            Expr::Call(c) if matches!(c.callee.as_str(), "forall" | "exists") => {
                self.quantifier(c, state, gate)
            }
            Expr::Unary(u) => {
                let v = self.expr_expecting(&u.expr, state, gate, expected)?;
                match u.op {
                    UnaryOp::Neg if v.ty.integer_layout().is_some() => {
                        let r = self.typed_node(format!("(- {})", v.text), &v.ty)?;
                        self.guard(gate, data::domain(&r.ty, &r.text))?;
                        Ok(r)
                    }
                    UnaryOp::Not if v.boolean => self.node(format!("(not {})", v.text), true),
                    _ => Err(Unsupported.error("unsupported unary operation")),
                }
            }
            Expr::Binary(b) => self.binary_expr(b, state, gate, expected),
            Expr::Ternary(t) => {
                let c = self.expr(&t.condition, state, gate)?;
                if !c.boolean {
                    return Err(Unsupported.error("non-boolean condition"));
                }
                let yes = self.and(gate, &c)?;
                let no = self.node(format!("(and {} (not {}))", gate.text, c.text), true)?;
                let a = self.expr_expecting(&t.then_expr, state, &yes, expected)?;
                let b = self.expr_expecting(&t.else_expr, state, &no, Some(&a.ty))?;
                if a.ty != b.ty {
                    return Err(Unsupported.error("branch type mismatch"));
                }
                self.typed_node(format!("(ite {} {} {})", c.text, a.text, b.text), &a.ty)
            }
            Expr::Call(c) => self.call(c.id, &c.callee, &c.type_args, &c.args, state, gate),
            Expr::StaticCall(c) if self.data.targets.is_some_and(|t| t.contains_key(&c.id)) => self
                .call(
                    c.id,
                    &format!("{}::{}", c.class, c.method),
                    &c.type_args,
                    &c.args,
                    state,
                    gate,
                ),
            _ => Err(Unsupported.error("unsupported expression in i64/bool V1")),
        }
    }
    fn call(
        &mut self,
        expr_id: ExprId,
        callee: &str,
        type_args: &[Type],
        arguments: &[CallArg],
        state: &State,
        gate: &Value,
    ) -> VerificationResult<Value> {
        if !type_args.is_empty() || state.env.contains_key(callee) {
            return Err(Unsupported.error("generic or indirect call"));
        }
        let name = self
            .data
            .targets
            .and_then(|targets| targets.get(&expr_id))
            .map(String::as_str)
            .unwrap_or(callee);
        let &id = self
            .names
            .get(name)
            .ok_or_else(|| Unsupported.error("callee body unavailable"))?;
        let f = self.functions[id];
        if !has(f, EffectContractKind::Pure) {
            return Err(Unsupported.error("callee is not @pure"));
        }
        if arguments.len() != f.params.len() {
            return Err(Unsupported.error("call arity mismatch"));
        }
        let mut args = Vec::new();
        for (a, p) in arguments.iter().zip(&f.params) {
            if a.mode != CallArgMode::Value {
                return Err(Unsupported.error("reference argument"));
            }
            let v = self.expr_expecting(a, state, gate, Some(&p.ty))?;
            if v.ty != p.ty {
                return Err(Unsupported.error("argument type mismatch"));
            }
            args.push(v.text);
        }
        self.dependencies.insert(id);
        let id = format!("{}{}", self.data.symbol_namespace, symbol(f));
        let call = |suffix: &str| application(&format!("f{id}_{suffix}"), &args);
        self.guard(gate, call("pre_safe"))?;
        self.guard(gate, call("pre"))?;
        if !has(f, EffectContractKind::Verified) {
            self.guard(gate, call("safe"))?;
            self.guard(gate, call("post_safe"))?;
            // A runtime ensures clause is executed even for an unverified helper.
            // It is an obligation, never an assumed axiom.
            self.guard(gate, call("post"))?;
        }
        let result = self.typed_node(
            call(if has(f, EffectContractKind::Verified) {
                "contract"
            } else {
                "value"
            }),
            &f.return_type,
        )?;
        let assumption = if has(f, EffectContractKind::Verified) {
            let mut domain = vec![gate.text.clone(), call("pre_safe"), call("pre")];
            for (a, p) in args.iter().zip(&f.params) {
                domain.push(data::domain(&p.ty, a));
            }
            let mut post_args = args.clone();
            post_args.push(result.text.clone());
            let post = application(&format!("f{id}_ensures"), &post_args);
            let range = data::domain(&result.ty, &result.text);
            format!("(=> (and {}) (and {range} {post}))", domain.join(" "))
        } else {
            format!("(=> {} {})", gate.text, call("assume"))
        };
        let assumption = self.node(
            format!(
                "(and (=> {} {}) {assumption})",
                gate.text,
                call("pre_assume")
            ),
            true,
        )?;
        self.assumptions.push(assumption.text);
        Ok(result)
    }
    fn quantifier(
        &mut self,
        c: &CallExpr,
        state: &State,
        gate: &Value,
    ) -> VerificationResult<Value> {
        if c.args.len() != 3
            || !c.type_args.is_empty()
            || c.args.iter().any(|a| a.mode != CallArgMode::Value)
        {
            return Err(Unsupported.error("quantifier requires bounds and a predicate"));
        }
        let low = self.expr(&c.args[0], state, gate)?;
        let high = self.expr(&c.args[1], state, gate)?;
        if low.ty != Type::I64 || high.ty != Type::I64 {
            return Err(Unsupported.error("quantifier bounds must be i64"));
        }
        let Expr::Lambda(lambda) = &c.args[2].expr else {
            return Err(Unsupported.error("quantifier requires a lambda binder"));
        };
        if lambda.params.len() != 1
            || lambda.params[0]
                .ty
                .as_ref()
                .is_some_and(|t| *t != Type::I64)
        {
            return Err(Unsupported.error("quantifier binder must be i64"));
        }
        let LambdaBody::Expr(predicate) = &lambda.body else {
            return Err(Unsupported.error("quantifier predicate must be an expression"));
        };
        let first = self.nodes.len();
        let safe_first = self.safety.len();
        let assumption_first = self.assumptions.len();
        let symbol = format!("q{}", self.next_quantifier);
        self.next_quantifier += 1;
        let mut bound = state.clone();
        bound.env.insert(
            lambda.params[0].name.clone(),
            Value::literal(&symbol, false),
        );
        let range = format!(
            "(and (<= {} {symbol}) (< {symbol} {}))",
            low.text, high.text
        );
        let value = self.expr(predicate, &bound, &Value::literal(&range, true))?;
        if value.ty != Type::Bool || self.assumptions.len() != assumption_first {
            return Err(Unsupported.error("unsupported quantified predicate"));
        }
        let safety = self.safety.split_off(safe_first);
        let nodes = self.nodes.split_off(first);
        self.scoped_nodes += nodes.len();
        let wrap = |result: String| scoped_lets(&nodes, &result);
        let safe = wrap(format!("(and true {})", safety.join(" ")));
        let safe = self.fragment(format!("(forall (({symbol} Int)) {safe})"), first);
        self.guard(gate, safe)?;
        let formula = if c.callee == "forall" {
            format!("(=> {range} {})", value.text)
        } else {
            format!("(and {range} {})", value.text)
        };
        let formula = wrap(formula);
        let formula = self.fragment(format!("({} (({symbol} Int)) {formula})", c.callee), first);
        self.node(formula, true)
    }
    fn match_expr(
        &mut self,
        m: &MatchExpr,
        state: &State,
        gate: &Value,
        expected: Option<&Type>,
    ) -> VerificationResult<Value> {
        let value = self.expr(&m.scrutinee, state, gate)?;
        let mut remaining = gate.clone();
        let mut result: Option<Value> = None;
        for arm in &m.arms {
            let mut branch = state.clone();
            let condition = self.pattern_test(&arm.pattern, &value, &mut branch)?;
            let mut selected = self.and(&remaining, &condition)?;
            for guard in &arm.guard {
                let test = match guard {
                    Guard::If(guard) => {
                        let test = self.expr(guard, &branch, &selected)?;
                        if !test.boolean {
                            return Err(Unsupported.error("non-boolean match guard"));
                        }
                        test
                    }
                    Guard::Let(step) => {
                        let scrutinee = self.expr(&step.scrutinee, &branch, &selected)?;
                        self.pattern_test(&step.pattern, &scrutinee, &mut branch)?
                    }
                };
                selected = self.and(&selected, &test)?;
            }
            let MatchBody::Expr(expression) = &arm.body else {
                return Err(Unsupported.error("proof match requires expression arms"));
            };
            let branch_value = self.expr_expecting(expression, &branch, &selected, expected)?;
            result = Some(if let Some(previous) = result {
                if previous.ty != branch_value.ty {
                    return Err(Unsupported.error("match result type mismatch"));
                }
                self.typed_node(
                    format!(
                        "(ite {} {} {})",
                        selected.text, branch_value.text, previous.text
                    ),
                    &previous.ty,
                )?
            } else {
                self.typed_node(
                    format!(
                        "(ite {} {} {})",
                        selected.text,
                        branch_value.text,
                        default_value(&branch_value.ty)
                    ),
                    &branch_value.ty,
                )?
            });
            remaining = self.node(
                format!("(and {} (not {}))", remaining.text, selected.text),
                true,
            )?;
        }
        self.guard(gate, format!("(not {})", remaining.text))?;
        result.ok_or_else(|| Unsupported.error("empty match"))
    }

    /// The condition under which `value` matches `pattern`, binding the
    /// pattern's names in `branch`. Payloads nest by recursion over the
    /// pattern, whose depth the parser bounds; each node adds one VC node.
    /// Unqualified spellings resolve against `value`'s enum as the checker
    /// resolves them: `Some(x)` and `Circle(r)` as that enum's variant, a
    /// bare name as its fieldless variant.
    fn pattern_test(
        &mut self,
        pattern: &Pattern,
        value: &Value,
        branch: &mut State,
    ) -> VerificationResult<Value> {
        let variant_index = |this: &Self, name: &str| {
            this.data
                .variants
                .get(&value.ty)
                .and_then(|variants| variants.get(name))
                .copied()
        };
        let binding_field;
        let (enum_name, variant, fields): (&str, &str, &[Pattern]) = match pattern {
            Pattern::Wildcard(..) => return Ok(Value::literal("true", true)),
            Pattern::Binding { name, .. } => {
                let fieldless = variant_index(self, name).is_some_and(|i| {
                    self.data
                        .constructors
                        .get(&value.ty)
                        .is_some_and(|ctors| ctors[i].fields.is_empty())
                });
                if !fieldless {
                    branch.env.insert(name.clone(), value.clone());
                    return Ok(Value::literal("true", true));
                }
                ("", name, &[])
            }
            Pattern::LiteralBool(b, ..) if value.ty == Type::Bool => {
                return self.node(format!("(= {} {b})", value.text), true);
            }
            Pattern::LiteralChar(c, ..) if value.ty == Type::Char => {
                return self.node(
                    format!("(= {} {})", value.text, integer(u32::from(*c))),
                    true,
                );
            }
            Pattern::LiteralInt(n, ..) if value.ty.integer_layout().is_some() => {
                return self.node(format!("(= {} {})", value.text, integer(*n)), true);
            }
            Pattern::EnumVariant {
                enum_name, variant, ..
            } => (enum_name, variant, &[]),
            Pattern::EnumVariantTuple {
                enum_name,
                variant,
                fields,
                ..
            } => (enum_name, variant, fields),
            Pattern::ClassDowncast {
                class_name,
                binding,
                span,
                ..
            } if variant_index(self, class_name).is_some() => {
                binding_field = [if binding == "_" {
                    Pattern::Wildcard(*span, PatternId::fresh())
                } else {
                    Pattern::Binding {
                        name: binding.clone(),
                        span: *span,
                        id: PatternId::fresh(),
                    }
                }];
                ("", class_name, &binding_field)
            }
            _ => return Err(Unsupported.error("unsupported match pattern")),
        };
        if !enum_name.is_empty() && value.ty != Type::Named(enum_name.to_owned()) {
            return Err(Unsupported.error("unsupported enum pattern type"));
        }
        let i = variant_index(self, variant)
            .ok_or_else(|| Unsupported.error("unknown enum variant"))?;
        let field_types: Vec<Type> = self
            .data
            .constructors
            .get(&value.ty)
            .ok_or_else(|| Unsupported.error("unsupported enum"))?[i]
            .fields
            .iter()
            .map(|(_, ty)| ty.clone())
            .collect();
        if fields.len() != field_types.len() {
            return Err(Unsupported.error(if fields.is_empty() {
                "enum payload required"
            } else {
                "enum pattern arity mismatch"
            }));
        }
        let mut condition = self.node(
            format!("((_ is {}) {})", constructor(&value.ty, i), value.text),
            true,
        )?;
        for (j, (field, ty)) in fields.iter().zip(field_types).enumerate() {
            let field_value = Value {
                text: format!("({} {})", selector(&value.ty, i, j), value.text),
                boolean: ty == Type::Bool,
                ty,
            };
            let test = self.pattern_test(field, &field_value, branch)?;
            if test.text != "true" {
                condition = self.and(&condition, &test)?;
            }
        }
        Ok(condition)
    }

    fn block(&mut self, block: &Block, state: &mut State, depth: usize) -> VerificationResult<()> {
        if depth > 256 {
            return Err(Unsupported.error("block depth budget exceeded"));
        }
        let mut locals = std::collections::BTreeMap::new();
        for stmt in &block.stmts {
            match stmt {
                Stmt::Let(s) => {
                    let v =
                        self.expr_expecting(&s.init, state, &state.alive.clone(), s.ty.as_ref())?;
                    if let Some(t) = &s.ty
                        && v.ty != *t
                    {
                        return Err(Unsupported.error("local type mismatch"));
                    }
                    locals
                        .entry(s.name.clone())
                        .or_insert_with(|| state.env.get(&s.name).cloned());
                    state.env.insert(s.name.clone(), v);
                }
                Stmt::Assign(s) => {
                    let old = state
                        .env
                        .get(&s.name)
                        .ok_or_else(|| Unsupported.error("unknown assignment"))?
                        .clone();
                    let v =
                        self.expr_expecting(&s.value, state, &state.alive.clone(), Some(&old.ty))?;
                    if old.ty != v.ty {
                        return Err(Unsupported.error("assignment type mismatch"));
                    }
                    let merged = self.typed_node(
                        format!("(ite {} {} {})", state.alive.text, v.text, old.text),
                        &old.ty,
                    )?;
                    state.env.insert(s.name.clone(), merged);
                }
                Stmt::If(s) => {
                    let c = self.expr(&s.cond, state, &state.alive.clone())?;
                    if !c.boolean {
                        return Err(Unsupported.error("non-boolean condition"));
                    }
                    let incoming = state.alive.clone();
                    state.alive = self.and(&incoming, &c)?;
                    self.block(&s.then_block, state, depth + 1)?;
                    let then_alive = state.alive.clone();
                    state.alive =
                        self.node(format!("(and {} (not {}))", incoming.text, c.text), true)?;
                    if let Some(block) = &s.else_block {
                        self.block(block, state, depth + 1)?;
                    }
                    state.alive = self.node(
                        format!("(or {} {})", then_alive.text, state.alive.text),
                        true,
                    )?;
                }
                Stmt::While(s) => self.loop_body(
                    LoopShape {
                        cond: &s.cond,
                        invariants: &s.invariants,
                        variant: s.variant.as_ref(),
                        body: &s.body,
                        span: s.span,
                        step: None,
                        element: None,
                        bounds: None,
                    },
                    state,
                    depth + 1,
                )?,
                Stmt::For(s) => self.for_loop(s, state, depth + 1)?,
                Stmt::Break(_) | Stmt::Continue(_) => {
                    let (broken, continued) = self
                        .jumps
                        .last()
                        .cloned()
                        .ok_or_else(|| Unsupported.error("jump outside loop"))?;
                    let is_break = matches!(stmt, Stmt::Break(_));
                    let old = if is_break { &broken } else { &continued };
                    let merged =
                        self.node(format!("(or {} {})", old.text, state.alive.text), true)?;
                    *self.jumps.last_mut().unwrap() = if is_break {
                        (merged, continued)
                    } else {
                        (broken, merged)
                    };
                    state.alive = Value::literal("false", true);
                }
                Stmt::Return(s) => {
                    let v = self.expr_expecting(
                        s.value
                            .as_ref()
                            .ok_or_else(|| Unsupported.error("void return outside V1"))?,
                        state,
                        &state.alive.clone(),
                        Some(&state.result.ty),
                    )?;
                    if v.ty != state.result.ty {
                        return Err(Unsupported.error("return type mismatch"));
                    }
                    state.result = self.typed_node(
                        format!(
                            "(ite {} {} {})",
                            state.alive.text, v.text, state.result.text
                        ),
                        &v.ty,
                    )?;
                    state.returned = self.node(
                        format!("(or {} {})", state.returned.text, state.alive.text),
                        true,
                    )?;
                    state.alive = Value::literal("false", true);
                }
                Stmt::Expr(s) => {
                    self.expr(&s.expr, state, &state.alive.clone())?;
                }
                _ => {
                    return Err(
                        Unsupported.error("unsupported statement: verification rejects for, break, continue, defer and heap operations"),
                    );
                }
            }
        }
        for (name, saved) in locals {
            if let Some(v) = saved {
                state.env.insert(name, v);
            } else {
                state.env.remove(&name);
            }
        }
        Ok(())
    }
    fn loop_annotation(
        &mut self,
        expr: &Expr,
        state: &State,
        gate: &Value,
        index: Option<&str>,
    ) -> VerificationResult<Value> {
        if let Some(index) = index {
            let mut annotated = state.clone();
            annotated
                .env
                .insert("iteration".into(), state.env[index].clone());
            self.expr(expr, &annotated, gate)
        } else {
            self.expr(expr, state, gate)
        }
    }
    fn loop_invariant(
        &mut self,
        invariants: &[Expr],
        state: &State,
        gate: &Value,
        index: Option<&str>,
        automatic: Option<(&str, &Value, &Value)>,
    ) -> VerificationResult<Value> {
        let mut result = gate.clone();
        if let Some((counter, start, upper)) = automatic {
            let counter = &state.env[counter].text;
            let value = self.node(
                format!(
                    "(and (<= {} {counter}) (<= {counter} {}))",
                    start.text, upper.text
                ),
                true,
            )?;
            result = self.and(&result, &value)?;
        }
        for expr in invariants {
            let value = self.loop_annotation(expr, state, &result, index)?;
            if !value.boolean {
                return Err(Unsupported.error("loop invariant must have type bool"));
            }
            result = self.and(&result, &value)?;
        }
        Ok(result)
    }

    fn loop_body(
        &mut self,
        shape: LoopShape<'_>,
        state: &mut State,
        depth: usize,
    ) -> VerificationResult<()> {
        let LoopShape {
            cond,
            invariants,
            variant,
            body: body_block,
            span,
            step,
            element,
            bounds: automatic_bounds,
        } = shape;
        if !self.verified {
            return Err(Unsupported.error("a helper containing loops must itself be @verified"));
        }
        let variant = variant
            .filter(|_| automatic_bounds.is_some() || !invariants.is_empty())
            .ok_or_else(|| {
                Unsupported.error("while verification requires both invariant and variant")
            })?;
        let incoming = state.alive.clone();
        let annotation_index = element.and(step);
        let automatic =
            automatic_bounds.map(|(start, upper)| (step.expect("for counter"), start, upper));
        let entry =
            self.loop_invariant(invariants, state, &incoming, annotation_index, automatic)?;
        self.guard(&incoming, entry.text)?;

        // One arbitrary loop head represents every iteration. Only writes to
        // bindings visible at the head are forgotten; lexical locals stay local.
        let mut head = state.clone();
        let mut writes = self.loop_writes.remove(&body_block.id).unwrap_or_default();
        if let Some(name) = step {
            writes.insert(name.to_owned());
        }
        let written = writes.clone();
        for name in writes {
            let Some(old) = head.env.get(&name) else {
                continue;
            };
            let value = Value::literal(
                format!("h{}_{}", self.function_id, self.havoc.len()),
                old.boolean,
            );
            self.loop_symbols.insert(
                value.text.clone(),
                QualifiedName::join(
                    format_args!("loop@{}:{}", span.line, span.col),
                    if annotation_index == Some(name.as_str()) {
                        "iteration"
                    } else {
                        &name
                    },
                ),
            );
            let mut value = value;
            value.ty = old.ty.clone();
            self.havoc.push(value.clone());
            if value.ty != Type::Bool {
                let domain = self.node(domain(&value.ty, &value.text), true)?;
                head.alive = self.and(&head.alive, &domain)?;
            }
            let previous = head.env.get(&name).unwrap();
            let merged = self.typed_node(
                format!("(ite {} {} {})", incoming.text, value.text, previous.text),
                &value.ty,
            )?;
            head.env.insert(name, merged);
        }
        head.alive = self.loop_invariant(
            invariants,
            &head,
            &head.alive.clone(),
            annotation_index,
            automatic,
        )?;
        let condition = self.expr(cond, &head, &head.alive.clone())?;
        if !condition.boolean {
            return Err(Unsupported.error("non-boolean loop condition"));
        }
        let mut body = head.clone();
        body.alive = self.and(&head.alive, &condition)?;
        let rank = self.loop_annotation(variant, &head, &body.alive.clone(), annotation_index)?;
        if rank.ty != Type::I64 {
            return Err(Unsupported.error("loop variant must have type i64"));
        }
        self.guard(&body.alive, format!("(<= 0 {})", rank.text))?;
        if let Some((binding, array)) = element {
            let index = &head.env[step.expect("array iteration index")];
            let ty = frozen_element(&array.ty).expect("frozen element");
            let value = self.typed_node(
                format!(
                    "(select (values_{} {}) {})",
                    type_key(&array.ty),
                    array.text,
                    index.text
                ),
                ty,
            )?;
            body.env.insert(binding.to_owned(), value);
        }
        self.jumps
            .push((Value::literal("false", true), Value::literal("false", true)));
        self.block(body_block, &mut body, depth)?;
        let (broken, continued) = self.jumps.pop().unwrap();
        body.alive = self.node(format!("(or {} {})", body.alive.text, continued.text), true)?;
        if let Some(name) = step {
            let old = body.env[name].clone();
            let next = self.node(format!("(+ {} 1)", old.text), false)?;
            self.guard(&body.alive, bounds(&next.text))?;
            let next = self.node(
                format!("(ite {} {} {})", body.alive.text, next.text, old.text),
                false,
            )?;
            body.env.insert(name.to_owned(), next);
        }
        let preserved = self.loop_invariant(
            invariants,
            &body,
            &body.alive.clone(),
            annotation_index,
            automatic,
        )?;
        self.guard(&body.alive, preserved.text)?;
        let next = self.loop_annotation(variant, &body, &body.alive.clone(), annotation_index)?;
        if next.ty != Type::I64 {
            return Err(Unsupported.error("loop variant must have type i64"));
        }
        self.guard(&body.alive, format!("(< {} {})", next.text, rank.text))?;

        // Backedges end the induction step, not the function. Only the false
        // condition continues after the loop; returns in the body remain exits.
        head.alive = self.node(
            format!("(and {} (not {}))", head.alive.text, condition.text),
            true,
        )?;
        for name in written {
            if let (Some(old), Some(exit)) = (head.env.get(&name), body.env.get(&name)) {
                let merged = self.typed_node(
                    format!("(ite {} {} {})", broken.text, exit.text, old.text),
                    &old.ty,
                )?;
                head.env.insert(name, merged);
            }
        }
        head.alive = self.node(format!("(or {} {})", head.alive.text, broken.text), true)?;
        head.result = body.result;
        head.returned = body.returned;
        *state = head;
        Ok(())
    }

    fn for_loop(&mut self, s: &ForStmt, state: &mut State, depth: usize) -> VerificationResult<()> {
        let array;
        let (start, end, counter, element) = if let Expr::Range(range) = &s.iterable {
            let start = self.expr(&range.start, state, &state.alive.clone())?;
            let end = self.expr(&range.end, state, &state.alive.clone())?;
            (start, end, s.name.clone(), None)
        } else {
            array = self.expr(&s.iterable, state, &state.alive.clone())?;
            frozen_element(&array.ty)
                .ok_or_else(|| Unsupported.error("for requires range or immutable FrozenArray"))?;
            // An element does not exist at the empty/exhausted loop head.
            // Head invariants/ranks use outer bindings, not the element binder.
            for expression in s.invariants.iter().chain(s.variant.iter()) {
                if AstWalk::new(AstEvent::Expr(expression))
                    .any(|e| matches!(e,AstEvent::Expr(Expr::Var(name,..)) if *name == s.name))
                {
                    return Err(Unsupported.error("array loop annotations cannot reference the element binder at an exhausted head"));
                }
            }
            let end = self.node(
                format!("(len_{} {})", type_key(&array.ty), array.text),
                false,
            )?;
            (
                Value::literal("0", false),
                end,
                format!("$for_index_{}", s.body.id),
                Some((s.name.as_str(), &array)),
            )
        };
        if start.ty != Type::I64 || end.ty != Type::I64 {
            return Err(Unsupported.error("non-integer for range"));
        }
        // Bounds are snapshots, not expressions re-evaluated at an arbitrary
        // head. max(start, end) also admits the initial head of reversed ranges.
        let upper = self.node(
            format!(
                "(ite (< {} {}) {} {})",
                start.text, end.text, end.text, start.text
            ),
            false,
        )?;
        let saved = state.env.insert(counter.clone(), start.clone());
        let saved_element = state.env.get(&s.name).cloned();
        let limit = format!("$for_end_{}", s.body.id);
        state.env.insert(limit.clone(), end);
        let cond = Expr::Binary(Box::new(BinaryExpr {
            id: ExprId::fresh(),
            lhs: Expr::Var(counter.clone(), s.span, ExprId::fresh()),
            op: BinOp::Lt,
            rhs: Expr::Var(limit.clone(), s.span, ExprId::fresh()),
            span: s.span,
        }));
        let default_variant = Expr::Binary(Box::new(BinaryExpr {
            id: ExprId::fresh(),
            lhs: Expr::Var(limit.clone(), s.span, ExprId::fresh()),
            op: BinOp::Sub,
            rhs: Expr::Var(counter.clone(), s.span, ExprId::fresh()),
            span: s.span,
        }));
        self.inferred_loop_annotations.push(InferredLoopAnnotations {
            location: s.span,
            kind: if element.is_some() { "frozen_array" } else { "range" },
            counter: if element.is_some() { "iteration".into() } else { counter.clone() },
            captured_start: if let Expr::Range(range) = &s.iterable {
                contract_text(&range.start)
            } else {
                "0".into()
            },
            captured_end: if let Expr::Range(range) = &s.iterable {
                contract_text(&range.end)
            } else {
                format!("({}).len()", contract_text(&s.iterable))
            },
            invariant: "captured_start <= counter && counter <= max(captured_start, captured_end)",
            variant: s.variant.is_none().then_some("captured_end - counter"),
        });
        self.loop_body(
            LoopShape {
                cond: &cond,
                invariants: &s.invariants,
                variant: Some(s.variant.as_ref().unwrap_or(&default_variant)),
                body: &s.body,
                span: s.span,
                step: Some(&counter),
                element,
                bounds: Some((&start, &upper)),
            },
            state,
            depth,
        )?;
        state.env.remove(&limit);
        if let Some(saved) = saved {
            state.env.insert(counter.clone(), saved);
        } else {
            state.env.remove(&counter);
        }
        if element.is_some() {
            if let Some(saved) = saved_element {
                state.env.insert(s.name.clone(), saved);
            } else {
                state.env.remove(&s.name);
            }
        }
        Ok(())
    }

    fn clauses(&mut self, clauses: &[Expr], state: &State) -> VerificationResult<Value> {
        let mut gate = Value::literal("true", true);
        for expr in clauses {
            let value = self.expr(expr, state, &gate)?;
            if !value.boolean {
                return Err(Unsupported.error("non-boolean contract"));
            }
            gate = self.and(&gate, &value)?;
        }
        Ok(gate)
    }
    fn safety(&mut self) -> String {
        let terms = std::mem::take(&mut self.safety);
        if terms.is_empty() {
            "true".into()
        } else {
            format!("(and {})", terms.join(" "))
        }
    }
    // Retain nested quantified text as an acyclic fragment graph. Each binder
    // manipulates only its own text; final emission expands into one output sink.
    // The marker carries free let-node dependencies for scoped liveness, and is
    // never emitted to the solver.
    fn fragment(&mut self, text: String, first_local: usize) -> String {
        let mut dependencies = BTreeSet::new();
        for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if let Some(index) = token
                .strip_prefix('v')
                .and_then(|n| n.parse::<usize>().ok())
                && index < first_local
            {
                dependencies.insert(token);
            }
        }
        let marker = format!(
            "\u{1f}{}:{}\u{1f}",
            self.fragments.len(),
            dependencies.into_iter().collect::<Vec<_>>().join(",")
        );
        self.fragments.push(text);
        marker
    }
    fn emit_fragment(&self, text: &str, output: &mut String) {
        let mut pending = vec![text];
        while let Some(mut text) = pending.pop() {
            if let Some(start) = text.find('\u{1f}') {
                output.push_str(&text[..start]);
                text = &text[start + 1..];
                let end = text.find('\u{1f}').expect("fragment marker end");
                let id: usize = text[..end]
                    .split(':')
                    .next()
                    .unwrap()
                    .parse()
                    .expect("fragment id");
                pending.push(&text[end + 1..]);
                pending.push(&self.fragments[id]);
            } else {
                output.push_str(text);
            }
        }
    }
    fn definition(&self, name: &str, params: &str, ty: &str, result: &str) -> String {
        let mut s = format!("(define-fun {name} ({params}) {ty} ");
        for (v, expr) in &self.nodes {
            s.push_str(&format!("(let (({} ", v.text));
            self.emit_fragment(expr, &mut s);
            s.push_str(")) ");
        }
        s.push_str(result);
        s.extend(std::iter::repeat_n(')', self.nodes.len() + 1));
        s.push('\n');
        s
    }
}
// Keep quantified safety and truth DAGs separate. Copying every local let into
// both formulas doubles nested quantified bodies at each binder.
fn scoped_lets(nodes: &[(Value, String)], result: &str) -> String {
    let mut needed = std::collections::HashSet::new();
    fn references(text: &str, needed: &mut std::collections::HashSet<String>) {
        for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if token
                .strip_prefix('v')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            {
                needed.insert(token.to_owned());
            }
        }
    }
    references(result, &mut needed);
    let mut selected = vec![false; nodes.len()];
    for (i, (value, expr)) in nodes.iter().enumerate().rev() {
        if needed.contains(&value.text) {
            selected[i] = true;
            references(expr, &mut needed);
        }
    }
    let mut text = String::new();
    let mut count = 0;
    for ((value, expr), selected) in nodes.iter().zip(selected) {
        if selected {
            text += &format!("(let (({} {expr})) ", value.text);
            count += 1;
        }
    }
    text += result;
    text.extend(std::iter::repeat_n(')', count));
    text
}

pub(super) fn application(name: &str, args: &[String]) -> String {
    if args.is_empty() {
        name.into()
    } else {
        format!("({name} {})", args.join(" "))
    }
}
pub(super) fn encode(
    id: usize,
    functions: &[&FunctionDecl],
    names: &HashMap<&str, usize>,
    data: &DataTypes,
) -> VerificationResult<Summary> {
    let f = functions[id];
    if !f.type_params.is_empty() || f.is_async() || f.rust_bridge.is_some() {
        return Err(Unsupported.error("generic, async or external function outside V1"));
    }
    data.check(&f.return_type)?;
    let ret = sort(&f.return_type)?;
    let mut params = Vec::new();
    let mut env = im::HashMap::new();
    for (i, p) in f.params.iter().enumerate() {
        if p.mode != ParamMode::Value {
            return Err(Unsupported.error("reference parameter outside V1"));
        }
        data.check(&p.ty)?;
        let ty = sort(&p.ty)?;
        params.push(format!("(p{i} {ty})"));
        env.insert(
            p.name.clone(),
            Value {
                text: format!("p{i}"),
                boolean: ty == "Bool",
                ty: p.ty.clone(),
            },
        );
    }
    let mut state = State {
        env,
        alive: Value::literal("true", true),
        result: Value {
            text: default_value(&f.return_type),
            boolean: ret == "Bool",
            ty: f.return_type.clone(),
        },
        returned: Value::literal("false", true),
    };
    let initial = state.clone();
    let mut e = Encoder::new(id, functions, names, data);
    let id = format!("{}{}", data.symbol_namespace, symbol(f));
    let pre = e.clauses(&f.requires, &state)?;
    let pre_safe = e.safety();
    let mut definitions = e.definition(&format!("f{id}_pre"), &params.join(" "), "Bool", &pre.text);
    definitions += &e.definition(
        &format!("f{id}_pre_safe"),
        &params.join(" "),
        "Bool",
        &pre_safe,
    );
    definitions += &e.definition(
        &format!("f{id}_pre_assume"),
        &params.join(" "),
        "Bool",
        &format!("(and true {})", e.assumptions.join(" ")),
    );
    let mut contract_definitions = definitions.clone();
    let mut contract_dependencies = e.dependencies.clone();
    index_loop_writes(&f.body, &mut e.loop_writes, 0)?;
    e.block(&f.body, &mut state, 0)?;
    for h in &e.havoc {
        definitions += &format!("(declare-const {} {})\n", h.text, sort(&h.ty)?);
    }
    e.guard(
        &Value::literal("true", true),
        format!("(not {})", state.alive.text),
    )?;
    let safety = e.safety();

    definitions += &e.definition(
        &format!("f{id}_value"),
        &params.join(" "),
        &ret,
        &state.result.text,
    );
    definitions += &e.definition(&format!("f{id}_safe"), &params.join(" "), "Bool", &safety);
    let mut post_state = initial.clone();
    post_state.env.insert("result".into(), state.result);
    // Arbitrary heads outside an invariant describe no execution. Check the
    // postcondition only on paths that actually returned, including loop returns.
    let mut post = state.returned.clone();
    for clause in &f.ensures {
        let value = e.expr(clause, &post_state, &post)?;
        if !value.boolean {
            return Err(Unsupported.error("non-boolean contract"));
        }
        post = e.and(&post, &value)?;
    }
    let post = e.node(format!("(=> {} {})", state.returned.text, post.text), true)?;
    let post_safe = e.safety();
    let assumptions = format!("(and true {})", e.assumptions.join(" "));
    definitions += &e.definition(
        &format!("f{id}_assume"),
        &params.join(" "),
        "Bool",
        &assumptions,
    );
    definitions += &e.definition(
        &format!("f{id}_post"),
        &params.join(" "),
        "Bool",
        &post.text,
    );
    definitions += &e.definition(
        &format!("f{id}_post_safe"),
        &params.join(" "),
        "Bool",
        &post_safe,
    );
    // Separate contract predicate, with an arbitrary return, for callers.
    let mut c = Encoder::new(names[f.name.as_str()], functions, names, data);
    let mut cs = initial;
    cs.env.insert(
        "result".into(),
        Value {
            text: "returned".into(),
            boolean: ret == "Bool",
            ty: f.return_type.clone(),
        },
    );
    let cp = c.clauses(&f.ensures, &cs)?;
    let safe = c.safety();
    params.push(format!("(returned {ret})"));
    definitions += &c.definition(
        &format!("f{id}_ensures"),
        &params.join(" "),
        "Bool",
        &format!("(and {safe} {} {})", cp.text, c.assumptions.join(" ")),
    );
    contract_definitions += &c.definition(
        &format!("f{id}_ensures"),
        &params.join(" "),
        "Bool",
        &format!("(and {safe} {} {})", cp.text, c.assumptions.join(" ")),
    );
    contract_dependencies.extend(&c.dependencies);
    Ok(Summary {
        contract_dependencies,
        #[cfg(test)]
        assembly_bytes: e
            .fragments
            .iter()
            .chain(&c.fragments)
            .map(String::len)
            .sum::<usize>()
            + e.nodes
                .iter()
                .chain(&c.nodes)
                .map(|(_, text)| text.len())
                .sum::<usize>(),
        data_digest: data.digest.clone(),
        contract_definitions,
        definitions,
        nodes: e.nodes.len() + c.nodes.len() + e.scoped_nodes + c.scoped_nodes,
        dependencies: e.dependencies,
        loop_symbols: e.loop_symbols,
        inferred_loop_annotations: e.inferred_loop_annotations,
    })
}
// Summarize lexical writes once, bottom-up. Nested loops reuse their summary;
// no enclosing loop rescans a nested body. Set merging is small-to-large.
fn index_loop_writes(
    block: &Block,
    index: &mut HashMap<BodyId, BTreeSet<String>>,
    depth: usize,
) -> VerificationResult<BTreeSet<String>> {
    if depth > 256 {
        return Err(Unsupported.error("block depth budget exceeded"));
    }
    let mut writes = BTreeSet::new();
    let mut locals = BTreeSet::new();
    for stmt in &block.stmts {
        let mut child = match stmt {
            Stmt::Let(s) => {
                locals.insert(s.name.clone());
                BTreeSet::new()
            }
            Stmt::Assign(s) => BTreeSet::from([s.name.clone()]),
            Stmt::If(s) => {
                let mut names = index_loop_writes(&s.then_block, index, depth + 1)?;
                if let Some(b) = &s.else_block {
                    let mut other = index_loop_writes(b, index, depth + 1)?;
                    if names.len() < other.len() {
                        std::mem::swap(&mut names, &mut other);
                    }
                    names.extend(other);
                }
                names
            }
            Stmt::For(s) => {
                let mut names = index_loop_writes(&s.body, index, depth + 1)?;
                names.remove(&s.name);
                index.insert(s.body.id, names.clone());
                names
            }
            Stmt::While(s) => {
                let names = index_loop_writes(&s.body, index, depth + 1)?;
                index.insert(s.body.id, names.clone());
                names
            }
            _ => BTreeSet::new(),
        };
        if locals.len() < child.len() {
            for local in &locals {
                child.remove(local);
            }
        } else {
            child.retain(|name| !locals.contains(name));
        }
        if writes.len() < child.len() {
            std::mem::swap(&mut writes, &mut child);
        }
        writes.extend(child);
    }
    Ok(writes)
}

// Scan call edges once, including unreachable statements: recursion is not V1.
pub(super) fn resolved_edges(
    f: &FunctionDecl,
    names: &HashMap<&str, usize>,
    targets: Option<&HashMap<ExprId, String>>,
) -> BTreeSet<usize> {
    AstWalk::new(AstEvent::Block(&f.body))
        .chain(
            f.requires
                .iter()
                .chain(&f.ensures)
                .flat_map(|e| AstWalk::new(AstEvent::Expr(e))),
        )
        .filter_map(|event| match event {
            AstEvent::Expr(e @ (Expr::Call(_) | Expr::StaticCall(_)))
                if targets.is_some_and(|t| t.contains_key(&e.id())) =>
            {
                targets
                    .and_then(|t| t.get(&e.id()))
                    .and_then(|n| names.get(n.as_str()))
                    .copied()
            }
            AstEvent::Expr(Expr::Call(c)) => names.get(c.callee.as_str()).copied(),
            _ => None,
        })
        .collect()
}

/// Contract-only predicates use arbitrary inputs and return, never the edited body.
pub(super) fn contract_predicates(
    id: usize,
    functions: &[&FunctionDecl],
    names: &HashMap<&str, usize>,
    data: &DataTypes,
) -> VerificationResult<String> {
    let f = functions[id];
    if !f.type_params.is_empty() || f.is_async() || f.rust_bridge.is_some() {
        return Err(Unsupported.error("unsupported contract signature"));
    }
    let mut params = Vec::new();
    let mut env = im::HashMap::new();
    for (i, p) in f.params.iter().enumerate() {
        if p.mode != ParamMode::Value {
            return Err(Unsupported.error("reference contract parameter"));
        }
        data.check(&p.ty)?;
        let ty = sort(&p.ty)?;
        params.push(format!("(p{i} {ty})"));
        env.insert(
            p.name.clone(),
            Value {
                text: format!("p{i}"),
                boolean: ty == "Bool",
                ty: p.ty.clone(),
            },
        );
    }
    data.check(&f.return_type)?;
    let ret = sort(&f.return_type)?;
    let mut state = State {
        env,
        alive: Value::literal("true", true),
        result: Value {
            text: "returned".into(),
            boolean: ret == "Bool",
            ty: f.return_type.clone(),
        },
        returned: Value::literal("true", true),
    };
    let mut e = Encoder::new(id, functions, names, data);
    let pre = e.clauses(&f.requires, &state)?;
    let safety = e.safety();
    let assumptions = format!("(and true {})", e.assumptions.join(" "));
    let mut out = e.definition(&format!("c{id}_pre"), &params.join(" "), "Bool", &pre.text);
    out += &e.definition(
        &format!("c{id}_pre_safe"),
        &params.join(" "),
        "Bool",
        &safety,
    );
    out += &e.definition(
        &format!("c{id}_pre_assume"),
        &params.join(" "),
        "Bool",
        &assumptions,
    );
    e.nodes.clear();
    e.assumptions.clear();
    state.env.insert("result".into(), state.result.clone());
    params.push(format!("(returned {ret})"));
    let post = e.clauses(&f.ensures, &state)?;
    let safety = e.safety();
    out += &e.definition(
        &format!("c{id}_post"),
        &params.join(" "),
        "Bool",
        &post.text,
    );
    out += &e.definition(
        &format!("c{id}_post_safe"),
        &params.join(" "),
        "Bool",
        &safety,
    );
    out += &e.definition(
        &format!("c{id}_post_assume"),
        &params.join(" "),
        "Bool",
        &format!("(and true {})", e.assumptions.join(" ")),
    );
    Ok(out)
}

#[cfg(test)]
mod synthetic_loop_tests {
    use super::*;

    fn parse(source: &str) -> Program {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        program
    }

    #[test]
    fn range_counter_prefix_does_not_change_its_display_kind() {
        let mut program = parse("@pure @verified fn f()->i64 {for index in 0..2 {} return 0;}");
        let Item::Function(function) = &mut program.items[0] else {
            panic!("function");
        };
        let Stmt::For(range) = &mut function.body.stmts[0] else {
            panic!("range loop");
        };
        range.name = "$for_index_source".into();
        let summary = encode(
            0,
            &[function],
            &HashMap::from([("f", 0)]),
            &DataTypes::default(),
        )
        .unwrap();
        assert_eq!(summary.loop_symbols.len(), 1);
        assert!(
            summary
                .loop_symbols
                .values()
                .all(|name| { QualifiedName::new(name).last_segment() == "$for_index_source" })
        );
        assert_eq!(
            summary.inferred_loop_annotations[0].counter,
            "$for_index_source"
        );
    }

    #[test]
    fn nested_array_counter_names_and_mutation_labels_scale_linearly() {
        for count in [8, 16, 32] {
            let mut source =
                String::from("@pure @verified fn f(a:FrozenArray<i64>)->i64 {let mut total=0;");
            for index in 0..count {
                source += &format!("for value{index} in a {{");
            }
            source += "total=total+1;";
            source += &"}".repeat(count);
            source += "return total;}";
            let program = parse(&source);
            let writes = crate::semantic::field_writes::FieldWrites::from_program(&program);
            let data = DataTypes::new(&program, &writes);
            let Item::Function(function) = &program.items[0] else {
                panic!("function");
            };
            let summary = encode(0, &[function], &HashMap::from([("f", 0)]), &data).unwrap();
            let mut iterations = 0;
            let mut mutations = 0;
            for name in summary.loop_symbols.values() {
                match QualifiedName::new(name).last_segment() {
                    "iteration" => iterations += 1,
                    "total" => mutations += 1,
                    other => panic!("unexpected loop label: {other}"),
                }
            }
            assert_eq!(iterations, count);
            assert_eq!(mutations, count);
            assert_eq!(summary.inferred_loop_annotations.len(), count);
            assert!(summary.nodes < count * 45 + 50);
        }
    }
}
