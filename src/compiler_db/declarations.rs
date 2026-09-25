//! Frozen unit-visible declarations shared by body checking and later phases.
//! Executable data and evaluator state are never retained in this query table.
use super::query::QueryTable;
use crate::{
    module::{ModuleId, UnitId, artifacts::ArtifactStore},
    semantic::{
        ids::{FunctionId, TypeId},
        symbols::{ClassInfo, DeclarationSymbols, EnumInfo, FuncInfo, InterfaceInfo, SymbolTable},
    },
};
use anyhow::{Context, Result};
use std::{cell::OnceCell, collections::HashMap, hash::Hash, rc::Rc};

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) enum TypeDeclaration {
    Class(ClassInfo),
    Enum(EnumInfo),
    Interface(InterfaceInfo),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TypeNamespace {
    Class,
    Enum,
    Interface,
}

#[derive(serde::Serialize)]
enum TypeDeclarationRef<'a> {
    Class(&'a ClassInfo),
    Enum(&'a EnumInfo),
    Interface(&'a InterfaceInfo),
}

/// Entries equal to the session's prelude declarations are recorded by ID
/// only; `visible_scope` restores them from the resident prelude copy.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct ModuleDeclarations {
    scope: SymbolTable,
    functions: Vec<FunctionId>,
    classes: Vec<TypeId>,
    enums: Vec<TypeId>,
    interfaces: Vec<TypeId>,
    prelude_functions: Vec<FunctionId>,
    prelude_classes: Vec<TypeId>,
    prelude_enums: Vec<TypeId>,
    prelude_interfaces: Vec<TypeId>,
    prelude_modules: Vec<ModuleId>,
}

/// A frozen declaration: spooled to the unit pack, or equal to the prelude's.
#[derive(Clone, Copy)]
enum Stored {
    Artifact(usize),
    Prelude,
}

pub(crate) struct DeclarationQueries {
    store: Rc<ArtifactStore>,
    /// Declarations every unit starts from (built-in modules and the prelude).
    /// Its size is fixed by the compiler, not by the program being built.
    prelude: OnceCell<DeclarationSymbols>,
    scopes: QueryTable<UnitId, usize>,
    modules: QueryTable<UnitId, usize>,
    functions: QueryTable<(UnitId, FunctionId), Stored>,
    // A spelling can occur in separate namespaces (e.g. an enum alias and a
    // local class). Keep that distinction until name resolution chooses one.
    types: QueryTable<(UnitId, TypeId, TypeNamespace), Stored>,
}

/// Split one declaration map into spooled IDs and IDs equal to the prelude's.
fn partition<K: Copy + Eq + Hash, V: PartialEq>(
    entries: &HashMap<K, V>,
    prelude: Option<&HashMap<K, V>>,
) -> (Vec<K>, Vec<K>) {
    let (mut own, mut shared) = (Vec::new(), Vec::new());
    for (&id, value) in entries {
        if prelude.and_then(|prelude| prelude.get(&id)) == Some(value) {
            shared.push(id);
        } else {
            own.push(id);
        }
    }
    (own, shared)
}

impl DeclarationQueries {
    pub(crate) fn new(store: Rc<ArtifactStore>) -> Self {
        Self {
            store,
            prelude: OnceCell::new(),
            scopes: QueryTable::named("visible_scope"),
            modules: QueryTable::named("module_declarations"),
            functions: QueryTable::named("function_signature"),
            types: QueryTable::named("type_decl"),
        }
    }

    /// Record the declarations a checker holds right after prelude
    /// registration; the first call wins. It is only a sharing dictionary:
    /// entries are shared when equal, so a differing unit stays exact. Without
    /// one, every declaration is spooled.
    pub(crate) fn set_prelude(&self, prelude: &SymbolTable) {
        if self.prelude.get().is_none() {
            // Detach module spelling registries: the checker keeps declaring
            // into its own, and the shared copy must not drift after a freeze.
            let mut copy = (**prelude).clone();
            for info in copy.modules.values_mut() {
                *info = info.detached_clone();
            }
            let _ = self.prelude.set(copy);
        }
    }

    /// Declaration collection is the only writer. Payloads are spooled once
    /// per declaration; requesting one signature never deserializes its unit.
    /// Unit identity remains part of the key because imports have local aliases.
    pub(crate) fn freeze(&self, unit: UnitId, symbols: &SymbolTable) -> Result<SymbolTable> {
        let mut fresh = false;
        self.scopes.query(unit, || {
            fresh = true;
            self.modules
                .query(unit, || {
                    let prelude = self.prelude.get();
                    let (functions, prelude_functions) =
                        partition(&symbols.functions, prelude.map(|p| &p.functions));
                    let (classes, prelude_classes) =
                        partition(&symbols.classes, prelude.map(|p| &p.classes));
                    let (enums, prelude_enums) =
                        partition(&symbols.enums, prelude.map(|p| &p.enums));
                    let (interfaces, prelude_interfaces) =
                        partition(&symbols.interfaces, prelude.map(|p| &p.interfaces));
                    let (_, prelude_modules) =
                        partition(&symbols.modules, prelude.map(|p| &p.modules));
                    for &id in &functions {
                        self.functions.query((unit, id), || {
                            Ok(Stored::Artifact(self.store.write(&symbols.functions[&id])?))
                        })?;
                    }
                    for &id in &prelude_functions {
                        self.functions.query((unit, id), || Ok(Stored::Prelude))?;
                    }
                    for &id in &classes {
                        self.types.query((unit, id, TypeNamespace::Class), || {
                            let info = &symbols.classes[&id];
                            Ok(Stored::Artifact(
                                self.store.write(&TypeDeclarationRef::Class(info))?,
                            ))
                        })?;
                    }
                    for &id in &enums {
                        self.types.query((unit, id, TypeNamespace::Enum), || {
                            let info = &symbols.enums[&id];
                            Ok(Stored::Artifact(
                                self.store.write(&TypeDeclarationRef::Enum(info))?,
                            ))
                        })?;
                    }
                    for &id in &interfaces {
                        self.types.query((unit, id, TypeNamespace::Interface), || {
                            let info = &symbols.interfaces[&id];
                            Ok(Stored::Artifact(
                                self.store.write(&TypeDeclarationRef::Interface(info))?,
                            ))
                        })?;
                    }
                    for (ids, namespace) in [
                        (&prelude_classes, TypeNamespace::Class),
                        (&prelude_enums, TypeNamespace::Enum),
                        (&prelude_interfaces, TypeNamespace::Interface),
                    ] {
                        for &id in ids {
                            self.types
                                .query((unit, id, namespace), || Ok(Stored::Prelude))?;
                        }
                    }
                    let record = ModuleDeclarations {
                        scope: symbols.declaration_shell(&prelude_modules),
                        functions,
                        classes,
                        enums,
                        interfaces,
                        prelude_functions,
                        prelude_classes,
                        prelude_enums,
                        prelude_interfaces,
                        prelude_modules,
                    };
                    self.store.write(&record)
                })
                .map(|record| *record)
        })?;
        if fresh {
            Ok(symbols.fork_body_scope())
        } else {
            self.visible_scope(unit)
        }
    }

    pub(crate) fn module_declarations(&self, unit: UnitId) -> Result<ModuleDeclarations> {
        let artifact = self.modules.query(unit, || {
            anyhow::bail!("module declarations requested before declaration: {unit:?}")
        })?;
        self.store.read(*artifact)
    }

    fn prelude(&self) -> Result<&DeclarationSymbols> {
        self.prelude
            .get()
            .context("prelude declaration requested without a session prelude")
    }

    pub(crate) fn function_signature(&self, unit: UnitId, id: FunctionId) -> Result<FuncInfo> {
        let stored = self.functions.query((unit, id), || {
            anyhow::bail!("function signature requested before declaration: {unit:?} {id:?}")
        })?;
        match *stored {
            Stored::Artifact(artifact) => self.store.read(artifact),
            Stored::Prelude => Ok(self.prelude()?.functions[&id].clone()),
        }
    }

    pub(crate) fn type_decl(
        &self,
        unit: UnitId,
        id: TypeId,
        namespace: TypeNamespace,
    ) -> Result<TypeDeclaration> {
        let stored = self.types.query((unit, id, namespace), || {
            anyhow::bail!("type requested before declaration: {unit:?} {id:?}")
        })?;
        let artifact = match *stored {
            Stored::Artifact(artifact) => artifact,
            Stored::Prelude => {
                let prelude = self.prelude()?;
                return Ok(match namespace {
                    TypeNamespace::Class => TypeDeclaration::Class(prelude.classes[&id].clone()),
                    TypeNamespace::Enum => TypeDeclaration::Enum(prelude.enums[&id].clone()),
                    TypeNamespace::Interface => {
                        TypeDeclaration::Interface(prelude.interfaces[&id].clone())
                    }
                });
            }
        };
        self.store.read(artifact)
    }

    pub(crate) fn visible_scope(&self, unit: UnitId) -> Result<SymbolTable> {
        let record = self.module_declarations(unit)?;
        let mut scope = record.scope;
        for id in record.functions {
            scope
                .functions
                .insert(id, self.function_signature(unit, id)?);
        }
        for id in record.classes {
            let TypeDeclaration::Class(info) = self.type_decl(unit, id, TypeNamespace::Class)?
            else {
                anyhow::bail!("class declaration kind mismatch")
            };
            scope.classes.insert(id, info);
        }
        for id in record.enums {
            let TypeDeclaration::Enum(info) = self.type_decl(unit, id, TypeNamespace::Enum)? else {
                anyhow::bail!("enum declaration kind mismatch")
            };
            scope.enums.insert(id, info);
        }
        for id in record.interfaces {
            let TypeDeclaration::Interface(info) =
                self.type_decl(unit, id, TypeNamespace::Interface)?
            else {
                anyhow::bail!("interface declaration kind mismatch")
            };
            scope.interfaces.insert(id, info);
        }
        let shared = record.prelude_functions.len()
            + record.prelude_classes.len()
            + record.prelude_enums.len()
            + record.prelude_interfaces.len()
            + record.prelude_modules.len();
        if shared > 0 {
            let prelude = self.prelude()?;
            let declarations = &mut *scope;
            for id in record.prelude_functions {
                declarations
                    .functions
                    .insert(id, prelude.functions[&id].clone());
            }
            for id in record.prelude_classes {
                declarations
                    .classes
                    .insert(id, prelude.classes[&id].clone());
            }
            for id in record.prelude_enums {
                declarations.enums.insert(id, prelude.enums[&id].clone());
            }
            for id in record.prelude_interfaces {
                declarations
                    .interfaces
                    .insert(id, prelude.interfaces[&id].clone());
            }
            for id in record.prelude_modules {
                declarations
                    .modules
                    .insert(id, prelude.modules[&id].detached_clone());
            }
        }
        Ok(scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{module::artifacts::UnitArtifacts, semantic::TypeChecker};

    #[test]
    fn individual_declarations_are_order_independent_without_unit_rehydration() {
        for count in [16, 64, 256] {
            let artifacts = UnitArtifacts::new().unwrap();
            let queries = DeclarationQueries::new(Rc::clone(&artifacts.store));
            let source: String = (0..count).map(|i| format!(
                "fn decl_fn_{i}(x: i64) -> i64 {{ return x; }} class C{i} {{ value: i64; }}\n"
            )).collect();
            let (program, errors) =
                crate::parser::Parser::new(crate::lexer::Lexer::new(&source).tokenize().unwrap())
                    .parse();
            assert!(errors.is_empty());
            let mut checker = TypeChecker::new();
            checker.check_program(&program);
            assert!(checker.errors.is_empty(), "{:?}", checker.errors);
            queries.freeze(UnitId::ENTRY, &checker.symbols).unwrap();
            let module_calls = queries.modules.stats().calls;
            let functions = queries.functions.stats().computations;
            let types = queries.types.stats().computations;
            for i in (0..count).rev() {
                for _ in 0..4 {
                    let signature = queries
                        .function_signature(UnitId::ENTRY, FunctionId::free(format!("decl_fn_{i}")))
                        .unwrap();
                    assert_eq!(signature.return_type, crate::parser::ast::Type::I64);
                    let info = queries
                        .type_decl(
                            UnitId::ENTRY,
                            TypeId::local(format!("C{i}")),
                            TypeNamespace::Class,
                        )
                        .unwrap();
                    let TypeDeclaration::Class(info) = info else {
                        panic!()
                    };
                    assert_eq!(info.instance_field_order.len(), 1);
                }
            }
            assert_eq!(queries.modules.stats().calls, module_calls);
            assert_eq!(queries.functions.stats().computations, functions);
            assert_eq!(queries.types.stats().computations, types);
            assert_eq!(queries.functions.stats().hits, 4 * count);
            assert_eq!(queries.types.stats().hits, 4 * count);
            let restored = queries.visible_scope(UnitId::ENTRY).unwrap();
            for (id, expected) in &checker.symbols.functions {
                assert_eq!(
                    serde_json::to_value(&restored.functions[id]).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
            }
        }
    }

    #[test]
    fn frozen_scope_reuses_signatures_and_keeps_units_separate() {
        let artifacts = UnitArtifacts::new().unwrap();
        let queries = Rc::new(DeclarationQueries::new(Rc::clone(&artifacts.store)));
        for (unit, ty) in [(UnitId::ENTRY, "i64"), (crate::module::ModuleId(1), "bool")] {
            let source = format!("fn value(x: {ty}) -> {ty} {{ return x; }}");
            let (program, errors) =
                crate::parser::Parser::new(crate::lexer::Lexer::new(&source).tokenize().unwrap())
                    .parse();
            assert!(errors.is_empty());
            let mut checker = TypeChecker::new();
            crate::register_prelude(&mut checker).unwrap();
            checker.set_declaration_queries(Rc::clone(&queries), unit);
            checker.check_module_program(&program);
            checker.finish_body_queries().unwrap();
            assert!(checker.errors.is_empty());
            eprintln!(
                "visible-scope serialized_bytes={}",
                serde_json::to_vec(&checker.symbols).unwrap().len()
            );
            let expected =
                serde_json::to_value(checker.symbols.lookup_func("value").unwrap()).unwrap();
            // A cache hit ignores a different caller's mutable scratch symbols.
            let empty = SymbolTable::default();
            for _ in 0..4 {
                let scope = queries.freeze(unit, &empty).unwrap();
                assert_eq!(
                    serde_json::to_value(scope.lookup_func("value").unwrap()).unwrap(),
                    expected
                );
                assert!(scope.lookup_var("x").is_none());
            }
        }
        assert_eq!(queries.scopes.stats().computations, 2);
        assert_eq!(queries.scopes.stats().hits, 8);
    }
}
