//! Declarative description of Willow's public standard-library namespace.
//!
//! Import validation, type-argument checking, and builtin module registration
//! all consume this table. Semantics that require bespoke lowering (notably
//! `Array`) remain in their respective compiler phases, but names and public
//! signatures are defined here exactly once.

/// A type used in a standard-library function signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdType {
    I64,
    Bool,
    String,
    StringArray,
    Void,
    /// `Result<String, IoError>` (std::fs, willow-2s3).
    StringIoResult,
    /// `Result<void, IoError>` (std::fs, willow-2s3).
    VoidIoResult,
    /// `Task<Result<String, IoError>>` from blocking-pool file I/O.
    TaskStringIoResult,
    /// `Task<Result<void, IoError>>` from blocking-pool file I/O.
    TaskVoidIoResult,
    /// `Task<bool>` from blocking-pool metadata I/O.
    TaskBool,
    /// The I/O functions accept every printable Willow value.
    Printable,
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
    ($name:literal, [$($param:ident),* $(,)?] -> $ret:ident) => {
        StdItemSchema {
            name: $name,
            kind: StdItemKind::Function {
                params: &[$(StdType::$param),*],
                return_type: StdType::$ret,
            },
        }
    };
}

const COLLECTIONS: &[StdItemSchema] = &[std_type!("Array", 1, "Array"), std_type!("Map", 2, "Map")];
const OPTION: &[StdItemSchema] = &[std_type!("Option", 1, "Option")];
const RESULT: &[StdItemSchema] = &[std_type!("Result", 2, "Result")];
const IO: &[StdItemSchema] = &[
    std_function!("println", [Printable] -> Void),
    std_function!("print", [Printable] -> Void),
    std_function!("eprintln", [Printable] -> Void),
];
// Compatibility calls remain synchronous. The `_async` forms isolate regular
// file operations on the bounded blocking pool and return scheduler Tasks.
const FS: &[StdItemSchema] = &[
    std_function!("temp_path", [String] -> String),
    std_function!("read_to_string", [String] -> StringIoResult),
    std_function!("write_string", [String, String] -> VoidIoResult),
    std_function!("exists", [String] -> Bool),
    std_function!("remove_file", [String] -> VoidIoResult),
    std_function!("read_to_string_async", [String] -> TaskStringIoResult),
    std_function!("write_string_async", [String, String] -> TaskVoidIoResult),
    std_function!("exists_async", [String] -> TaskBool),
    std_function!("remove_file_async", [String] -> TaskVoidIoResult),
];

const ENV: &[StdItemSchema] = &[
    std_function!("args", [] -> StringArray),
    std_function!("arg", [I64] -> String),
    std_function!("args_len", [] -> I64),
    std_function!("program_name", [] -> String),
];

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
        assert_eq!(type_item("io", "println"), None);
    }
}
