use super::*;
use crate::parser::iter::{AstEvent, AstWalk};
use crate::qualified_name::QualifiedName;

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
pub(super) fn integer(n: i64) -> String {
    if n < 0 {
        format!("(- {})", n.unsigned_abs())
    } else {
        n.to_string()
    }
}
pub(super) fn bounds(v: &str) -> String {
    format!("(and (<= (- 9223372036854775808) {v}) (<= {v} 9223372036854775807))")
}
pub(super) fn has(f: &FunctionDecl, k: EffectContractKind) -> bool {
    f.effect_contracts.iter().any(|c| c.kind == k)
}
pub(super) struct Summary {
    #[cfg(test)]
    pub assembly_bytes: usize,
    pub data_digest: String,
    pub definitions: String,
    pub contract_definitions: String,
    pub nodes: usize,
    pub dependencies: BTreeSet<usize>,
    pub loop_symbols: std::collections::BTreeMap<String, String>,
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
struct Encoder<'a> {
    data: &'a DataTypes,
    functions: &'a [&'a FunctionDecl],
    names: &'a HashMap<&'a str, usize>,
    nodes: Vec<(Value, String)>,
    safety: Vec<String>,
    assumptions: Vec<String>,
    dependencies: BTreeSet<usize>,
    depth: usize,
    function_id: usize,
    havoc: Vec<Value>,
    loop_symbols: std::collections::BTreeMap<String, String>,
    loop_writes: HashMap<BodyId, BTreeSet<String>>,
    verified: bool,
    jumps: Vec<(Value, Value)>,
    next_quantifier: usize,
    scoped_nodes: usize,
    fragments: Vec<String>,
}
impl Encoder<'_> {
    fn typed_node(&mut self, expr: String, ty: &Type) -> Result<Value, String> {
        self.data.check(ty)?;
        let mut value = self.node(expr, *ty == Type::Bool)?;
        value.ty = ty.clone();
        self.nodes.last_mut().unwrap().0.ty = ty.clone();
        Ok(value)
    }

    fn node(&mut self, expr: String, boolean: bool) -> Result<Value, String> {
        if self.nodes.len() + self.scoped_nodes >= 100_000 {
            return Err("VC node budget exceeded".into());
        }
        let value = Value::literal(format!("v{}", self.nodes.len()), boolean);
        self.nodes.push((value.clone(), expr));
        Ok(value)
    }
    fn and(&mut self, a: &Value, b: &Value) -> Result<Value, String> {
        self.node(format!("(and {} {})", a.text, b.text), true)
    }
    fn guard(&mut self, gate: &Value, condition: String) -> Result<(), String> {
        let v = self.node(format!("(=> {} {condition})", gate.text), true)?;
        self.safety.push(v.text);
        Ok(())
    }
    fn expr(&mut self, expr: &Expr, state: &State, gate: &Value) -> Result<Value, String> {
        self.depth += 1;
        if self.depth > 256 {
            return Err("expression depth budget exceeded".into());
        }
        let result = self.expr_inner(expr, state, gate);
        self.depth -= 1;
        result
    }
    fn expr_inner(&mut self, expr: &Expr, state: &State, gate: &Value) -> Result<Value, String> {
        match expr {
            Expr::Integer(n, ..) => Ok(Value::literal(integer(*n), false)),
            Expr::Bool(b, ..) => Ok(Value::literal(b.to_string(), true)),
            Expr::Var(n, ..) => {
                if let Some(v) = state.env.get(n) {
                    return Ok(v.clone());
                }
                match self
                    .names
                    .get(n.as_str())
                    .and_then(|i| self.functions[*i].constant.as_ref())
                {
                    Some(ConstValue::Integer(n)) => Ok(Value::literal(integer(*n), false)),
                    Some(ConstValue::Bool(b)) => Ok(Value::literal(b.to_string(), true)),
                    _ => Err(format!("unknown scalar binding `{n}`")),
                }
            }
            Expr::Index(array, index, ..) => {
                let array = self.expr(array, state, gate)?;
                let index = self.expr(index, state, gate)?;
                let element = frozen_element(&array.ty)
                    .ok_or("index requires immutable FrozenArray")?
                    .clone();
                if index.ty != Type::I64 {
                    return Err("array index must be i64".into());
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
                let fields = self
                    .data
                    .fields
                    .get(&object.ty)
                    .ok_or("field access requires an immutable record")?;
                let (index, ty) = fields.get(field).ok_or("unknown record field")?;
                let index = *index;
                let ty = ty.clone();
                self.typed_node(
                    format!("({} {})", selector(&object.ty, 0, index), object.text),
                    &ty,
                )
            }
            Expr::MethodCall(c) if c.method == "len" && c.args.is_empty() => {
                let object = self.expr(&c.object, state, gate)?;
                frozen_element(&object.ty).ok_or("len requires immutable FrozenArray")?;
                self.node(
                    format!("(len_{} {})", type_key(&object.ty), object.text),
                    false,
                )
            }
            Expr::Match(m) => self.match_expr(m, state, gate),
            Expr::Call(c) if matches!(c.callee.as_str(), "forall" | "exists") => {
                self.quantifier(c, state, gate)
            }
            Expr::Unary(u) => {
                let v = self.expr(&u.expr, state, gate)?;
                match u.op {
                    UnaryOp::Neg if v.ty == Type::I64 => {
                        let r = self.node(format!("(- {})", v.text), false)?;
                        self.guard(gate, bounds(&r.text))?;
                        Ok(r)
                    }
                    UnaryOp::Not if v.boolean => self.node(format!("(not {})", v.text), true),
                    _ => Err("unsupported unary operation".into()),
                }
            }
            Expr::Binary(b) => {
                let l = self.expr(&b.lhs, state, gate)?;
                let rhs_gate = match b.op {
                    BinOp::And if l.boolean => self.and(gate, &l)?,
                    BinOp::Or if l.boolean => {
                        self.node(format!("(and {} (not {}))", gate.text, l.text), true)?
                    }
                    _ => gate.clone(),
                };
                let r = self.expr(&b.rhs, state, &rhs_gate)?;
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
                    _ => return Err("unsupported binary operation".into()),
                };
                let valid = match b.op {
                    BinOp::Eq | BinOp::Ne => l.ty == r.ty && matches!(l.ty, Type::I64 | Type::Bool),
                    BinOp::And | BinOp::Or => l.boolean && r.boolean,
                    _ => l.ty == Type::I64 && r.ty == Type::I64,
                };
                if !valid {
                    return Err("invalid operand types".into());
                }
                if matches!(b.op, BinOp::Div | BinOp::Rem) {
                    self.guard(gate, format!("(and (distinct {} 0) (not (and (= {} (- 9223372036854775808)) (= {} (- 1)))))", r.text, l.text, r.text))?;
                }
                let value = self.node(format!("({op} {} {})", l.text, r.text), boolean)?;
                if !boolean {
                    self.guard(gate, bounds(&value.text))?;
                }
                Ok(value)
            }
            Expr::Ternary(t) => {
                let c = self.expr(&t.condition, state, gate)?;
                if !c.boolean {
                    return Err("non-boolean condition".into());
                }
                let yes = self.and(gate, &c)?;
                let no = self.node(format!("(and {} (not {}))", gate.text, c.text), true)?;
                let a = self.expr(&t.then_expr, state, &yes)?;
                let b = self.expr(&t.else_expr, state, &no)?;
                if a.ty != b.ty {
                    return Err("branch type mismatch".into());
                }
                self.typed_node(format!("(ite {} {} {})", c.text, a.text, b.text), &a.ty)
            }
            Expr::Call(c) => {
                if !c.type_args.is_empty() || state.env.contains_key(&c.callee) {
                    return Err("generic or indirect call".into());
                }
                let &id = self
                    .names
                    .get(c.callee.as_str())
                    .ok_or("callee body unavailable")?;
                let f = self.functions[id];
                if !has(f, EffectContractKind::Pure) {
                    return Err("callee is not @pure".into());
                }
                if c.args.len() != f.params.len() {
                    return Err("call arity mismatch".into());
                }
                let mut args = Vec::new();
                for (a, p) in c.args.iter().zip(&f.params) {
                    if a.mode != CallArgMode::Value {
                        return Err("reference argument".into());
                    }
                    let v = self.expr(a, state, gate)?;
                    if v.ty != p.ty {
                        return Err("argument type mismatch".into());
                    }
                    args.push(v.text);
                }
                self.dependencies.insert(id);
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
                let assumption = self.node(assumption, true)?;
                self.assumptions.push(assumption.text);
                Ok(result)
            }
            _ => Err("unsupported expression in i64/bool V1".into()),
        }
    }
    fn quantifier(&mut self, c: &CallExpr, state: &State, gate: &Value) -> Result<Value, String> {
        if c.args.len() != 3
            || !c.type_args.is_empty()
            || c.args.iter().any(|a| a.mode != CallArgMode::Value)
        {
            return Err("quantifier requires bounds and a predicate".into());
        }
        let low = self.expr(&c.args[0], state, gate)?;
        let high = self.expr(&c.args[1], state, gate)?;
        if low.ty != Type::I64 || high.ty != Type::I64 {
            return Err("quantifier bounds must be i64".into());
        }
        let Expr::Lambda(lambda) = &c.args[2].expr else {
            return Err("quantifier requires a lambda binder".into());
        };
        if lambda.params.len() != 1
            || lambda.params[0]
                .ty
                .as_ref()
                .is_some_and(|t| *t != Type::I64)
        {
            return Err("quantifier binder must be i64".into());
        }
        let LambdaBody::Expr(predicate) = &lambda.body else {
            return Err("quantifier predicate must be an expression".into());
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
            return Err("unsupported quantified predicate".into());
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
    fn match_expr(&mut self, m: &MatchExpr, state: &State, gate: &Value) -> Result<Value, String> {
        let value = self.expr(&m.scrutinee, state, gate)?;
        let mut remaining = gate.clone();
        let mut result: Option<Value> = None;
        for arm in &m.arms {
            let mut branch = state.clone();
            let condition = match &arm.pattern {
                Pattern::Wildcard(..) => Value::literal("true", true),
                Pattern::Binding { name, .. } => {
                    branch.env.insert(name.clone(), value.clone());
                    Value::literal("true", true)
                }
                Pattern::LiteralBool(b, ..) if value.ty == Type::Bool => {
                    self.node(format!("(= {} {b})", value.text), true)?
                }
                Pattern::LiteralInt(n, ..) if value.ty == Type::I64 => {
                    self.node(format!("(= {} {})", value.text, integer(*n)), true)?
                }
                Pattern::EnumVariant {
                    enum_name, variant, ..
                }
                | Pattern::EnumVariantTuple {
                    enum_name, variant, ..
                } => {
                    if value.ty != Type::Named(enum_name.clone()) {
                        return Err("unsupported enum pattern type".into());
                    }
                    let ctors = self
                        .data
                        .constructors
                        .get(&value.ty)
                        .ok_or("unsupported enum")?;
                    let i = *self
                        .data
                        .variants
                        .get(&value.ty)
                        .and_then(|variants| variants.get(variant))
                        .ok_or("unknown enum variant")?;
                    let ctor = &ctors[i];
                    if let Pattern::EnumVariantTuple { bindings, .. } = &arm.pattern {
                        if bindings.len() != ctor.fields.len() {
                            return Err("enum pattern arity mismatch".into());
                        }
                        for (j, (binding, (_, ty))) in bindings.iter().zip(&ctor.fields).enumerate()
                        {
                            branch.env.insert(
                                binding.clone(),
                                Value {
                                    text: format!("({} {})", selector(&value.ty, i, j), value.text),
                                    boolean: *ty == Type::Bool,
                                    ty: ty.clone(),
                                },
                            );
                        }
                    } else if !ctor.fields.is_empty() {
                        return Err("enum payload required".into());
                    }
                    self.node(
                        format!("((_ is {}) {})", constructor(&value.ty, i), value.text),
                        true,
                    )?
                }
                _ => return Err("unsupported match pattern".into()),
            };
            let mut selected = self.and(&remaining, &condition)?;
            if let Some(guard) = &arm.guard {
                let test = self.expr(guard, &branch, &selected)?;
                if !test.boolean {
                    return Err("non-boolean match guard".into());
                }
                selected = self.and(&selected, &test)?;
            }
            let MatchBody::Expr(expression) = &arm.body else {
                return Err("proof match requires expression arms".into());
            };
            let branch_value = self.expr(expression, &branch, &selected)?;
            result = Some(if let Some(previous) = result {
                if previous.ty != branch_value.ty {
                    return Err("match result type mismatch".into());
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
        result.ok_or_else(|| "empty match".into())
    }

    fn block(&mut self, block: &Block, state: &mut State, depth: usize) -> Result<(), String> {
        if depth > 256 {
            return Err("block depth budget exceeded".into());
        }
        let mut locals = std::collections::BTreeMap::new();
        for stmt in &block.stmts {
            match stmt {
                Stmt::Let(s) => {
                    let v = self.expr(&s.init, state, &state.alive.clone())?;
                    if let Some(t) = &s.ty
                        && v.ty != *t
                    {
                        return Err("local type mismatch".into());
                    }
                    locals
                        .entry(s.name.clone())
                        .or_insert_with(|| state.env.get(&s.name).cloned());
                    state.env.insert(s.name.clone(), v);
                }
                Stmt::Assign(s) => {
                    let old = state.env.get(&s.name).ok_or("unknown assignment")?.clone();
                    let v = self.expr(&s.value, state, &state.alive.clone())?;
                    if old.ty != v.ty {
                        return Err("assignment type mismatch".into());
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
                        return Err("non-boolean condition".into());
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
                    &s.cond,
                    &s.invariants,
                    s.variant.as_ref(),
                    &s.body,
                    s.span,
                    state,
                    depth + 1,
                    None,
                    None,
                )?,
                Stmt::For(s) => self.for_loop(s, state, depth + 1)?,
                Stmt::Break(_) | Stmt::Continue(_) => {
                    let (broken, continued) =
                        self.jumps.last().cloned().ok_or("jump outside loop")?;
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
                    let v = self.expr(
                        s.value.as_ref().ok_or("void return outside V1")?,
                        state,
                        &state.alive.clone(),
                    )?;
                    if v.ty != state.result.ty {
                        return Err("return type mismatch".into());
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
                        "unsupported statement: verification rejects for, break, continue, defer and heap operations".into(),
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
    ) -> Result<Value, String> {
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
    ) -> Result<Value, String> {
        let mut result = gate.clone();
        for expr in invariants {
            let value = self.loop_annotation(expr, state, &result, index)?;
            if !value.boolean {
                return Err("loop invariant must have type bool".into());
            }
            result = self.and(&result, &value)?;
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn loop_body(
        &mut self,
        cond: &Expr,
        invariants: &[Expr],
        variant: Option<&Expr>,
        body_block: &Block,
        span: Span,
        state: &mut State,
        depth: usize,
        step: Option<&str>,
        element: Option<(&str, &Value)>,
    ) -> Result<(), String> {
        if !self.verified {
            return Err("a helper containing loops must itself be @verified".into());
        }
        let variant = variant
            .filter(|_| !invariants.is_empty())
            .ok_or("while verification requires both invariant and variant")?;
        let incoming = state.alive.clone();
        let annotation_index = element.and(step);
        let entry = self.loop_invariant(invariants, state, &incoming, annotation_index)?;
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
                    if name.starts_with("$for_index_") {
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
        if let Some((_, array)) = element {
            let index = &head.env[step.expect("array iteration index")];
            let valid = self.node(
                format!(
                    "(and (<= 0 {}) (<= {} (len_{} {})))",
                    index.text,
                    index.text,
                    type_key(&array.ty),
                    array.text
                ),
                true,
            )?;
            head.alive = self.and(&head.alive, &valid)?;
        }
        head.alive =
            self.loop_invariant(invariants, &head, &head.alive.clone(), annotation_index)?;
        let condition = self.expr(cond, &head, &head.alive.clone())?;
        if !condition.boolean {
            return Err("non-boolean loop condition".into());
        }
        let mut body = head.clone();
        body.alive = self.and(&head.alive, &condition)?;
        let rank = self.loop_annotation(variant, &head, &body.alive.clone(), annotation_index)?;
        if rank.ty != Type::I64 {
            return Err("loop variant must have type i64".into());
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
        let preserved =
            self.loop_invariant(invariants, &body, &body.alive.clone(), annotation_index)?;
        self.guard(&body.alive, preserved.text)?;
        let next = self.loop_annotation(variant, &body, &body.alive.clone(), annotation_index)?;
        if next.ty != Type::I64 {
            return Err("loop variant must have type i64".into());
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

    fn for_loop(&mut self, s: &ForStmt, state: &mut State, depth: usize) -> Result<(), String> {
        let array;
        let (start, end, counter, element) = if let Expr::Range(range) = &s.iterable {
            let start = self.expr(&range.start, state, &state.alive.clone())?;
            let end = self.expr(&range.end, state, &state.alive.clone())?;
            (start, end, s.name.clone(), None)
        } else {
            array = self.expr(&s.iterable, state, &state.alive.clone())?;
            frozen_element(&array.ty).ok_or("for requires range or immutable FrozenArray")?;
            // An element does not exist at the empty/exhausted loop head.
            // Head invariants/ranks use outer bindings, not the element binder.
            for expression in s.invariants.iter().chain(s.variant.iter()) {
                if AstWalk::new(AstEvent::Expr(expression))
                    .any(|e| matches!(e,AstEvent::Expr(Expr::Var(name,..)) if *name == s.name))
                {
                    return Err("array loop annotations cannot reference the element binder at an exhausted head".into());
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
            return Err("non-integer for range".into());
        }
        let saved = state.env.insert(counter.clone(), start);
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
        self.loop_body(
            &cond,
            &s.invariants,
            s.variant.as_ref(),
            &s.body,
            s.span,
            state,
            depth,
            Some(&counter),
            element,
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

    fn clauses(&mut self, clauses: &[Expr], state: &State) -> Result<Value, String> {
        let mut gate = Value::literal("true", true);
        for expr in clauses {
            let value = self.expr(expr, state, &gate)?;
            if !value.boolean {
                return Err("non-boolean contract".into());
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
) -> Result<Summary, String> {
    let f = functions[id];
    if !f.type_params.is_empty() || f.is_async || f.rust_bridge.is_some() {
        return Err("generic, async or external function outside V1".into());
    }
    data.check(&f.return_type)?;
    let ret = sort(&f.return_type)?;
    let mut params = Vec::new();
    let mut env = im::HashMap::new();
    for (i, p) in f.params.iter().enumerate() {
        if p.mode != ParamMode::Value {
            return Err("reference parameter outside V1".into());
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
    let mut e = Encoder {
        data,
        functions,
        names,
        nodes: Vec::new(),
        safety: Vec::new(),
        assumptions: Vec::new(),
        dependencies: BTreeSet::new(),
        depth: 0,
        function_id: id,
        havoc: Vec::new(),
        loop_symbols: std::collections::BTreeMap::new(),
        loop_writes: HashMap::new(),
        verified: has(f, EffectContractKind::Verified),
        jumps: Vec::new(),
        next_quantifier: 0,
        scoped_nodes: 0,
        fragments: Vec::new(),
    };
    let pre = e.clauses(&f.requires, &state)?;
    let pre_safe = e.safety();
    let mut definitions = e.definition(&format!("f{id}_pre"), &params.join(" "), "Bool", &pre.text);
    definitions += &e.definition(
        &format!("f{id}_pre_safe"),
        &params.join(" "),
        "Bool",
        &pre_safe,
    );
    let mut contract_definitions = definitions.clone();
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
    let assumptions = if e.assumptions.is_empty() {
        "true".into()
    } else {
        format!("(and {})", e.assumptions.join(" "))
    };
    definitions += &e.definition(
        &format!("f{id}_assume"),
        &params.join(" "),
        "Bool",
        &assumptions,
    );
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
            return Err("non-boolean contract".into());
        }
        post = e.and(&post, &value)?;
    }
    let post = e.node(format!("(=> {} {})", state.returned.text, post.text), true)?;
    let post_safe = e.safety();
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
    let mut c = Encoder {
        data,
        functions,
        names,
        nodes: Vec::new(),
        safety: Vec::new(),
        assumptions: Vec::new(),
        dependencies: BTreeSet::new(),
        depth: 0,
        function_id: id,
        havoc: Vec::new(),
        loop_symbols: std::collections::BTreeMap::new(),
        loop_writes: HashMap::new(),
        verified: has(f, EffectContractKind::Verified),
        jumps: Vec::new(),
        next_quantifier: 0,
        scoped_nodes: 0,
        fragments: Vec::new(),
    };
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
        &format!("(and {safe} {})", cp.text),
    );
    contract_definitions += &c.definition(
        &format!("f{id}_ensures"),
        &params.join(" "),
        "Bool",
        &format!("(and {safe} {})", cp.text),
    );
    Ok(Summary {
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
    })
}
// Summarize lexical writes once, bottom-up. Nested loops reuse their summary;
// no enclosing loop rescans a nested body. Set merging is small-to-large.
fn index_loop_writes(
    block: &Block,
    index: &mut HashMap<BodyId, BTreeSet<String>>,
    depth: usize,
) -> Result<BTreeSet<String>, String> {
    if depth > 256 {
        return Err("block depth budget exceeded".into());
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
pub(super) fn edges(f: &FunctionDecl, names: &HashMap<&str, usize>) -> BTreeSet<usize> {
    AstWalk::new(AstEvent::Block(&f.body))
        .filter_map(|event| match event {
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
) -> Result<String, String> {
    let f = functions[id];
    if !f.type_params.is_empty() || f.is_async || f.rust_bridge.is_some() {
        return Err("unsupported contract signature".into());
    }
    let mut params = Vec::new();
    let mut env = im::HashMap::new();
    for (i, p) in f.params.iter().enumerate() {
        if p.mode != ParamMode::Value {
            return Err("reference contract parameter".into());
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
    let mut e = Encoder {
        data,
        functions,
        names,
        nodes: Vec::new(),
        safety: Vec::new(),
        assumptions: Vec::new(),
        dependencies: BTreeSet::new(),
        depth: 0,
        function_id: id,
        havoc: Vec::new(),
        loop_symbols: std::collections::BTreeMap::new(),
        loop_writes: HashMap::new(),
        verified: has(f, EffectContractKind::Verified),
        jumps: Vec::new(),
        next_quantifier: 0,
        scoped_nodes: 0,
        fragments: Vec::new(),
    };
    let pre = e.clauses(&f.requires, &state)?;
    let safety = e.safety();
    let mut out = e.definition(&format!("c{id}_pre"), &params.join(" "), "Bool", &pre.text);
    out += &e.definition(
        &format!("c{id}_pre_safe"),
        &params.join(" "),
        "Bool",
        &safety,
    );
    e.nodes.clear();
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
    if !e.dependencies.is_empty() {
        return Err("contract calls require additional comparison summaries".into());
    }
    Ok(out)
}
