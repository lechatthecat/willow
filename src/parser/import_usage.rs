//! Import spellings used by generic interface defaults, collected while parsing.
//! These defaults may never be type checked. Keep lexical bindings here rather
//! than scheduling an extra body traversal just for unused-import diagnostics.
use super::ast::{ImportDecl, Pattern};
use std::collections::{HashMap, HashSet};

/// Bare callees/values and type/module heads have separate shadowing rules.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ReferenceKind {
    Value,
    Type,
}

#[derive(Default)]
pub(super) struct DefaultImportUsage {
    candidates: HashSet<String>,
    active: bool,
    implicit_collections: bool,
    used: Vec<String>,
    seen: HashSet<String>,
    scopes: Vec<Vec<String>>,
    bound: HashMap<String, usize>,
    type_parameters: HashSet<String>,
}

impl DefaultImportUsage {
    pub fn imports(&mut self, imports: &[ImportDecl]) {
        self.implicit_collections = imports.iter().any(|i| {
            i.path == "std::collections"
                && i.alias
                    .as_deref()
                    .is_none_or(|alias| alias == "collections")
        });
        self.candidates = imports
            .iter()
            .map(|i| {
                i.alias
                    .as_deref()
                    .unwrap_or_else(|| i.path.rsplit("::").next().unwrap())
                    .to_owned()
            })
            .collect();
    }

    pub fn begin(&mut self, generic: bool) {
        self.active = generic && !self.candidates.is_empty();
        // Do not retain a large earlier interface's hash-table capacity:
        // clearing that capacity for many later small defaults would multiply work.
        self.used = Vec::new();
        self.seen = HashSet::new();
        self.scopes = Vec::new();
        self.bound = HashMap::new();
        self.type_parameters = HashSet::new();
    }

    pub fn type_parameters(&mut self, names: &[String]) {
        if self.active {
            for name in names {
                if self.tracks_binding(name) {
                    self.type_parameters.insert(name.clone());
                }
            }
        }
    }

    pub fn finish(&mut self) -> Vec<String> {
        let used = std::mem::take(&mut self.used);
        self.begin(false);
        used
    }

    pub fn scope(&mut self) -> usize {
        let depth = self.scopes.len();
        if self.active {
            self.scopes.push(Vec::new());
        }
        depth
    }

    pub fn restore(&mut self, depth: usize) {
        while self.scopes.len() > depth {
            for name in self.scopes.pop().unwrap() {
                let count = self.bound.get_mut(&name).unwrap();
                *count -= 1;
                if *count == 0 {
                    self.bound.remove(&name);
                }
            }
        }
    }

    fn tracks_binding(&self, name: &str) -> bool {
        self.candidates.contains(name)
            || (self.implicit_collections && matches!(name, "Array" | "Map"))
    }

    pub fn bind(&mut self, name: &str) {
        if self.active && self.tracks_binding(name) {
            *self.bound.entry(name.to_owned()).or_default() += 1;
            self.scopes
                .last_mut()
                .expect("binding scope")
                .push(name.to_owned());
        }
    }

    pub fn bind_pattern(&mut self, pattern: &Pattern) {
        if !self.active {
            return;
        }
        match pattern {
            Pattern::Binding { name, .. } => self.bind(name),
            Pattern::ClassDowncast { binding, .. } => self.bind(binding),
            Pattern::EnumVariantTuple { bindings, .. } => {
                for name in bindings {
                    self.bind(name);
                }
            }
            _ => {}
        }
    }

    pub fn reference(&mut self, name: &str, kind: ReferenceKind) {
        if !self.active {
            return;
        }
        let head = name.split("::").next().unwrap();
        let local = if self.candidates.contains(head) {
            head
        } else if self.implicit_collections && matches!(head, "Array" | "Map") {
            "collections"
        } else {
            return;
        };
        let visible = match kind {
            // Qualified callees are looked up by their complete value name;
            // a local binding of their module head does not shadow that name.
            ReferenceKind::Value => !self.bound.contains_key(name),
            ReferenceKind::Type => !self.type_parameters.contains(head),
        };
        if visible && !self.seen.contains(local) {
            self.seen.insert(local.to_owned());
            self.used.push(local.to_owned());
        }
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static DEFAULT_BODY_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use crate::{
        lexer::Lexer,
        parser::{Parser, ast::Item},
    };

    #[test]
    fn iflh_default_import_lexical_scopes() {
        // These are parser/name-scope tests; bodies need not be valid concrete
        // instantiations. No type-check traversal is used to obtain their facts.
        for (params, body, used) in [
            ("", "read();", true),
            ("", "let f = read;", true),
            ("", "let s = \"read()\"; /* read() */", false),
            (", read: i64", "println(read);", false),
            ("", "let read = 1; println(read);", false),
            ("", "let read = read();", true),
            ("", "let f = |read: i64| read;", false),
            ("", "let f = |read: i64| { println(read); };", false),
            ("", "let f = |read: i64| read; read();", true),
            ("", "let f = || read();", true),
            ("", "if true { let read = 1; println(read); }", false),
            ("", "if true { let read = 1; } read();", true),
            ("", "for read in 0..2 { println(read); }", false),
            ("", "for read in read() { println(read); }", true),
            ("", "for read in 0..2 {} read();", true),
            ("", "if let Some(read) = value { println(read); }", false),
            ("", "if let Some(read) = read() { println(read); }", true),
            ("", "if let Some(read) = value {} else { read(); }", true),
            ("", "while let Some(read) = value { println(read); }", false),
            ("", "while let Some(read) = read() { println(read); }", true),
            (
                "",
                "match value { Some(read) if read > 0 => read, _ => 0 };",
                false,
            ),
            ("", "match value { Some(read) => read, _ => read() };", true),
            ("", "let (read, other) = value; println(read);", false),
            ("", "let (read, other) = read(); println(read);", true),
            ("", "lock value as read { println(read); }", false),
            ("", "lock read as read { println(read); }", true),
            (
                "",
                "select { let read = ch.recv() => { println(read); } }",
                false,
            ),
            (
                "",
                "select { let read = ch.recv() => {} default => { read(); } }",
                true,
            ),
            (
                "",
                "select { let read = await task => { println(read); } }",
                false,
            ),
            ("", "defer { read(); }", true),
            (
                "",
                "match value { read::Kind::Some(x) => x, _ => 0 };",
                true,
            ),
            ("", "match value { read(x) => x, _ => 0 };", true),
            ("", "let x = new read();", true),
            ("", "let x = read<i64> { field: 1 };", true),
            ("", "let x = obj.read();", false),
            ("", "let x = Obj { read: 1 };", false),
            ("", "let read = 1; read::function();", true),
            ("", "let read = 1; read<i64>::function();", true),
        ] {
            let source = format!(
                "import helpers as read; interface I<T> {{ fn f(self{params}) {{ {body} }} }}"
            );
            let (program, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            assert!(errors.is_empty(), "{source}: {errors:?}");
            let Item::Interface(interface) = &program.items[0] else {
                unreachable!()
            };
            assert_eq!(
                interface.default_import_uses,
                if used { vec!["read"] } else { vec![] },
                "{source}"
            );
            // The summary must survive the module artifact's serialization.
            let wire = serde_json::to_vec(&program).unwrap();
            let restored: crate::parser::ast::Program = serde_json::from_slice(&wire).unwrap();
            let Item::Interface(restored) = &restored.items[0] else {
                unreachable!()
            };
            assert_eq!(restored.default_import_uses, interface.default_import_uses);
        }
    }

    #[test]
    fn iflh_default_import_scopes_do_not_leak_between_methods_or_interfaces() {
        let source = "import helpers as read; interface A<T> { fn a(self, read: i64) { println(read); } fn b(self) { read(); } } interface B<T> { fn a(self) { let read = 1; println(read); } } interface C { fn a(self) { read(); } }";
        let (program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let facts: Vec<_> = program
            .items
            .iter()
            .map(|item| {
                let Item::Interface(interface) = item else {
                    unreachable!()
                };
                interface.default_import_uses.clone()
            })
            .collect();
        assert_eq!(facts, vec![vec!["read".to_string()], vec![], vec![]]);
        let source = "import helpers as T; interface I<T> { fn f(self) { T::build(); } }";
        let (program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let Item::Interface(interface) = &program.items[0] else {
            unreachable!()
        };
        assert!(interface.default_import_uses.is_empty());
    }
}

#[cfg(test)]
mod namespace_tests {
    use crate::{
        lexer::Lexer,
        parser::{Parser, ast::Item},
    };

    #[test]
    fn iflh_reference_namespaces_keep_type_and_value_shadowing_separate() {
        for (body, used) in [
            ("T();", true),
            ("T<i64>();", true),
            ("T::generic<i64>();", true),
            ("let T = 1; T::generic<i64>();", true),
            ("let f = T;", true),
            ("let T = || 1; T();", false),
            ("let f = |T: i64| T;", false),
            ("T::new();", false),
            ("T<i64>::new();", false),
            ("let x = new T();", false),
            ("let x = T { x: 1 };", false),
            ("let x = T<i64> { x: 1 };", false),
            ("match x { T(value) => value, _ => 0 };", false),
            ("match x { T::Value(value) => value, _ => 0 };", false),
            ("if true { let T = || 1; T(); } T();", true),
            ("let T = T();", true),
        ] {
            let source =
                format!("import other::seven as T; interface I<T> {{ fn f(self) {{ {body} }} }}");
            let (program, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            assert!(errors.is_empty(), "{source}: {errors:?}");
            let Item::Interface(interface) = &program.items[0] else {
                unreachable!()
            };
            assert_eq!(
                interface.default_import_uses,
                if used { vec!["T"] } else { vec![] },
                "{source}"
            );
        }
    }
}
