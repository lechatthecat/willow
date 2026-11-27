//! Symbol registration, name/import resolution, and interface instantiation/
//! conformance methods for the type checker (extracted from `mod.rs`). The
//! `register_*` entry points stay `pub`; the rest are `pub(super)`.

mod registration;

use crate::qualified_name::QualifiedName;
use crate::semantic::builtin_types::BuiltinTypeId as Builtin;
use std::collections::{HashMap, HashSet};

use crate::diagnostics::{Diagnostic, ErrorCode, FixSuggestion, Label, Severity, Span};
use crate::parser::ast::*;
use crate::semantic::symbols::*;

use super::*;

/// The inputs of one `Type::method(..)` call, shared by its resolvers.
#[derive(Clone, Copy)]
struct StaticCall<'a> {
    id: ExprId,
    class_name: &'a str,
    type_args: &'a [Type],
    method_name: &'a str,
    args: &'a [CallArg],
    span: Span,
    method_span: Span,
    context: CallContext<'a>,
}

#[cfg(test)]
thread_local! {
    static ENUM_IDENTITY_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Qualify a type's module-LOCAL declared type names to `module::Type`, leaving
/// builtin generics (Array/Map/Result/Option/Channel/Future/...) and primitives
/// untouched. Used so a module function signature that references one of its own
/// types resolves in the importing file.
pub(crate) fn qualify_local_type(
    ty: &Type,
    module: &str,
    local: &std::collections::HashSet<String>,
) -> Type {
    ty.map_names(|name| {
        if !QualifiedName::new(&name).is_qualified()
            && local.contains(name)
            && !crate::parser::tuples::is_tuple(name)
        {
            QualifiedName::join(module, name)
        } else {
            name.clone()
        }
    })
}

/// Replace type names a module reached through its OWN item imports with the
/// identity they name. `import signal::Level as Grade;` lets a module write
/// `Grade` in a public signature, and the importing file has no `Grade` at all;
/// only the enum's build-wide identity is a name both units answer to. Complements
/// [`qualify_local_type`], which does the same job
/// for the names a module DECLARES.
fn rename_imported_type(ty: &Type, renames: &HashMap<String, String>) -> Type {
    ty.map_names(|name| renames.get(name).cloned().unwrap_or_else(|| name.clone()))
}

/// Translate a module-qualified path out of the spelling the module that WROTE
/// it uses and into the spelling this checker registered that module under.
///
/// `ledger` may export `biz::Amount` because it wrote `import sales as biz;`,
/// while the checker registered `sales` under whatever spelling reached it
/// first -- `market`, say, because the entry file aliased it. `biz` names
/// nothing here, and the two spellings made one class into two types. Only the leading segment
/// is rewritten: what follows it is the
/// type's own name, which every unit spells the same way.
fn rename_module_prefix(ty: &Type, prefixes: &HashMap<String, String>) -> Type {
    ty.map_names(|name| match QualifiedName::new(&name).split_first() {
        Some((module, item)) => prefixes
            .get(module)
            .map(|registered| QualifiedName::join(registered, item))
            .unwrap_or_else(|| name.clone()),
        None => name.clone(),
    })
}

/// How much of a module one registration brings into a checker.
enum ModuleScope {
    /// The unit imported it: its functions, its types, and its access name,
    /// which local declarations may then not shadow.
    Imported,
    /// The unit only reaches it THROUGH another module's public signature. Its
    /// types are registered so those signatures resolve; nothing else is.
    ///
    Types,
}

/// The AST type a std signature entry denotes.
///
/// The schema is recursive (shared compiler infrastructure, catalog item 11), so this is a thin
/// wrapper over [`StdType::to_ast_type`] rather than the per-combination match
/// it used to be. `Printable` is the only schema type without an AST type; it
/// reaches this function only if a non-`std::io` signature starts using it,
/// which the schema's own tests forbid.
fn std_schema_type(ty: crate::stdlib_schema::StdType) -> Type {
    ty.to_ast_type().unwrap_or_else(|| {
        unreachable!("polymorphic printable types are handled by std::io resolution")
    })
}

fn imported_enum_info(
    decl: &EnumDecl,
    canonical: &str,
    qualify: &dyn Fn(&Type) -> Type,
) -> EnumInfo {
    let mut variant_infos = Vec::new();
    for (tag, variant) in decl.variants.iter().enumerate() {
        variant_infos.push(EnumVariantInfo {
            name: variant.name.clone(),
            payload_types: variant
                .payload
                .iter()
                .map(|ty| {
                    // Imported metadata must preserve the same parameter
                    // bindings as the declaring module's normalization.
                    ty.map_names(|name| {
                        if crate::parser::tuples::is_tuple(&decl.name)
                            || decl.type_params.contains(name)
                        {
                            return name.clone();
                        }
                        let Type::Named(ref qualified) = qualify(&Type::Named(name.clone())) else {
                            unreachable!("module qualification preserves named types");
                        };
                        qualified.clone()
                    })
                })
                .collect(),
            tag: tag as i64,
            declaration_span: variant.span,
        });
    }
    EnumInfo {
        name: if crate::parser::tuples::is_tuple(&decl.name) {
            decl.name.clone()
        } else {
            QualifiedName::join(canonical, &decl.name)
        },
        public: decl.public,
        type_params: decl.type_params.clone(),
        variants: variant_infos,
        declaration_span: decl.span,
    }
}

#[willow_continuations::checker]
impl TypeChecker {
    /// Whether `method` of an interface value `obj_ty` names `Self` where
    /// the value's unknown class matters, reporting it: no argument is known
    /// to be that class, and a result that only contains `Self` would carry
    /// the class object unconverted. A whole `-> Self` result is rebuilt as
    /// the interface by the backend, and an interface default's own `self`
    /// is the opaque receiver, so neither is rejected.
    fn unknown_self_error(
        &self,
        iface: &InterfaceInfo,
        method: &str,
        obj_ty: &Type,
        span: Span,
    ) -> Option<Diagnostic> {
        let m = iface.methods.get(method)?;
        let is_self = |n: &str| n == "Self";
        let position = if m
            .params
            .iter()
            .any(|ty| super::generics::mentions(ty, is_self))
        {
            "a parameter"
        } else if m.return_type != Type::Named("Self".into())
            && super::generics::mentions(&m.return_type, is_self)
        {
            "its result"
        } else {
            return None;
        };
        Some(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0201,
                format!(
                    "method `{method}` names `Self` in {position}, so it cannot be called through interface type `{}`",
                    self.diagnostic_type_name(obj_ty)
                ),
            )
            .with_label(Label::primary(span, "the implementing class is unknown here"))
            .with_help("call it on a value of the implementing class"),
        )
    }

    /// Substitute an interface's generic type parameters with `type_args` and
    /// `Self` with `class_name`, yielding concrete required method signatures
    /// for conformance checking.
    pub(super) fn instantiate_interface(
        &self,
        iface: &InterfaceInfo,
        type_args: &[Type],
        self_ty: &Type,
    ) -> InterfaceInfo {
        let mut param_map: HashMap<String, Type> = iface
            .type_params
            .iter()
            .cloned()
            .zip(type_args.iter().cloned())
            .collect();
        // `Self` resolves to the receiver type: the implementing class during
        // conformance checking, or the (possibly generic) interface instantiation
        // when a method is called on an interface-typed value so `-> Self` keeps
        // its type arguments (`Box<i64>`, not bare `Box`).
        param_map.insert("Self".to_string(), self_ty.clone());
        if param_map.is_empty() {
            return iface.clone();
        }
        let methods = iface
            .methods
            .iter()
            .map(|(k, m)| {
                (
                    k.clone(),
                    InterfaceMethodInfo {
                        effect_contracts: m.effect_contracts.clone(),
                        name: m.name.clone(),
                        params: m
                            .params
                            .iter()
                            .map(|t| crate::semantic::symbols::substitute_type(t, &param_map))
                            .collect(),
                        // Substituting type arguments never changes how a
                        // parameter is PASSED, so the modes carry through.
                        param_infos: m
                            .param_infos
                            .iter()
                            .map(|p| ParamInfo {
                                ty: crate::semantic::symbols::substitute_type(&p.ty, &param_map),
                                mode: p.mode.clone(),
                                sending: p.sending,
                                span: p.span,
                                type_span: p.type_span,
                            })
                            .collect(),
                        is_static: m.is_static,
                        return_type: crate::semantic::symbols::substitute_type(
                            &m.return_type,
                            &param_map,
                        ),
                        declaration_span: m.declaration_span,
                    },
                )
            })
            .collect();
        InterfaceInfo {
            methods,
            ..iface.clone()
        }
    }

    pub(super) fn resolve_field(
        &mut self,
        obj_ty: &Type,
        field_name: &str,
        span: Span,
        check_visibility: bool,
        role: &str,
    ) -> Type {
        if Self::contains_error_type(obj_ty) {
            return Self::error_type();
        }
        // `Range<i64>` exposes its bounds as read-only `.start` / `.end` (i64).
        if is_i64_range_type(obj_ty) {
            if field_name == "start" || field_name == "end" {
                return Type::I64;
            }
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!("`Range<i64>` has no field `{field_name}`"),
                )
                .with_label(Label::primary(span, "unknown range field"))
                .with_help("ranges expose `.start` and `.end`"),
            );
            return Type::Void;
        }
        if crate::semantic::builtin_types::unary_arg(
            obj_ty,
            crate::semantic::builtin_types::BuiltinTypeId::Option,
        )
        .is_some()
        {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "cannot access field `{field_name}` on `{}` without handling absence",
                        self.diagnostic_type_name(obj_ty)
                    ),
                )
                .with_label(Label::primary(span, "this Option may be `None`"))
                .with_help("use `match`, `.unwrap()`, or `.expect(...)` to obtain the inner value"),
            );
            return Type::Void;
        }
        let class_name = match obj_ty {
            Type::Named(n) => n.clone(),
            _ => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("type `{}` has no fields", self.diagnostic_type_name(obj_ty)),
                    )
                    .with_label(Label::primary(span, "field access on non-class type")),
                );
                return Type::Void;
            }
        };
        // An interface default's receiver is any implementing class, so it
        // has only the interface's methods.
        if class_name == "Self"
            && let Some(interface) = &self.local.self_interface
        {
            let interface = self.diagnostic_type_name(interface);
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0502,
                    format!("no field `{field_name}` on `Self` in a default of `{interface}`"),
                )
                .with_label(Label::primary(span, "fields are not part of an interface"))
                .with_help(format!(
                    "declare an accessor method in `{interface}` and call it on `self`"
                )),
            );
            return Type::Void;
        }
        if self.symbols.lookup_class(&class_name).is_none() {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0350,
                    format!(
                        "class `{}` not found",
                        self.diagnostic_class_name(&class_name)
                    ),
                )
                .with_label(Label::primary(span, "unknown class")),
            );
            return Type::Void;
        }
        match self.lookup_field_in_hierarchy(&class_name, field_name) {
            None => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0502,
                        format!(
                            "no field `{}` on class `{}`",
                            field_name,
                            self.diagnostic_class_name(&class_name)
                        ),
                    )
                    .with_label(Label::primary(span, "field not found")),
                );
                Type::Void
            }
            Some((owner, fi)) => {
                self.record_member_use(
                    field_name,
                    "field",
                    span,
                    fi.declaration_span,
                    fi.ty.clone(),
                    role,
                );
                if check_visibility && !fi.public {
                    if fi.protected {
                        if !self.can_access_protected_member(&owner) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0503,
                                    format!("field `{}` of class `{}` is protected", field_name, self.diagnostic_class_name(&owner)),
                                )
                                .with_label(Label::primary(span, "protected field"))
                                .with_label(Label::secondary(fi.declaration_span, "field defined here"))
                                .with_help(format!(
                                    "prot members are accessible only within `{}` and its subclasses",
                                    self.diagnostic_class_name(&owner)
                                )),
                            );
                        }
                    } else if !self.can_access_private_member(&owner) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0501,
                                format!(
                                    "field `{}` of class `{}` is private",
                                    field_name,
                                    self.diagnostic_class_name(&owner)
                                ),
                            )
                            .with_label(Label::primary(span, "private field"))
                            .with_label(Label::secondary(fi.declaration_span, "field defined here"))
                            .with_help(format!(
                                "private members are accessible only within their declaring class, even in the same module; expose it using `pub {}: {}` or provide a public getter method",
                                field_name,
                                self.diagnostic_type_name(&fi.ty)
                            )),
                        );
                    }
                }
                // Writes need the same concrete expected type as reads and
                // call parameters during generic discovery.
                if let Type::Generic(name, _) = &fi.ty
                    && self.generics.classes.contains_key(name)
                {
                    self.normalize_type(&fi.ty, span)
                } else {
                    fi.ty.clone()
                }
            }
        }
    }

    /// Label the already-checked receiver without rechecking or walking its expression tree.
    pub(super) fn label_receiver_call_origin(
        &self,
        diagnostic: &mut Diagnostic,
        receiver: &Expr,
        ty: &Type,
    ) {
        if matches!(
            receiver,
            Expr::Call(_) | Expr::MethodCall(_) | Expr::StaticCall(_)
        ) {
            diagnostic.labels.push(Label::secondary(
                receiver.span(),
                format!(
                    "this call returns `{}`, the receiver type",
                    self.diagnostic_type_name(ty)
                ),
            ));
        }
    }

    /// Inspect std and user declarations without checking a body to choose a diagnostic.
    pub(super) fn type_declares_method(&self, ty: &Type, method: &str) -> bool {
        let name = match ty {
            Type::Named(name) | Type::Generic(name, _) => name.as_str(),
            Type::Array(_) | Type::FixedArray(..) => {
                if let Some(owner) = self.array_class(ty) {
                    return self.resolved_method_class(owner, method).is_some();
                }
                "$std::collections::Array"
            }
            _ => return false,
        };
        if let Some(interface) = self.symbols.lookup_interface(name) {
            return interface.methods.contains_key(method);
        }
        if self.resolved_method_class(name, method).is_some() {
            return true;
        }
        self.template_has_instance_method(name, method)
    }

    pub(super) fn resolve_method(
        &mut self,
        obj_ty: &Type,
        call: &MethodCallExpr,
        method_name: &str,
        virtual_dispatch: bool,
        builtin: Option<(MethodName, Option<ResolvedMethod>)>,
    ) -> Type {
        let args = call.args.as_slice();
        let classified = builtin.as_ref().map(|(name, _)| *name);
        let retired_wait = classified.is_some_and(MethodName::is_retired_wait);
        let span = call.span;
        let receiver = &call.object;
        let abi_debug = matches!(builtin, Some((MethodName::AbiDebug, _)));
        if abi_debug
            && self.array_abi_files.contains(&span.file_id)
            && matches!(
                obj_ty,
                Type::I8
                    | Type::I16
                    | Type::I32
                    | Type::U16
                    | Type::U32
                    | Type::U64
                    | Type::Char
                    | Type::U8
                    | Type::I64
                    | Type::F64
                    | Type::Bool
                    | Type::String
            )
        {
            self.check_call_argument_count(method_name, 0, args.len(), span);
            return Type::String;
        }
        if let Some((_, Some(sig))) = builtin {
            use Intrinsic::*;
            let params: &[Type] = match sig.intrinsic {
                StringSubstring => &[Type::I64, Type::I64],
                StringRepeat | StringByteAt => &[Type::I64],
                StringReplace => &[Type::String, Type::String],
                StringSplit | StringContains | StringFind | StringStartsWith | StringEndsWith => {
                    &[Type::String]
                }
                I64WrappingAdd | I64WrappingSub | I64WrappingMul | I64CheckedAdd
                | I64CheckedSub | I64CheckedMul => std::slice::from_ref(obj_ty),
                _ => &[],
            };
            // Scalar conversions and debug formatting do not check extra arguments.
            match sig.intrinsic {
                I64ToString | F64ToString | BoolToString | StringToString | ScalarToString
                    if !abi_debug =>
                {
                    self.check_builtin_arity(
                        BuiltinArity::new(0, ArityText::Display).label("unexpected arguments"),
                        "primitive",
                        method_name,
                        args.len(),
                        span,
                    );
                    return Type::String;
                }
                IntegerConvert | IntegerWrappingConvert | IntegerCheckedConvert | ScalarParse => {
                    self.check_call_argument_count(method_name, 0, args.len(), span);
                    return sig.return_type(|_| None);
                }
                _ if !abi_debug => {
                    self.check_call_argument_count(method_name, params.len(), args.len(), span);
                    self.check_value_call_args(params, args);
                    return sig.return_type(|_| None);
                }
                _ => {}
            }
        }
        if let Some(inner) = crate::semantic::builtin_types::unary_arg(
            obj_ty,
            crate::semantic::builtin_types::BuiltinTypeId::Option,
        ) {
            // A missing Option member can still be available on its payload.
            let payload_has_method = !matches!(
                crate::semantic::intrinsics::MethodName::classify(method_name),
                crate::semantic::intrinsics::MethodName::AbiConcat
                    | crate::semantic::intrinsics::MethodName::AbiCopy
                    | crate::semantic::intrinsics::MethodName::AbiDebug
                    | crate::semantic::intrinsics::MethodName::AbiLen
                    | crate::semantic::intrinsics::MethodName::AbiResize
            ) && method_name != "init"
                && {
                    let name = classified.unwrap_or(call.method.kind());
                    name.has_method_on(inner) || self.type_declares_method(inner, method_name)
                };
            let mut diagnostic = if payload_has_method {
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "cannot call method `{method_name}` on `{}` without handling absence",
                        self.diagnostic_type_name(obj_ty)
                    ),
                )
                .with_label(Label::primary(span, "this Option may be `None`"))
                .with_help("use `match`, `.unwrap()`, or `.expect(...)` to obtain the inner value")
            } else {
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "no method `{method_name}` on type `{}`",
                        self.diagnostic_type_name(obj_ty)
                    ),
                )
                .with_label(Label::primary(span, "unknown method"));
                if let Some(suggestion) =
                    suggest_similar_name(method_name, AdtMethod::OPTION_NAMES.iter())
                {
                    diagnostic = diagnostic.with_help(format!(
                        "there is a method with a similar name: `{suggestion}`"
                    ));
                }
                diagnostic
            };
            self.label_receiver_call_origin(&mut diagnostic, receiver, obj_ty);
            self.report_missing_method_with_args(diagnostic, args, retired_wait);
            return Self::error_type();
        }
        // A receiver typed as a generic interface instantiation (`Box<String>`):
        // resolve against the interface with its type parameters substituted, so
        // `fn get(self) -> T` reports `String` here.
        if let Type::Generic(name, type_args) = obj_ty
            && let Some(iface) = self.symbols.lookup_interface(name).cloned()
        {
            if let Some(error) = self.unknown_self_error(&iface, method_name, obj_ty, span) {
                if retired_wait {
                    self.check_discarded_builtin_args(args);
                }
                self.push(error);
                return Self::error_type();
            }
            // `Self` in a method called through the interface is the full
            // receiver instantiation (`Box<i64>`), not bare `Box`.
            let instantiated = self.instantiate_interface(&iface, type_args, obj_ty);
            return self.resolve_interface_method(
                &instantiated,
                method_name,
                args,
                span,
                retired_wait,
            );
        }
        // An interface default's opaque receiver reaches its interface's
        // methods, whose `Self` stays the receiver.
        if matches!(obj_ty, Type::Named(n) if n == "Self")
            && let Some((name, type_args)) = match &self.local.self_interface {
                Some(Type::Named(name)) => Some((name.clone(), Vec::new())),
                Some(Type::Generic(name, args)) => Some((name.clone(), args.clone())),
                _ => None,
            }
            && let Some(iface) = self.symbols.lookup_interface(&name).cloned()
        {
            let iface = if type_args.is_empty() {
                iface
            } else {
                self.instantiate_interface(&iface, &type_args, obj_ty)
            };
            return self.resolve_interface_method(&iface, method_name, args, span, retired_wait);
        }
        let class_name = match obj_ty {
            Type::Named(n) => n.clone(),
            _ => {
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "no method `{method_name}` on type `{}`",
                        self.diagnostic_type_name(obj_ty)
                    ),
                )
                .with_label(Label::primary(span, "unknown method"));
                let methods = match obj_ty {
                    Type::String => {
                        "len, byte_at, trim, substring, slice, repeat, split, contains, find, starts_with, ends_with, replace, trim_start, trim_end, to_lower, to_upper, to_string"
                    }
                    Type::I64 => {
                        "wrapping_add, wrapping_sub, wrapping_mul, wrapping_neg, checked_add, checked_sub, checked_mul, checked_neg, wrapping_to_u8, checked_to_u8, to_string"
                    }
                    Type::U8 => {
                        "wrapping_add, wrapping_sub, wrapping_mul, wrapping_neg, checked_add, checked_sub, checked_mul, checked_neg, to_i64, to_string"
                    }
                    Type::F64 | Type::Bool => "to_string",
                    _ => "",
                };
                if let Some(help) = primitive_function_help(obj_ty, receiver, method_name, args) {
                    diagnostic = diagnostic.with_help(help);
                }
                if !methods.is_empty() {
                    diagnostic = diagnostic.with_help(format!("available methods: {methods}"));
                }
                self.label_receiver_call_origin(&mut diagnostic, receiver, obj_ty);
                if retired_wait {
                    self.check_discarded_builtin_args(args);
                }
                self.push(diagnostic);
                return Type::Void;
            }
        };
        // Interface-typed receiver: only the interface's declared methods are
        // callable, and the call dispatches through the interface vtable.
        if let Some(mut iface) = self.symbols.lookup_interface(&class_name).cloned() {
            if let Some(error) = self.unknown_self_error(&iface, method_name, obj_ty, span) {
                if retired_wait {
                    self.check_discarded_builtin_args(args);
                }
                self.push(error);
                return Self::error_type();
            }
            // Dispatch returns the concrete object for `-> Self`; the backend
            // rebuilds the receiver's interface pair. Only specialize the
            // selected return, without traversing every interface signature or
            // broadening the rules for concrete `Self` parameters.
            if let Some(method) = iface.methods.get_mut(method_name)
                && method.return_type == Type::Named("Self".into())
            {
                method.return_type = obj_ty.clone();
            }
            return self.resolve_interface_method(&iface, method_name, args, span, retired_wait);
        }
        if self.symbols.lookup_class(&class_name).is_none() {
            if retired_wait {
                self.check_discarded_builtin_args(args);
            }
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0350,
                    format!(
                        "class `{}` not found",
                        self.diagnostic_class_name(&class_name)
                    ),
                )
                .with_label(Label::primary(span, "unknown class")),
            );
            return Type::Void;
        }
        // Constructors are not directly callable as `obj.init(...)`.
        if method_name == "init" {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0843,
                    "constructor `init` can only be called with `new`",
                )
                .with_label(Label::primary(span, "`init` called directly"))
                .with_help(format!("write `new {}(...)` instead", class_name)),
            );
            return Type::Void;
        }
        match self.lookup_method_in_hierarchy(&class_name, method_name) {
            None => {
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0502,
                    format!(
                        "no method `{}` on class `{}`",
                        method_name,
                        self.diagnostic_class_name(&class_name)
                    ),
                )
                .with_label(Label::primary(span, "method not found"));

                if let Some((_, field)) = self.lookup_field_in_hierarchy(&class_name, method_name)
                    && matches!(field.ty, Type::Closure(..) | Type::Fn(..))
                {
                    diagnostic = diagnostic
                        .with_label(Label::secondary(
                            field.declaration_span,
                            "callable field declared here",
                        ))
                        .with_help(callable_field_help(receiver, method_name, args));
                    self.push(diagnostic);
                    return Self::error_type();
                }

                let method_names = self.method_names_in_hierarchy(&class_name);
                if let Some(suggestion) = suggest_similar_name(method_name, method_names.iter()) {
                    diagnostic = diagnostic
                        .with_help(format!(
                            "there is a method with a similar name: `{}`",
                            suggestion
                        ))
                        .with_fix(FixSuggestion::new(
                            span,
                            suggestion,
                            "replace with suggested method",
                        ));
                }

                self.push(diagnostic);
                Type::Void
            }
            Some((owner, mi)) => {
                self.request_class_method(&owner, method_name);
                self.record_typed_method_use(
                    method_name,
                    "method",
                    span,
                    mi.declaration_span,
                    args,
                    Some((&mi.params, &mi.return_type)),
                );
                if mi.is_static {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0834,
                            format!(
                                "static method `{}::{}` cannot be called through an instance",
                                owner, method_name
                            ),
                        )
                        .with_label(Label::primary(span, "static method called with `.`"))
                        .with_help(format!("write `{}::{}` instead", owner, method_name)),
                    );
                    return method_call_return_type(&mi);
                }
                if !mi.public {
                    if mi.protected {
                        if !self.can_access_protected_member(&owner) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0503,
                                    format!("method `{}` of class `{}` is protected", method_name, self.diagnostic_class_name(&owner)),
                                )
                                .with_label(Label::primary(span, "protected method"))
                                .with_label(Label::secondary(mi.declaration_span, "method defined here"))
                                .with_help(format!(
                                    "prot members are accessible only within `{}` and its subclasses",
                                    self.diagnostic_class_name(&owner)
                                )),
                            );
                        }
                    } else if !self.can_access_private_member(&owner) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0501,
                                format!(
                                    "method `{}` of class `{}` is private",
                                    method_name,
                                    self.diagnostic_class_name(&owner)
                                ),
                            )
                            .with_label(Label::primary(span, "private method"))
                            .with_label(Label::secondary(
                                mi.declaration_span,
                                "method defined here",
                            ))
                            .with_help(format!("private members are accessible only within their declaring class, even in the same module; make it public with `pub fn {}`", method_name)),
                        );
                    }
                }
                let checked = retired_wait && !virtual_dispatch;
                if checked {
                    self.check_discarded_builtin_args(args);
                }
                if let Some(arity) =
                    Self::method_arity_error(method_name, mi.params.len(), args.len(), span)
                {
                    self.push(arity);
                }
                self.check_call_args_against_param_infos(&mi.param_infos, args, checked);
                let retains = mi.is_async
                    || (virtual_dispatch
                        && matches!(&mi.return_type, Type::Generic(name, _) if Builtin::from_name(name) == Some(Builtin::Task))
                        && self
                            .symbols
                            .dispatch_may_start_task(&class_name, method_name));
                self.check_async_call_captures(&mi.param_infos, args, retains);
                method_call_return_type(&mi)
            }
        }
    }

    fn method_arity_error(
        name: &str,
        expected: usize,
        supplied: usize,
        span: Span,
    ) -> Option<Diagnostic> {
        (expected != supplied).then(|| {
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0201,
                format!(
                    "method `{name}` takes {expected} argument(s) but {supplied} were supplied"
                ),
            )
            .with_label(Label::primary(span, "wrong number of arguments"))
        })
    }

    /// Resolve a method call on an interface-typed receiver. Only methods declared
    /// by the interface are callable; the return type is the interface method's.
    pub(super) fn resolve_interface_method(
        &mut self,
        iface: &InterfaceInfo,
        method_name: &str,
        args: &[CallArg],
        span: Span,
        retired_wait: bool,
    ) -> Type {
        let Some(m) = iface.methods.get(method_name) else {
            let mut diag = Diagnostic::new(
                Severity::Error,
                ErrorCode::E0418,
                format!("no method `{}` on interface `{}`", method_name, iface.name),
            )
            .with_label(Label::primary(
                span,
                "method not declared by this interface",
            ));
            if let Some(suggestion) = suggest_similar_name(method_name, iface.methods.keys()) {
                diag = diag.with_help(format!("the interface declares a method `{suggestion}`"));
            } else {
                diag = diag
                    .with_help("only methods declared in the interface are callable on its values");
            }
            self.report_missing_method_with_args(diag, args, retired_wait);
            return Type::Void;
        };

        self.record_typed_method_use(
            method_name,
            "method",
            span,
            m.declaration_span,
            args,
            Some((&m.params, &m.return_type)),
        );
        if retired_wait {
            self.check_discarded_builtin_args(args);
        }
        if let Some(arity) = Self::method_arity_error(method_name, m.params.len(), args.len(), span)
        {
            self.push(arity);
        }
        for (idx, arg) in args.iter().enumerate() {
            // A `&`/`&mut` parameter is passed as a pointer, so an interface
            // call has to obey the same argument-mode rules as a direct call:
            // without this, `m.set(x)` on `fn set(self, value: &mut i64)` type
            // checks and then hands the callee an integer to dereference. The shared checker
            // also validates the place and
            // its mutability, and reports `&` on a by-value parameter.
            if let Some(param) = m.param_infos.get(idx)
                && (matches!(param.mode, ParamMode::Reference { .. })
                    || matches!(arg.mode, CallArgMode::Reference { .. }))
            {
                self.check_call_arg_against_param(param, arg, retired_wait);
                continue;
            }
            let arg_ty = match m.params.get(idx) {
                Some(param_ty) => self.check_argument_expecting(&arg.expr, param_ty, retired_wait),
                None if retired_wait => continue,
                None => self.check_expr(&arg.expr),
            };
            if let Some(param_ty) = m.params.get(idx)
                && !self.types_compatible(param_ty, &arg_ty)
            {
                self.push(
                    self.with_callable_kind_help(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(param_ty, &arg_ty),
                            format!(
                                "argument {} of `{}` expects `{}`, found `{}`",
                                idx + 1,
                                method_name,
                                self.diagnostic_type_name(param_ty),
                                self.diagnostic_type_name(&arg_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            arg.expr.span(),
                            format!("expected `{}`", self.diagnostic_type_name(param_ty)),
                        )),
                        param_ty,
                        &arg_ty,
                    ),
                );
            }
        }
        // Two `&mut` arguments naming the same place alias through the call,
        // exactly as they would on a direct call.
        self.check_mut_reference_aliases(&m.param_infos, args);
        let retains = matches!(&m.return_type, Type::Generic(name, _) if Builtin::from_name(name) == Some(Builtin::Task))
            && self
                .symbols
                .dispatch_may_start_task(&iface.name, method_name);
        self.check_async_call_captures(&m.param_infos, args, retains);
        m.return_type.clone()
    }

    pub(super) fn resolve_static_call(
        &mut self,
        class_name: &str,
        type_args: &[Type],
        method_name: &str,
        args: &[CallArg],
        spans: (ExprId, Span, Span),
        context: CallContext<'_>,
    ) -> Type {
        let (id, span, method_span) = spans;
        let references = self.analysis_symbols.references.len();
        let errors = self.error_generation;
        let result = self.resolve_static_call_inner(&StaticCall {
            id,
            class_name,
            type_args,
            method_name,
            args,
            span,
            method_span,
            context,
        });
        if self.capture_call_sites
            && self.error_generation == errors
            && !Self::contains_error_type(&result)
        {
            let owner = self.resolve_static_call_class_quiet(class_name);
            let is_variant = owner
                .as_deref()
                .is_some_and(|owner| self.symbols.lookup_enum(owner).is_some());
            let has_source = self
                .member_uses
                .get(&(span.file_id, span.start))
                .is_some_and(|&index| index >= references);
            if !is_variant && !has_source {
                self.record_builtin_static_call(method_name, span, method_span, args, &result);
            }
        }
        result
    }

    fn resolve_static_call_inner(&mut self, call: &StaticCall<'_>) -> Type {
        let StaticCall {
            class_name,
            type_args,
            method_name,
            args,
            span,
            ..
        } = *call;
        if let Some(ty) =
            self.resolve_fully_qualified_std_module_call(class_name, method_name, args, span)
        {
            return ty;
        }

        let Some(resolved_class_name) = self.resolve_static_call_class_name(class_name, span)
        else {
            return Type::Void;
        };
        let class_name = resolved_class_name.as_str();
        // Generic expansion already diagnosed this receiver. No signature can
        // provide argument context, but independent argument errors still matter.
        if Self::is_error_name(class_name) {
            for arg in args {
                self.check_expr(&arg.expr);
            }
            return Self::error_type();
        }
        // Reject a static call on a private module type from another module.
        self.check_type_visibility(class_name, span);
        // Constructors are not directly callable as `Type::init(...)` — use `new`.
        // Check args first so their errors still surface.
        if method_name == "init" && self.symbols.lookup_class(class_name).is_some() {
            for arg in args {
                self.check_expr(&arg.expr);
            }
            self.push(
                Self::error_at(
                    ErrorCode::E0843,
                    "constructor `init` can only be called with `new`",
                    span,
                    "`init` called directly",
                )
                .with_help(format!("write `new {}(...)` instead", class_name)),
            );
            return Type::Named(class_name.to_string());
        }
        let type_args = type_args
            .iter()
            .map(|ty| {
                let ty = self.normalize_type(ty, span);
                // Static constructor arguments are parsed individually, unlike
                // `Channel<fn(...)>` annotations. Give both routes the same
                // Send-required callable contract after resolving the owner.
                if SEND_ONLY_BUILTINS.contains(&class_name) {
                    ty.with_callable_kind(true)
                } else {
                    ty
                }
            })
            .collect::<Vec<_>>();
        let call = StaticCall {
            class_name,
            type_args: &type_args,
            ..*call
        };

        let builtin_class = Builtin::from_name(class_name);
        // Built-in `Map<K, V>` constructor. The type parameters are resolved
        // from the binding's annotation (Map<Void, Void> is a placeholder, like
        // an empty array literal).
        if builtin_class == Some(Builtin::Map) && method_name == "new" {
            return self.resolve_map_new(&call);
        }

        // An enum receiver constructs a variant. Every enum path returns here.
        if let Some(enum_info) = self.symbols.lookup_enum(class_name).cloned() {
            return if enum_info.type_params.is_empty() {
                self.resolve_enum_variant_call(&call, &enum_info)
            } else {
                self.resolve_generic_enum_variant_call(&call, &enum_info)
            };
        }

        if method_name == "new" {
            match builtin_class {
                Some(
                    Builtin::Mutex
                    | Builtin::RwLock
                    | Builtin::BlockingCell
                    | Builtin::BlockingRwCell,
                ) => {
                    return self.resolve_lock_new(&call);
                }
                Some(Builtin::AtomicI64 | Builtin::AtomicBool) => {
                    return self.resolve_atomic_new(&call);
                }
                Some(Builtin::CancellationToken | Builtin::TaskScope) => {
                    return self.resolve_no_arg_new(&call);
                }
                _ => {}
            }
        }

        if builtin_class == Some(Builtin::Channel) && method_name == "with_capacity" {
            return self.resolve_channel_with_capacity(&call);
        }

        if builtin_class == Some(Builtin::Channel) && method_name == "new" {
            return self.resolve_channel_new(&call);
        }

        if let Some(entry) =
            crate::semantic::intrinsics::math_namespace_builtin(class_name, method_name)
        {
            return self.resolve_math_namespace_call(&call, entry);
        }

        if let Some(ty) = self.resolve_number_static_call(&call) {
            return ty;
        }

        // Check if `class_name` refers to an imported module (e.g. `math::add`).
        if self.symbols.lookup_module(class_name).is_some() {
            return self.resolve_module_function_call(&call);
        }

        self.resolve_class_static_method_call(&call)
    }

    /// The error for a call whose argument count does not match its signature.
    fn push_wrong_argument_count(&mut self, code: ErrorCode, message: String, span: Span) {
        self.push_error(code, message, span, "wrong number of arguments");
    }

    fn resolve_map_new(&mut self, call: &StaticCall<'_>) -> Type {
        let StaticCall {
            type_args,
            args,
            span,
            ..
        } = *call;
        self.check_collection_type_imported(Builtin::Map.source_name(), span);
        self.check_builtin_arity(
            constructor_arity(Builtin::Map.source_name(), "new"),
            Builtin::Map.source_name(),
            "new",
            args.len(),
            span,
        );
        match type_args {
            [] => Builtin::Map.apply(vec![Type::Void, Type::Void]),
            [key, value] => Builtin::Map.apply(vec![key.clone(), value.clone()]),
            _ => {
                self.push_error(
                    ErrorCode::E0201,
                    format!(
                        "`Map::new` expects 2 type arguments, got {}",
                        type_args.len()
                    ),
                    span,
                    "wrong number of type arguments",
                );
                Builtin::Map.apply(vec![Type::Void, Type::Void])
            }
        }
    }

    /// A variant of a non-generic enum.
    fn resolve_enum_variant_call(&mut self, call: &StaticCall<'_>, enum_info: &EnumInfo) -> Type {
        let StaticCall {
            class_name,
            method_name,
            args,
            span,
            method_span,
            ..
        } = *call;
        let Some(variant) = enum_info.variants.iter().find(|v| v.name == method_name) else {
            self.push_error(
                ErrorCode::E1208,
                format!("no variant `{}` in enum `{}`", method_name, class_name),
                span,
                "unknown enum variant",
            );
            return Type::Named(enum_info.name.clone());
        };
        self.record_variant_use(&enum_info.name, method_name, method_span);
        if variant.payload_types.is_empty() {
            // Fieldless variant: no args expected
            if !args.is_empty() {
                self.push_error(
                    ErrorCode::E0201,
                    format!(
                        "enum variant `{}::{}` takes no arguments, got {}",
                        class_name,
                        method_name,
                        args.len()
                    ),
                    span,
                    "unexpected arguments",
                );
            }
            return Type::Named(enum_info.name.clone());
        }
        // Payload variant: check arg count and types
        let expected = variant.payload_types.len();
        if args.len() != expected {
            self.push_wrong_argument_count(
                ErrorCode::E0201,
                format!(
                    "enum variant `{}::{}` takes {} argument(s), got {}",
                    class_name,
                    method_name,
                    expected,
                    args.len()
                ),
                span,
            );
        }
        for (param_ty, arg) in variant.payload_types.iter().zip(args.iter()) {
            let arg_ty = self.check_expr_expecting(&arg.expr, param_ty);
            if !self.types_compatible(param_ty, &arg_ty) {
                self.push_type_mismatch_with_code(
                    ErrorCode::E0201,
                    param_ty,
                    &arg_ty,
                    arg.expr.span(),
                );
            }
        }
        Type::Named(enum_info.name.clone())
    }

    /// A variant of a generic enum, including Option<T>, Result<T, E> and
    /// tuples defined in the prelude.
    fn resolve_generic_enum_variant_call(
        &mut self,
        call: &StaticCall<'_>,
        enum_info: &EnumInfo,
    ) -> Type {
        let StaticCall {
            class_name,
            type_args,
            method_name,
            args,
            span,
            method_span,
            ..
        } = *call;
        let Some(variant) = enum_info.variants.iter().find(|v| v.name == method_name) else {
            // Unknown variant in generic enum
            let valid: Vec<&str> = enum_info.variants.iter().map(|v| v.name.as_str()).collect();
            self.push_error(
                ErrorCode::E1801,
                format!(
                    "unknown variant `{}` in `{}`; expected one of: {}",
                    method_name,
                    class_name,
                    valid.join(", ")
                ),
                span,
                "unknown variant",
            );
            return Type::Void;
        };
        self.record_variant_use(&enum_info.name, method_name, method_span);
        // Validate arg count.
        if args.len() != variant.payload_types.len() {
            self.push_wrong_argument_count(
                ErrorCode::E0201,
                format!(
                    "`{}::{}` expects {} argument(s), got {}",
                    class_name,
                    method_name,
                    variant.payload_types.len(),
                    args.len()
                ),
                span,
            );
            let void_args = vec![Type::Void; enum_info.type_params.len()];
            return Type::Generic(enum_info.name.clone(), void_args);
        }
        let explicit: Vec<_> = type_args
            .iter()
            .map(|t| self.normalize_type(t, span))
            .collect();
        for ty in &explicit {
            self.validate_type(ty, span);
        }
        let bindings: HashMap<_, _> = enum_info
            .type_params
            .iter()
            .cloned()
            .zip(explicit.iter().cloned())
            .collect();
        let checked_args: Vec<Type> = args
            .iter()
            .zip(&variant.payload_types)
            .map(|(arg, param)| {
                if explicit.is_empty() {
                    self.check_expr(&arg.expr)
                } else {
                    let expected = param.substitute_names(|n| bindings.get(n).cloned());
                    self.check_expr_expecting(&arg.expr, &expected)
                }
            })
            .collect();

        if crate::parser::tuples::is_tuple(&enum_info.name) {
            return Type::Generic(enum_info.name.clone(), checked_args);
        }
        let bindings = match crate::semantic::generic_inference::infer_partial(
            &enum_info.type_params,
            &variant.payload_types,
            &checked_args,
            &explicit,
        ) {
            Ok(bindings) => bindings,
            Err(message) => {
                self.generic_error(span, message);
                return Self::error_type();
            }
        };
        let type_args = enum_info
            .type_params
            .iter()
            .map(|p| bindings[p].clone())
            .collect();
        Type::Generic(enum_info.name.clone(), type_args)
    }

    /// Lock primitives: `Mutex::new(v)` / `RwLock::new(v)` / native
    /// compatibility-cell constructors infer their element type from the
    /// argument, e.g. `Mutex<i64>::new(0)`.
    fn resolve_lock_new(&mut self, call: &StaticCall<'_>) -> Type {
        let StaticCall {
            class_name,
            type_args,
            args,
            span,
            context,
            ..
        } = *call;
        self.reject_reference_args(&QualifiedName::join(class_name, "new"), args);
        if !self.check_builtin_arity(
            constructor_arity(class_name, "new"),
            class_name,
            "new",
            args.len(),
            span,
        ) {
            return Type::Generic(class_name.to_string(), vec![Type::Void]);
        }
        // The written type argument, else the expected lock's element, is
        // the argument's context: `Mutex::new([])` expected as
        // `Mutex<Array<i64>>` types `[]` as `Array<i64>`.
        let element = match (type_args, context.expected) {
            ([t], _) => Some(t),
            ([], Some(Type::Generic(name, elem))) if name == class_name && elem.len() == 1 => {
                Some(&elem[0])
            }
            _ => None,
        };
        let arg_ty = match element {
            Some(element) => self.check_argument_expecting(&args[0].expr, element, context.checked),
            None => self.check_or_reuse_expr(&args[0].expr, context.checked),
        };
        let elem = match type_args {
            [] => arg_ty.clone(),
            [t] => {
                self.check_builtin_argument(
                    BuiltinArgument::LockConstructor,
                    class_name,
                    "new",
                    t,
                    &arg_ty,
                    args[0].expr.span(),
                );
                t.clone()
            }
            _ => {
                self.push_error(
                    ErrorCode::E0201,
                    format!("`{class_name}::new` expects 1 type argument"),
                    span,
                    "too many type arguments",
                );
                arg_ty.clone()
            }
        };
        Type::Generic(class_name.to_string(), vec![elem])
    }

    /// Atomic primitives: `AtomicI64::new(i64)` / `AtomicBool::new(bool)`
    /// return the (non-generic) atomic type.
    fn resolve_atomic_new(&mut self, call: &StaticCall<'_>) -> Type {
        let StaticCall {
            class_name,
            args,
            span,
            ..
        } = *call;
        self.reject_reference_args(&QualifiedName::join(class_name, "new"), args);
        let expected = if Builtin::from_name(class_name) == Some(Builtin::AtomicI64) {
            Type::I64
        } else {
            Type::Bool
        };
        if self.check_builtin_arity(
            constructor_arity(class_name, "new"),
            class_name,
            "new",
            args.len(),
            span,
        ) {
            let arg_ty = self.check_expr(&args[0].expr);
            self.check_builtin_argument(
                BuiltinArgument::Constructor,
                class_name,
                "new",
                &expected,
                &arg_ty,
                args[0].expr.span(),
            );
        }
        Type::Named(class_name.to_string())
    }

    /// `CancellationToken::new()` / `TaskScope::new()`.
    fn resolve_no_arg_new(&mut self, call: &StaticCall<'_>) -> Type {
        let StaticCall {
            class_name,
            type_args,
            args,
            span,
            ..
        } = *call;
        self.reject_reference_args(&QualifiedName::join(class_name, "new"), args);
        if !type_args.is_empty() || !args.is_empty() {
            self.check_discarded_builtin_args(args);
            self.push(constructor_arity(class_name, "new").diagnostic(
                class_name,
                "new",
                args.len(),
                span,
            ));
        }
        Type::Named(class_name.to_string())
    }

    /// Bounded channel: one i64 capacity argument.
    fn resolve_channel_with_capacity(&mut self, call: &StaticCall<'_>) -> Type {
        let StaticCall {
            type_args,
            args,
            span,
            ..
        } = *call;
        self.reject_reference_args("Channel::with_capacity", args);
        if self.check_builtin_arity(
            constructor_arity("Channel", "with_capacity"),
            "Channel",
            "with_capacity",
            args.len(),
            span,
        ) {
            let cap_ty = self.check_expr(&args[0].expr);
            self.check_builtin_argument(
                BuiltinArgument::Capacity,
                "Channel",
                "with_capacity",
                &Type::I64,
                &cap_ty,
                args[0].expr.span(),
            );
        }
        match type_args {
            [] => Type::Generic("Channel".to_string(), vec![Type::Void]),
            [element_ty] => Type::Generic("Channel".to_string(), vec![element_ty.clone()]),
            _ => {
                self.push_error(
                    ErrorCode::E0201,
                    format!(
                        "function `Channel::with_capacity` expects 1 type argument, got {}",
                        type_args.len()
                    ),
                    span,
                    "wrong number of type arguments",
                );
                Type::Generic("Channel".to_string(), vec![Type::Void])
            }
        }
    }

    fn resolve_channel_new(&mut self, call: &StaticCall<'_>) -> Type {
        let StaticCall {
            type_args,
            args,
            span,
            ..
        } = *call;
        self.check_builtin_arity(
            constructor_arity("Channel", "new"),
            "Channel",
            "new",
            args.len(),
            span,
        );
        match type_args {
            [] => Type::Generic("Channel".to_string(), vec![Type::Void]),
            [element_ty] => Type::Generic("Channel".to_string(), vec![element_ty.clone()]),
            _ => {
                self.push_error(
                    ErrorCode::E0201,
                    format!(
                        "function `Channel::new` expects 1 type argument, got {}",
                        type_args.len()
                    ),
                    span,
                    "wrong number of type arguments",
                );
                Type::Void
            }
        }
    }

    /// The type-argument error shared by non-generic builtin functions.
    fn reject_static_call_type_args(&mut self, call: &StaticCall<'_>) {
        if !call.type_args.is_empty() {
            self.push_error(
                ErrorCode::E0201,
                format!(
                    "function `{}::{}` expects no type arguments",
                    call.class_name, call.method_name
                ),
                call.span,
                "unexpected type arguments",
            );
        }
    }

    fn resolve_math_namespace_call(
        &mut self,
        call: &StaticCall<'_>,
        entry: crate::semantic::intrinsics::NamespaceBuiltin,
    ) -> Type {
        let StaticCall {
            class_name,
            method_name,
            args,
            span,
            ..
        } = *call;
        self.reject_static_call_type_args(call);
        self.check_builtin_arity(
            BuiltinArity::new(entry.params.len(), ArityText::FunctionPlural),
            class_name,
            method_name,
            args.len(),
            span,
        );
        let params: Vec<_> = entry.params.iter().map(|ty| ty.to_source()).collect();
        self.check_value_call_args(&params, args);
        entry.ret.to_source()
    }

    /// `f64::from_i64`, `f64::trunc_to_i64`, `f64::to_string` and
    /// `f64::parse` / `i64::parse`; `None` for any other call.
    fn resolve_number_static_call(&mut self, call: &StaticCall<'_>) -> Option<Type> {
        let StaticCall {
            class_name,
            method_name,
            args,
            span,
            ..
        } = *call;
        let (param, ret) = match (class_name, method_name) {
            ("f64", "from_i64") => (Type::I64, Type::F64),
            ("f64", "trunc_to_i64") => (Type::F64, Type::I64),
            ("f64", "to_string") => (Type::F64, Type::String),
            ("f64", "parse") => (
                Type::String,
                Builtin::Result.apply(vec![Type::F64, Type::Named("ParseFloatError".to_string())]),
            ),
            ("i64", "parse") => (
                Type::String,
                Builtin::Result.apply(vec![Type::I64, Type::Named("ParseIntError".to_string())]),
            ),
            _ => return None,
        };
        // Only the conversions have always rejected type arguments.
        if matches!(method_name, "from_i64" | "trunc_to_i64") {
            self.reject_static_call_type_args(call);
        }
        self.check_builtin_arity(
            constructor_arity(class_name, method_name),
            class_name,
            method_name,
            args.len(),
            span,
        );
        self.check_value_call_args(&[param], args);
        Some(ret)
    }

    /// A module-qualified function call, e.g. `math::add(..)`.
    fn resolve_module_function_call(&mut self, call: &StaticCall<'_>) -> Type {
        let StaticCall {
            class_name,
            method_name,
            args,
            span,
            ..
        } = *call;
        let Some(fi) = self
            .symbols
            .lookup_module_func(class_name, method_name)
            .cloned()
        else {
            self.push_error(
                ErrorCode::E0350,
                format!(
                    "function `{}` not found in module `{}`",
                    method_name, class_name
                ),
                span,
                "not found in module",
            );
            return Type::Void;
        };
        if fi.constant.is_some() {
            let qualified = QualifiedName::join(class_name, method_name);
            return self.reject_constant_call(&qualified, &fi, args, span);
        }
        if fi.foreign {
            self.check_foreign_call(span, fi.is_async);
        }
        self.record_method_use(method_name, "function", span, fi.declaration_span, args);
        // `m::f(..)` is a call, unlike a `m::CONST` member read.
        self.mark_last_member_use_call();
        if !fi.public {
            self.push_private_module_function(method_name, &fi, span);
        }
        if args.len() != fi.params.len() {
            self.push_wrong_argument_count(
                ErrorCode::E0203,
                format!(
                    "function `{}::{}` expects {} argument(s), got {}",
                    class_name,
                    method_name,
                    fi.params.len(),
                    args.len()
                ),
                span,
            );
        }
        self.check_call_args_against_param_infos(&fi.param_infos, args, call.context.checked);
        self.check_async_call_captures(&fi.param_infos, args, fi.is_async);
        self.record_sending_call(call.id, &fi, args);
        // A module-qualified call to an async fn yields `Task<T>`,
        // just like a local async call — without this the call site
        // types as the bare `T` and `await` rejects it.
        function_call_return_type(&fi)
    }

    fn push_private_module_function(&mut self, method_name: &str, fi: &FuncInfo, span: Span) {
        let defined_at = fi
            .module_path
            .as_deref()
            .map(|path| {
                format!(
                    "`{}` is defined at {}:{}:{}",
                    method_name, path, fi.declaration_span.line, fi.declaration_span.col
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "`{}` is defined at line {}, column {}",
                    method_name, fi.declaration_span.line, fi.declaration_span.col
                )
            });
        self.push(
            Self::error_at(
                ErrorCode::E0402,
                format!("function `{}` is private", method_name),
                span,
                "private function",
            )
            .with_note(defined_at)
            .with_help(format!("make it public with `pub fn {}`", method_name)),
        );
    }

    /// A static method of a class, e.g. `Point::origin()`.
    fn resolve_class_static_method_call(&mut self, call: &StaticCall<'_>) -> Type {
        let StaticCall {
            class_name,
            method_name,
            args,
            span,
            ..
        } = *call;
        let Some(class) = self.symbols.lookup_class(class_name).cloned() else {
            if PRIMITIVE_TYPE_NAMES.contains(&class_name) {
                self.push_unknown_primitive_static(call);
                return Self::error_type();
            }
            self.push_error(
                ErrorCode::E0350,
                format!("unknown name `{}` (not a module or class)", class_name),
                span,
                "unknown module or class",
            );
            return Type::Void;
        };
        let Some(mi) = class.methods.get(method_name).cloned() else {
            self.push_unknown_static_method(call, &class);
            return Type::Void;
        };
        self.request_class_method(class_name, method_name);
        self.record_method_use(method_name, "method", span, mi.declaration_span, args);
        if !mi.is_static {
            self.push_instance_method_static_call(call);
            return method_call_return_type(&mi);
        }
        if !mi.public {
            self.check_static_method_access(call, &mi);
        }
        if mi.params.len() != args.len() {
            self.push_wrong_argument_count(
                ErrorCode::E0201,
                format!(
                    "function `{}::{}` expects {} argument(s), got {}",
                    class_name,
                    method_name,
                    mi.params.len(),
                    args.len()
                ),
                span,
            );
        }
        self.check_call_args_against_param_infos(&mi.param_infos, args, call.context.checked);
        self.check_async_call_captures(&mi.param_infos, args, mi.is_async);
        method_call_return_type(&mi)
    }

    /// `f64::floorr(x)`: the type exists, the member does not. Suggest a near
    /// static function, or a free function such as `powf` when that is what
    /// the call meant.
    fn push_unknown_primitive_static(&mut self, call: &StaticCall<'_>) {
        let StaticCall {
            class_name,
            method_name,
            args,
            span,
            ..
        } = *call;
        let statics: Vec<&str> = PRIMITIVE_STATICS
            .iter()
            .filter(|(owner, ..)| *owner == class_name)
            .map(|(_, name, _)| *name)
            .collect();
        let mut diagnostic = Self::error_at(
            ErrorCode::E0502,
            format!("no static function `{method_name}` on `{class_name}`"),
            span,
            "unknown static function",
        );
        // `pow` and `powf` take `f64` arguments.
        let float = class_name == "f64";
        if let Some(near) = suggest_similar_name(method_name, statics.iter()) {
            diagnostic = diagnostic.with_help(format!("did you mean `{class_name}::{near}`?"));
        } else if float
            && let Some(free) = suggest_similar_name(
                method_name,
                crate::semantic::intrinsics::ADDRESSABLE_BUILTINS.iter(),
            )
        {
            let args = super::expr_text::simple_args_text(args).unwrap_or_else(|| "...".into());
            diagnostic = diagnostic.with_help(format!(
                "`{free}` is a free function, not a static function of `{class_name}`: write `{free}({args})`"
            ));
        }
        diagnostic = if statics.is_empty() {
            diagnostic.with_note(format!("`{class_name}` has no static functions"))
        } else {
            diagnostic.with_note(format!(
                "static functions of `{class_name}`: {}",
                statics.join(", ")
            ))
        };
        self.push(diagnostic);
    }

    fn push_unknown_static_method(&mut self, call: &StaticCall<'_>, class: &ClassInfo) {
        let StaticCall {
            class_name,
            method_name,
            span,
            ..
        } = *call;
        let mut diagnostic = Self::error_at(
            ErrorCode::E0502,
            format!(
                "no method `{}` on class `{}`",
                method_name,
                self.diagnostic_class_name(class_name)
            ),
            span,
            "method not found",
        );

        if let Some(suggestion) = suggest_similar_name(method_name, class.methods.keys()) {
            diagnostic = diagnostic
                .with_help(format!(
                    "there is a method with a similar name: `{}`",
                    suggestion
                ))
                .with_fix(FixSuggestion::new(
                    span,
                    suggestion,
                    "replace with suggested method",
                ));
        }

        self.push(diagnostic);
    }

    fn push_instance_method_static_call(&mut self, call: &StaticCall<'_>) {
        let StaticCall {
            class_name,
            method_name,
            span,
            ..
        } = *call;
        let diagnostic = Self::error_at(
            ErrorCode::E0835,
            format!(
                "instance method `{}::{}` requires an object",
                class_name, method_name
            ),
            span,
            "instance method called with `::`",
        );

        let diagnostic = if class_name == self.local.current_class.as_deref().unwrap_or("")
            && self.symbols.lookup_var("self").is_some()
        {
            diagnostic.with_help(format!("write `self.{}` instead", method_name))
        } else {
            diagnostic.with_help("call it on an object value with `object.method(...)`")
        };

        self.push(diagnostic);
    }

    /// Protected and private static methods are reachable only from the
    /// classes their visibility admits.
    fn check_static_method_access(&mut self, call: &StaticCall<'_>, mi: &MethodInfo) {
        let StaticCall {
            class_name,
            method_name,
            span,
            ..
        } = *call;
        if mi.protected {
            if !self.can_access_protected_member(class_name) {
                self.push(
                    Self::error_at(
                        ErrorCode::E0503,
                        format!(
                            "method `{}` of class `{}` is protected",
                            method_name,
                            self.diagnostic_class_name(class_name)
                        ),
                        span,
                        "protected method",
                    )
                    .with_label(Label::secondary(mi.declaration_span, "method defined here"))
                    .with_help(format!(
                        "prot members are accessible only within `{}` and its subclasses",
                        self.diagnostic_class_name(class_name)
                    )),
                );
            }
        } else if !self.can_access_private_member(class_name) {
            self.push(
                Self::error_at(
                    ErrorCode::E0501,
                    format!(
                        "method `{}` of class `{}` is private",
                        method_name,
                        self.diagnostic_class_name(class_name)
                    ),
                    span,
                    "private method",
                )
                .with_label(Label::secondary(
                    mi.declaration_span,
                    "method defined here",
                ))
                .with_help(format!("private members are accessible only within their declaring class, even in the same module; make it public with `pub fn {}`", method_name)),
            );
        }
    }

    pub(super) fn resolve_fully_qualified_std_module_call(
        &mut self,
        class_name: &str,
        method_name: &str,
        args: &[CallArg],
        span: Span,
    ) -> Option<Type> {
        if class_name != "std::io" {
            return None;
        }
        let path = QualifiedName::join(class_name, method_name);
        match std_registry::resolve_std_import(&path, span) {
            Ok(std_registry::StdImport::Item { module, item }) if module == "io" => {
                let Some(crate::stdlib_schema::StdItemSchema {
                    kind:
                        crate::stdlib_schema::StdItemKind::Function {
                            params,
                            return_type,
                        },
                    ..
                }) = crate::stdlib_schema::item(&module, &item)
                else {
                    return None;
                };
                if args.len() != params.len() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "function `std::io::{item}` expects {} argument{}, got {}",
                                params.len(),
                                if params.len() == 1 { "" } else { "s" },
                                args.len()
                            ),
                        )
                        .with_label(Label::primary(span, "wrong number of arguments")),
                    );
                }
                for arg in args {
                    self.check_expr(&arg.expr);
                }
                Some(std_schema_type(*return_type))
            }
            Err(diag) => {
                self.push(diag);
                Some(Type::Void)
            }
            _ => None,
        }
    }

    /// An unparameterized reference to the enclosing generic declaration uses
    /// its current receiver. Compare declaration identity, never name prefixes.
    pub(super) fn enclosing_static_receiver(&self, written: &str) -> Option<&str> {
        let current = self.local.current_class.as_deref()?;
        let template = self.generics.classes.get(written)?;
        let receiver = self.symbols.lookup_class(current)?;
        (template.span == receiver.declaration_span).then_some(current)
    }

    /// What a written static-call class names, reporting nothing.
    ///
    /// The same answer [`Self::resolve_static_call_class_name`] gives, without
    /// its diagnostics, its collection-import bookkeeping, or `&mut self`. Two
    /// callers need that: the contextual paths that run BEFORE the checking one
    /// and must not report the same bad std path twice, and `check_expr`, which
    /// records the resolution per expression ID for HIR lowering. Every branch
    /// that can name an enum must agree with the checking version, or lowering
    /// keeps the written spelling while the node's type carries the identity,
    /// and the LIR walker rejects the call as outside its subset.
    pub(super) fn resolve_static_call_class_quiet(&self, class_name: &str) -> Option<String> {
        if class_name == "Self" {
            return self.local.current_class.clone();
        }
        if let Some(receiver) = self.enclosing_static_receiver(class_name) {
            return Some(receiver.to_string());
        }
        if let Some(item) = self.imported_collection_aliases.get(class_name) {
            return Some(item.clone());
        }
        let builtin_or_path = |module: &str, item: &str| {
            crate::stdlib_schema::type_item(module, item)
                .map(|(_, builtin)| builtin.to_string())
                .unwrap_or_else(|| QualifiedName::join(module, item))
        };
        if std_registry::is_builtin_std_path(class_name)
            && let Ok(std_registry::StdImport::Item { module, item }) =
                std_registry::resolve_std_import(class_name, Span::dummy())
        {
            return Some(builtin_or_path(&module, &item));
        }
        if let Some((module_local, item)) = QualifiedName::new(&class_name).split_first()
            && let Some(imported) = self.imported_std_modules.get(module_local)
            && let Ok(std_registry::StdImport::Item { module, item }) =
                std_registry::resolve_std_import(
                    &QualifiedName::from_segments(["std", &imported.module, item]),
                    Span::dummy(),
                )
        {
            return Some(builtin_or_path(&module, &item));
        }
        // An enum reached by any spelling this unit can write -- its own bare
        // `Color` for `pal::Color`, `Level` or `Rank` for `signal::Level` --
        // resolves to the build-wide identity the enum tables are keyed by. A
        // class or interface name is its own identity and comes back unchanged.
        //
        Some(self.canonical_type_name(class_name))
    }

    pub(super) fn resolve_static_call_class_name(
        &mut self,
        class_name: &str,
        span: Span,
    ) -> Option<String> {
        if !self.check_source_type_name(class_name, span) {
            return None;
        }
        if let Some(receiver) = self.enclosing_static_receiver(class_name) {
            return Some(receiver.to_string());
        }
        if class_name != "Self" {
            // The same answer [`Self::resolve_static_call_class_quiet`] gives,
            // with the diagnostics a bad std path owes. `check_expr` records
            // what the written class resolved to, per expression, so HIR
            // lowering does not have to keep the source's own spelling.
            //
            if let Some(item) = self.imported_collection_aliases.get(class_name).cloned() {
                return Some(item);
            }
            if let Some((module, item)) = self.resolve_fully_qualified_std_item(class_name, span) {
                if module == "collections" {
                    self.fully_qualified_collection_types.insert(item.clone());
                }
                return Some(
                    crate::stdlib_schema::type_item(&module, &item)
                        .map(|(_, builtin)| builtin.to_string())
                        .unwrap_or_else(|| QualifiedName::join(&module, &item)),
                );
            }
            if let Some((module, item)) = self.resolve_imported_std_module_item(class_name, span) {
                return Some(
                    crate::stdlib_schema::type_item(&module, &item)
                        .map(|(_, builtin)| builtin.to_string())
                        .unwrap_or_else(|| QualifiedName::join(&module, &item)),
                );
            }
            // An enum an item import brought in (`import signal::Level;`, or
            // `as Rank`) is registered under the spelling THIS unit writes but
            // carries the build-wide identity, and that identity is the only
            // name the enum tables answer to. It is what lets `Level::High`
            // reach the back end as `signal::Level::High`; a
            // class or interface name is its own identity and passes through
            // unchanged.
            return Some(self.canonical_type_name(class_name));
        }

        match self.local.current_class.clone() {
            Some(class_name) => Some(class_name),
            None => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0550,
                        "`Self` can only be used inside a class method",
                    )
                    .with_label(Label::primary(span, "`Self` used outside class method"))
                    .with_help("use an explicit class name for static calls outside a class"),
                );
                None
            }
        }
    }

    /// Type-check a `ClassName::property` static read (static member support §7.1).
    pub(super) fn resolve_static_field_read(
        &mut self,
        class_name: &str,
        field: &str,
        span: Span,
    ) -> Type {
        // `config::LIMIT`: a module-qualified `const`. A
        // module's constants take precedence over a same-named class, exactly
        // as HIR lowering resolves the read.
        if let Some(info) = self
            .symbols
            .lookup_module_func(class_name, field)
            .filter(|info| info.constant.is_some())
            .cloned()
        {
            self.record_method_use(field, "function", span, info.declaration_span, &[]);
            if !info.public {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0402,
                        format!("constant `{class_name}::{field}` is private"),
                    )
                    .with_label(Label::primary(span, "private constant"))
                    .with_help(format!("make it public with `pub const {field}`")),
                );
            }
            return info.return_type;
        }
        let Some(resolved) = self.resolve_static_call_class_name(class_name, span) else {
            return Type::Void;
        };
        match self.lookup_static_prop_in_hierarchy(&resolved, field) {
            Some((owner, info)) => {
                if let Some(caller) = self.local.current_effect_callable {
                    self.static_reads.entry(caller).or_default().insert(
                        crate::compiler_db::static_init::initializer_id(&owner, field),
                    );
                }
                self.record_member_use(
                    field,
                    "static-field",
                    span,
                    info.declaration_span,
                    info.ty.clone(),
                    "read",
                );
                // Visibility: non-public static props are reachable only from
                // inside the class (private) or subclasses (protected).
                if !info.public {
                    if info.protected {
                        if !self.can_access_protected_member(&owner) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0503,
                                    format!(
                                        "static property `{}::{}` is protected",
                                        self.diagnostic_class_name(&owner),
                                        field
                                    ),
                                )
                                .with_label(Label::primary(span, "protected static property")),
                            );
                        }
                    } else if !self.can_access_private_member(&owner) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0419,
                                format!(
                                    "static property `{}::{}` is private",
                                    self.diagnostic_class_name(&owner),
                                    field
                                ),
                            )
                            .with_label(Label::primary(span, "private static property"))
                            .with_help("declare it `pub static` to expose it"),
                        );
                    }
                }
                self.record_static_sharing(&owner, field, &info, span);
                info.ty.clone()
            }
            None => {
                // Distinguish an instance field accessed via `::` from an unknown
                // static member, for a clearer diagnostic (static member support §7.4).
                if self.lookup_field_in_hierarchy(&resolved, field).is_some() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0835,
                            format!(
                                "instance field `{}::{}` requires an object",
                                resolved, field
                            ),
                        )
                        .with_label(Label::primary(span, "instance field accessed with `::`"))
                        .with_help("access it on an object value with `object.field`"),
                    );
                } else {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0502,
                            format!(
                                "no static property `{}::{}`",
                                self.diagnostic_class_name(&resolved),
                                field
                            ),
                        )
                        .with_label(Label::primary(span, "static property not found")),
                    );
                }
                Type::Void
            }
        }
    }

    /// Resolve the concrete payload types for a variant of a generic enum.
    /// Uses the type arguments from the scrutinee type to instantiate the enum.
    pub(super) fn resolve_generic_variant_payload(
        &self,
        enum_name: &str,
        variant_name: &str,
        scrutinee_ty: &Type,
    ) -> Option<Vec<Type>> {
        if !matches!(scrutinee_ty, Type::Named(n) | Type::Generic(n, _) if n == enum_name) {
            return None;
        }
        let enum_info = self.symbols.lookup_enum(enum_name)?;
        let type_args: &[Type] = if let Type::Generic(n, args) = scrutinee_ty {
            if n == enum_name { args.as_slice() } else { &[] }
        } else {
            &[]
        };
        let concrete = if enum_info.type_params.is_empty() || type_args.is_empty() {
            enum_info.clone()
        } else {
            enum_info.instantiate(type_args)
        };
        let variant = concrete.variants.iter().find(|v| v.name == variant_name)?;
        Some(variant.payload_types.clone())
    }

    /// True when `child` is an interface that transitively extends interface
    /// `parent`.
    pub(super) fn interface_extends(&self, child: &str, parent: &str) -> bool {
        if self.symbols.lookup_interface(child).is_none() {
            return false;
        }
        // Compare super names canonically so a directly-imported alias (`Named`)
        // matches a module interface's qualified `extends` entry (`proto::Named`).
        //
        let canon = |n: &str| -> String {
            self.symbols
                .lookup_interface(n)
                .map(|i| i.name.clone())
                .unwrap_or_else(|| n.to_string())
        };
        let parent_canon = canon(parent);
        let mut stack = vec![child.to_string()];
        let mut seen = HashSet::new();
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(info) = self.symbols.lookup_interface(&name) else {
                continue;
            };
            for sup in &info.extends {
                if canon(sup) == parent_canon {
                    return true;
                }
                stack.push(sup.clone());
            }
        }
        false
    }

    /// True when `class` (or one of its ancestors) declares `implements target`,
    /// where `target` is the (possibly generic) interface type. `target` must
    /// name a registered interface, and generic instantiations must match
    /// exactly (e.g. `Box<String>` != `Box<i64>`).
    pub(super) fn class_implements_interface(&self, class: &str, target: &Type) -> bool {
        let target_name = match target {
            Type::Named(n) | Type::Generic(n, _) => n,
            _ => return false,
        };
        if self.symbols.lookup_interface(target_name).is_none() {
            return false;
        }
        // Compare by declaration identity, not spelling: a directly-imported
        // local alias (`import mod::Iface` -> bare `Iface`) must match a class's
        // qualified `implements mod::Iface` entry, and so must
        // its type arguments -- `Pred<Item>` written where `Item` is an item
        // import names the same instantiation as the defining module's
        // `implements Pred<Item>`.
        let mut current = Some(class.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return false;
            }
            let Some(info) = self.symbols.lookup_class(&name) else {
                return false;
            };
            if info
                .implements
                .iter()
                .any(|i| self.same_type_identity(i, target))
            {
                return true;
            }
            current = info.base_class.clone();
        }
        false
    }

    /// Canonicalize an interface type to its registered `InterfaceInfo.name`,
    /// folding a directly-imported local alias (`Iface`) onto the qualified
    /// name (`mod::Iface`) it was bound from. Generic type arguments are left
    /// untouched (instantiation matching stays exact). Non-interface types pass
    /// through unchanged.
    pub(super) fn canonical_interface_type(&self, ty: &Type) -> Type {
        match ty {
            Type::Named(n) => match self.symbols.lookup_interface(n) {
                Some(info) => Type::Named(info.name.clone()),
                None => ty.clone(),
            },
            Type::Generic(n, args) => {
                let canon = self
                    .symbols
                    .lookup_interface(n)
                    .map(|info| info.name.clone())
                    .unwrap_or_else(|| n.clone());
                Type::Generic(canon, args.clone())
            }
            _ => ty.clone(),
        }
    }
}

#[cfg(test)]
mod iterative_resolution_tests;

#[cfg(test)]
mod package_registration_scaling;

/// How to call a callable field `receiver.name(args)`: bind the field to a
/// local first. Uses the written receiver and arguments when they are short.
fn callable_field_help(receiver: &Expr, name: &str, args: &[CallArg]) -> String {
    let Some(object) = super::expr_text::simple_expr_text(receiver) else {
        return format!(
            "`{name}` is a callable field; bind it to a local variable first: `let callback = receiver.{name}; callback(...)` (replace `receiver` with the object expression)"
        );
    };
    let args = super::expr_text::simple_args_text(args).unwrap_or_else(|| "...".into());
    format!(
        "`{name}` is a callable field; bind it to a local variable first: `let {name} = {object}.{name}; {name}({args})`"
    )
}

/// Primitive types a static call can name, as in `f64::sqrt(x)`.
const PRIMITIVE_TYPE_NAMES: [&str; 4] = ["i64", "f64", "u8", "String"];

/// The `T::name(..)` functions of primitive types, with the type of their
/// first parameter: the receiver a method-style call would have supplied.
/// The type checker and `intrinsics::namespace_builtin` must agree.
pub(super) const PRIMITIVE_STATICS: [(&str, &str, &str); 17] = [
    ("i64", "abs", "i64"),
    ("i64", "min", "i64"),
    ("i64", "max", "i64"),
    ("i64", "clamp", "i64"),
    ("i64", "parse", "String"),
    ("f64", "abs", "f64"),
    ("f64", "min", "f64"),
    ("f64", "max", "f64"),
    ("f64", "clamp", "f64"),
    ("f64", "sqrt", "f64"),
    ("f64", "floor", "f64"),
    ("f64", "ceil", "f64"),
    ("f64", "round", "f64"),
    ("f64", "from_i64", "i64"),
    ("f64", "trunc_to_i64", "f64"),
    ("f64", "to_string", "f64"),
    ("f64", "parse", "String"),
];

/// `x.trunc_to_i64()` on an `f64`: the function exists as a static function
/// (or, for `pow`/`powf`, a free function) taking the receiver first.
fn primitive_function_help(
    receiver_ty: &Type,
    receiver: &Expr,
    method_name: &str,
    args: &[CallArg],
) -> Option<String> {
    let receiver_name = match receiver_ty {
        Type::I64 => "i64",
        Type::F64 => "f64",
        _ => return None,
    };
    let call_args = || {
        let object = super::expr_text::simple_expr_text(receiver)?;
        let rest = super::expr_text::simple_args_text(args)?;
        Some(if rest.is_empty() {
            object
        } else {
            format!("{object}, {rest}")
        })
    };
    let written = |callee: &str| match call_args() {
        Some(args) => format!("`{callee}({args})`"),
        None => format!("`{callee}(value, ...)` with the receiver as the first argument"),
    };
    if let Some((owner, name, _)) = PRIMITIVE_STATICS
        .iter()
        .find(|(_, name, first)| *name == method_name && *first == receiver_name)
    {
        let callee = format!("{owner}::{name}");
        return Some(format!(
            "`{name}` is a static function, not a method: write {}",
            written(&callee)
        ));
    }
    if receiver_name == "f64"
        && crate::semantic::intrinsics::ADDRESSABLE_BUILTINS.contains(&method_name)
    {
        return Some(format!(
            "`{method_name}` is a free function, not a method: write {}",
            written(method_name)
        ));
    }
    // A misspelled static function: `x.floorr()` meant `f64::floor(x)`.
    let candidates: Vec<&str> = PRIMITIVE_STATICS
        .iter()
        .filter(|(_, _, first)| *first == receiver_name)
        .map(|(_, name, _)| *name)
        .collect();
    let near = suggest_similar_name(method_name, candidates.iter())?;
    let (owner, ..) = PRIMITIVE_STATICS
        .iter()
        .find(|(_, name, first)| *name == near && *first == receiver_name)?;
    Some(format!(
        "did you mean the static function `{owner}::{near}`? write {}",
        written(&format!("{owner}::{near}"))
    ))
}
