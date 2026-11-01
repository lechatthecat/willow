//! Pre-expansion semantic results. Generic state changes are sparse replay logs.
use super::precheck_journal::{self as journal, Key};
use super::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

#[derive(Clone, Serialize, Deserialize)]
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
#[derive(Default)]
pub(crate) struct Cache {
    records: RefCell<HashMap<String, Rc<Record>>>,
    active: RefCell<HashSet<String>>,
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
        }
        result
    }
    pub(crate) fn finish(&self) {
        self.records
            .borrow_mut()
            .retain(|key, _| self.active.borrow().contains(key));
    }
    pub(crate) fn snapshot(&self) -> Vec<(String, Record)> {
        self.active
            .borrow()
            .iter()
            .filter_map(|key| {
                self.records
                    .borrow()
                    .get(key)
                    .map(|r| (key.clone(), (**r).clone()))
            })
            .collect()
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
            format!("import_collision:{name}"),
            json!([name, span]),
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
            format!("constant_collision:{}", f.name),
            json!([f.name, f.span, f.constant]),
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
            format!("duplicate:{name}"),
            json!([
                name,
                span,
                kind,
                generic,
                first_span,
                first_kind,
                first_generic
            ]),
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
        key: String,
        syntax: Value,
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
        let previous = self.prechecks.replace(cache);
        let result = self.precheck_operation(
            format!("{phase}_validate:{key}"),
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
            format!("enum:{}", decl.name),
            serde_json::to_value(decl).unwrap(),
            |checker| checker.check_enum_types(decl),
        );
    }
    pub(super) fn cached_interface_validation(&mut self, decl: &InterfaceDecl) {
        let mut signature = decl.clone();
        for method in &mut signature.methods {
            method.default_body = None;
        }
        self.cached_validation(
            format!("interface:{}", decl.name),
            serde_json::to_value(signature).unwrap(),
            |checker| checker.check_interface_header(decl),
        );
    }
    pub(super) fn cached_class_validation(&mut self, class: &ClassDecl) -> Type {
        self.cached_validation(
            format!("class:{}", class.name),
            serde_json::to_value(class).unwrap(),
            |checker| checker.check_class_header(class),
        )
    }
    pub(super) fn cached_class_defaults(&mut self, class: &ClassDecl) -> HashSet<String> {
        if !class.methods.iter().any(|m| {
            m.is_interface_default
                && !m.is_default_injected
                && m.body.span.file_id != class.span.file_id
        }) {
            return HashSet::new();
        }
        self.cached_validation(
            format!("defaults:{}", class.name),
            serde_json::to_value(class).unwrap(),
            |checker| {
                checker
                    .instantiate_foreign_defaults(class)
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            },
        )
    }
    pub(super) fn precheck_declaration(&mut self, item: &Item) {
        let check = |checker: &mut Self| match item {
            Item::Class(c) => checker.register_class(c),
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
            check(self);
            return;
        };
        let phase = if self.prechecks.is_some() {
            "precheck"
        } else {
            "final"
        };
        let (kind, name, syntax) = match item {
            Item::Class(c) => (
                "class",
                c.name.as_str(),
                serde_json::to_value(generics::class_signature(c)).unwrap(),
            ),
            Item::Enum(e) => ("enum", e.name.as_str(), serde_json::to_value(e).unwrap()),
            Item::Interface(i) => {
                let mut signature = i.clone();
                for method in &mut signature.methods {
                    method.default_body = None;
                }
                (
                    "interface",
                    i.name.as_str(),
                    serde_json::to_value(signature).unwrap(),
                )
            }
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
                check(checker);
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
                    checker.register_class(class);
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
                Rc::new(Cache::restore(initial.snapshot()).unwrap())
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
            checker.register_class(classes[0]);
            checker.register_class(classes[1]);
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
                    checker.register_class(classes[2]);
                    let result = checker.symbols.dispatch_may_start_task("Root", "go");
                    checker.register_class(classes[3]);
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
                Rc::new(Cache::restore(initial.snapshot()).unwrap())
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
