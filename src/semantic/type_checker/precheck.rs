//! Pre-expansion semantic results. Generic state changes are sparse replay logs.
use super::precheck_journal::{self as journal, Key};
use super::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

/// Registration and signature validation depend on the interface header. Default
/// bodies have their own precheck records.
fn interface_header_syntax(decl: &InterfaceDecl) -> Value {
    let InterfaceDecl {
        default_import_uses,
        name,
        public,
        type_params,
        extends,
        methods,
        instance_of,
        template_checked,
        span,
    } = decl;
    let mut header = json!({
        "name": name,
        "public": public,
        "type_params": type_params,
        "extends": extends,
        "instance_of": instance_of,
        "span": span,
    });
    if !default_import_uses.is_empty() {
        header["default_import_uses"] = json!(default_import_uses);
    }
    if *template_checked {
        header["template_checked"] = Value::Bool(true);
    }
    header["methods"] = Value::Array(
        methods
            .iter()
            .map(|method| {
                let InterfaceMethodDecl {
                    effect_contracts,
                    name,
                    params,
                    is_static,
                    return_type,
                    default_body: _,
                    span,
                } = method;
                let mut signature = json!({
                    "name": name,
                    "params": params,
                    "is_static": is_static,
                    "return_type": return_type,
                    "default_body": null,
                    "span": span,
                });
                if !effect_contracts.is_empty() {
                    signature["effect_contracts"] = json!(effect_contracts);
                }
                signature
            })
            .collect(),
    );
    header
}

/// Registration keeps the same wire form as a signature-only class without
/// constructing a second declaration or walking executable syntax.
fn class_registration_syntax(class: &ClassDecl, isolation: &ClassIsolation) -> Value {
    let ClassDecl {
        abi_storage,
        type_params,
        name,
        public,
        is_open,
        base_class,
        implements,
        source_implements_len,
        fields,
        methods,
        constructors,
        span,
    } = class;
    let mut header = json!({
        "abi_storage": abi_storage,
        "type_params": type_params,
        "name": name,
        "public": public,
        "is_open": is_open,
        "base_class": base_class,
        "implements": implements,
        "source_implements_len": source_implements_len,
        "span": span,
    });
    let empty_block = |body: &Block| Block {
        id: body.id,
        span: body.span,
        stmts: Vec::new(),
    };
    header["fields"] = Value::Array(
        fields
            .iter()
            .map(|f| {
                let FieldDecl {
                    name,
                    ty,
                    public,
                    protected,
                    is_static,
                    is_mut,
                    span,
                    initializer: _,
                } = f;
                json!({
                    "name": name,
                    "ty": ty,
                    "public": public,
                    "protected": protected,
                    "is_static": is_static,
                    "is_mut": is_mut,
                    "initializer": null,
                    "span": span,
                })
            })
            .collect(),
    );
    header["constructors"] = Value::Array(
        constructors
            .iter()
            .map(|c| {
                let ConstructorDecl {
                    public,
                    protected,
                    params,
                    body,
                    span,
                } = c;
                json!({
                    "public": public,
                    "protected": protected,
                    "params": params,
                    "body": empty_block(body),
                    "span": span,
                })
            })
            .collect(),
    );
    header["methods"] = Value::Array(
        methods
            .iter()
            .map(|m| {
                let MethodDecl {
                    effect_contracts,
                    array_body_optimization,
                    constraint,
                    name,
                    public,
                    protected,
                    is_async,
                    is_open,
                    is_override,
                    is_static,
                    params,
                    return_type,
                    body,
                    span,
                    is_default_injected,
                    is_interface_default,
                } = m;
                let mut method = json!({
                    "array_body_optimization": array_body_optimization,
                    "constraint": constraint,
                    "name": name,
                    "public": public,
                    "protected": protected,
                    "is_async": is_async,
                    "is_open": is_open,
                    "is_override": is_override,
                    "is_static": is_static,
                    "params": params,
                    "return_type": return_type,
                    "body": empty_block(body),
                    "span": span,
                    "is_default_injected": is_default_injected,
                    "is_interface_default": is_interface_default,
                });
                if !effect_contracts.is_empty() {
                    method["effect_contracts"] = json!(effect_contracts);
                }
                method
            })
            .collect(),
    );
    Value::Array(vec![header, json!(isolation)])
}

fn class_header_syntax(class: &ClassDecl, constructors: &[Rc<SuperInitPlacement>]) -> Value {
    let methods: Vec<_> = class
        .methods
        .iter()
        .map(|m| {
            let signature =
                (class.base_class.is_some() && m.is_override && !m.is_static).then(|| {
                    let params: Vec<_> = m.params.iter().map(|p| (&p.ty, p.type_span)).collect();
                    (params, &m.return_type, m.is_async)
                });
            (&m.name, m.is_static, m.is_override, signature, m.span)
        })
        .collect();
    let fields: Vec<_> = class
        .fields
        .iter()
        .map(|f| (f.is_static.then_some(&f.name), &f.ty, f.span))
        .collect();
    let constructors: Vec<_> = constructors
        .iter()
        .zip(&class.constructors)
        .map(|(placement, ctor)| (placement, ctor.span))
        .collect();
    json!({
        "name": class.name, "span": class.span, "abi_storage": class.abi_storage,
        "type_params": class.type_params, "base_class": class.base_class,
        "implements": class.implements, "source_implements_len": class.source_implements_len,
        "fields": fields, "methods": methods, "constructors": constructors,
    })
}

fn foreign_defaults_syntax(class: &ClassDecl, copies: &[(&str, Span)]) -> Value {
    // The provider compares the complete body span for equality. Encode it as
    // an identity, since the cache normally relocates diagnostic spans.
    let origins: Vec<_> = copies
        .iter()
        .map(|(name, span)| {
            (
                name,
                (span.file_id, span.start, span.end, span.line, span.col),
            )
        })
        .collect();
    json!({"implements": class.implements, "span": class.span, "copies": origins})
}

#[derive(Serialize)]
pub(super) struct ClassIsolation {
    pub(super) constructor: Option<bool>,
    pub(super) methods: Vec<bool>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Record {
    syntax: Value,
    input_spans: Vec<Span>,
    context: Value,
    reads: Vec<(Key, Value)>,
    dispatch: Vec<journal::DispatchEvent>,
    writes: Vec<(Key, Value)>,
    body: Value,
    result: Value,
    pending: Vec<generics::Instantiation>,
    emitted: Vec<Item>,
    source_symbols: crate::semantic::analysis_symbols::Facts,
}
#[derive(Default, Serialize, Deserialize)]
#[cfg_attr(test, derive(Clone))]
pub(crate) struct ClassBodyFacts {
    #[cfg_attr(test, serde(serialize_with = "fact_map_probe::serialize"))]
    isolation: HashMap<BodyId, bool>,
    #[cfg_attr(test, serde(serialize_with = "fact_map_probe::serialize"))]
    placements: HashMap<BodyId, Rc<SuperInitPlacement>>,
}

#[cfg(test)]
pub(crate) mod fact_map_probe {
    use super::*;

    thread_local! {
        static ENTRIES: RefCell<Option<Vec<usize>>> = const { RefCell::new(None) };
    }

    pub(crate) fn observe<T>(write: impl FnOnce() -> T) -> (T, Vec<usize>) {
        ENTRIES.set(Some(Vec::new()));
        let result = write();
        (result, ENTRIES.take().unwrap())
    }

    pub(super) fn serialize<V: Serialize, S: serde::Serializer>(
        map: &HashMap<BodyId, V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        ENTRIES.with_borrow_mut(|entries| {
            if let Some(entries) = entries {
                entries.extend(map.keys().map(|key| std::ptr::from_ref(key).addr()));
            }
        });
        map.serialize(serializer)
    }
}

struct ActiveRecords<'a>(&'a Cache);

impl Serialize for ActiveRecords<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let records = self.0.records.borrow();
        serializer.collect_seq(
            self.0
                .active
                .borrow()
                .iter()
                .filter_map(|key| records.get(key).map(|record| (key, record.as_ref()))),
        )
    }
}

#[derive(Default)]
pub(crate) struct Cache {
    records: RefCell<HashMap<String, Rc<Record>>>,
    active: RefCell<HashSet<String>>,
    /// Source and expanded bodies reconcile at different stages of the frontend.
    pub(crate) body_facts: [RefCell<ClassBodyFacts>; 2],
    correspondence: crate::compiler_db::syntax::Correspondence,
    pub replay_us: Cell<u128>,
    pub semantic_us: Cell<u128>,
    pub bodies: Cell<usize>,
    pub signatures: Cell<usize>,
    pub instances: Cell<usize>,
    pub templates: Cell<usize>,
    pub declarations: Cell<usize>,
    pub validations: Cell<usize>,
}
impl Cache {
    pub(crate) fn revision(
        previous: Option<&Self>,
        correspondence: &crate::compiler_db::syntax::Correspondence,
    ) -> Self {
        let mut result = Self::default();
        result.correspondence.spans = correspondence.spans.clone();
        if let Some(previous) = previous {
            *result.records.borrow_mut() = previous.records.borrow().clone();
            result.reuse_body_facts(previous, correspondence, true, |id| id);
        }
        result
    }
    pub(crate) fn reuse_body_facts(
        &self,
        previous: &Self,
        correspondence: &crate::compiler_db::syntax::Correspondence,
        precheck: bool,
        source_body: impl Fn(BodyId) -> BodyId,
    ) {
        let index = usize::from(precheck);
        let previous = previous.body_facts[index].borrow();
        let mut facts = self.body_facts[index].borrow_mut();
        let unchanged = |id| correspondence.unchanged.contains(&source_body(id));
        facts.isolation.extend(
            previous
                .isolation
                .iter()
                .filter(|(id, _)| unchanged(**id))
                .map(|(&id, &value)| (id, value)),
        );
        for (&id, placement) in &previous.placements {
            if unchanged(id)
                && let Some(mapped) = placement.relocated(correspondence)
            {
                facts.placements.insert(id, mapped);
            }
        }
    }
    fn constructor_placements(
        &self,
        class: &ClassDecl,
        precheck: bool,
    ) -> Vec<Rc<SuperInitPlacement>> {
        if class.base_class.is_none() {
            return Vec::new();
        }
        let mut facts = self.body_facts[usize::from(precheck)].borrow_mut();
        class
            .constructors
            .iter()
            .map(|ctor| {
                // A signature shell can share its body ID with the hydrated constructor.
                if ctor.body.stmts.is_empty() {
                    return Rc::new(SuperInitPlacement::of(ctor));
                }
                Rc::clone(
                    facts
                        .placements
                        .entry(ctor.body.id)
                        .or_insert_with(|| Rc::new(SuperInitPlacement::of(ctor))),
                )
            })
            .collect()
    }
    fn class_isolation(&self, class: &ClassDecl, precheck: bool) -> ClassIsolation {
        let mut facts = self.body_facts[usize::from(precheck)].borrow_mut();
        let mut isolated = |body: &Block| {
            // Empty signature shells need no walk and can share a body ID with
            // the hydrated definition.
            if body.stmts.is_empty() {
                return true;
            }
            *facts
                .isolation
                .entry(body.id)
                .or_insert_with(|| transfer_ownership::isolated_body(body))
        };
        ClassIsolation {
            constructor: class.constructors.first().map(|c| isolated(&c.body)),
            methods: class.methods.iter().map(|m| isolated(&m.body)).collect(),
        }
    }

    pub(crate) fn finish(&self) {
        self.records
            .borrow_mut()
            .retain(|key, _| self.active.borrow().contains(key));
    }
    pub(crate) fn active_records(&self) -> impl Serialize + '_ {
        ActiveRecords(self)
    }
    pub(crate) fn restore(records: Vec<(String, Record)>) -> anyhow::Result<Self> {
        let result = Self::default();
        let mut scratch = TypeChecker::new();
        for (key, record) in records {
            // Decode into an isolated checker before a persisted record can
            // mutate the live build. Malformed cache data is a cache miss.
            let body: super::body::TypedBody = serde_json::from_value(record.body.clone())?;
            scratch.absorb_body(body, None)?;
            for (key, _) in &record.reads {
                scratch.validate_precheck_key(key)?;
            }
            for event in &record.dispatch {
                let read: crate::semantic::symbols::SymbolRead =
                    serde_json::from_value(event.read.clone())?;
                anyhow::ensure!(
                    matches!(read, crate::semantic::symbols::SymbolRead::TaskDispatch(..)),
                    "invalid dispatch read"
                );
                for (key, value) in &event.writes {
                    anyhow::ensure!(key.field == "symbols", "invalid dispatch write");
                    scratch.validate_precheck_key(key)?;
                    scratch.precheck_apply(key, value.clone())?;
                }
            }
            for (key, value) in &record.writes {
                scratch.validate_precheck_key(key)?;
                scratch.precheck_apply(key, value.clone())?;
            }
            anyhow::ensure!(
                result
                    .records
                    .borrow_mut()
                    .insert(key, Rc::new(record))
                    .is_none(),
                "duplicate precheck record"
            );
        }
        Ok(result)
    }
}
impl TypeChecker {
    fn validate_precheck_key(&self, key: &Key) -> anyhow::Result<()> {
        match key.field.as_str() {
            "registration_path" | "type_help" | "import_span" | "function_span" => {
                anyhow::ensure!(
                    key.key.as_ref().is_some_and(Value::is_string),
                    "invalid name dependency"
                );
                Ok(())
            }

            "resolution.imported_names" => self.imported_names.validate_key(key),
            "resolution.module_access_names" => self.module_access_names.validate_key(key),
            "resolution.imported_collection_aliases" => {
                self.imported_collection_aliases.validate_key(key)
            }
            "resolution.imported_std_modules" => self.imported_std_modules.validate_key(key),
            "resolution.imported_blocking_std_functions" => {
                self.imported_blocking_std_functions.validate_key(key)
            }
            "resolution.signature_only_modules" => self.signature_only_modules.validate_key(key),
            "resolution.imported_collection_types" => {
                self.imported_collection_types.validate_key(key)
            }
            "resolution.array_abi_files" => self.array_abi_files.validate_key(key),
            "resolution.provider_template_files" => self.provider_template_files.validate_key(key),
            "resolution.array_classes" => self.array_classes.validate_key(key),
            "resolution.imported_collection_targets" => {
                self.imported_collection_targets.validate_key(key)
            }
            "resolution.marker_contracts" => self.marker_contracts.validate_key(key),
            "resolution.marker_contract_proven" => self.marker_contract_proven.validate_key(key),
            "field_write" => {
                let _: (String, Span) = serde_json::from_value(
                    key.key
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("missing field key"))?,
                )?;
                Ok(())
            }
            "symbols" => {
                let read: crate::semantic::symbols::SymbolRead = serde_json::from_value(
                    key.key
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("missing symbol key"))?,
                )?;
                anyhow::ensure!(
                    !matches!(read, crate::semantic::symbols::SymbolRead::Dispatch(..)),
                    "unsupported symbol dependency"
                );
                Ok(())
            }
            "resolution.invalid_type_names" => self.invalid_type_names.borrow().validate_key(key),
            "resolution.reported_class_cycles" => self.reported_class_cycles.validate_key(key),
            "resolution.missing_collection_imports_reported" => {
                self.missing_collection_imports_reported.validate_key(key)
            }
            "resolution.fully_qualified_collection_types" => {
                self.fully_qualified_collection_types.validate_key(key)
            }
            _ => self.generics.validate_journal_key(key),
        }
    }
    fn precheck_input(&self, key: &Key, raw: bool) -> Value {
        if key.field == "import_span" {
            return serde_json::to_value(
                self.imported_names
                    .get(key.key.as_ref().unwrap().as_str().unwrap()),
            )
            .unwrap();
        }
        if key.field == "function_span" {
            return serde_json::to_value(
                self.symbols
                    .lookup_func(key.key.as_ref().unwrap().as_str().unwrap())
                    .map(|info| info.declaration_span),
            )
            .unwrap();
        }

        if key.field == "resolution.marker_contracts" {
            return if raw {
                self.marker_contracts.output_value(key)
            } else {
                self.marker_contracts.read_value(key)
            };
        }
        if key.field == "resolution.marker_contract_proven" {
            return if raw {
                self.marker_contract_proven.output_value(key)
            } else {
                self.marker_contract_proven.read_value(key)
            };
        }

        if key.field == "resolution.imported_collection_targets" {
            return if raw {
                self.imported_collection_targets.output_value(key)
            } else {
                self.imported_collection_targets.read_value(key)
            };
        }
        if key.field == "registration_path" {
            return serde_json::to_value(
                self.registration_import_paths
                    .get(key.key.as_ref().unwrap().as_str().unwrap()),
            )
            .unwrap();
        }
        if key.field == "type_help" {
            return self
                .unknown_type_help_value(key.key.as_ref().unwrap().as_str().unwrap())
                .into();
        }

        if key.field == "resolution.array_classes" {
            return if raw {
                self.array_classes.output_value(key)
            } else {
                self.array_classes.read_value(key)
            };
        }
        if key.field == "resolution.imported_names" {
            return if raw {
                self.imported_names.output_value(key)
            } else {
                self.imported_names.read_value(key)
            };
        }
        if key.field == "resolution.module_access_names" {
            return if raw {
                self.module_access_names.output_value(key)
            } else {
                self.module_access_names.read_value(key)
            };
        }
        if key.field == "resolution.imported_collection_aliases" {
            return if raw {
                self.imported_collection_aliases.output_value(key)
            } else {
                self.imported_collection_aliases.read_value(key)
            };
        }
        if key.field == "resolution.imported_std_modules" {
            return if raw {
                self.imported_std_modules.output_value(key)
            } else {
                self.imported_std_modules.read_value(key)
            };
        }
        if key.field == "resolution.imported_blocking_std_functions" {
            return if raw {
                self.imported_blocking_std_functions.output_value(key)
            } else {
                self.imported_blocking_std_functions.read_value(key)
            };
        }
        if key.field == "resolution.signature_only_modules" {
            return if raw {
                self.signature_only_modules.output_value(key)
            } else {
                self.signature_only_modules.read_value(key)
            };
        }
        if key.field == "resolution.imported_collection_types" {
            return if raw {
                self.imported_collection_types.output_value(key)
            } else {
                self.imported_collection_types.read_value(key)
            };
        }
        if key.field == "resolution.array_abi_files" {
            return if raw {
                self.array_abi_files.output_value(key)
            } else {
                self.array_abi_files.read_value(key)
            };
        }
        if key.field == "resolution.provider_template_files" {
            return if raw {
                self.provider_template_files.output_value(key)
            } else {
                self.provider_template_files.read_value(key)
            };
        }

        if key.field == "field_write" {
            let (name, span): (String, Span) = serde_json::from_value(key.key.clone().unwrap())
                .expect("validated field dependency");
            return self
                .field_writes
                .as_ref()
                .is_some_and(|writes| writes.is_reassigned(&name, span))
                .into();
        }

        if key.field == "resolution.invalid_type_names" {
            return if raw {
                self.invalid_type_names.borrow().output_value(key)
            } else {
                self.invalid_type_names.borrow().read_value(key)
            };
        }

        if key.field == "resolution.reported_class_cycles" {
            return if raw {
                self.reported_class_cycles.output_value(key)
            } else {
                self.reported_class_cycles.read_value(key)
            };
        }

        if key.field == "resolution.missing_collection_imports_reported" {
            return if raw {
                self.missing_collection_imports_reported.output_value(key)
            } else {
                self.missing_collection_imports_reported.read_value(key)
            };
        }

        if key.field == "resolution.fully_qualified_collection_types" {
            return if raw {
                self.fully_qualified_collection_types.output_value(key)
            } else {
                self.fully_qualified_collection_types.read_value(key)
            };
        }

        if key.field == "symbols" {
            let read: crate::semantic::symbols::SymbolRead =
                serde_json::from_value(key.key.clone().unwrap()).unwrap();
            if raw {
                self.symbols.precheck_output(&read)
            } else {
                self.symbols.precheck_value(&read)
            }
        } else {
            self.generics.journal_value(key, raw)
        }
    }
    fn precheck_apply(&mut self, key: &Key, value: Value) -> anyhow::Result<()> {
        if key.field == "resolution.marker_contracts" {
            return self.marker_contracts.restore_value(key, value);
        }
        if key.field == "resolution.marker_contract_proven" {
            return self.marker_contract_proven.restore_value(key, value);
        }

        if key.field == "resolution.imported_collection_targets" {
            return self.imported_collection_targets.restore_value(key, value);
        }
        if key.field == "resolution.array_classes" {
            return self.array_classes.restore_value(key, value);
        }
        if key.field == "resolution.imported_names" {
            return self.imported_names.restore_value(key, value);
        }
        if key.field == "resolution.module_access_names" {
            return self.module_access_names.restore_value(key, value);
        }
        if key.field == "resolution.imported_collection_aliases" {
            return self.imported_collection_aliases.restore_value(key, value);
        }
        if key.field == "resolution.imported_std_modules" {
            return self.imported_std_modules.restore_value(key, value);
        }
        if key.field == "resolution.signature_only_modules" {
            return self.signature_only_modules.restore_value(key, value);
        }
        if key.field == "resolution.imported_collection_types" {
            return self.imported_collection_types.restore_value(key, value);
        }
        if key.field == "resolution.array_abi_files" {
            return self.array_abi_files.restore_value(key, value);
        }
        if key.field == "resolution.provider_template_files" {
            return self.provider_template_files.restore_value(key, value);
        }

        if key.field == "resolution.invalid_type_names" {
            return self
                .invalid_type_names
                .borrow_mut()
                .restore_value(key, value);
        }

        if key.field == "resolution.reported_class_cycles" {
            return self.reported_class_cycles.restore_value(key, value);
        }

        if key.field == "resolution.missing_collection_imports_reported" {
            return self
                .missing_collection_imports_reported
                .restore_value(key, value);
        }

        if key.field == "resolution.fully_qualified_collection_types" {
            return self
                .fully_qualified_collection_types
                .restore_value(key, value);
        }

        if key.field == "symbols" {
            self.symbols
                .restore_precheck(serde_json::from_value(key.key.clone().unwrap())?, value)
        } else {
            self.generics.restore_journal(key, value)
        }
    }
    fn precheck_matches(&self, record: &Record) -> bool {
        if !record
            .reads
            .iter()
            .all(|(key, value)| self.precheck_input(key, false) == *value)
        {
            return false;
        }
        // Replay the operation's own declaration deltas at each recorded read.
        // One fork and one ordered log avoid cumulative declaration snapshots.
        if !record.dispatch.is_empty() {
            let mut symbols = self.symbols.fork_body_scope();
            for event in &record.dispatch {
                for (key, value) in &event.writes {
                    symbols
                        .restore_precheck(
                            serde_json::from_value(key.key.clone().unwrap())
                                .expect("validated symbol key"),
                            value.clone(),
                        )
                        .expect("validated symbol output");
                }
                let read =
                    serde_json::from_value(event.read.clone()).expect("validated dispatch read");
                if symbols.symbol_value(&read) != Value::Bool(event.result) {
                    return false;
                }
            }
        }
        true
    }
    pub(super) fn precheck_operation<T: Serialize + DeserializeOwned>(
        &mut self,
        key: String,
        id: BodyId,
        syntax: Value,
        check: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let Some(cache) = self.prechecks.clone().filter(|_| !journal::is_active()) else {
            return check(self);
        };
        let replay_start = std::time::Instant::now();
        let key = format!("{}:{key}", self.module_path.as_deref().unwrap_or(""));
        // Distinct declarations can share a name even in an invalid program.
        // Keep their ordered operations separate so stable diagnostics replay.
        let occurrence = self.precheck_occurrences.entry(key.clone()).or_default();
        let key = format!("{key}:occurrence:{occurrence}");
        *occurrence += 1;
        let input_spans = crate::compiler_db::syntax::spans(&syntax);
        // Recreated specializations carry new node identities. Their outputs
        // must be checked against those identities; replayed requests retain
        // the original identities and can reuse their checked instance.
        let syntax = if key.contains(":instance:") || key.contains("_validate:defaults:") {
            crate::compiler_db::syntax::without_spans(&syntax)
        } else {
            crate::compiler_db::syntax::semantic(&syntax)
        };
        let context = json!([
            self.precheck_context,
            self.local.current_class,
            self.local.declared_type_params,
            self.generics.abstract_check
        ]);
        let prior = cache.records.borrow().get(&key).cloned();
        if let Some(prior) = prior
            && prior.syntax == syntax
            && prior.context == context
            && prior.input_spans.len() == input_spans.len()
        {
            let local = prior
                .input_spans
                .iter()
                .copied()
                .zip(input_spans.iter().copied())
                .collect();
            let remapped: anyhow::Result<Record> = cache
                .correspondence
                .remap_with_spans(prior.as_ref(), &local);
            if let Ok(record) = remapped
                && self.generics.replay_budget_allows(&record.writes)
                && self.precheck_matches(&record)
            {
                let result = serde_json::from_value(record.result.clone());
                let body = serde_json::from_value(record.body.clone());
                if let (Ok(result), Ok(body)) = (result, body) {
                    for (key, value) in &record.writes {
                        self.precheck_apply(key, value.clone())
                            .expect("validated precheck replay");
                    }
                    self.generics.pending.extend(record.pending.clone());
                    self.generics.emitted.extend(record.emitted.clone());
                    self.generics
                        .source_symbols
                        .extend(record.source_symbols.clone());
                    self.absorb_body(body, None)
                        .expect("inline precheck output");
                    cache
                        .records
                        .borrow_mut()
                        .insert(key.clone(), Rc::new(record));
                    cache.active.borrow_mut().insert(key);
                    cache
                        .replay_us
                        .set(cache.replay_us.get() + replay_start.elapsed().as_micros());
                    return result;
                }
            }
        }
        cache
            .replay_us
            .set(cache.replay_us.get() + replay_start.elapsed().as_micros());
        let semantic_start = std::time::Instant::now();
        let mut saved = super::body::BodyOutputs::default();
        self.swap_body_outputs(&mut saved);
        let pending = std::mem::take(&mut self.generics.pending);
        let emitted = std::mem::take(&mut self.generics.emitted);
        let source_symbols = std::mem::take(&mut self.generics.source_symbols);
        let generation = self.error_generation;
        let capture = journal::begin();
        let result = check(self);
        let journal = capture.finish();
        cache
            .semantic_us
            .set(cache.semantic_us.get() + semantic_start.elapsed().as_micros());
        let writes = journal
            .touched
            .iter()
            .map(|key| (key.clone(), self.precheck_input(key, true)))
            .collect();
        let mut outputs = super::body::BodyOutputs::default();
        self.swap_body_outputs(&mut outputs);
        self.swap_body_outputs(&mut saved);
        let body = super::body::TypedBody::new(id, outputs, HashSet::new(), HashSet::new());
        let record = Record {
            syntax,
            input_spans,
            context,
            reads: journal.reads.into_iter().collect(),
            dispatch: journal.dispatch,
            writes,
            body: serde_json::to_value(&body).expect("precheck outputs"),
            result: serde_json::to_value(&result).expect("precheck result"),
            pending: self.generics.pending.iter().cloned().collect(),
            emitted: self.generics.emitted.clone(),
            source_symbols: self.generics.source_symbols.clone(),
        };
        let mut all_pending = pending;
        all_pending.append(&mut self.generics.pending);
        self.generics.pending = all_pending;
        let mut all_emitted = emitted;
        all_emitted.append(&mut self.generics.emitted);
        self.generics.emitted = all_emitted;
        let generated = std::mem::replace(&mut self.generics.source_symbols, source_symbols);
        self.generics.source_symbols.extend(generated);
        self.error_generation = generation;
        self.absorb_body(body, None)
            .expect("inline precheck output");
        cache
            .records
            .borrow_mut()
            .insert(key.clone(), Rc::new(record));
        cache.active.borrow_mut().insert(key);
        result
    }
    pub(super) fn cached_static_initializer(
        &mut self,
        field: &FieldDecl,
        init: &Expr,
        class: &ClassDecl,
    ) {
        if self.prechecks.is_none() || journal::is_active() {
            self.check_static_initializer(field, init, class);
            return;
        }
        self.precheck_operation(
            format!("initializer:{}:{}", class.name, field.name),
            BodyId::fresh(),
            json!([class.name, field]),
            |checker| {
                let cache = checker.prechecks.as_ref().unwrap();
                cache.bodies.set(cache.bodies.get() + 1);
                checker.check_static_initializer(field, init, class);
            },
        );
    }
    pub(super) fn check_local_decl_collision(&mut self, name: &str, span: Span) {
        self.cached_validation(
            "import_collision",
            name,
            |_| json!([name, span]),
            |checker| {
                if journal::is_active() {
                    journal::read(
                        Key::new("import_span", Some(name.into())),
                        serde_json::to_value(checker.imported_names.get(name)).unwrap(),
                    );
                }
                checker.check_local_decl_collision_uncached(name, span);
            },
        );
    }
    pub(super) fn cached_constant_collision(&mut self, f: &FunctionDecl) -> bool {
        self.cached_validation(
            "constant_collision",
            &f.name,
            |_| json!([f.name, f.span, f.constant]),
            |checker| {
                if journal::is_active() {
                    journal::read(
                        Key::new("function_span", Some(f.name.clone().into())),
                        serde_json::to_value(
                            checker
                                .symbols
                                .lookup_func(&f.name)
                                .map(|info| info.declaration_span),
                        )
                        .unwrap(),
                    );
                }
                checker.reject_constant_redeclaration_uncached(f)
            },
        )
    }
    pub(super) fn cached_duplicate_declaration(
        &mut self,
        name: &str,
        (span, kind, generic): (Span, &str, bool),
        (first_span, first_kind, first_generic): (Span, &str, bool),
    ) {
        self.cached_validation(
            "duplicate",
            name,
            |_| {
                json!([
                    name,
                    span,
                    kind,
                    generic,
                    first_span,
                    first_kind,
                    first_generic
                ])
            },
            |checker| {
                if generic || first_generic {
                    checker.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0706,
                            format!("{kind} `{name}` is already declared as a {first_kind}"),
                        )
                        .with_label(Label::primary(span, "second declaration"))
                        .with_label(Label::secondary(first_span, "first declared here"))
                        .with_help("rename one of them"),
                    );
                }
            },
        );
    }
    fn cached_validation<T: Serialize + DeserializeOwned>(
        &mut self,
        kind: &str,
        name: &str,
        syntax: impl FnOnce(&Cache) -> Value,
        check: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let Some(cache) = self
            .prechecks
            .clone()
            .or_else(|| self.declaration_cache.clone())
            .filter(|_| !journal::is_active())
        else {
            return check(self);
        };
        let phase = if self.prechecks.is_some() {
            "precheck"
        } else {
            "final"
        };
        let syntax = syntax(&cache);
        let previous = self.prechecks.replace(cache);
        let result = self.precheck_operation(
            format!("{phase}_validate:{kind}:{name}"),
            BodyId::fresh(),
            syntax,
            |checker| {
                let cache = checker.prechecks.as_ref().unwrap();
                cache.validations.set(cache.validations.get() + 1);
                check(checker)
            },
        );
        self.prechecks = previous;
        result
    }
    pub(super) fn cached_enum_validation(&mut self, decl: &EnumDecl) {
        self.cached_validation(
            "enum",
            &decl.name,
            |_| serde_json::to_value(decl).unwrap(),
            |checker| checker.check_enum_types(decl),
        );
    }
    pub(super) fn cached_interface_validation(&mut self, decl: &InterfaceDecl) {
        self.cached_validation(
            "interface",
            &decl.name,
            |_| interface_header_syntax(decl),
            |checker| checker.check_interface_header(decl),
        );
    }
    pub(super) fn cached_class_validation(&mut self, class: &ClassDecl) -> Type {
        let constructors = std::cell::OnceCell::new();
        let precheck = self.prechecks.is_some();
        self.cached_validation(
            "class",
            &class.name,
            |cache| {
                let placements =
                    constructors.get_or_init(|| cache.constructor_placements(class, precheck));
                class_header_syntax(class, placements)
            },
            |checker| checker.check_class_header(class, constructors.get().map(Vec::as_slice)),
        )
    }
    pub(super) fn cached_class_defaults(&mut self, class: &ClassDecl) -> HashSet<String> {
        let copies: Vec<_> = class
            .methods
            .iter()
            .filter(|m| {
                m.is_interface_default
                    && !m.is_default_injected
                    && m.body.span.file_id != class.span.file_id
            })
            .map(|m| (m.name.as_str(), m.body.span))
            .collect();
        if copies.is_empty() {
            return HashSet::new();
        }
        self.cached_validation(
            "defaults",
            &class.name,
            |_| foreign_defaults_syntax(class, &copies),
            |checker| {
                checker
                    .instantiate_foreign_defaults(class, &copies)
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            },
        )
    }
    pub(super) fn precheck_declaration(&mut self, item: &Item) {
        let check = |checker: &mut Self, isolation: Option<&ClassIsolation>| match item {
            Item::Class(c) => checker.register_class(c, isolation),
            Item::Enum(e) => checker.register_enum(e),
            Item::Interface(i) => checker.register_interface(i, None),
            _ => unreachable!(),
        };
        let Some(cache) = self
            .prechecks
            .clone()
            .or_else(|| self.declaration_cache.clone())
            .filter(|_| !journal::is_active())
        else {
            check(self, None);
            return;
        };
        let phase = if self.prechecks.is_some() {
            "precheck"
        } else {
            "final"
        };
        let (kind, name, syntax, isolation) = match item {
            Item::Class(c) => {
                let isolation = cache.class_isolation(c, self.prechecks.is_some());
                (
                    "class",
                    c.name.as_str(),
                    class_registration_syntax(c, &isolation),
                    Some(isolation),
                )
            }
            Item::Enum(e) => (
                "enum",
                e.name.as_str(),
                serde_json::to_value(e).unwrap(),
                None,
            ),
            Item::Interface(i) => (
                "interface",
                i.name.as_str(),
                interface_header_syntax(i),
                None,
            ),
            _ => unreachable!(),
        };
        let previous = self.prechecks.replace(cache);
        self.precheck_operation(
            format!("{phase}_declaration:{kind}:{name}"),
            BodyId::fresh(),
            syntax,
            |checker| {
                let cache = checker.prechecks.as_ref().unwrap();
                cache.declarations.set(cache.declarations.get() + 1);
                check(checker, isolation.as_ref());
            },
        );
        self.prechecks = previous;
    }
    pub(super) fn prepare_precheck(&mut self, program: &Program) {
        if self.prechecks.is_none() && self.declaration_cache.is_none() {
            return;
        }
        self.precheck_context = crate::compiler_db::syntax::semantic(&json!([
            self.module_path,
            self.class_module_path,
            self.enforce_send_sync,
            self.task_sync_preemption,
            self.signature_metadata,
            self.entry_program,
        ]));
        if self.prechecks.is_none() {
            return;
        }
        for item in &program.items {
            match item {
                Item::Function(f) => {
                    self.precheck_bodies
                        .insert(f.body.id, serde_json::to_value(f).unwrap());
                }
                Item::Class(c) => {
                    for m in &c.methods {
                        self.precheck_bodies.insert(m.body.id, json!([c.name, m]));
                    }
                    for ctor in &c.constructors {
                        self.precheck_bodies
                            .insert(ctor.body.id, json!([c.name, ctor]));
                    }
                }
                Item::Interface(i) => {
                    for method in &i.methods {
                        if let Some(body) = &method.default_body {
                            self.precheck_bodies
                                .insert(body.id, json!([i.name, i.type_params, method]));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restore_records(cache: &Cache) -> Cache {
        let bytes = serde_json::to_vec(&cache.active_records()).unwrap();
        Cache::restore(serde_json::from_slice(&bytes).unwrap()).unwrap()
    }

    #[test]
    fn validation_builds_syntax_only_for_an_active_cache_lookup() {
        let builds = Cell::new(0);
        let validate = |checker: &mut TypeChecker| {
            checker.cached_validation(
                "enum",
                "Example",
                |_| {
                    builds.set(builds.get() + 1);
                    json!("header")
                },
                |_| 42,
            )
        };
        let mut checker = TypeChecker::new();
        assert_eq!(validate(&mut checker), 42);
        assert_eq!(builds.get(), 0);

        checker.declaration_cache = Some(Rc::default());
        let capture = journal::begin();
        assert_eq!(validate(&mut checker), 42);
        assert_eq!(builds.get(), 0);
        drop(capture);
        assert_eq!(validate(&mut checker), 42);
        assert_eq!(builds.get(), 1);
    }

    #[test]
    fn uncached_enum_validation_skips_payload_serialization() {
        use crate::parser::ast::SERIALIZED_TYPE_NODES;

        for count in [1, 16, 128] {
            let variants = (0..count)
                .map(|i| format!("V{i}(i64)"))
                .collect::<Vec<_>>()
                .join(", ");
            let source = format!("enum Example {{ {variants} }}");
            let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
            let (program, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(errors.is_empty(), "{errors:?}");
            let Item::Enum(decl) = &program.items[0] else {
                panic!("expected enum")
            };
            let mut checker = TypeChecker::new();
            checker.register_enum(decl);
            SERIALIZED_TYPE_NODES.set(0);
            checker.cached_enum_validation(decl);
            assert_eq!(SERIALIZED_TYPE_NODES.get(), 0);
            assert!(checker.errors.is_empty());
            // The eager key would have serialized every variant's payload.
            let _ = serde_json::to_value(decl).unwrap();
            assert_eq!(SERIALIZED_TYPE_NODES.get(), count);
        }
    }

    #[test]
    fn interface_header_cache_reuses_signatures_without_copying_default_bodies() {
        use crate::parser::ownership::CLONED_EXPR_NODES;

        let parse = |size, return_type: &str| {
            let body = "1;".repeat(size);
            let source = format!(
                "interface Example {{ @pure fn read(self, x: i64) -> {return_type} {{ {body} return x; }} }}"
            );
            let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
            let (mut program, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(errors.is_empty(), "{errors:?}");
            program.items.pop().unwrap()
        };
        let check = |item: &Item, cache: Option<Rc<Cache>>| {
            let Item::Interface(interface) = item else {
                panic!("expected interface")
            };
            let mut checker = TypeChecker::new();
            checker.prechecks = cache;
            CLONED_EXPR_NODES.set(0);
            checker.precheck_declaration(item);
            checker.cached_interface_validation(interface);
            assert_eq!(CLONED_EXPR_NODES.get(), 0);
            assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        };

        let mut previous = None;
        for size in [0, 1, 16, 256] {
            let item = parse(size, "i64");
            let Item::Interface(interface) = &item else {
                panic!("expected interface")
            };
            // Compare the wire representation and independently calibrate the
            // AST copy counter against the original clone-and-strip operation.
            CLONED_EXPR_NODES.set(0);
            let mut signature = interface.clone();
            assert_eq!(CLONED_EXPR_NODES.get(), size + 1);
            signature.methods[0].default_body = None;
            assert_eq!(interface_header_syntax(interface), json!(signature));
            check(&item, None);

            let cold = Rc::new(Cache::default());
            check(&item, Some(Rc::clone(&cold)));
            assert_eq!((cold.declarations.get(), cold.validations.get()), (1, 1));

            let warm = Rc::new(Cache::revision(
                previous.as_deref().or(Some(&cold)),
                &Default::default(),
            ));
            check(&item, Some(Rc::clone(&warm)));
            assert_eq!((warm.declarations.get(), warm.validations.get()), (0, 0));
            previous = Some(warm);
        }

        let changed = Rc::new(Cache::revision(previous.as_deref(), &Default::default()));
        check(&parse(256, "bool"), Some(Rc::clone(&changed)));
        assert_eq!(
            (changed.declarations.get(), changed.validations.get()),
            (1, 1)
        );
    }

    #[test]
    fn constructor_placement_replays_reconciled_and_relocated_bodies() {
        use crate::compiler_db::syntax::Correspondence;

        fn parse(size: usize, ordinary: usize, nested: bool) -> Program {
            let init = if nested {
                "if true { super.init(); }"
            } else {
                "super.init();"
            };
            let source = format!(
                "open class Base {{ pub init(self) {{}} }}
                 class Child extends Base {{
                   pub fn ordinary(self) {{ {} }}
                   pub init(self) {{ {} {init} }}
                 }}",
                "2;\n".repeat(ordinary),
                "1;".repeat(size)
            );
            let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
            let (program, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(errors.is_empty(), "{errors:?}");
            program
        }
        fn validate(program: &Program, cache: Option<Rc<Cache>>) -> (Vec<Diagnostic>, usize) {
            let [Item::Class(base), Item::Class(child)] = program.items.as_slice() else {
                unreachable!()
            };
            let mut checker = TypeChecker::new();
            checker.register_class(base, None);
            checker.register_class(child, None);
            checker.declaration_cache = cache;
            PLACEMENT_STATEMENT_VISITS.set(0);
            checker.cached_class_validation(child);
            (checker.errors, PLACEMENT_STATEMENT_VISITS.get())
        }
        for size in [1, 16, 128, 256] {
            let mut program = parse(size, 1, false);
            let mut cache = Rc::new(Cache::default());
            let (errors, visits) = validate(&program, Some(Rc::clone(&cache)));
            assert!(errors.is_empty());
            assert_eq!(visits, size + 1);
            let mut previous_nested = false;
            for (ordinary, nested) in [
                (8, false),
                (16, false),
                (16, true),
                (32, true),
                (64, true),
                (64, false),
            ] {
                let mut edited = parse(size, ordinary, nested);
                let header_edit = ordinary == 64;
                if header_edit {
                    let Item::Class(child) = &mut edited.items[1] else {
                        unreachable!()
                    };
                    child.methods[0].is_static = true;
                }
                let mut correspondence = Correspondence::default();
                correspondence.reconcile(&program, &mut edited).unwrap();
                let next = Rc::new(Cache::revision(Some(&cache), &correspondence));
                next.reuse_body_facts(&cache, &correspondence, false, |id| id);
                let (errors, visits) = validate(&edited, Some(Rc::clone(&next)));
                let (clean, cold_visits) = validate(&edited, None);
                assert_eq!(errors, clean);
                assert_eq!(errors.len(), usize::from(nested));
                if nested {
                    assert_eq!(errors[0].code, ErrorCode::E0848);
                }
                let changed = nested != previous_nested;
                assert_eq!(visits, if changed { cold_visits } else { 0 });
                assert_eq!(next.validations.get(), usize::from(changed || header_edit));
                assert_eq!(cold_visits, size + 1 + usize::from(nested));
                (program, cache, previous_nested) = (edited, next, nested);
            }
        }
    }

    #[test]
    fn class_registration_serializes_borrowed_signature_types() {
        for depth in [0, 4, 16] {
            let ty = format!("{}i64{}", "FrozenArray<".repeat(depth), ">".repeat(depth));
            for width in [1, 8, 32] {
                let mut source = format!(
                    "class C {{ pub value: {ty}; pub init(self, arg: {ty}) {{ self.value = arg; }}"
                );
                for i in 0..width {
                    source.push_str(&format!(
                        "pub fn f{i}(self, arg: {ty}) -> {ty} {{ return arg; }}"
                    ));
                }
                source.push('}');
                let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
                let (program, errors) = crate::parser::Parser::new(tokens).parse();
                assert!(errors.is_empty(), "{errors:?}");
                let Item::Class(class) = &program.items[0] else {
                    unreachable!()
                };
                let isolation = Cache::default().class_isolation(class, false);
                REBUILT_TYPE_NODES.set(0);
                let original = json!([generics::class_signature(class), isolation]);
                assert_eq!(REBUILT_TYPE_NODES.get(), (2 + 2 * width) * (depth + 1));
                REBUILT_TYPE_NODES.set(0);
                assert_eq!(class_registration_syntax(class, &isolation), original);
                assert_eq!(REBUILT_TYPE_NODES.get(), 0);
            }
        }
    }

    #[test]
    fn class_validation_keys_track_constructor_placement_and_foreign_origins() {
        fn parse(source: &str) -> Program {
            let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
            let (program, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(errors.is_empty(), "{errors:?}");
            program
        }
        let base = parse("open class Base { pub init(self) {} }");
        let Item::Class(base) = &base.items[0] else {
            unreachable!()
        };
        let provider = parse("pub interface Get<T> { fn get(self) -> i64 { return 1; } }");
        let Item::Interface(interface) = &provider.items[0] else {
            unreachable!()
        };
        let origin = interface.methods[0].default_body.as_ref().unwrap().span;

        let make = |size, constructor: &str| {
            let program = parse(&format!(
                "class Example extends Base implements util::Get<i64> {{
                  pub init(self) {{ {constructor} }}
                  pub fn get(self) -> i64 {{ return 1; }}
                  pub fn ordinary(self) {{ {} }}
                }}",
                "1;".repeat(size)
            ));
            let Item::Class(mut class) = program.items.into_iter().next().unwrap() else {
                unreachable!()
            };
            class.span.file_id = crate::diagnostics::FileId(1);
            class.methods[0].is_interface_default = true;
            class.methods[0].body.span = origin;
            class
        };
        let run = |class: &ClassDecl, previous: Option<&Cache>| {
            let cache = Rc::new(Cache::revision(previous, &Default::default()));
            let mut checker = TypeChecker::new();
            checker
                .generics
                .registry
                .borrow_mut()
                .intern_interface(interface);
            checker.register_module("util", "util", "util.wi", &provider);
            checker.register_class(base, None);
            checker.register_class(class, None);
            checker.prechecks = Some(Rc::clone(&cache));

            checker.cached_class_validation(class);
            let header_checks = cache.validations.get();
            let handled = checker.cached_class_defaults(class);
            let default_checks = cache.validations.get() - header_checks;
            let diagnostics: Vec<_> = checker.errors.iter().map(|d| d.message.as_str()).collect();
            let nested = diagnostics
                .contains(&"`super.init(...)` must be a top-level statement of the constructor");
            assert_eq!(diagnostics.len(), usize::from(nested), "{diagnostics:?}");
            (
                cache,
                (
                    header_checks,
                    default_checks,
                    nested,
                    handled.contains("get"),
                ),
            )
        };

        let mut previous = None;
        for size in [1, 16, 128, 256] {
            let class = make(size, "super.init();");
            SERIALIZED_EXPR_NODES.set(0);
            let _ = serde_json::to_value(&class).unwrap();
            assert_eq!(SERIALIZED_EXPR_NODES.get(), 2 * (size + 1));
            SERIALIZED_EXPR_NODES.set(0);
            let placement = SuperInitPlacement::of(&class.constructors[0]);
            let _ = class_header_syntax(&class, &[Rc::new(placement)]);
            assert_eq!(SERIALIZED_EXPR_NODES.get(), 0);
            let _ = foreign_defaults_syntax(&class, &[("get", origin)]);
            assert_eq!(SERIALIZED_EXPR_NODES.get(), 0);

            let (cache, result) = run(&class, previous.as_deref());
            let cold = usize::from(previous.is_none());
            assert_eq!(result, (cold, cold, false, true), "body size {size}");
            previous = Some(cache);
        }
        let mut class = make(256, "if true { super.init(); }");
        let (previous, result) = run(&class, previous.as_deref());
        assert_eq!(result, (1, 0, true, true));
        class.methods[0].body.span.start += 1;
        let (previous, result) = run(&class, Some(&previous));
        assert_eq!(result, (0, 1, true, false));
        class.methods[0].body.span = origin;
        let (_, result) = run(&class, Some(&previous));
        assert_eq!(result, (0, 1, true, true));
    }

    #[test]
    fn class_registration_tracks_changed_isolation_and_reuses_unchanged_bodies() {
        let parse = |body: &str| {
            let source =
                format!("class C {{ pub init(self) {{ 1; }} pub fn f(self) {{ {body} }} }}");
            let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
            let (program, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(errors.is_empty(), "{errors:?}");
            program.items.into_iter().next().unwrap()
        };
        let mut item = parse("1;");
        let Item::Class(class) = &item else {
            unreachable!()
        };
        let ctor = class.constructors[0].body.id;
        let method = class.methods[0].body.id;
        let run = |item: &Item, previous: Option<&Cache>, unchanged: &[BodyId]| {
            let mut correspondence = crate::compiler_db::syntax::Correspondence::default();
            correspondence.unchanged.extend(unchanged);
            let cache = Rc::new(Cache::revision(previous, &correspondence));
            let mut checker = TypeChecker::new();
            if let Some(previous) = previous {
                cache.reuse_body_facts(previous, &correspondence, false, |id| id);
            }
            checker.declaration_cache = Some(Rc::clone(&cache));
            transfer_ownership::ISOLATION_WALKS.set(0);
            checker.precheck_declaration(item);
            let info = checker.symbols.lookup_class("C").unwrap();
            let result = (
                info.methods["f"].isolated_body,
                cache.declarations.get(),
                transfer_ownership::ISOLATION_WALKS.get(),
            );
            (cache, result)
        };
        let (cache, result) = run(&item, None, &[]);
        assert_eq!(result, (true, 1, 2));
        let (mut cache, result) = run(&item, Some(&cache), &[ctor, method]);
        assert_eq!(result, (true, 0, 0));
        for (body, isolated, registrations) in
            [("2;", true, 0), ("other();", false, 1), ("1;", true, 1)]
        {
            let Item::Class(other) = parse(body) else {
                unreachable!()
            };
            let Item::Class(class) = &mut item else {
                unreachable!()
            };
            // Reconciliation preserves the body owner across an edit.
            class.methods[0].body.stmts = other.methods.into_iter().next().unwrap().body.stmts;
            let (next, result) = run(&item, Some(&cache), &[ctor]);
            assert_eq!(result, (isolated, registrations, 1));
            cache = next;
        }
        let Item::Class(class) = &mut item else {
            unreachable!()
        };
        class.methods[0].return_type = Type::Bool;
        let (_, result) = run(&item, Some(&cache), &[ctor, method]);
        assert_eq!(result, (true, 1, 0));
    }

    #[test]
    fn precheck_own_async_dispatch_replays_without_semantic_work() {
        let tokens =
            crate::lexer::Lexer::new("class Local { pub async fn go(self) -> i64 { return 1; } }")
                .tokenize()
                .unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let Item::Class(class) = &program.items[0] else {
            panic!("expected class")
        };
        let body = class.methods[0].body.id;
        let syntax = serde_json::to_value(class).unwrap();
        let execute = |cache: Rc<Cache>| {
            let mut checker = TypeChecker::new();
            checker.prechecks = Some(Rc::clone(&cache));
            let result = checker.precheck_operation(
                "own_async_dispatch".into(),
                body,
                syntax.clone(),
                |checker| {
                    cache.bodies.set(cache.bodies.get() + 1);
                    checker.register_class(class, None);
                    checker.symbols.dispatch_may_start_task("Local", "go")
                },
            );
            let facts = json!([
                result,
                checker.errors,
                checker
                    .symbols
                    .symbol_value(&crate::semantic::symbols::SymbolRead::Class("Local".into())),
            ]);
            (cache, facts)
        };
        let (initial, clean) = execute(Rc::new(Cache::default()));
        assert_eq!(clean[0], Value::Bool(true));
        assert_eq!(initial.bodies.get(), 1);
        assert!(
            initial
                .records
                .borrow()
                .values()
                .any(|record| !record.dispatch.is_empty())
        );
        // Exercise retained records and the persisted decoder independently.
        for disk in [false, true] {
            let mut prior = if disk {
                Rc::new(restore_records(&initial))
            } else {
                Rc::clone(&initial)
            };
            for _ in 0..3 {
                let next = Rc::new(Cache::revision(
                    Some(&prior),
                    &crate::compiler_db::syntax::Correspondence::default(),
                ));
                let (next, warm) = execute(next);
                assert_eq!(warm, clean, "disk={disk}");
                assert_eq!(next.bodies.get(), 0, "disk={disk}");
                prior = next;
            }
        }
    }
    #[test]
    fn precheck_dispatch_overlay_uses_state_at_read_before_later_descendants() {
        let tokens = crate::lexer::Lexer::new(
            "class Root { pub fn go(self) -> i64 { return 0; } }
             class External extends Root { pub async fn go(self) -> i64 { return 1; } }
             class Marker {}
             class Late extends Root { pub async fn go(self) -> i64 { return 2; } }",
        )
        .tokenize()
        .unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let classes: Vec<_> = program
            .items
            .iter()
            .map(|item| {
                let Item::Class(class) = item else {
                    panic!("expected class")
                };
                class
            })
            .collect();
        let body = BodyId::fresh();
        let execute = |cache: Rc<Cache>, external_async: bool| {
            let mut checker = TypeChecker::new();
            checker.register_class(classes[0], None);
            checker.register_class(classes[1], None);
            let mut external = checker.symbols.lookup_class("External").unwrap().clone();
            external.methods.get_mut("go").unwrap().is_async = external_async;
            checker.symbols.define_class("External".into(), external);
            checker.prechecks = Some(Rc::clone(&cache));
            let result = checker.precheck_operation(
                "dispatch_before_later_descendant".into(),
                body,
                json!([classes[2], classes[3]]),
                |checker| {
                    cache.bodies.set(cache.bodies.get() + 1);
                    checker.register_class(classes[2], None);
                    let result = checker.symbols.dispatch_may_start_task("Root", "go");
                    checker.register_class(classes[3], None);
                    result
                },
            );
            let final_dispatch = checker.symbols.dispatch_may_start_task("Root", "go");
            assert!(
                final_dispatch,
                "Late always starts a task in the final graph"
            );
            (cache, json!([result, checker.errors, final_dispatch]))
        };
        let (initial, before) = execute(Rc::new(Cache::default()), true);
        assert_eq!(before[0], Value::Bool(true));
        let (_, clean) = execute(Rc::new(Cache::default()), false);
        assert_eq!(clean[0], Value::Bool(false));
        for disk in [false, true] {
            let mut prior = if disk {
                Rc::new(restore_records(&initial))
            } else {
                Rc::clone(&initial)
            };
            for iteration in 0..3 {
                let next = Rc::new(Cache::revision(
                    Some(&prior),
                    &crate::compiler_db::syntax::Correspondence::default(),
                ));
                let (next, actual) = execute(next, false);
                assert_eq!(actual, clean, "disk={disk}, iteration={iteration}");
                assert_eq!(
                    next.bodies.get(),
                    usize::from(iteration == 0),
                    "disk={disk}, iteration={iteration}"
                );
                prior = next;
            }
        }
    }
}
