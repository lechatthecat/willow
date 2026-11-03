use super::*;
use crate::parser::iter::{AstEvent, AstWalk};

#[derive(Clone, Debug)]
pub(super) struct Value {
    pub text: String,
    pub boolean: bool,
}
impl Value {
    fn literal(text: impl Into<String>, boolean: bool) -> Self {
        Self {
            text: text.into(),
            boolean,
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
pub(super) fn sort(t: &Type) -> Result<&'static str, String> {
    match t {
        Type::I64 => Ok("Int"),
        Type::Bool => Ok("Bool"),
        _ => Err("V1 supports only i64/bool".into()),
    }
}
pub(super) struct Summary {
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
}
impl Encoder<'_> {
    fn node(&mut self, expr: String, boolean: bool) -> Result<Value, String> {
        if self.nodes.len() >= 100_000 {
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
            Expr::Unary(u) => {
                let v = self.expr(&u.expr, state, gate)?;
                match u.op {
                    UnaryOp::Neg if !v.boolean => {
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
                    BinOp::Eq | BinOp::Ne => l.boolean == r.boolean,
                    BinOp::And | BinOp::Or => l.boolean && r.boolean,
                    _ => !l.boolean && !r.boolean,
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
                if a.boolean != b.boolean {
                    return Err("branch type mismatch".into());
                }
                self.node(format!("(ite {} {} {})", c.text, a.text, b.text), a.boolean)
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
                    if v.boolean != (sort(&p.ty)? == "Bool") {
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
                let result = self.node(
                    call(if has(f, EffectContractKind::Verified) {
                        "contract"
                    } else {
                        "value"
                    }),
                    sort(&f.return_type)? == "Bool",
                )?;
                let assumption = if has(f, EffectContractKind::Verified) {
                    let mut domain = vec![gate.text.clone(), call("pre_safe"), call("pre")];
                    for (a, p) in args.iter().zip(&f.params) {
                        if p.ty == Type::I64 {
                            domain.push(bounds(a));
                        }
                    }
                    let mut post_args = args.clone();
                    post_args.push(result.text.clone());
                    let post = application(&format!("f{id}_ensures"), &post_args);
                    let range = if result.boolean {
                        "true".into()
                    } else {
                        bounds(&result.text)
                    };
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
                        && v.boolean != (sort(t)? == "Bool")
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
                    if old.boolean != v.boolean {
                        return Err("assignment type mismatch".into());
                    }
                    let merged = self.node(
                        format!("(ite {} {} {})", state.alive.text, v.text, old.text),
                        old.boolean,
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
                Stmt::While(s) => self.while_loop(s, state, depth + 1)?,
                Stmt::Return(s) => {
                    let v = self.expr(
                        s.value.as_ref().ok_or("void return outside V1")?,
                        state,
                        &state.alive.clone(),
                    )?;
                    if v.boolean != state.result.boolean {
                        return Err("return type mismatch".into());
                    }
                    state.result = self.node(
                        format!(
                            "(ite {} {} {})",
                            state.alive.text, v.text, state.result.text
                        ),
                        v.boolean,
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
    fn loop_invariant(
        &mut self,
        s: &WhileStmt,
        state: &State,
        gate: &Value,
    ) -> Result<Value, String> {
        let mut result = gate.clone();
        for expr in &s.invariants {
            let value = self.expr(expr, state, &result)?;
            if !value.boolean {
                return Err("loop invariant must have type bool".into());
            }
            result = self.and(&result, &value)?;
        }
        Ok(result)
    }

    fn while_loop(&mut self, s: &WhileStmt, state: &mut State, depth: usize) -> Result<(), String> {
        if !self.verified {
            return Err("a helper containing loops must itself be @verified".into());
        }
        let variant = s
            .variant
            .as_ref()
            .filter(|_| !s.invariants.is_empty())
            .ok_or("while verification requires both invariant and variant")?;
        let incoming = state.alive.clone();
        let entry = self.loop_invariant(s, state, &incoming)?;
        self.guard(&incoming, entry.text)?;

        // One arbitrary loop head represents every iteration. Only writes to
        // bindings visible at the head are forgotten; lexical locals stay local.
        let mut head = state.clone();
        let writes = self.loop_writes.remove(&s.body.id).unwrap_or_default();
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
                format!("loop@{}:{}::{name}", s.span.line, s.span.col),
            );
            self.havoc.push(value.clone());
            if !value.boolean {
                let domain = self.node(bounds(&value.text), true)?;
                head.alive = self.and(&head.alive, &domain)?;
            }
            let previous = head.env.get(&name).unwrap();
            let merged = self.node(
                format!("(ite {} {} {})", incoming.text, value.text, previous.text),
                value.boolean,
            )?;
            head.env.insert(name, merged);
        }
        head.alive = self.loop_invariant(s, &head, &head.alive.clone())?;
        let condition = self.expr(&s.cond, &head, &head.alive.clone())?;
        if !condition.boolean {
            return Err("non-boolean loop condition".into());
        }
        let mut body = head.clone();
        body.alive = self.and(&head.alive, &condition)?;
        let rank = self.expr(variant, &head, &body.alive.clone())?;
        if rank.boolean {
            return Err("loop variant must have type i64".into());
        }
        self.guard(&body.alive, format!("(<= 0 {})", rank.text))?;
        self.block(&s.body, &mut body, depth)?;
        let preserved = self.loop_invariant(s, &body, &body.alive.clone())?;
        self.guard(&body.alive, preserved.text)?;
        let next = self.expr(variant, &body, &body.alive.clone())?;
        if next.boolean {
            return Err("loop variant must have type i64".into());
        }
        self.guard(&body.alive, format!("(< {} {})", next.text, rank.text))?;

        // Backedges end the induction step, not the function. Only the false
        // condition continues after the loop; returns in the body remain exits.
        head.alive = self.node(
            format!("(and {} (not {}))", head.alive.text, condition.text),
            true,
        )?;
        head.result = body.result;
        head.returned = body.returned;
        *state = head;
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
    fn definition(&self, name: &str, params: &str, ty: &str, result: &str) -> String {
        let mut s = format!("(define-fun {name} ({params}) {ty} ");
        for (v, expr) in &self.nodes {
            s.push_str(&format!("(let (({} {expr})) ", v.text));
        }
        s.push_str(result);
        s.extend(std::iter::repeat_n(')', self.nodes.len() + 1));
        s.push('\n');
        s
    }
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
) -> Result<Summary, String> {
    let f = functions[id];
    if !f.type_params.is_empty() || f.is_async || f.rust_bridge.is_some() {
        return Err("generic, async or external function outside V1".into());
    }
    let ret = sort(&f.return_type)?;
    let mut params = Vec::new();
    let mut env = im::HashMap::new();
    for (i, p) in f.params.iter().enumerate() {
        if p.mode != ParamMode::Value {
            return Err("reference parameter outside V1".into());
        }
        let ty = sort(&p.ty)?;
        params.push(format!("(p{i} {ty})"));
        env.insert(
            p.name.clone(),
            Value::literal(format!("p{i}"), ty == "Bool"),
        );
    }
    let mut state = State {
        env,
        alive: Value::literal("true", true),
        result: Value::literal(if ret == "Bool" { "false" } else { "0" }, ret == "Bool"),
        returned: Value::literal("false", true),
    };
    let initial = state.clone();
    let mut e = Encoder {
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
        definitions += &format!(
            "(declare-const {} {})\n",
            h.text,
            if h.boolean { "Bool" } else { "Int" }
        );
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
        ret,
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
    };
    let mut cs = initial;
    cs.env
        .insert("result".into(), Value::literal("returned", ret == "Bool"));
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
        contract_definitions,
        definitions,
        nodes: e.nodes.len() + c.nodes.len(),
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
        let ty = sort(&p.ty)?;
        params.push(format!("(p{i} {ty})"));
        env.insert(
            p.name.clone(),
            Value::literal(format!("p{i}"), ty == "Bool"),
        );
    }
    let ret = sort(&f.return_type)?;
    let mut state = State {
        env,
        alive: Value::literal("true", true),
        result: Value::literal("returned", ret == "Bool"),
        returned: Value::literal("true", true),
    };
    let mut e = Encoder {
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
