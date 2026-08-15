//! Declarative description of Willow's public standard-library namespace.
//!
//! Import validation, type-argument checking, and builtin module registration
//! all consume this table. Semantics that require bespoke lowering (notably
//! `Array`) remain in their respective compiler phases, but names and public
//! signatures are defined here exactly once.

use crate::parser::ast::Type;

/// A type used in a standard-library function signature.
///
/// Recursive on purpose (willow-uqzx, catalog item 11). This enum used to carry
/// one variant per concrete *combination* the table happened to need —
/// `OptionString`, `StringArray`, `StringIoResult`, `VoidIoResult`,
/// `TaskStringIoResult`, `TaskVoidIoResult`, `TaskBool`, `TcpListenerIoResult`,
/// `TaskTcpStreamIoResult`, `FrozenI64Array`, `I64Mapper`, `TaskI64Array`. Every
/// new signature shape cost a variant here *and* an arm in the type checker's
/// lowering, the enum could not express a shape nobody had written down yet, and
/// nothing tied `TaskStringIoResult` to `StringIoResult` beyond the two names
/// looking alike.
///
/// Constructors now compose: `Task<Result<String, IoError>>` is
/// `Task(&Result(&String, &Named("IoError")))`, and the lowering is one
/// recursive walk with an arm per constructor rather than per combination.
///
/// `&'static` rather than `Box` because the whole schema is a compile-time
/// constant; that also keeps the enum `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdType {
    I64,
    Bool,
    String,
    Void,
    /// A named std type with no type arguments (`TcpListener`, `IoError`).
    Named(&'static str),
    /// `[T]` — the built-in array constructor, which has its own [`Type`] node
    /// rather than a generic name.
    Array(&'static StdType),
    /// `Option<T>`; used where a runtime raw null is the niche encoding of
    /// semantic absence rather than a source-level value.
    Option(&'static StdType),
    /// `Result<T, E>`.
    Result(&'static StdType, &'static StdType),
    /// `Task<T>` — a scheduler handle, as returned by the blocking-pool and
    /// netpoll-driven functions.
    Task(&'static StdType),
    /// Any other applied generic (`FrozenArray<i64>`, `Map<K, V>`).
    Generic(&'static str, &'static [StdType]),
    /// `fn(params) -> ret` — a non-capturing function value.
    Fn(&'static [StdType], &'static StdType),
    /// The I/O functions accept every printable Willow value. Not a shape but a
    /// polymorphism marker, resolved by `std::io` checking — which is why it is
    /// the one variant with no AST type.
    Printable,
}

impl StdType {
    /// The AST type this schema type denotes, or `None` for [`StdType::Printable`],
    /// which stands for a set of types rather than one.
    ///
    /// One arm per constructor. Adding `Task<Result<Array<i64>, IoError>>` to the
    /// table below needs no change here.
    pub fn to_ast_type(&self) -> Option<Type> {
        Some(match self {
            StdType::I64 => Type::I64,
            StdType::Bool => Type::Bool,
            StdType::String => Type::String,
            StdType::Void => Type::Void,
            StdType::Named(name) => Type::Named((*name).to_string()),
            StdType::Array(elem) => Type::Array(Box::new(elem.to_ast_type()?)),
            StdType::Option(inner) => {
                Type::Generic("Option".to_string(), vec![inner.to_ast_type()?])
            }
            StdType::Result(ok, err) => Type::Generic(
                "Result".to_string(),
                vec![ok.to_ast_type()?, err.to_ast_type()?],
            ),
            StdType::Task(inner) => Type::Generic("Task".to_string(), vec![inner.to_ast_type()?]),
            StdType::Generic(name, args) => Type::Generic(
                (*name).to_string(),
                args.iter()
                    .map(StdType::to_ast_type)
                    .collect::<Option<Vec<_>>>()?,
            ),
            StdType::Fn(params, ret) => Type::Fn(
                params
                    .iter()
                    .map(StdType::to_ast_type)
                    .collect::<Option<Vec<_>>>()?,
                Box::new(ret.to_ast_type()?),
            ),
            StdType::Printable => return None,
        })
    }
}

/// The public shape of a standard-library item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdItemKind {
    Type {
        type_params: usize,
        /// Name used after the compiler has resolved an import.
        builtin_name: &'static str,
    },
    Function {
        params: &'static [StdType],
        return_type: StdType,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdItemSchema {
    pub name: &'static str,
    pub kind: StdItemKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdModuleSchema {
    pub name: &'static str,
    pub items: &'static [StdItemSchema],
}

macro_rules! std_type {
    ($name:literal, $arity:literal, $builtin:literal) => {
        StdItemSchema {
            name: $name,
            kind: StdItemKind::Type {
                type_params: $arity,
                builtin_name: $builtin,
            },
        }
    };
}

macro_rules! std_function {
    ($name:literal, [$($param:expr),* $(,)?] -> $ret:expr) => {
        StdItemSchema {
            name: $name,
            kind: StdItemKind::Function {
                params: &[$($param),*],
                return_type: $ret,
            },
        }
    };
}

// Short alias so the signature table below reads as types rather than as paths.
use StdType as T;

const IO_ERROR: StdType = T::Named("IoError");
const TCP_LISTENER: StdType = T::Named("TcpListener");
const TCP_STREAM: StdType = T::Named("TcpStream");

// Shapes the table repeats. These are ordinary compositions of the constructors
// above, not enum variants: a new combination costs one line here (or none, if
// it is written inline) and no code anywhere else.
const STRING_IO: StdType = T::Result(&T::String, &IO_ERROR);
const VOID_IO: StdType = T::Result(&T::Void, &IO_ERROR);
const LISTENER_IO: StdType = T::Result(&TCP_LISTENER, &IO_ERROR);
const STREAM_IO: StdType = T::Result(&TCP_STREAM, &IO_ERROR);

const COLLECTIONS: &[StdItemSchema] = &[std_type!("Array", 1, "Array"), std_type!("Map", 2, "Map")];
const OPTION: &[StdItemSchema] = &[std_type!("Option", 1, "Option")];
const RESULT: &[StdItemSchema] = &[std_type!("Result", 2, "Result")];
const IO: &[StdItemSchema] = &[
    std_function!("println", [T::Printable] -> T::Void),
    std_function!("print", [T::Printable] -> T::Void),
    std_function!("eprintln", [T::Printable] -> T::Void),
];
// Compatibility calls remain synchronous. The `_async` forms isolate regular
// file operations on the bounded blocking pool and return scheduler Tasks.
const FS: &[StdItemSchema] = &[
    std_function!("temp_path", [T::String] -> T::String),
    std_function!("read_to_string", [T::String] -> STRING_IO),
    std_function!("write_string", [T::String, T::String] -> VOID_IO),
    std_function!("exists", [T::String] -> T::Bool),
    std_function!("remove_file", [T::String] -> VOID_IO),
    std_function!("read_to_string_async", [T::String] -> T::Task(&STRING_IO)),
    std_function!("write_string_async", [T::String, T::String] -> T::Task(&VOID_IO)),
    std_function!("exists_async", [T::String] -> T::Task(&T::Bool)),
    std_function!("remove_file_async", [T::String] -> T::Task(&VOID_IO)),
];

const ENV: &[StdItemSchema] = &[
    std_function!("args", [] -> T::Array(&T::String)),
    std_function!("arg", [T::I64] -> T::Option(&T::String)),
    std_function!("args_len", [] -> T::I64),
    std_function!("program_name", [] -> T::String),
];

// Addresses are numeric `IP:port` strings in v1, so no hidden DNS lookup can
// block a scheduler worker. Socket readiness operations return Tasks and are
// driven by the platform netpoll backend.
const NET: &[StdItemSchema] = &[
    std_type!("TcpListener", 0, "TcpListener"),
    std_type!("TcpStream", 0, "TcpStream"),
    std_function!("bind", [T::String] -> LISTENER_IO),
    std_function!("local_addr", [TCP_LISTENER] -> STRING_IO),
    std_function!("peer_addr", [TCP_STREAM] -> STRING_IO),
    std_function!("shutdown", [TCP_STREAM] -> VOID_IO),
    std_function!("connect_async", [T::String] -> T::Task(&STREAM_IO)),
    std_function!("accept_async", [TCP_LISTENER] -> T::Task(&STREAM_IO)),
    std_function!("read_async", [TCP_STREAM, T::I64] -> T::Task(&STRING_IO)),
    std_function!("write_async", [TCP_STREAM, T::String] -> T::Task(&VOID_IO)),
];

const PARALLEL: &[StdItemSchema] = &[std_function!(
    "map",
    [T::Generic("FrozenArray", &[T::I64]), T::Fn(&[T::I64], &T::I64)]
        -> T::Task(&T::Array(&T::I64))
)];

/// Complete public `std` surface.
pub const STDLIB_SCHEMA: &[StdModuleSchema] = &[
    StdModuleSchema {
        name: "collections",
        items: COLLECTIONS,
    },
    StdModuleSchema {
        name: "option",
        items: OPTION,
    },
    StdModuleSchema {
        name: "result",
        items: RESULT,
    },
    StdModuleSchema {
        name: "io",
        items: IO,
    },
    StdModuleSchema {
        name: "env",
        items: ENV,
    },
    StdModuleSchema {
        name: "fs",
        items: FS,
    },
    StdModuleSchema {
        name: "net",
        items: NET,
    },
    StdModuleSchema {
        name: "parallel",
        items: PARALLEL,
    },
];

pub fn module(name: &str) -> Option<&'static StdModuleSchema> {
    STDLIB_SCHEMA.iter().find(|module| module.name == name)
}

pub fn item(module_name: &str, item_name: &str) -> Option<&'static StdItemSchema> {
    module(module_name)?
        .items
        .iter()
        .find(|item| item.name == item_name)
}

pub fn type_item(module_name: &str, item_name: &str) -> Option<(usize, &'static str)> {
    match item(module_name, item_name)?.kind {
        StdItemKind::Type {
            type_params,
            builtin_name,
        } => Some((type_params, builtin_name)),
        StdItemKind::Function { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn schema_names_are_unique() {
        let mut modules = HashSet::new();
        for module in STDLIB_SCHEMA {
            assert!(
                modules.insert(module.name),
                "duplicate std module: {}",
                module.name
            );
            let mut items = HashSet::new();
            for item in module.items {
                assert!(
                    items.insert(item.name),
                    "duplicate item in std::{}: {}",
                    module.name,
                    item.name
                );
            }
        }
    }

    #[test]
    fn type_lookup_exposes_arity_and_builtin_name() {
        assert_eq!(type_item("collections", "Array"), Some((1, "Array")));
        assert_eq!(type_item("result", "Result"), Some((2, "Result")));
        assert_eq!(type_item("net", "TcpStream"), Some((0, "TcpStream")));
        assert_eq!(type_item("io", "println"), None);
    }
}

#[cfg(test)]
mod recursive_schema_tests {
    //! Perspectives 1-22 for the recursive [`StdType`] (willow-uqzx, catalog
    //! item 11).
    //!
    //! The refactor that made `StdType` recursive was meant to change no
    //! behavior, so the load-bearing test is the golden signature table: every
    //! std function's parameter and return types spelled out as the AST types
    //! the checker will register. If a composition is wrong, the golden row
    //! disagrees; if a constructor's lowering is wrong, many rows do.

    use super::*;

    fn ast(ty: StdType) -> Type {
        ty.to_ast_type().expect("schema type has an AST type")
    }

    fn generic(name: &str, args: Vec<Type>) -> Type {
        Type::Generic(name.to_string(), args)
    }

    fn named(name: &str) -> Type {
        Type::Named(name.to_string())
    }

    fn io_error() -> Type {
        named("IoError")
    }

    fn signature(module_name: &str, item_name: &str) -> (Vec<Type>, Type) {
        let StdItemKind::Function {
            params,
            return_type,
        } = item(module_name, item_name)
            .unwrap_or_else(|| panic!("std::{module_name}::{item_name} exists"))
            .kind
        else {
            panic!("std::{module_name}::{item_name} is a type, not a function");
        };
        (params.iter().copied().map(ast).collect(), ast(return_type))
    }

    /// Perspective 1: the four scalar leaves map to their AST counterparts.
    #[test]
    fn p01_scalar_leaves() {
        assert_eq!(ast(StdType::I64), Type::I64);
        assert_eq!(ast(StdType::Bool), Type::Bool);
        assert_eq!(ast(StdType::String), Type::String);
        assert_eq!(ast(StdType::Void), Type::Void);
    }

    /// Perspective 2: `Named` carries an arbitrary nominal name through, which
    /// is what lets `IoError`, `TcpListener` and `TcpStream` stop being
    /// variants.
    #[test]
    fn p02_named_passes_the_name_through() {
        assert_eq!(ast(StdType::Named("IoError")), named("IoError"));
        assert_eq!(ast(StdType::Named("TcpListener")), named("TcpListener"));
        assert_eq!(ast(StdType::Named("Anything")), named("Anything"));
    }

    /// Perspective 3: `Array` lowers to the dedicated `Type::Array` node, not to
    /// a generic named "Array" — the two are different types downstream.
    #[test]
    fn p03_array_uses_the_array_node() {
        assert_eq!(
            ast(StdType::Array(&StdType::String)),
            Type::Array(Box::new(Type::String))
        );
        assert!(matches!(ast(StdType::Array(&StdType::I64)), Type::Array(_)));
    }

    /// Perspective 4: `Option<T>` and `Result<T, E>` lower to the prelude
    /// generics by name, with arguments in declaration order.
    #[test]
    fn p04_option_and_result() {
        assert_eq!(
            ast(StdType::Option(&StdType::String)),
            generic("Option", vec![Type::String])
        );
        assert_eq!(
            ast(StdType::Result(&StdType::I64, &StdType::String)),
            generic("Result", vec![Type::I64, Type::String])
        );
    }

    /// Perspective 5: `Result`'s two arguments do not commute. A lowering that
    /// swapped them would still type-check but would report the error type as
    /// the success type.
    #[test]
    fn p05_result_argument_order_matters() {
        let ok_first = ast(StdType::Result(&StdType::String, &StdType::Void));
        let ok_second = ast(StdType::Result(&StdType::Void, &StdType::String));
        assert_ne!(ok_first, ok_second);
    }

    /// Perspective 6: `Task<T>` nests over anything, including another Task.
    /// The old enum could only express the three Task shapes it had variants for.
    #[test]
    fn p06_task_nests_arbitrarily() {
        assert_eq!(
            ast(StdType::Task(&StdType::Bool)),
            generic("Task", vec![Type::Bool])
        );
        assert_eq!(
            ast(StdType::Task(&StdType::Task(&StdType::I64))),
            generic("Task", vec![generic("Task", vec![Type::I64])])
        );
    }

    /// Perspective 7: three levels of nesting compose in one walk —
    /// `Task<Result<Array<String>, IoError>>` needs no new variant.
    #[test]
    fn p07_deep_composition() {
        let ty = StdType::Task(&StdType::Result(
            &StdType::Array(&StdType::String),
            &StdType::Named("IoError"),
        ));
        assert_eq!(
            ast(ty),
            generic(
                "Task",
                vec![generic(
                    "Result",
                    vec![Type::Array(Box::new(Type::String)), io_error()]
                )]
            )
        );
    }

    /// Perspective 8: `Generic` handles applied generics of any arity, including
    /// zero arguments and two.
    #[test]
    fn p08_generic_arity() {
        assert_eq!(
            ast(StdType::Generic("Marker", &[])),
            generic("Marker", vec![])
        );
        assert_eq!(
            ast(StdType::Generic("FrozenArray", &[StdType::I64])),
            generic("FrozenArray", vec![Type::I64])
        );
        assert_eq!(
            ast(StdType::Generic("Map", &[StdType::String, StdType::I64])),
            generic("Map", vec![Type::String, Type::I64])
        );
    }

    /// Perspective 9: `Fn` lowers parameters and return type independently, and
    /// a zero-parameter function is expressible.
    #[test]
    fn p09_function_types() {
        assert_eq!(
            ast(StdType::Fn(&[StdType::I64], &StdType::I64)),
            Type::Fn(vec![Type::I64], Box::new(Type::I64))
        );
        assert_eq!(
            ast(StdType::Fn(&[], &StdType::Void)),
            Type::Fn(vec![], Box::new(Type::Void))
        );
        assert_eq!(
            ast(StdType::Fn(
                &[StdType::String, StdType::Bool],
                &StdType::Array(&StdType::I64)
            )),
            Type::Fn(
                vec![Type::String, Type::Bool],
                Box::new(Type::Array(Box::new(Type::I64)))
            )
        );
    }

    /// Perspective 10: `Printable` is the one schema type with no AST type. It
    /// marks polymorphism, and `std::io` resolution handles it separately.
    #[test]
    fn p10_printable_has_no_ast_type() {
        assert_eq!(StdType::Printable.to_ast_type(), None);
    }

    /// Perspective 11: `Printable` nested inside a constructor propagates the
    /// `None` rather than lowering to a bogus type. Nothing in the table does
    /// this, and this test is why it stays that way.
    #[test]
    fn p11_printable_poisons_its_container() {
        assert_eq!(StdType::Task(&StdType::Printable).to_ast_type(), None);
        assert_eq!(StdType::Array(&StdType::Printable).to_ast_type(), None);
        assert_eq!(
            StdType::Generic("Box", &[StdType::I64, StdType::Printable]).to_ast_type(),
            None
        );
        assert_eq!(
            StdType::Fn(&[StdType::Printable], &StdType::Void).to_ast_type(),
            None
        );
    }

    /// Perspective 12: every type in the whole schema lowers, except the
    /// `Printable` markers on `std::io`. A table entry that cannot be lowered is
    /// a signature the checker could never register.
    #[test]
    fn p12_whole_schema_lowers() {
        for module in STDLIB_SCHEMA {
            for entry in module.items {
                let StdItemKind::Function {
                    params,
                    return_type,
                } = entry.kind
                else {
                    continue;
                };
                for (index, param) in params.iter().enumerate() {
                    if *param == StdType::Printable {
                        assert_eq!(
                            module.name, "io",
                            "only std::io takes Printable; std::{}::{} does too",
                            module.name, entry.name
                        );
                        continue;
                    }
                    assert!(
                        param.to_ast_type().is_some(),
                        "std::{}::{} parameter {index} does not lower",
                        module.name,
                        entry.name
                    );
                }
                assert!(
                    return_type.to_ast_type().is_some(),
                    "std::{}::{} return type does not lower",
                    module.name,
                    entry.name
                );
            }
        }
    }

    /// Perspective 13: `std::env` — the array and option shapes that used to be
    /// the `StringArray` and `OptionString` variants.
    #[test]
    fn p13_env_signatures() {
        assert_eq!(
            signature("env", "args"),
            (vec![], Type::Array(Box::new(Type::String)))
        );
        assert_eq!(
            signature("env", "arg"),
            (vec![Type::I64], generic("Option", vec![Type::String]))
        );
        assert_eq!(signature("env", "args_len"), (vec![], Type::I64));
        assert_eq!(signature("env", "program_name"), (vec![], Type::String));
    }

    /// Perspective 14: the synchronous `std::fs` surface.
    #[test]
    fn p14_fs_sync_signatures() {
        let string_io = generic("Result", vec![Type::String, io_error()]);
        let void_io = generic("Result", vec![Type::Void, io_error()]);
        assert_eq!(
            signature("fs", "temp_path"),
            (vec![Type::String], Type::String)
        );
        assert_eq!(
            signature("fs", "read_to_string"),
            (vec![Type::String], string_io)
        );
        assert_eq!(
            signature("fs", "write_string"),
            (vec![Type::String, Type::String], void_io.clone())
        );
        assert_eq!(signature("fs", "exists"), (vec![Type::String], Type::Bool));
        assert_eq!(
            signature("fs", "remove_file"),
            (vec![Type::String], void_io)
        );
    }

    /// Perspective 15: the `_async` `std::fs` surface is the synchronous one
    /// wrapped in `Task`. Before this refactor the relationship was invisible:
    /// `TaskStringIoResult` and `StringIoResult` were unrelated variants.
    #[test]
    fn p15_fs_async_is_the_sync_shape_in_a_task() {
        for (sync, asynchronous) in [
            ("read_to_string", "read_to_string_async"),
            ("write_string", "write_string_async"),
            ("exists", "exists_async"),
            ("remove_file", "remove_file_async"),
        ] {
            let (sync_params, sync_ret) = signature("fs", sync);
            let (async_params, async_ret) = signature("fs", asynchronous);
            assert_eq!(sync_params, async_params, "{asynchronous} parameters");
            assert_eq!(
                async_ret,
                generic("Task", vec![sync_ret]),
                "{asynchronous} return type"
            );
        }
    }

    /// Perspective 16: `std::net`'s synchronous surface, including the two
    /// nominal handle types.
    #[test]
    fn p16_net_sync_signatures() {
        let string_io = generic("Result", vec![Type::String, io_error()]);
        assert_eq!(
            signature("net", "bind"),
            (
                vec![Type::String],
                generic("Result", vec![named("TcpListener"), io_error()])
            )
        );
        assert_eq!(
            signature("net", "local_addr"),
            (vec![named("TcpListener")], string_io.clone())
        );
        assert_eq!(
            signature("net", "peer_addr"),
            (vec![named("TcpStream")], string_io)
        );
        assert_eq!(
            signature("net", "shutdown"),
            (
                vec![named("TcpStream")],
                generic("Result", vec![Type::Void, io_error()])
            )
        );
    }

    /// Perspective 17: `std::net`'s async surface.
    #[test]
    fn p17_net_async_signatures() {
        let stream_io = generic("Result", vec![named("TcpStream"), io_error()]);
        let string_io = generic("Result", vec![Type::String, io_error()]);
        let void_io = generic("Result", vec![Type::Void, io_error()]);
        assert_eq!(
            signature("net", "connect_async"),
            (vec![Type::String], generic("Task", vec![stream_io.clone()]))
        );
        assert_eq!(
            signature("net", "accept_async"),
            (vec![named("TcpListener")], generic("Task", vec![stream_io]))
        );
        assert_eq!(
            signature("net", "read_async"),
            (
                vec![named("TcpStream"), Type::I64],
                generic("Task", vec![string_io])
            )
        );
        assert_eq!(
            signature("net", "write_async"),
            (
                vec![named("TcpStream"), Type::String],
                generic("Task", vec![void_io])
            )
        );
    }

    /// Perspective 18: `std::parallel::map` — the only signature using both
    /// `Generic` and `Fn`, and the only one whose parameters were three separate
    /// combination variants (`FrozenI64Array`, `I64Mapper`, `TaskI64Array`).
    #[test]
    fn p18_parallel_map_signature() {
        assert_eq!(
            signature("parallel", "map"),
            (
                vec![
                    generic("FrozenArray", vec![Type::I64]),
                    Type::Fn(vec![Type::I64], Box::new(Type::I64)),
                ],
                generic("Task", vec![Type::Array(Box::new(Type::I64))])
            )
        );
    }

    /// Perspective 19: `std::io`'s functions take one `Printable` and return
    /// void. Their parameter is deliberately not lowerable.
    #[test]
    fn p19_io_signatures_are_printable_to_void() {
        for name in ["println", "print", "eprintln"] {
            let StdItemKind::Function {
                params,
                return_type,
            } = item("io", name).expect("io function").kind
            else {
                panic!("std::io::{name} is a function");
            };
            assert_eq!(params, &[StdType::Printable], "std::io::{name} parameters");
            assert_eq!(return_type, StdType::Void, "std::io::{name} return type");
        }
    }

    /// Perspective 20: the schema's type items are unchanged by the refactor —
    /// arity and builtin name still resolve for every declared type.
    #[test]
    fn p20_type_items_survive() {
        assert_eq!(type_item("collections", "Array"), Some((1, "Array")));
        assert_eq!(type_item("collections", "Map"), Some((2, "Map")));
        assert_eq!(type_item("option", "Option"), Some((1, "Option")));
        assert_eq!(type_item("result", "Result"), Some((2, "Result")));
        assert_eq!(type_item("net", "TcpListener"), Some((0, "TcpListener")));
        assert_eq!(type_item("net", "TcpStream"), Some((0, "TcpStream")));
    }

    /// Perspective 21: the module list and its item counts are what the rest of
    /// the compiler expects. A module accidentally dropped while rewriting the
    /// table would otherwise show up only as a missing-import diagnostic.
    #[test]
    fn p21_module_surface_is_complete() {
        let names: Vec<&str> = STDLIB_SCHEMA.iter().map(|m| m.name).collect();
        assert_eq!(
            names,
            vec![
                "collections",
                "option",
                "result",
                "io",
                "env",
                "fs",
                "net",
                "parallel"
            ]
        );
        for (name, count) in [
            ("collections", 2),
            ("option", 1),
            ("result", 1),
            ("io", 3),
            ("env", 4),
            ("fs", 9),
            ("net", 10),
            ("parallel", 1),
        ] {
            assert_eq!(
                module(name).expect("module exists").items.len(),
                count,
                "std::{name} item count"
            );
        }
    }

    /// Perspective 22: lowering is pure — the same schema type always produces
    /// an equal AST type, and distinct shapes stay distinct.
    #[test]
    fn p22_lowering_is_pure_and_injective_on_shape() {
        let ty = StdType::Task(&STRING_IO);
        assert_eq!(ty.to_ast_type(), ty.to_ast_type());
        assert_ne!(ast(STRING_IO), ast(VOID_IO));
        assert_ne!(ast(STRING_IO), ast(StdType::Task(&STRING_IO)));
        assert_ne!(ast(LISTENER_IO), ast(STREAM_IO));
    }
}
