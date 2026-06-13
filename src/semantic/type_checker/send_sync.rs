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
//! - interface: Send iff it `extends Send`; Sync iff it `extends Sync`
//! - function/closure values: conservatively neither (captures unknown)

use std::collections::HashSet;

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity};
use crate::parser::ast::*;
use crate::semantic::symbols::ParamInfo;

use super::*;

/// Value (non-GC, by-copy) types: scalars and the unit-like types. Everything
/// else (String, Array, Map, classes, enums, Channel, Mutex, Task, …) is a
/// heap/GC reference that is shared when passed to a task.
fn is_value_type(ty: &Type) -> bool {
    matches!(
        ty,
        Type::I64 | Type::F64 | Type::Bool | Type::Void | Type::Nil | Type::Never
    )
}

impl TypeChecker {
    /// True if a value of `ty` may be transferred across worker/task boundaries.
    pub(super) fn is_send(&self, ty: &Type) -> bool {
        self.marker_holds(ty, Marker::Send, &mut HashSet::new())
    }

    /// True if a value of `ty` may be shared concurrently by multiple tasks.
    pub(super) fn is_sync(&self, ty: &Type) -> bool {
        self.marker_holds(ty, Marker::Sync, &mut HashSet::new())
    }

    /// Check the arguments passed to an async fn call: each value is captured
    /// into a `Task` that may run on another worker, so a GC-reference argument
    /// must be `Sync` and a scalar/value argument must be `Send` (willow-dgwo.4,
    /// spec §8). Reports E2402 per offending argument.
    ///
    /// Only enforced when `enforce_send_sync` is set — the single-worker
    /// cooperative scheduler never runs tasks in parallel, so the check is a
    /// precondition turned on with multi-worker execution (willow-dgwo.9).
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
            if !ok {
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
                    .with_help(
                        "share it safely with `Mutex<T>`, `RwLock<T>`, `Atomic*`, a `Channel<T>`, or a frozen value",
                    ),
                );
            }
        }
    }

    fn marker_holds(&self, ty: &Type, marker: Marker, visiting: &mut HashSet<String>) -> bool {
        let send = matches!(marker, Marker::Send);
        match ty {
            // Primitives + immutable String are Send + Sync; void/nil/never carry
            // no shared mutable state.
            Type::I64
            | Type::F64
            | Type::Bool
            | Type::String
            | Type::Void
            | Type::Nil
            | Type::Never => true,

            // A mutable array/map may be sent if its contents are Send, but it is
            // NOT Sync (concurrent push/set/insert races).
            Type::Array(elem) => send && self.marker_holds(elem, Marker::Send, visiting),
            Type::Nullable(inner) => self.marker_holds(inner, marker, visiting),

            // Function/closure values capture unknown state — conservatively
            // neither Send nor Sync in the MVP.
            Type::Fn(_, _) => false,

            Type::Generic(name, args) => match name.as_str() {
                // Immutable: Send iff args Send, Sync iff args Sync (the frozen
                // collections that may be shared across tasks — willow-dgwo.7).
                "Option" | "Result" | "FrozenArray" | "FrozenMap" => {
                    args.iter().all(|a| self.marker_holds(a, marker, visiting))
                }
                // Send if K/V Send; never Sync (mutable).
                "Map" => {
                    send && args
                        .iter()
                        .all(|a| self.marker_holds(a, Marker::Send, visiting))
                }
                // Channel<T>/Mutex<T>: Send + Sync iff T: Send.
                "Channel" | "Mutex" => args
                    .first()
                    .is_none_or(|t| self.marker_holds(t, Marker::Send, visiting)),
                // RwLock<T>: Send + Sync iff T: Send + Sync (concurrent readers).
                "RwLock" => args.first().is_none_or(|t| {
                    self.marker_holds(t, Marker::Send, visiting)
                        && self.marker_holds(t, Marker::Sync, visiting)
                }),
                // Task/JoinHandle/Future: Send iff T: Send; a task handle is not
                // itself Sync (share results, not the task).
                "Task" | "JoinHandle" | "Future" => {
                    send && args
                        .first()
                        .is_none_or(|t| self.marker_holds(t, Marker::Send, visiting))
                }
                // Range<i64> is a scalar pair.
                "Range" => true,
                _ => self.named_marker_holds(name, args, marker, visiting),
            },

            Type::Named(name) => match name.as_str() {
                "AtomicI64" | "AtomicBool" => true,
                _ => self.named_marker_holds(name, &[], marker, visiting),
            },
        }
    }

    /// Classify a named user type (class / enum / interface), substituting any
    /// generic `args` for the type's parameters.
    fn named_marker_holds(
        &self,
        name: &str,
        args: &[Type],
        marker: Marker,
        visiting: &mut HashSet<String>,
    ) -> bool {
        // Break recursive-type cycles optimistically: a self-reference adds no
        // new constraint beyond the other fields/payloads.
        if !visiting.insert(name.to_string()) {
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
                v.payload_types
                    .iter()
                    .all(|p| self.marker_holds(&substitute(p, &subst), marker, visiting))
            })
        } else if let Some(class) = self.symbols.lookup_class(name) {
            // Send iff all fields Send; Sync iff all fields Sync.
            class
                .fields
                .values()
                .all(|f| self.marker_holds(&f.ty, marker, visiting))
        } else if self.symbols.lookup_interface(name).is_some() {
            // An interface value is Send/Sync only if the interface contract
            // requires it (`interface I extends Send` / `... extends Sync`).
            match marker {
                Marker::Send => self.interface_extends(name, "Send"),
                Marker::Sync => self.interface_extends(name, "Sync"),
            }
        } else {
            // Unknown type: conservative.
            false
        };
        visiting.remove(name);
        result
    }
}

#[derive(Clone, Copy)]
enum Marker {
    Send,
    Sync,
}

/// Substitute `Named(param)` occurrences using `subst` (type param → arg).
fn substitute(ty: &Type, subst: &[(String, Type)]) -> Type {
    match ty {
        Type::Named(n) => subst
            .iter()
            .find(|(p, _)| p == n)
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| ty.clone()),
        Type::Array(e) => Type::Array(Box::new(substitute(e, subst))),
        Type::Nullable(i) => Type::Nullable(Box::new(substitute(i, subst))),
        Type::Generic(n, a) => {
            Type::Generic(n.clone(), a.iter().map(|x| substitute(x, subst)).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

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
