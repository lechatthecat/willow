//! Deliberately closed, straight-line wait regions. Unknown syntax invalidates a
//! region, never establishes safety. Summaries are built once; leaf task bodies
//! are instantiated without recursive call expansion or repeated AST walks.
use std::collections::HashMap;

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::parser::iter::{AstEvent, AstWalk};
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::ids::TypeId;

#[derive(Default, Debug)]
pub(crate) struct Counts {
    pub syntax: usize,
    pub operations: usize,
    pub tasks: usize,
    pub channels: usize,
    pub waits: usize,
    pub send_checks: usize,
    pub graph_steps: usize,
}

#[derive(Default)]
pub(crate) struct Analysis {
    pub diagnostics: Vec<Diagnostic>,
    pub counts: Counts,
}

type Slot = usize;
#[derive(Clone, Copy)]
enum Value {
    Unknown,
    Channel(usize),
    Task(usize),
    Received(usize),
}

enum Op {
    Channel {
        out: Slot,
        span: Span,
        bounded: bool,
        safe_capacity: bool,
    },
    Receive {
        out: Slot,
        channel: Slot,
        span: Span,
    },
    Send {
        channel: Slot,
        value: Slot,
    },
    Close(Slot),
    Await {
        value: Slot,
        span: Span,
    },
    Spawn {
        out: Slot,
        function: usize,
        args: Vec<Slot>,
        span: Span,
    },
    Call {
        function: usize,
        args: Vec<Slot>,
    },
    Escape(Slot),
}

struct Summary {
    params: usize,
    slots: usize,
    ops: Vec<Op>,
    leaf: bool,
    span: Span,
}

/// Types distinguish builtins from user methods with the same spelling. Local
/// names are resolved in declaration order; function values are not direct calls.
fn tracked(e: &Expr, types: &HashMap<ExprId, Type>) -> bool {
    types
        .get(&e.id())
        .and_then(builtin_types::resolve)
        .is_some_and(|t| matches!(t.id, B::Channel | B::Task | B::TaskResult))
}

fn summarize(
    f: &FunctionDecl,
    functions: &HashMap<&str, (usize, bool)>,
    types: &HashMap<ExprId, Type>,
    owners: &HashMap<ExprId, TypeId>,
    counts: &mut Counts,
) -> Option<Summary> {
    if !f.type_params.is_empty()
        || f.rust_bridge.is_some()
        || !f.requires.is_empty()
        || !f.ensures.is_empty()
    {
        return None;
    }
    let mut summary = Summary {
        params: f.params.len(),
        slots: f.params.len() + 1,
        ops: vec![],
        leaf: true,
        span: f.span,
    };
    // Slot zero represents untracked scalar values, never a known handle.
    let mut names: HashMap<&str, Slot> = f
        .params
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name.as_str(), i + 1))
        .collect();
    let mut values = HashMap::new();
    // Only zero, one and many sends matter. Keep the first two per identity;
    // later sends cannot restore unique delivery or introduce an earlier
    // closed/bounded-send barrier. Argument ASTs are still fully traversed.
    let mut sends: HashMap<Slot, u8> = HashMap::new();
    let walk = AstWalk::new(AstEvent::Block(&f.body));
    for event in walk {
        counts.syntax += 1;
        match event {
            AstEvent::Stmt(stmt)
                if !matches!(stmt, Stmt::Let(_) | Stmt::Expr(_) | Stmt::Return(_)) =>
            {
                return None;
            }
            AstEvent::ExitStmt(Stmt::Let(s)) => {
                names.insert(&s.name, *values.get(&s.init.id())?);
            }
            AstEvent::ExitStmt(Stmt::Return(s)) => {
                if let Some(e) = &s.value
                    && tracked(e, types)
                {
                    summary.ops.push(Op::Escape(*values.get(&e.id())?));
                }
                break; // Unreachable suffixes cannot send, cancel or wait.
            }
            AstEvent::Expr(e) => {
                if !matches!(
                    e,
                    Expr::Integer(..)
                        | Expr::Float(..)
                        | Expr::Bool(..)
                        | Expr::String(..)
                        | Expr::Var(..)
                        | Expr::StaticCall(_)
                        | Expr::MethodCall(_)
                        | Expr::Call(_)
                        | Expr::Await(_)
                        | Expr::Print(..)
                ) {
                    return None;
                }
            }
            AstEvent::ExitExpr(e) => {
                let slot = summary.slots;
                let mut out = 0;
                let builtin = |e: &Expr, id| {
                    types
                        .get(&e.id())
                        .and_then(builtin_types::resolve)
                        .is_some_and(|t| t.id == id)
                };
                match e {
                    Expr::Var(name, ..) => out = *names.get(name.as_str())?,
                    Expr::StaticCall(c)
                        if builtin(e, B::Channel)
                            && owners.get(&c.id).is_some_and(|owner| {
                                builtin_types::resolve(&Type::Named(*owner))
                                    .is_some_and(|t| t.id == B::Channel)
                            })
                            && matches!(c.method.as_str(), "new" | "with_capacity") =>
                    {
                        summary.ops.push(Op::Channel {
                            out: slot,
                            span: c.span,
                            bounded: c.method == "with_capacity",
                            safe_capacity: c.method == "new"
                                || matches!(c.args.first().map(|a| &a.expr), Some(Expr::Integer(n, ..)) if *n > 0),
                        });
                        out = slot;
                    }
                    Expr::MethodCall(c) if builtin(&c.object, B::Channel) => {
                        let channel = *values.get(&c.object.id())?;
                        match c.method.as_str() {
                            "recv" | "recv_opt" => {
                                summary.ops.push(Op::Receive {
                                    out: slot,
                                    channel,
                                    span: c.span,
                                });
                                // recv_opt carries an Option, not a Task handle.
                                out = slot;
                            }
                            "send" => {
                                let count = sends.entry(channel).or_default();
                                if *count < 2 {
                                    summary.ops.push(Op::Send {
                                        channel,
                                        value: *values.get(&c.args.first()?.expr.id())?,
                                    });
                                    *count += 1;
                                }
                            }
                            "close" => summary.ops.push(Op::Close(channel)),
                            _ => return None,
                        }
                    }
                    Expr::MethodCall(c) if builtin(&c.object, B::Task) && c.method == "result" => {
                        out = *values.get(&c.object.id())?
                    }
                    Expr::Await(a) => summary.ops.push(Op::Await {
                        value: *values.get(&a.expr.id())?,
                        span: a.span,
                    }),
                    Expr::Call(c) if !names.contains_key(c.callee.as_str()) => {
                        let (function, asynchronous) = *functions.get(c.callee.as_str())?;
                        let args = c
                            .args
                            .iter()
                            .map(|a| {
                                if a.mode != CallArgMode::Value {
                                    return None;
                                }
                                values.get(&a.expr.id()).copied()
                            })
                            .collect::<Option<Vec<_>>>()?;
                        if asynchronous {
                            summary.ops.push(Op::Spawn {
                                out: slot,
                                function,
                                args,
                                span: c.span,
                            });
                            out = slot;
                        } else {
                            summary.ops.push(Op::Call { function, args });
                        }
                        summary.leaf = false;
                    }
                    Expr::Print(value, ..) => {
                        if tracked(value, types) {
                            summary.ops.push(Op::Escape(*values.get(&value.id())?));
                        }
                    }
                    Expr::Integer(..) | Expr::Float(..) | Expr::Bool(..) | Expr::String(..) => {}
                    _ => return None,
                }
                if out == slot {
                    summary.slots += 1;
                }
                values.insert(e.id(), out);
            }
            _ => {}
        }
    }
    Some(summary)
}

struct Channel {
    span: Span,
    bounded: bool,
    closed: bool,
    sends: usize,
    receives: usize,
    payload: Value,
}
enum Wait {
    Receive(usize, Span),
    Send(usize),
    Await(Value, Span),
    Barrier,
}
struct Task {
    span: Span,
    waits: Vec<Wait>,
}
#[derive(Default)]
struct Region {
    channels: Vec<Channel>,
    tasks: Vec<Task>,
}

impl Region {
    fn instantiate(
        &mut self,
        summary: &Summary,
        args: &[Value],
        task: usize,
        summaries: &[Option<Summary>],
        counts: &mut Counts,
    ) -> Option<()> {
        let mut slots = vec![Value::Unknown; summary.slots];
        for (slot, value) in slots[1..=summary.params].iter_mut().zip(args) {
            *slot = *value;
        }
        for op in &summary.ops {
            counts.operations += 1;
            match op {
                Op::Channel {
                    out,
                    span,
                    bounded,
                    safe_capacity,
                } => {
                    // Invalid capacities panic before any subsequent wait.
                    // Nonliteral capacities are unknown without scalar proof.
                    if !safe_capacity {
                        self.tasks[task].waits.push(Wait::Barrier);
                    }
                    slots[*out] = Value::Channel(self.channels.len());
                    self.channels.push(Channel {
                        span: *span,
                        bounded: *bounded,
                        closed: false,
                        sends: 0,
                        receives: 0,
                        payload: Value::Unknown,
                    });
                    counts.channels += 1;
                }
                Op::Receive { out, channel, span } => {
                    if let Value::Channel(c) = slots[*channel] {
                        self.channels[c].receives += 1;
                        slots[*out] = Value::Received(c);
                        self.tasks[task].waits.push(Wait::Receive(c, *span));
                    } else {
                        self.tasks[task].waits.push(Wait::Barrier);
                    }
                    counts.waits += 1;
                }
                Op::Send { channel, value } => {
                    let Value::Channel(c) = slots[*channel] else {
                        return None;
                    };
                    let ch = &mut self.channels[c];
                    ch.sends += 1;
                    ch.payload = slots[*value];
                    // A peer may close even an unbounded channel, causing a
                    // panic before later waits. Resolve closure after collection.
                    self.tasks[task].waits.push(Wait::Send(c));
                    counts.send_checks += 1;
                }
                Op::Close(channel) => {
                    let Value::Channel(c) = slots[*channel] else {
                        return None;
                    };
                    self.channels[c].closed = true;
                }
                Op::Await { value, span } => {
                    self.tasks[task]
                        .waits
                        .push(Wait::Await(slots[*value], *span));
                    counts.waits += 1;
                }
                Op::Spawn {
                    out,
                    function,
                    args,
                    span,
                } => {
                    let callee = summaries[*function].as_ref()?;
                    // No recursive expansion: nested/recursive task factories are unknown.
                    if !callee.leaf {
                        return None;
                    }
                    let args: Vec<_> = args.iter().map(|s| slots[*s]).collect();
                    let child = self.tasks.len();
                    self.tasks.push(Task {
                        span: *span,
                        waits: vec![],
                    });
                    counts.tasks += 1;
                    self.instantiate(callee, &args, child, summaries, counts)?;
                    slots[*out] = Value::Task(child);
                }
                Op::Call { function, args } => {
                    let callee = summaries[*function].as_ref()?;
                    if !callee.leaf {
                        return None;
                    }
                    let args: Vec<_> = args.iter().map(|s| slots[*s]).collect();
                    self.instantiate(callee, &args, task, summaries, counts)?;
                }
                Op::Escape(value) if !matches!(slots[*value], Value::Unknown) => return None,
                Op::Escape(_) => {}
            }
        }
        Some(())
    }

    fn target(&self, value: Value) -> Option<usize> {
        match value {
            Value::Task(t) => Some(t),
            Value::Received(c) => {
                let channel = &self.channels[c];
                if channel.sends == 1
                    && channel.receives == 1
                    && !channel.closed
                    && let Value::Task(t) = channel.payload
                {
                    return Some(t);
                }
                None
            }
            _ => None,
        }
    }

    fn diagnose(&self, counts: &mut Counts) -> Option<Diagnostic> {
        // Follow only the first unresolved await of each task. A later await
        // could be unreachable because an earlier task panics/cancels. Root
        // reachability also excludes detached cycles after the caller returns.
        let mut visited = vec![false; self.tasks.len()];
        let mut task = 0;
        let mut labels = Vec::new();
        loop {
            counts.graph_steps += 1;
            if visited[task] {
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2607,
                    "permanent Task await cycle",
                )
                .with_label(Label::primary(
                    self.tasks[task].span,
                    "witness: this Task waits for its own completion through the await chain",
                ));
                for label in labels {
                    diagnostic = diagnostic.with_label(label);
                }
                return Some(diagnostic.with_help("break the await cycle; an acyclic dependency or a timeout/select escape is required"));
            }
            visited[task] = true;
            let mut next = None;
            for wait in &self.tasks[task].waits {
                counts.graph_steps += 1;
                match *wait {
                    Wait::Send(c) => {
                        if self.channels[c].bounded || self.channels[c].closed {
                            return None;
                        }
                    }
                    Wait::Receive(c, span) => {
                        let channel = &self.channels[c];
                        if channel.closed {
                            return None;
                        }
                        if channel.sends == 0 {
                            let mut diagnostic = Diagnostic::new(Severity::Error, ErrorCode::E2607, "permanent receive on a channel without a sender")
                                .with_label(Label::primary(span, "witness: receive waits here"))
                                .with_label(Label::secondary(channel.span, "fresh channel: no send, close or external escape in this closed region"));
                            for label in labels {
                                diagnostic = diagnostic.with_label(label);
                            }
                            return Some(diagnostic.with_help("provide a sender, close the channel, or use select with a timeout/default escape"));
                        }
                        // Multiple receives or sends do not establish which value arrives.
                        if channel.sends != 1 || channel.receives != 1 {
                            return None;
                        }
                    }
                    Wait::Await(value, span) => {
                        let target = self.target(value)?;
                        next = Some(target);
                        labels.push(Label::secondary(
                            span,
                            "witness: awaits the next Task in this chain",
                        ));
                        labels.push(Label::secondary(
                            self.tasks[target].span,
                            "witness: awaited Task was created here",
                        ));
                        if let Value::Received(channel) = value {
                            labels.push(Label::secondary(
                                self.channels[channel].span,
                                "witness: this channel delivers that Task (one send and one receive)",
                            ));
                        }
                        break;
                    }
                    Wait::Barrier => return None,
                }
            }
            task = next?;
        }
    }
}

pub(crate) fn check(
    program: &Program,
    types: &HashMap<ExprId, Type>,
    owners: &HashMap<ExprId, TypeId>,
) -> Analysis {
    let mut result = Analysis::default();
    let functions: Vec<_> = program
        .items
        .iter()
        .filter_map(|item| {
            if let Item::Function(f) = item {
                Some(f)
            } else {
                None
            }
        })
        .collect();
    let index = functions
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), (i, f.is_async)))
        .collect();
    let summaries: Vec<_> = functions
        .iter()
        .map(|f| summarize(f, &index, types, owners, &mut result.counts))
        .collect();
    // A helper may be detached, cancelled or bounded by its caller's select.
    // Only the actual entry's reachable wait chain establishes a program wait.
    if let Some(summary) = functions
        .iter()
        .position(|f| f.name == "main")
        .and_then(|i| summaries[i].as_ref())
    {
        let mut region = Region::default();
        region.tasks.push(Task {
            span: summary.span,
            waits: vec![],
        });
        result.counts.tasks += 1;
        if region
            .instantiate(summary, &[], 0, &summaries, &mut result.counts)
            .is_some()
            && let Some(diagnostic) = region.diagnose(&mut result.counts)
        {
            result.diagnostics.push(diagnostic);
        }
    }
    result
}

#[cfg(test)]
mod tests;
