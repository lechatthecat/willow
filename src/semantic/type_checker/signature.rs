//! Final signature normalization, with the same dependency/restore boundary as
//! body checking. Template expansion supplies concrete instantiation declarations
//! before this pass; ABI templates only substitute their storage representation.
use super::*;
use crate::compiler_db::checks::{CheckInput, CheckKind, CheckRecord, CheckValue, SignatureState};
use crate::qualified_name::QualifiedName;
use crate::semantic::builtin_types::BuiltinTypeId as Builtin;
use crate::semantic::symbols::{FuncInfo, SymbolReadCapture};
use serde_json::{Value, json};

impl TypeChecker {
    pub(super) fn record_signature_normalized(&mut self, input: &Type, output: &Type) {
        self.normalized_types.insert(input.clone(), output.clone());
        if self.signature_reads.is_some() {
            self.signature_normalized
                .insert(input.clone(), output.clone());
        }
    }

    pub(super) fn record_signature_collection(&mut self, name: String) {
        if self.signature_reads.is_some() {
            self.signature_qualified.insert(name.clone());
        }
        self.fully_qualified_collection_types.insert(name);
    }

    pub(super) fn signature_input(&self, input: &CheckInput) -> Value {
        // Declaration symbols are frozen during final signature registration.
        // Share fingerprints so F uses of a large enum do not serialize its
        // declaration F times. Array visibility/deduplication is sequential state.
        let cached = self.signature_values.borrow().get(input).cloned();
        let value = cached.unwrap_or_else(|| {
            use sha2::{Digest, Sha256};
            let value = self.signature_input_value(input);
            let value = Value::String(format!(
                "{:x}",
                crate::hex::Hex(&Sha256::digest(
                    serde_json::to_vec(&value).expect("signature dependency serialization")
                ))
            ));
            self.signature_values
                .borrow_mut()
                .insert(input.clone(), value.clone());
            value
        });
        if matches!(input, CheckInput::SignatureType(n, _) if Builtin::from_name(n) == Some(Builtin::Array))
        {
            let (qualified, missing) = self.signature_array_before.unwrap_or_else(|| {
                (
                    self.fully_qualified_collection_types
                        .contains(Builtin::Array.source_name()),
                    self.missing_collection_imports_reported
                        .contains(Builtin::Array.source_name()),
                )
            });
            json!([value, qualified, missing])
        } else {
            value
        }
    }

    fn signature_input_value(&self, input: &CheckInput) -> Value {
        match input {
            CheckInput::SignatureSymbol(read) => self.symbols.symbol_value(read),
            CheckInput::SignatureType(name, file) => {
                let template = self.generics.classes.get(name);
                let module = QualifiedName::new(&name)
                    .split_last()
                    .map(|(module, _)| module);
                let std_module = QualifiedName::new(name)
                    .split_first()
                    .and_then(|(module, _)| self.imported_std_modules.get(module));
                let collection = (Builtin::from_name(name) == Some(Builtin::Array)).then(|| {
                    json!([
                        self.array_abi_files.contains(file),
                        self.provider_template_files.contains(file),
                        self.imported_collection_types.contains(name),
                        self.imported_collection_targets.contains(name),
                    ])
                });
                let class_is_abi = self.symbols.lookup_class(name).is_some_and(|class| {
                    self.array_abi_files
                        .contains(&class.declaration_span.file_id)
                });
                crate::compiler_db::syntax::semantic(&json!([
                    template.map(|t| (&t.type_params, &t.abi_storage, t.public, t.span.file_id)),
                    self.local.declared_type_params.contains(name),
                    self.signature_metadata,
                    module.is_some_and(|module| self.signature_only_modules.contains(module)),
                    class_is_abi,
                    self.imported_collection_aliases.get(name),
                    std_module.map(|m| (&m.module, m.span)),
                    collection,
                ]))
            }
            _ => unreachable!("non-signature input"),
        }
    }

    pub(super) fn record_signature_type(&mut self, ty: &Type, span: Span) {
        let Some(reads) = &self.signature_reads else {
            return;
        };
        // Concrete user templates are expanded before the query-backed final
        // pass. A direct checker using queries during expansion must not retain
        // a result without its queued instantiations.
        if let Type::Generic(name, _) = ty
            && self
                .generics
                .classes
                .get(name)
                .is_some_and(|t| t.abi_storage.is_none())
        {
            self.signature_reads = None;
            return;
        }
        let (Type::Named(name) | Type::Generic(name, _)) = ty else {
            return;
        };
        let key = CheckInput::SignatureType(name.clone(), span.file_id);
        if !reads.contains_key(&key) {
            let value = self.signature_input(&key);
            self.signature_reads.as_mut().unwrap().insert(key, value);
        }
    }

    pub(super) fn normalized_signature(&mut self, f: &FunctionDecl) -> FuncInfo {
        if let Some(cache) = self.prechecks.clone()
            && !super::precheck_journal::is_active()
        {
            let syntax = json!([
                f.name,
                f.params,
                f.return_type,
                f.public,
                f.is_async,
                f.constant,
                f.rust_bridge
            ]);
            return self.precheck_operation(
                format!("signature:{:?}", f.body.id),
                f.body.id,
                syntax,
                |checker| {
                    cache.signatures.set(cache.signatures.get() + 1);
                    checker.normalized_signature(f)
                },
            );
        }
        let syntax = crate::compiler_db::syntax::semantic(&json!([
            f.name,
            f.params,
            f.return_type,
            f.public,
            f.is_async,
            f.constant,
            f.rust_bridge,
        ]));
        let queries = self.body_queries.clone();
        let cached = queries.as_ref().and_then(|q| {
            q.cached_check(f.body.id, CheckKind::Signature, &syntax, |key| {
                self.signature_input(key)
            })
            .ok()
            .flatten()
        });
        if let Some(CheckValue::Signature(info, diagnostics, state)) = cached {
            self.normalized_types.extend(state.normalized);
            self.fully_qualified_collection_types
                .extend(state.qualified_collections);
            self.missing_collection_imports_reported
                .extend(state.missing_collections);
            for diagnostic in diagnostics {
                self.push(diagnostic);
            }
            return *info;
        }
        if let Some(q) = &queries {
            q.signature_checks.set(q.signature_checks.get() + 1);
            self.signature_reads = Some(HashMap::new());
            self.signature_array_before = Some((
                self.fully_qualified_collection_types
                    .contains(Builtin::Array.source_name()),
                self.missing_collection_imports_reported
                    .contains(Builtin::Array.source_name()),
            ));
            self.signature_qualified.clear();
            self.signature_normalized.clear();
        }
        let missing_before = self.missing_collection_imports_reported.clone();
        let capture = queries.as_ref().map(|_| SymbolReadCapture::begin());
        let start = self.errors.len();
        let params = self.normalize_param_types(&f.params);
        let param_infos = self.normalize_param_infos(&f.params);
        let return_type = self.normalize_type(
            &f.return_type,
            f.return_type_span.as_deref().copied().unwrap_or(f.span),
        );
        let info = FuncInfo {
            foreign: f
                .rust_bridge
                .as_ref()
                .is_some_and(|bridge| bridge.nonblocking.is_none()),
            param_infos,
            params,
            return_type,
            public: f.public,
            is_async: f.is_async,
            declaration_span: f.span,
            module_path: None,
            constant: f.constant.clone(),
        };
        let symbols = capture.map(SymbolReadCapture::finish).unwrap_or_default();
        self.signature_array_before = None;
        if let Some(mut reads) = self.signature_reads.take()
            && let Some(q) = &queries
        {
            for read in symbols {
                let key = CheckInput::SignatureSymbol(read);
                let value = self.signature_input(&key);
                reads.insert(key, value);
            }
            q.store_check(
                f.body.id,
                CheckKind::Signature,
                CheckRecord {
                    syntax: std::rc::Rc::new(syntax),
                    inputs: reads.into_iter().collect(),
                    value: CheckValue::Signature(
                        Box::new(info.clone()),
                        self.errors[start..].to_vec(),
                        SignatureState {
                            normalized: std::mem::take(&mut self.signature_normalized)
                                .into_iter()
                                .collect(),
                            qualified_collections: std::mem::take(&mut self.signature_qualified),
                            missing_collections: self
                                .missing_collection_imports_reported
                                .difference(&missing_before)
                                .cloned()
                                .collect(),
                        },
                    ),
                },
            );
        }
        info
    }
}
