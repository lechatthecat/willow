//! Immutable, session-local compiler query results. Executable side tables are
//! stored in the existing unit artifact store rather than retained per module.
pub(crate) mod analysis;
pub(crate) mod body;
mod checked;
pub(crate) mod declarations;
pub(crate) mod dependencies;
pub(crate) mod effects;
pub mod ids;
pub mod inputs;
pub(crate) mod layout;
pub(crate) mod lir;
mod lir_artifact;
pub(crate) mod normalize;
pub mod query;
pub mod revision;
pub mod scope;
pub use checked::CheckedUnit;
pub type HelperSummary = std::collections::HashMap<
    crate::semantic::ids::FunctionId,
    crate::semantic::concurrency::NonpreemptibleHelper,
>;

use crate::module::UnitId;
use crate::module::artifacts::{LiveUnit, UnitArtifacts, UnitKind};
use anyhow::Result;
use query::QueryTable;
use std::sync::Arc;

struct CheckedUnitRecord {
    artifact: usize,
    bodies: Vec<crate::parser::ast::BodyId>,
    diagnostics: Arc<[crate::diagnostics::Diagnostic]>,
}

pub struct CompilerDb {
    inputs: inputs::CompilerInputs,
    scopes: scope::ScopeQueries,
    pub(crate) declarations: std::rc::Rc<declarations::DeclarationQueries>,
    pub(crate) effects: std::rc::Rc<effects::EffectQueries>,
    pub(crate) layouts: std::rc::Rc<layout::LayoutQueries>,
    pub(crate) typed_bodies: std::rc::Rc<body::BodyQueries>,
    pub(crate) lir: std::rc::Rc<lir::LirQueries>,
    bodies: std::rc::Rc<ids::BodyIndex>,
    dependencies: std::rc::Rc<dependencies::ModuleDependencies>,
    checked: QueryTable<UnitId, CheckedUnitRecord>,
    pub(crate) revision_work: std::cell::Cell<(usize, usize)>,
}

impl CompilerDb {
    pub(crate) fn with_dependencies(
        inputs: inputs::CompilerInputs,
        modules: &[crate::module::ResolvedModule],
        bodies: std::rc::Rc<ids::BodyIndex>,
        store: std::rc::Rc<crate::module::artifacts::ArtifactStore>,
        dependencies: dependencies::ModuleDependencies,
    ) -> Self {
        let dependencies = std::rc::Rc::new(dependencies);
        Self {
            scopes: scope::ScopeQueries::new(std::rc::Rc::clone(&store)),
            declarations: std::rc::Rc::new(declarations::DeclarationQueries::new(
                std::rc::Rc::clone(&store),
            )),
            lir: std::rc::Rc::new(lir::LirQueries::new(std::rc::Rc::clone(&store))),
            typed_bodies: std::rc::Rc::new(body::BodyQueries::new(
                store,
                std::rc::Rc::clone(&bodies),
            )),
            inputs,
            layouts: Default::default(),
            effects: std::rc::Rc::new(effects::EffectQueries::new(
                modules,
                std::rc::Rc::clone(&dependencies),
            )),
            bodies,
            dependencies,
            checked: QueryTable::named("checked_unit"),
            revision_work: Default::default(),
        }
    }
    pub(crate) fn unit_scope(
        &self,
        unit: UnitId,
        program: &crate::parser::ast::Program,
        modules: &[crate::module::ResolvedModule],
        symbols: &crate::semantic::symbols::SymbolTable,
    ) -> Result<scope::UnitScope> {
        self.scopes.evaluate(unit, || scope::UnitScope {
            imports: crate::backend_unit_imports(program, modules, self.dependencies()),
            enum_aliases: symbols
                .enums
                .iter()
                .filter(|(name, info)| {
                    name.to_string() != info.name
                        && !symbols.classes.contains_key(*name)
                        && !symbols.interfaces.contains_key(*name)
                })
                .map(|(name, info)| (name.to_string(), info.to_semantic()))
                .collect(),
        })
    }

    pub(crate) fn dependencies(&self) -> &dependencies::ModuleDependencies {
        &self.dependencies
    }

    pub fn package_graph(&self) -> Option<&crate::package::PackageGraph> {
        self.inputs.package_graph.as_deref()
    }

    pub fn bodies(&self) -> &ids::BodyIndex {
        &self.bodies
    }

    pub fn inputs(&self) -> &inputs::CompilerInputs {
        &self.inputs
    }

    pub(crate) fn nonpreemptible_helpers(
        &self,
        unit: UnitId,
        program: &crate::parser::ast::Program,
    ) -> Result<std::sync::Arc<HelperSummary>> {
        self.effects.nonpreemptible_helpers(unit, program)
    }

    pub(crate) fn check_unit(
        &self,
        unit: UnitId,
        artifacts: &mut UnitArtifacts,
        compute: impl FnOnce(&UnitArtifacts) -> Result<CheckedUnit>,
    ) -> Result<()> {
        self.checked.query(unit, || {
            let checked = compute(artifacts)?;
            let CheckedUnit {
                bodies,
                diagnostics,
                normalized_types,
                ..
            } = checked;
            let artifact = artifacts.write(&checked::CheckedTypes { normalized_types })?;
            Ok(CheckedUnitRecord {
                artifact,
                bodies,
                diagnostics: diagnostics.into(),
            })
        })?;
        Ok(())
    }

    pub(crate) fn unit_declarations(
        &self,
        unit: UnitId,
        artifacts: &UnitArtifacts,
    ) -> Result<checked::CheckedDeclarations> {
        let types: checked::CheckedTypes = artifacts.read(self.checked_record(unit)?.artifact)?;
        Ok(checked::CheckedDeclarations {
            symbols: self.declarations.visible_scope(unit)?,
            normalized_types: types.normalized_types,
        })
    }

    pub(crate) fn checked_unit(
        &self,
        unit: UnitId,
        artifacts: &UnitArtifacts,
    ) -> Result<LiveUnit<CheckedUnit>> {
        let record = self.checked_record(unit)?;
        let declarations = self.unit_declarations(unit, artifacts)?;
        let mut unit = declarations.into_unit();
        for &body in &record.bodies {
            self.typed_bodies
                .read(body)?
                .merge_tables(&mut unit, &self.typed_bodies)?;
        }
        unit.bodies.clone_from(&record.bodies);
        // Driver diagnostics use the resident query value; backend consumers do
        // not need to allocate or deserialize them again.
        Ok(artifacts.track(UnitKind::Checker, unit))
    }

    pub(crate) fn unit_diagnostics(
        &self,
        unit: UnitId,
    ) -> Result<Arc<[crate::diagnostics::Diagnostic]>> {
        Ok(Arc::clone(&self.checked_record(unit)?.diagnostics))
    }

    fn checked_record(&self, unit: UnitId) -> Result<Arc<CheckedUnitRecord>> {
        self.checked.query(unit, || {
            anyhow::bail!("checked unit requested before the checking phase: {unit:?}")
        })
    }
}

/// JSON object keys cannot represent structured compiler identities. Serialize
/// maps as entry sequences, preserving identity without string reparsing.
pub(crate) mod map_entries {
    use serde::{
        Deserialize, Deserializer, Serialize, Serializer,
        de::{SeqAccess, Visitor},
    };
    use std::{collections::HashMap, hash::Hash};

    pub fn serialize<K: Serialize, V: Serialize, S: Serializer>(
        map: &HashMap<K, V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(map.iter())
    }

    pub fn deserialize<
        'de,
        K: Deserialize<'de> + Eq + Hash,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    >(
        deserializer: D,
    ) -> Result<HashMap<K, V>, D::Error> {
        struct Entries<K, V>(std::marker::PhantomData<(K, V)>);
        impl<'de, K: Deserialize<'de> + Eq + Hash, V: Deserialize<'de>> Visitor<'de> for Entries<K, V> {
            type Value = HashMap<K, V>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("compiler map entries")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut entries: A) -> Result<Self::Value, A::Error> {
                let mut map = HashMap::with_capacity(entries.size_hint().unwrap_or(0));
                while let Some((key, value)) = entries.next_element()? {
                    map.insert(key, value);
                }
                Ok(map)
            }
        }
        deserializer.deserialize_seq(Entries(std::marker::PhantomData))
    }
}
