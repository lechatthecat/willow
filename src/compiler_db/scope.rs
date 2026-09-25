//! Compiler-owned, immutable per-unit name-resolution inputs.
use super::query::QueryTable;
use crate::{
    module::{UnitId, artifacts::ArtifactStore},
    semantic::{ids::TypeId, symbols::EnumInfo},
};
use anyhow::Result;
use std::{collections::HashSet, rc::Rc};

/// A single-item import (`import calc::add;`) as the resolver classified it for
/// one unit: the local name this file calls it by, the module it comes from,
/// and the item's own name there.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ItemBinding {
    pub local: String,
    pub module: String,
    pub item: String,
}

/// One module spelling this unit writes that the build's tables are NOT keyed
/// by (willow-kd1v).
///
/// A module is registered once, under the name the module graph gave it — the
/// FIRST importer's spelling — while every other unit is free to reach it by
/// its own `import` alias, or by its canonical name when it was another file
/// that aliased it. Both spellings then name the same module and only one is a
/// key, so the unit's own spelling is bound to the registered one for the
/// length of that unit's phase.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModuleSpelling {
    /// The prefix this unit writes (`import sales as biz;` -> `biz`).
    pub access: String,
    /// The prefix the build's tables are keyed by.
    pub graph_name: String,
    /// The module's canonical identity, which is what its ENUMS are keyed by
    /// (willow-itcw) even when its classes carry the graph name.
    pub canonical_path: String,
    /// The class/enum/interface names the module declares, i.e. the suffixes
    /// worth binding. Taken from the module's own program rather than scanned
    /// out of the tables, so nothing another module registered under a
    /// coincidentally matching prefix is aliased.
    pub types: Vec<String>,
}

/// What one source unit's own `import` lines bind, as the module resolver
/// classified them.
///
/// The back end cannot classify them itself: `import a::b;` is a module import
/// when `a::b` is a module file and an item import of `a` otherwise, and only
/// the resolver knows which files it loaded. Guessing from the path shape marks
/// the parent of an item import visible, which is a module this file cannot
/// name at all (willow-vtlr).
///
/// Recorded on the unit's [`UnitScope`] by its declaration phase and
/// reinstalled from there before its bodies: another unit's declaration phase
/// overwrites both halves.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UnitImports {
    /// Module access names this file can write. Both spellings of an aliased
    /// import are here, since the back end's tables are keyed by whichever
    /// spelling reached the module graph first.
    pub visible_modules: HashSet<String>,
    /// The single items this file bound, in import order.
    pub item_imports: Vec<ItemBinding>,
    /// The module prefixes this file writes that are not themselves table keys
    /// (willow-kd1v).
    pub module_spellings: Vec<ModuleSpelling>,
}

/// Source bindings retained independently of backend symbol declarations.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct UnitScope {
    pub imports: UnitImports,
    pub enum_aliases: Vec<(String, EnumInfo<TypeId>)>,
}

/// Only artifact references remain resident across units.
pub(crate) struct ScopeQueries {
    store: Rc<ArtifactStore>,
    scopes: QueryTable<UnitId, usize>,
}

impl ScopeQueries {
    pub(crate) fn new(store: Rc<ArtifactStore>) -> Self {
        Self {
            store,
            scopes: QueryTable::named("unit_scope"),
        }
    }

    pub(crate) fn evaluate(
        &self,
        unit: UnitId,
        compute: impl FnOnce() -> UnitScope,
    ) -> Result<UnitScope> {
        let mut fresh = None;
        let artifact = self.scopes.query(unit, || {
            let scope = compute();
            let artifact = self.store.write(&scope)?;
            fresh = Some(scope);
            Ok(artifact)
        })?;
        match fresh {
            Some(scope) => Ok(scope),
            None => self.store.read(*artifact),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        diagnostics::Span,
        module::{ModuleId, artifacts::UnitArtifacts},
        parser::ast::Type,
        semantic::symbols::EnumVariantInfo,
    };

    #[test]
    fn scopes_compute_once_and_isolate_aliases_between_units() {
        let artifacts = UnitArtifacts::new().unwrap();
        let queries = ScopeQueries::new(Rc::clone(&artifacts.store));
        let units = [(UnitId::ENTRY, "first"), (ModuleId(1), "second")];
        for (unit, module) in units {
            let mut scope = queries
                .evaluate(unit, || UnitScope {
                    imports: UnitImports {
                        visible_modules: HashSet::from([module.to_string()]),
                        item_imports: vec![ItemBinding {
                            local: "pick".into(),
                            module: module.into(),
                            item: "select".into(),
                        }],
                        module_spellings: vec![ModuleSpelling {
                            access: "palette".into(),
                            graph_name: module.into(),
                            canonical_path: format!("package::{module}"),
                            types: vec!["Color".into()],
                        }],
                    },
                    enum_aliases: vec![(
                        "Shade".into(),
                        EnumInfo {
                            name: TypeId::from_source_name(&format!("package::{module}::Color")),
                            public: true,
                            type_params: vec![],
                            variants: vec![EnumVariantInfo {
                                name: "Rgb".into(),
                                payload_types: vec![Type::I64],
                                tag: 7,
                                declaration_span: Span::dummy(),
                            }],
                            declaration_span: Span::dummy(),
                        },
                    )],
                })
                .unwrap();
            // Mutating the returned fresh value cannot alter its cached artifact.
            scope.imports.item_imports.clear();
            scope.enum_aliases.clear();
        }
        for _ in 0..4 {
            for (unit, module) in units {
                let scope = queries
                    .evaluate(unit, || panic!("cached unit recomputed"))
                    .unwrap();
                assert_eq!(
                    scope.imports.visible_modules,
                    HashSet::from([module.to_string()])
                );
                assert_eq!(scope.imports.item_imports.len(), 1);
                let item = &scope.imports.item_imports[0];
                assert_eq!(
                    (&*item.local, &*item.module, &*item.item),
                    ("pick", module, "select")
                );
                assert_eq!(scope.imports.module_spellings.len(), 1);
                let spelling = &scope.imports.module_spellings[0];
                assert_eq!(spelling.access, "palette");
                assert_eq!(spelling.graph_name, module);
                assert_eq!(spelling.canonical_path, format!("package::{module}"));
                assert_eq!(spelling.types, ["Color"]);
                assert_eq!(scope.enum_aliases.len(), 1);
                let (alias, info) = &scope.enum_aliases[0];
                assert_eq!(alias, "Shade");
                assert_eq!(
                    info.name,
                    TypeId::from_source_name(&format!("package::{module}::Color"))
                );
                assert_eq!(info.variants[0].name, "Rgb");
                assert_eq!(info.variants[0].tag, 7);
                assert_eq!(info.variants[0].payload_types, [Type::I64]);
            }
        }
        assert_eq!(queries.scopes.stats().computations, 2);
        assert_eq!(queries.scopes.stats().hits, 8);
        assert_eq!(queries.scopes.stats().calls, 10);
    }
}
