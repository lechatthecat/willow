//! Send / Sync type classification (willow-dgwo.2).
//!
//! `Send`  = a value may be transferred across worker/task boundaries.
//! `Sync`  = a value may be shared by multiple workers/tasks concurrently.
//!
//! The compiler INFERS these structurally (users may not implement them — see
//! willow-dgwo.1 / E2401). These predicates are the foundation for spawn/async
//! capture checking (dgwo.4), `Task<T>` Send analysis (dgwo.5), and frozen
//! collections (dgwo.7).
//!
//! Summary of the rules (spec §5–§7):
//! - primitives (`i64`/`f64`/`bool`), immutable `String`: Send + Sync
//! - `Option`/`Result`: Send iff all args Send; Sync iff all args Sync
//! - fieldless enums: Send + Sync; payload enums: by all payload types
//! - `Array<T>`/`Map<K,V>`: Send iff elems Send; NOT Sync (mutable)
//! - `AtomicI64`/`AtomicBool`: Send + Sync
//! - `Mutex<T>`: Send + Sync iff T: Send
//! - `RwLock<T>`: Send + Sync iff T: Send + Sync
//! - `Channel<T>`: Send + Sync iff T: Send
//! - `Task<T>`/`JoinHandle<T>`: Send iff T: Send; not Sync
//! - class: Send iff all fields Send; Sync iff all fields Sync
//! - interface: Send iff it `extends Send` or `extends Sync`; Sync iff it
//!   `extends Sync`
//! - function/closure values: conservatively neither (captures unknown)

use std::collections::HashSet;

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::semantic::symbols::ParamInfo;

use super::*;

const ASYNC_CAPTURE_SHARING_NOTE: &str = "async calls share reference arguments with another task, even when immediately awaited; suspending the caller does not prove exclusive access to the referenced value";

/// Value (non-GC, by-copy) types: scalars and the unit-like types. Everything
/// else (String, Array, Map, classes, enums, Channel, Mutex, Task, …) is a
/// heap/GC reference that is shared when passed to a task.
fn is_value_type(ty: &Type) -> bool {
    matches!(
        ty,
        Type::I64 | Type::F64 | Type::Bool | Type::Void | Type::Never
    )
}

#[willow_continuations::checker]
impl TypeChecker {
    /// True if a value of `ty` may be transferred across worker/task boundaries.
    pub(super) fn is_send(&self, ty: &Type) -> bool {
        self.marker_holds(ty, Marker::Send, &mut MarkerWalk::default())
    }

    /// True if a value of `ty` may be shared concurrently by multiple tasks.
    pub(super) fn is_sync(&self, ty: &Type) -> bool {
        self.marker_holds(ty, Marker::Sync, &mut MarkerWalk::default())
    }

    /// Check the arguments passed to an async fn call: each value is captured
    /// into a `Task` that may run on another worker, so a GC-reference argument
    /// must be `Sync` and a scalar/value argument must be `Send` (willow-dgwo.4,
    /// spec §8). Reports E2402 per offending argument.
    ///
    /// Only enforced when `enforce_send_sync` is set. Normal compilation sets
    /// it because Willow runs at least five workers (willow-dgwo.9).
    pub(super) fn check_async_capture(&mut self, params: &[ParamInfo], args: &[CallArg]) {
        if !self.enforce_send_sync {
            return;
        }
        for (param, arg) in params.iter().zip(args.iter()) {
            let ty = &param.ty;
            // Scalars are copied into the task (need Send, always satisfied);
            // GC references are shared with the task (need Sync).
            let (ok, marker) = if is_value_type(ty) {
                (self.is_send(ty), "Send")
            } else {
                (self.is_sync(ty), "Sync")
            };
            if ok {
                continue;
            }
            // An interface value follows the interface contract, so give the
            // interface-specific diagnostic (willow-dgwo.5, spec §14): E2404 if it
            // is not even Send, else E2405 (Send but not Sync).
            if self.is_interface_type(ty) {
                let (code, kind) = if !self.is_send(ty) {
                    (ErrorCode::E2404, "Send")
                } else {
                    (ErrorCode::E2405, "Sync")
                };
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        code,
                        format!("interface value `{}` is not `{kind}`", type_name(ty)),
                    )
                    .with_label(Label::primary(
                        arg.expr.span(),
                        "interface value crosses a task boundary here",
                    ))
                    .with_note(ASYNC_CAPTURE_SHARING_NOTE)
                    .with_help(format!(
                        "declare `interface {} extends Sync` if every implementation is Sync",
                        type_name(ty)
                    )),
                );
                continue;
            }
            let (note, help) = self.marker_failure(
                ty,
                if marker == "Send" {
                    Marker::Send
                } else {
                    Marker::Sync
                },
            );
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2402,
                    format!(
                        "cannot pass `{}` to an async call: it is not `{marker}`",
                        type_name(ty)
                    ),
                )
                .with_label(Label::primary(
                    arg.expr.span(),
                    format!("`{}` crosses a task boundary here", type_name(ty)),
                ))
                .with_note(note)
                .with_note(ASYNC_CAPTURE_SHARING_NOTE)
                .with_help(help),
            );
        }
    }

    fn is_interface_type(&self, ty: &Type) -> bool {
        matches!(ty, Type::Named(n) if self.symbols.lookup_interface(n).is_some())
    }

    /// The `Task<T>` Send rule (willow-dgwo.5, spec §9): a task produced by an
    /// async fn may be moved between workers only if its worker-movable frame —
    /// the return value, parameters, and locals live across `await` — is entirely
    /// `Send`. Consumed by the multi-worker capstone (willow-dgwo.9) to reject
    /// stealing a non-`Send` task.
    pub(super) fn check_async_task_send(
        &mut self,
        span: Span,
        body: &Block,
        ret: &Type,
        params: &[(&Type, Span)],
        locals: &[Type],
        locals_offset: usize,
    ) {
        let bindings = std::mem::take(&mut self.local.async_local_bindings);
        // Declaration spans include the body; fallback diagnostics belong on
        // the signature. Parameter/local labels keep their more precise spans.
        let span = Span {
            end: body.span.start,
            ..span
        };
        if !self.enforce_send_sync {
            return;
        }
        // Classify frame slots once, retaining the offending slot for its label.
        let mut bad_slot = None;
        for (index, ty) in params
            .iter()
            .map(|(ty, _)| *ty)
            .chain(locals.iter())
            .chain(std::iter::once(ret))
            .enumerate()
        {
            if !self.is_send(ty) {
                bad_slot = Some((index, ty));
                break;
            }
        }
        let Some((index, first_bad)) = bad_slot else {
            return;
        };
        let binding = if index >= params.len() && index < params.len() + locals.len() {
            bindings.get(&(locals_offset + index - params.len()))
        } else {
            None
        };
        let (note, help) = self.marker_failure(first_bad, Marker::Send);
        let mut primary_span = params.get(index).map_or(span, |(_, span)| *span);
        primary_span.end = primary_span.end.min(body.span.start);
        let mut diagnostic = Diagnostic::new(
            Severity::Error,
            ErrorCode::E2402,
            format!(
                "async task frame is not `Send`: `{}` cannot move between workers",
                type_name(first_bad)
            ),
        )
        .with_label(Label::primary(
            binding.map_or(primary_span, |(_, span)| *span),
            binding.map_or_else(
                || "this async task may be scheduled on another worker".into(),
                |(name, _)| format!("`{name}` is retained in this async task frame"),
            ),
        ))
        .with_note(note)
        .with_help(help);
        if let Some((name, binding_span)) = binding {
            use crate::parser::iter::{AstEvent, AstWalk};
            let mut walk = AstWalk::new(AstEvent::Block(body));
            while let Some(event) = walk.next() {
                match event {
                    AstEvent::Lambda(_) => walk.skip_children(),
                    AstEvent::Expr(Expr::Await(wait)) if wait.span.start >= binding_span.start => {
                        diagnostic = diagnostic.with_label(Label::secondary(
                            wait.span,
                            format!("the frame containing `{name}` can suspend here"),
                        ));
                        break;
                    }
                    _ => {}
                }
            }
        }
        self.push(diagnostic);
    }

    /// Check the strongest inherited interface contract once per class. Traverse
    /// interface diamonds once rather than classifying fields per interface.
    pub(super) fn check_class_marker_contract(&mut self, c: &ClassDecl) {
        let required = self.inherited_marker_contract(&c.name, &mut HashSet::new());
        let Some(marker) = required else {
            return;
        };
        let mut reasons = Some(MarkerFailure::default());
        let mut walk = MarkerWalk::default();
        if self.marker_holds_with_reason(
            &Type::Named(c.name.clone()),
            marker,
            &mut walk,
            &mut reasons,
        ) {
            self.marker_contract_proven.extend(walk.proven);
            return;
        }
        // Without a recursive back-edge, completed positive subgraphs are
        // independently proven even when a later field rejects the root.
        if !walk.saw_cycle {
            self.marker_contract_proven.extend(walk.proven);
        }
        let mut failure = reasons.unwrap();
        failure.notes.reverse();
        let name = if matches!(marker, Marker::Send) {
            "Send"
        } else {
            "Sync"
        };
        self.push(Diagnostic::new(Severity::Error, ErrorCode::E2406,
            format!("class `{}` does not satisfy its implemented interface's `{name}` contract", c.name))
            .with_label(Label::primary(failure.field_span.unwrap_or(c.span),
                format!("this field prevents `{}` from being `{name}`", c.name)))
            .with_label(Label::secondary(c.span, format!("`{name}` is required by an implemented interface")))
            .with_note(failure.notes.join(" -> "))
            .with_help(failure.help.unwrap_or("all instance fields, including inherited fields, must satisfy the interface's marker contract")));
    }

    fn inherited_marker_contract(
        &mut self,
        name: &str,
        active: &mut HashSet<String>,
    ) -> Option<Marker> {
        if let Some(&cached) = self.marker_contracts.get(name) {
            return cached;
        }
        let builtin = match name {
            "Sync" => Some(Marker::Sync),
            "Send" => Some(Marker::Send),
            _ => None,
        };
        if builtin.is_some() {
            return builtin;
        }
        // Invalid inheritance cycles have their own diagnostics.
        if !active.insert(name.to_string()) {
            return None;
        }
        let parents: Vec<String> = if let Some(class) = self.symbols.lookup_class(name) {
            class
                .base_class
                .iter()
                .cloned()
                .chain(class.implements.iter().filter_map(|ty| match ty {
                    Type::Named(n) | Type::Generic(n, _) if n != "Send" && n != "Sync" => {
                        Some(n.clone())
                    }
                    _ => None,
                }))
                .collect()
        } else if let Some(interface) = self.symbols.lookup_interface(name) {
            interface.extends.clone()
        } else {
            Vec::new()
        };
        let mut required = None;
        for parent in parents {
            match self.inherited_marker_contract(&parent, active) {
                Some(Marker::Sync) => {
                    required = Some(Marker::Sync);
                    break;
                }
                Some(Marker::Send) => required = Some(Marker::Send),
                None => {}
            }
        }
        active.remove(name);
        self.marker_contracts.insert(name.to_string(), required);
        required
    }

    fn marker_holds(&self, ty: &Type, marker: Marker, visiting: &mut MarkerWalk) -> bool {
        self.marker_holds_with_reason(ty, marker, visiting, &mut None)
    }

    #[cfg(test)]
    fn marker_failure_note(&self, ty: &Type, marker: Marker) -> String {
        self.marker_failure(ty, marker).0
    }

    fn marker_failure(&self, ty: &Type, marker: Marker) -> (String, &'static str) {
        let mut reasons = Some(MarkerFailure::default());
        self.marker_holds_with_reason(ty, marker, &mut MarkerWalk::default(), &mut reasons);
        let mut failure = reasons.unwrap();
        failure.notes.reverse();
        (failure.notes.join(" -> "), failure.help.unwrap_or(
            "ensure the failing value satisfies the required Send/Sync contract before crossing a task boundary"))
    }

    fn marker_holds_with_reason(
        &self,
        ty: &Type,
        marker: Marker,
        visiting: &mut MarkerWalk,
        reasons: &mut Option<MarkerFailure>,
    ) -> bool {
        if Self::is_error_type(ty)
            || matches!(ty, Type::Named(name) | Type::Generic(name, _)
                if self.invalid_type_names.borrow().contains(name))
        {
            // The root type error is already reported. Recovery types must not
            // manufacture a second concurrency error.
            return true;
        }
        #[cfg(test)]
        if reasons.is_some() {
            tests::REASON_VISITS.with(|n| n.set(n.get() + 1));
        }
        let key = (ty.clone(), marker);
        if visiting.proven.contains(&key) || self.marker_contract_proven.contains(&key) {
            return true;
        }
        let send = matches!(marker, Marker::Send);
        let result = match ty {
            // Primitives + immutable String are Send + Sync; void/never carry
            // no shared mutable state.
            Type::I64 | Type::F64 | Type::Bool | Type::String | Type::Void | Type::Never => true,

            // A mutable array/map may be sent if its contents are Send, but it is
            // NOT Sync (concurrent push/set/insert races).
            Type::Array(elem) => {
                send && self.marker_holds_with_reason(elem, Marker::Send, visiting, reasons)
            }
            // Function/closure values capture unknown state — conservatively
            // neither Send nor Sync in the MVP.
            Type::Fn(_, _) | Type::Closure(_, _) => false,

            Type::Generic(name, args) => match name.as_str() {
                // Immutable: Send iff args Send, Sync iff args Sync (the frozen
                // collections that may be shared across tasks — willow-dgwo.7).
                "Option" | "Result" | "FrozenArray" | "FrozenMap" => args
                    .iter()
                    .all(|a| self.marker_holds_with_reason(a, marker, visiting, reasons)),
                // Send if K/V Send; never Sync (mutable).
                "Map" => {
                    send && args
                        .iter()
                        .all(|a| self.marker_holds_with_reason(a, Marker::Send, visiting, reasons))
                }
                // Channel<T>/Mutex<T>/BlockingCell<T>: Send + Sync iff T: Send.
                // Each hands out the value only under exclusive access, so T
                // never needs to be Sync.
                "Channel" | "Mutex" | "BlockingCell" => args.first().is_none_or(|t| {
                    self.marker_holds_with_reason(t, Marker::Send, visiting, reasons)
                }),
                // RwLock<T>: Send + Sync iff T: Send + Sync (concurrent readers).
                "RwLock" | "BlockingRwCell" => args.first().is_none_or(|t| {
                    self.marker_holds_with_reason(t, Marker::Send, visiting, reasons)
                        && self.marker_holds_with_reason(t, Marker::Sync, visiting, reasons)
                }),
                // Task/JoinHandle/Future: Send iff T: Send; a task handle is not
                // itself Sync (share results, not the task).
                // `TaskResult<T>` is a view of the same frame, so it carries the
                // same markers as the task it came from (willow-qrj9).
                "Task" | "JoinHandle" | "Future" | "TaskResult" => {
                    send && args.first().is_none_or(|t| {
                        self.marker_holds_with_reason(t, Marker::Send, visiting, reasons)
                    })
                }
                // Range<i64> is a scalar pair.
                "Range" => true,
                _ => self.named_marker_holds(name, args, marker, visiting, reasons),
            },

            Type::Named(name) => match name.as_str() {
                "AtomicI64" | "AtomicBool" | "TcpListener" | "TcpStream" | "CancellationToken"
                | "TaskScope" => true,
                _ => self.named_marker_holds(name, &[], marker, visiting, reasons),
            },
        };
        if !result && let Some(failure) = reasons {
            // Reuse the reason traversal: select advice at the failing leaf,
            // with reader-sharing wrappers explaining their stronger contract.
            let help = match ty {
                Type::Generic(name, _) if matches!(name.as_str(), "RwLock" | "BlockingRwCell") => {
                    Some(
                        "RwLock<T> and BlockingRwCell<T> require T: Send + Sync because readers share it; use Mutex<T> when T is Send, or freeze mutable data so it is Sync",
                    )
                }
                Type::Array(_) if !send => Some(
                    "mutable Array values are never Sync; use FrozenArray via .freeze(), or Mutex<Array<T>> when the elements are Send",
                ),
                Type::Generic(name, _) if name == "Map" && !send => Some(
                    "mutable Map values are never Sync; use FrozenMap via .freeze(), or Mutex<Map<K, V>> when keys and values are Send",
                ),
                Type::Named(name) if self.symbols.lookup_interface(name).is_some() => {
                    Some(if send {
                        "interface values need a declared `extends Send` (or `extends Sync`) contract if every implementation satisfies it"
                    } else {
                        "interface values need a declared `extends Sync` contract if every implementation satisfies it"
                    })
                }
                Type::Fn(_, _) | Type::Closure(_, _) => Some(
                    "function and closure values have unknown captures; pass Send/Sync data instead of retaining the callable across a task boundary",
                ),
                _ => None,
            };
            if help.is_some() {
                failure.help = help;
            }
        }
        if !result
            && let Some(reasons) = reasons
            && reasons.notes.is_empty()
        {
            reasons.notes.push(format!(
                "`{}` is not `{}`",
                type_name(ty),
                if send { "Send" } else { "Sync" }
            ));
        }
        if result {
            visiting.proven.insert(key);
        }
        result
    }

    /// Classify a named user type (class / enum / interface), substituting any
    /// generic `args` for the type's parameters.
    fn named_marker_holds(
        &self,
        name: &str,
        args: &[Type],
        marker: Marker,
        visiting: &mut MarkerWalk,
        reasons: &mut Option<MarkerFailure>,
    ) -> bool {
        // Break recursive-type cycles optimistically: a self-reference adds no
        // new constraint beyond the other fields/payloads.
        if !visiting.active.insert((name.to_string(), marker)) {
            visiting.saw_cycle = true;
            return true;
        }
        let result = if let Some(en) = self.symbols.lookup_enum(name) {
            // Fieldless enum → scalar tag (Send + Sync). Payload enum → every
            // payload type (with type params substituted) must hold the marker.
            let subst: Vec<(String, Type)> = en
                .type_params
                .iter()
                .cloned()
                .zip(args.iter().cloned())
                .collect();
            en.variants.iter().all(|v| {
                v.payload_types.iter().all(|p| {
                    self.marker_holds_with_reason(&substitute(p, &subst), marker, visiting, reasons)
                })
            })
        } else if let Some(class) = self.symbols.lookup_class(name) {
            // Send iff all fields Send; Sync iff all fields Sync.
            let base_ok = class.base_class.as_ref().is_none_or(|base| {
                self.marker_holds_with_reason(&Type::Named(base.clone()), marker, visiting, reasons)
            });
            base_ok
                && class.instance_field_order.iter().all(|(name, _)| {
                    let f = &class.fields[name];
                    let ok = self.marker_holds_with_reason(&f.ty, marker, visiting, reasons);
                    if !ok && let Some(reasons) = reasons {
                        reasons.field_span.get_or_insert(f.declaration_span);
                        let prefix = format!("`{}` is not `", type_name(&f.ty));
                        if reasons.notes.len() == 1 && reasons.notes[0].starts_with(&prefix) {
                            reasons.notes[0] = format!(
                                "field `{name}: {}` is not `{}`",
                                type_name(&f.ty),
                                if matches!(marker, Marker::Send) {
                                    "Send"
                                } else {
                                    "Sync"
                                }
                            );
                        } else {
                            reasons
                                .notes
                                .push(format!("field `{name}: {}`", type_name(&f.ty)));
                        }
                    }
                    ok
                })
        } else if self.symbols.lookup_interface(name).is_some() {
            // An interface value follows its declared contract. `extends Sync`
            // is sufficient for Send as well: a Sync interface promises safe
            // shared use, so moving the interface value between workers is okay.
            let holds = match marker {
                Marker::Send => {
                    self.interface_extends(name, "Send") || self.interface_extends(name, "Sync")
                }
                Marker::Sync => self.interface_extends(name, "Sync"),
            };
            if !holds && let Some(reasons) = reasons {
                reasons.notes.push(format!("interface `{name}` does not declare `extends {}`; interface values follow their declared contract, regardless of currently known implementations",
                    if matches!(marker, Marker::Send) { "Send` or `extends Sync" } else { "Sync" }));
            }
            holds
        } else {
            // Unknown type: conservative.
            false
        };
        visiting.active.remove(&(name.to_string(), marker));
        result
    }
}

#[derive(Default)]
struct MarkerFailure {
    field_span: Option<Span>,
    notes: Vec<String>,
    help: Option<&'static str>,
}

#[derive(Default)]
struct MarkerWalk {
    saw_cycle: bool,
    active: HashSet<(String, Marker)>,
    // Scoped to one root query: a false result short-circuits the entire walk.
    proven: HashSet<(Type, Marker)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Marker {
    Send,
    Sync,
}

/// Substitute `Named(param)` occurrences using `subst` (type param → arg).
fn substitute(ty: &Type, subst: &[(String, Type)]) -> Type {
    ty.substitute_names(|name| {
        subst
            .iter()
            .find(|(param, _)| param == name)
            .map(|(_, ty)| ty.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    thread_local! { pub(super) static REASON_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

    #[test]
    fn failure_reason_walk_counts_scale_with_field_chain() {
        for size in [16, 64, 256] {
            let mut source = String::from("class Leaf { pub bad: Array<i64>; }");
            for i in 0..size {
                let child = if i == 0 {
                    "Leaf".into()
                } else {
                    format!("Node{}", i - 1)
                };
                source.push_str(&format!("class Node{i} {{ pub child: {child}; }}"));
            }
            let c = checker(&source);
            REASON_VISITS.with(|n| n.set(0));
            let note = c.marker_failure_note(&named(&format!("Node{}", size - 1)), Marker::Sync);
            assert_eq!(note.matches("field `").count(), size + 1);
            let visits = REASON_VISITS.with(|n| n.get());
            assert_eq!(visits, size + 2);
            eprintln!(
                "marker-reason depth={size} visits={visits} fields={}",
                size + 1
            );
        }
    }

    #[test]
    fn failure_help_reuses_reason_walk_for_fanout_and_repeated_calls() {
        let c = checker("fn main() {}");
        for size in [16, 64, 256] {
            let mut args = vec![Type::I64; size];
            args.push(Type::Array(Box::new(Type::I64)));
            let ty = generic("Result", args);
            REASON_VISITS.with(|n| n.set(0));
            for _ in 0..4 {
                let (note, help) = c.marker_failure(&ty, Marker::Sync);
                assert!(note.contains("Array<i64>"));
                assert!(help.contains("FrozenArray"));
            }
            let visits = REASON_VISITS.with(|n| n.get());
            assert_eq!(visits, 4 * (size + 2));
            eprintln!("marker-help fanout={size} repetitions=4 visits={visits}");
        }
    }

    #[test]
    fn error_types_do_not_create_marker_failures() {
        let mut c = checker("fn main() {}");
        c.invalid_type_names.borrow_mut().insert("Missing".into());
        for ty in [
            TypeChecker::error_type(),
            named("Missing"),
            generic("Missing", vec![Type::I64]),
        ] {
            for wrapper in ["Option", "Result", "Mutex", "Channel", "FrozenArray"] {
                let wrapped = generic(wrapper, vec![ty.clone()]);
                assert!(c.is_send(&wrapped), "{wrapped:?}");
                assert!(c.is_sync(&wrapped), "{wrapped:?}");
            }
        }
        // Genuine constraints survive alongside an erroneous field.
        assert!(!c.is_sync(&Type::Array(Box::new(named("Missing")))));
        c.validate_type(&named("Unresolved"), Span::dummy());
        assert!(c.errors.iter().any(|d| d.code == ErrorCode::E0350));
        assert!(c.is_send(&named("Unresolved")));
        // Merely unknown names are still conservative until diagnosed.
        assert!(!c.is_send(&named("Unchecked")));
    }

    #[test]
    fn task_frame_fallback_labels_only_signatures() {
        for source in [
            "fn inc(x: i64) -> i64 { return x; } async fn bad() -> fn(i64) -> i64 { return inc; } fn main() {}",
            "class Bad { f: fn(i64) -> i64; pub async fn run(self) { println(1); } } fn main() {}",
        ] {
            let (program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
            assert!(errors.is_empty(), "{errors:?}");
            let mut c = TypeChecker::new();
            c.set_enforce_send_sync(true);
            c.check_program(&program);
            let diagnostic = c
                .errors
                .iter()
                .find(|d| d.code == ErrorCode::E2402)
                .expect("real non-Send frame");
            let span = diagnostic.primary_span().unwrap();
            assert!(
                !source[span.start..span.end].contains('{'),
                "{diagnostic:?}"
            );
            assert!(span.end > span.start);
        }
    }

    /// Build a checker with `src`'s declarations registered, so user
    /// class/enum/interface types can be classified.
    fn checker(src: &str) -> TypeChecker {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "parse errors: {errs:?}");
        let mut c = TypeChecker::new();
        c.check_program(&program);
        c
    }

    fn named(n: &str) -> Type {
        Type::Named(n.to_string())
    }
    fn generic(n: &str, args: Vec<Type>) -> Type {
        Type::Generic(n.to_string(), args)
    }

    #[test]
    fn failure_notes_follow_nested_fields_and_wrapper_marker_rules() {
        let c = checker(
            "class Inner { pub inboxes: Array<Channel<i64>>; } class Outer { pub nested: Inner; } class Callback { pub action: fn() -> void; } fn main() {}",
        );
        let note = c.marker_failure_note(&named("Outer"), Marker::Sync);
        assert!(
            note.contains("field `nested: Inner` -> field `inboxes: Array<Channel<i64>>`"),
            "{note}"
        );
        assert!(
            note.ends_with("field `inboxes: Array<Channel<i64>>` is not `Sync`"),
            "{note}"
        );
        for wrapper in ["Option", "Result", "FrozenArray", "FrozenMap", "RwLock"] {
            let note = c.marker_failure_note(&generic(wrapper, vec![named("Outer")]), Marker::Sync);
            assert!(note.contains("inboxes:"), "{wrapper}: {note}");
        }
        for wrapper in ["Mutex", "Channel"] {
            assert!(
                c.marker_failure_note(&generic(wrapper, vec![named("Outer")]), Marker::Sync)
                    .is_empty()
            );
            let note =
                c.marker_failure_note(&generic(wrapper, vec![named("Callback")]), Marker::Sync);
            assert!(note.contains("field `action:"), "{note}");
            assert!(note.ends_with("is not `Send`"), "{note}");
        }
        for ty in [
            Type::I64,
            Type::String,
            generic("FrozenArray", vec![Type::I64]),
        ] {
            assert!(c.marker_failure_note(&ty, Marker::Sync).is_empty());
        }
    }

    #[test]
    fn resolved_async_call_forms_enforce_the_same_capture_rules() {
        for (declaration, call) in [
            ("async fn take(value: Payload) {}", "take(value)"),
            (
                "class Receiver { pub async fn take(self, value: Payload) {} }",
                "new Receiver().take(value)",
            ),
            (
                "class Receiver { pub static async fn take(value: Payload) {} }",
                "Receiver::take(value)",
            ),
        ] {
            for (field_type, rejected) in [
                ("Array<i64>", true),
                ("i64", false),
                ("Mutex<Array<i64>>", false),
            ] {
                let source = format!(
                    "class Payload {{ pub data: {field_type}; }} {declaration} fn invoke(value: Payload) {{ {call}; }} fn main() {{}}"
                );
                let tokens = Lexer::new(&source).tokenize().unwrap();
                let (program, errors) = Parser::new(tokens).parse();
                assert!(errors.is_empty(), "{errors:?}");
                let mut c = TypeChecker::new();
                c.set_enforce_send_sync(true);
                c.check_program(&program);
                let captures: Vec<_> = c
                    .errors
                    .iter()
                    .filter(|d| d.code == ErrorCode::E2402)
                    .collect();
                assert_eq!(
                    captures.len(),
                    usize::from(rejected),
                    "{source}: {:?}",
                    c.errors
                );
                if rejected {
                    assert!(captures[0].notes[0].contains("field `data:"));
                }
            }
        }
    }

    #[test]
    fn immediate_await_explanation_scales_per_failing_argument() {
        for calls in [16, 64, 256] {
            let body = "await take(value);".repeat(calls);
            let source = format!(
                "import std::collections::Array; async fn take(value: Array<i64>) {{}} async fn invoke(value: Array<i64>) {{ {body} }} fn main() {{}}"
            );
            let (program, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            assert!(errors.is_empty(), "{errors:?}");
            let mut c = TypeChecker::new();
            c.set_enforce_send_sync(true);
            REASON_VISITS.with(|n| n.set(0));
            c.check_program(&program);
            assert_eq!(c.errors.len(), calls, "{:?}", c.errors);
            let notes = c
                .errors
                .iter()
                .flat_map(|d| &d.notes)
                .filter(|n| n.as_str() == ASYNC_CAPTURE_SHARING_NOTE)
                .count();
            let visits = REASON_VISITS.with(|n| n.get());
            assert_eq!(notes, calls);
            assert_eq!(visits, calls);
            eprintln!("immediate-await calls={calls} notes={notes} reason_visits={visits}");
        }
    }

    #[test]
    fn immediately_awaited_captures_explain_sharing_and_preserve_remedies() {
        // 3 call forms x 8 types x 3 wait styles = 72 explicit perspectives.
        for form in 0..3 {
            for (ty, code, help) in [
                ("Array<i64>", Some(ErrorCode::E2402), "FrozenArray"),
                ("Map<i64, i64>", Some(ErrorCode::E2402), "FrozenMap"),
                ("Sheet", Some(ErrorCode::E2402), "Mutex<Map"),
                ("Moving", Some(ErrorCode::E2405), "extends Sync"),
                ("FrozenArray<i64>", None, ""),
                ("FrozenMap<i64, i64>", None, ""),
                ("Mutex<Sheet>", None, ""),
                ("i64", None, ""),
            ] {
                for wait in ["direct", "parenthesized", "later"] {
                    let (declaration, call) = match form {
                        0 => (format!("async fn take(value: {ty}) {{}}"), "take(value)"),
                        1 => (
                            format!(
                                "class Receiver {{ pub async fn take(self, value: {ty}) {{}} }}"
                            ),
                            "new Receiver().take(value)",
                        ),
                        _ => (
                            format!(
                                "class Receiver {{ pub static async fn take(value: {ty}) {{}} }}"
                            ),
                            "Receiver::take(value)",
                        ),
                    };
                    let body = match wait {
                        "direct" => format!("await {call};"),
                        "parenthesized" => format!("await ({call});"),
                        _ => format!("let task = {call}; await task;"),
                    };
                    let source = format!(
                        "import std::collections::Array; import std::collections::Map; interface Send {{}} class Sheet {{ pub cells: Map<i64, i64>; }} interface Moving extends Send {{}} {declaration} async fn invoke(value: {ty}) {{ {body} }} fn main() {{}}"
                    );
                    let (program, errors) =
                        Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                    assert!(errors.is_empty(), "{errors:?}");
                    let mut c = TypeChecker::new();
                    c.set_enforce_send_sync(true);
                    c.check_program(&program);
                    assert_eq!(
                        c.errors
                            .iter()
                            .filter(|d| d.severity == Severity::Error)
                            .count(),
                        usize::from(code.is_some()),
                        "{source}: {:?}",
                        c.errors
                    );
                    if let Some(code) = code {
                        let d = &c.errors[0];
                        assert_eq!(d.code, code, "{d:?}");
                        let span = d.primary_span().unwrap();
                        assert_eq!(&source[span.start..span.end], "value");
                        assert!(
                            d.notes.iter().any(|n| n.contains("immediately awaited")
                                && n.contains("exclusive access")),
                            "{d:?}"
                        );
                        assert!(d.helps.iter().any(|h| h.contains(help)), "{d:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn async_diagnostic_help_and_parameter_spans_twenty_four_perspectives() {
        for form in 0..3 {
            for (ty, frame_error, call_code, help) in [
                (
                    "RwLock<Sheet>",
                    true,
                    Some(ErrorCode::E2402),
                    "readers share it",
                ),
                ("Array<i64>", false, Some(ErrorCode::E2402), "FrozenArray"),
                ("Map<i64, i64>", false, Some(ErrorCode::E2402), "FrozenMap"),
                ("Plain", true, Some(ErrorCode::E2404), "interface"),
                ("Moving", false, Some(ErrorCode::E2405), "interface"),
                ("Mutex<Sheet>", false, None, ""),
                ("RwLock<i64>", false, None, ""),
                ("Wrapped", true, Some(ErrorCode::E2402), "readers share it"),
            ] {
                let signature = format!("ok: i64, bad: {ty}");
                let (declaration, call) = match form {
                    0 => (format!("async fn take({signature}) {{}}"), "take(0, value)"),
                    1 => (
                        format!("class Receiver {{ pub async fn take(self, {signature}) {{}} }}"),
                        "new Receiver().take(0, value)",
                    ),
                    _ => (
                        format!("class Receiver {{ pub static async fn take({signature}) {{}} }}"),
                        "Receiver::take(0, value)",
                    ),
                };
                let source = format!(
                    "class Sheet {{ pub cells: Map<i64, i64>; }} class Wrapped {{ pub inner: RwLock<Sheet>; }} interface Plain {{}} interface Moving extends Send {{}} {declaration} fn invoke(value: {ty}) {{ {call}; }} fn main() {{}}"
                );
                let (program, parse_errors) =
                    Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                assert!(parse_errors.is_empty(), "{parse_errors:?}");
                let mut c = TypeChecker::new();
                c.set_enforce_send_sync(true);
                c.check_program(&program);
                let diagnostics: Vec<_> = c
                    .errors
                    .iter()
                    .filter(|d| {
                        matches!(
                            d.code,
                            ErrorCode::E2402 | ErrorCode::E2404 | ErrorCode::E2405
                        )
                    })
                    .collect();
                assert_eq!(
                    diagnostics.len(),
                    usize::from(frame_error) + usize::from(call_code.is_some()),
                    "form={form} {ty}: {:?}",
                    c.errors
                );
                if frame_error {
                    let d = diagnostics
                        .iter()
                        .find(|d| d.message.starts_with("async task frame"))
                        .unwrap();
                    let span = d.primary_span().unwrap();
                    assert_eq!(&source[span.start..span.end], "bad", "{d:?}");
                    assert!(d.helps.iter().any(|h| h.contains(help)), "{d:?}");
                }
                if let Some(code) = call_code {
                    let d = diagnostics
                        .iter()
                        .find(|d| !d.message.starts_with("async task frame"))
                        .unwrap();
                    assert_eq!(d.code, code, "{d:?}");
                    assert_eq!(
                        &source[d.primary_span().unwrap().start..d.primary_span().unwrap().end],
                        "value"
                    );
                    assert!(d.helps.iter().any(|h| h.contains(help)), "{d:?}");
                }
                if !matches!(ty, "Plain" | "Moving") {
                    for d in diagnostics {
                        assert!(!d.helps.iter().any(|h| h.contains("interface")), "{d:?}");
                    }
                }
            }
        }
    }

    // 1-4: primitives + immutable String are Send + Sync.
    #[test]
    fn primitives_and_string_are_send_sync() {
        let c = checker("fn main() {}");
        for t in [Type::I64, Type::F64, Type::Bool, Type::String] {
            assert!(c.is_send(&t), "{t:?} should be Send");
            assert!(c.is_sync(&t), "{t:?} should be Sync");
        }
    }

    // 5-6: Option/Result follow their components.
    #[test]
    fn option_result_follow_components() {
        let c = checker("fn main() {}");
        assert!(c.is_send(&generic("Option", vec![Type::I64])));
        assert!(c.is_sync(&generic("Option", vec![Type::I64])));
        assert!(c.is_send(&generic("Result", vec![Type::I64, Type::String])));
        assert!(c.is_sync(&generic("Result", vec![Type::I64, Type::String])));
        // Option<Array<i64>>: Send (Array Send) but not Sync (Array not Sync).
        let oa = generic("Option", vec![Type::Array(Box::new(Type::I64))]);
        assert!(c.is_send(&oa));
        assert!(!c.is_sync(&oa));
    }

    // 7-8: mutable Array/Map are Send (if elems Send) but never Sync.
    #[test]
    fn array_and_map_are_send_not_sync() {
        let c = checker("fn main() {}");
        let arr = Type::Array(Box::new(Type::I64));
        assert!(c.is_send(&arr));
        assert!(!c.is_sync(&arr));
        let map = generic("Map", vec![Type::String, Type::I64]);
        assert!(c.is_send(&map));
        assert!(!c.is_sync(&map));
    }

    // 9-10: atomics are Send + Sync.
    #[test]
    fn atomics_are_send_sync() {
        let c = checker("fn main() {}");
        for t in [named("AtomicI64"), named("AtomicBool")] {
            assert!(c.is_send(&t));
            assert!(c.is_sync(&t));
        }
    }

    // 11-12: Mutex<T> is Send + Sync iff T: Send (T need not be Sync).
    #[test]
    fn mutex_send_sync_iff_inner_send() {
        let c = checker("fn main() {}");
        let mi = generic("Mutex", vec![Type::I64]);
        assert!(c.is_send(&mi) && c.is_sync(&mi));
        // Mutex<Array<i64>>: Array is Send (not Sync), but Mutex only needs Send.
        let ma = generic("Mutex", vec![Type::Array(Box::new(Type::I64))]);
        assert!(c.is_send(&ma) && c.is_sync(&ma));
    }

    // 13-14: RwLock<T> needs T: Send + Sync.
    #[test]
    fn rwlock_needs_inner_send_and_sync() {
        let c = checker("fn main() {}");
        let ri = generic("RwLock", vec![Type::I64]);
        assert!(c.is_send(&ri) && c.is_sync(&ri));
        // RwLock<Array<i64>>: Array is not Sync → RwLock is neither.
        let ra = generic("RwLock", vec![Type::Array(Box::new(Type::I64))]);
        assert!(!c.is_send(&ra) && !c.is_sync(&ra));
    }

    // 15: Channel<T> is Send + Sync iff T: Send.
    #[test]
    fn channel_send_sync_iff_item_send() {
        let c = checker("fn main() {}");
        let ch = generic("Channel", vec![Type::I64]);
        assert!(c.is_send(&ch) && c.is_sync(&ch));
    }

    // 16: Task<T> is Send iff T: Send, and is not itself Sync.
    #[test]
    fn task_send_not_sync() {
        let c = checker("fn main() {}");
        let t = generic("Task", vec![Type::I64]);
        assert!(c.is_send(&t));
        assert!(!c.is_sync(&t));
    }

    #[test]
    fn buildgraph_interface_contract_twenty_perspectives() {
        for (contract, ok) in [
            ("", false),
            ("extends Send", true),
            ("extends Sync", true),
            ("extends Moving", true),
            ("extends Sharing", true),
        ] {
            for (ty, init) in [
                ("Named", "new A()"),
                ("Array<Named>", "[new A()]"),
                ("Option<Named>", "Option::Some(new A())"),
                ("Holder", "new Holder(new A())"),
            ] {
                let source = format!(
                    "import std::collections::Array; enum Option<T> {{ Some(T), None }} interface Send {{}} interface Sync extends Send {{}} interface Moving extends Send {{}} interface Sharing extends Sync {{}} interface Named {contract} {{ fn label(self) -> String; }} class A implements Named {{ pub fn label(self) -> String {{ return \"a\"; }} }} class Holder {{ pub value: Named; }} async fn main() {{\nlet value: {ty} = {init};\nawait sleep(0);\nprintln(1);\n}}"
                );
                let (program, errors) =
                    Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                assert!(errors.is_empty(), "{errors:?}");
                let mut c = TypeChecker::new();
                c.set_enforce_send_sync(true);
                c.check_program(&program);
                let errors: Vec<_> = c
                    .errors
                    .iter()
                    .filter(|e| e.severity == Severity::Error)
                    .collect();
                if ok {
                    assert!(errors.is_empty(), "{contract}/{ty}: {errors:?}");
                } else {
                    let d = errors
                        .iter()
                        .find(|e| e.code == ErrorCode::E2402)
                        .expect("plain interface is not Send");
                    assert_eq!(d.primary_span().unwrap().line, 2, "{d:?}");
                    assert!(
                        d.labels
                            .iter()
                            .any(|l| l.span.line == 3 && l.message.contains("value")),
                        "{d:?}"
                    );
                    assert!(
                        d.notes
                            .iter()
                            .any(|n| n.contains("declared contract") && n.contains("extends Send")),
                        "{d:?}"
                    );
                }
            }
        }
    }

    // 17: fieldless enums are Send + Sync.
    #[test]
    fn fieldless_enum_is_send_sync() {
        let c = checker("enum Color { Red, Green, Blue }\nfn main() {}");
        assert!(c.is_send(&named("Color")));
        assert!(c.is_sync(&named("Color")));
    }

    // 18: payload enums follow their payload types.
    #[test]
    fn payload_enum_follows_payloads() {
        let c = checker("enum Msg { Text(String), Count(i64) }\nfn main() {}");
        assert!(c.is_send(&named("Msg")));
        assert!(c.is_sync(&named("Msg")));
        // An enum carrying a mutable Array is Send but not Sync.
        let c2 =
            checker("import std::collections::Array;\nenum Box2 { Of(Array<i64>) }\nfn main() {}");
        assert!(c2.is_send(&named("Box2")));
        assert!(!c2.is_sync(&named("Box2")));
    }

    // 19: class follows its fields (Send iff all Send; Sync iff all Sync).
    #[test]
    fn class_follows_fields() {
        let c = checker("class P { x: i64; y: i64; }\nfn main() {}");
        assert!(c.is_send(&named("P")));
        assert!(c.is_sync(&named("P")));
        let c2 =
            checker("import std::collections::Array;\nclass Q { xs: Array<i64>; }\nfn main() {}");
        assert!(c2.is_send(&named("Q")));
        assert!(!c2.is_sync(&named("Q")));
    }

    // 20: interface values follow the interface contract (extends Send/Sync).
    #[test]
    fn interface_follows_extends() {
        let c = checker(
            "interface Plain { fn f(self) -> i64; }\n\
             interface S extends Send { fn f(self) -> i64; }\n\
             interface Y extends Sync { fn f(self) -> i64; }\n\
             fn main() {}",
        );
        assert!(!c.is_send(&named("Plain")) && !c.is_sync(&named("Plain")));
        assert!(c.is_send(&named("S")));
        assert!(c.is_send(&named("Y")));
        assert!(c.is_sync(&named("Y")));
    }

    // Recursive types must not loop forever.
    #[test]
    fn recursive_class_terminates() {
        let c = checker("class Node { next: Node; value: i64; }\nfn main() {}");
        // Should return (no infinite recursion); a class of Send fields is Send.
        assert!(c.is_send(&named("Node")));
    }

    // Function/closure values are conservatively neither.
    #[test]
    fn fn_values_are_neither() {
        let c = checker("fn main() {}");
        let f = Type::Fn(vec![Type::I64], Box::new(Type::I64));
        assert!(!c.is_send(&f) && !c.is_sync(&f));
    }
}

#[cfg(test)]
#[path = "interface_marker_tests.rs"]
mod interface_marker_tests;
