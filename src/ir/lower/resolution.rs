//! Declaration snapshots used by typed HIR.
use super::*;
use crate::ir::typed_ast::{
    HirClassInfo, HirEnumInfo, HirEnumVariant, HirInterfaceInfo, HirResolution, HirSignature,
};
use crate::parser::ast::ParamMode;
use crate::semantic::ids::{FunctionId, TypeId};

/// Freeze source declarations, overlay checked facts, then inherit static storage.
pub(super) fn lower_resolution_partial(
    program: &Program,
    tables: &PartialCheckerTables<'_>,
) -> HirResolution {
    let mut out = HirResolution::default();
    lower_namespaces(program, &mut out);
    lower_source_declarations(program, tables, &mut out);
    lower_checked_declarations(tables, &mut out);
    inherit_static_fields(&mut out);
    out
}

fn lower_namespaces(program: &Program, out: &mut HirResolution) {
    for namespace in ["time", "env", "fs", "net", "parallel", "text", "f64", "i64"] {
        out.namespaces
            .insert(TypeId::local(namespace), namespace.to_owned());
    }
    for import in &program.imports {
        use crate::module::std_registry;
        if std_registry::is_builtin_std_path(&import.path) {
            if let Ok(std_registry::StdImport::Module { module }) =
                std_registry::resolve_std_import(&import.path, import.span)
                && matches!(
                    module.as_str(),
                    "time" | "env" | "fs" | "net" | "parallel" | "text"
                )
            {
                out.namespaces.insert(
                    TypeId::local(import.alias.as_deref().unwrap_or(&module)),
                    module,
                );
            }
        } else {
            let access = import
                .alias
                .as_deref()
                .unwrap_or_else(|| QualifiedName::new(&import.path).last_segment());
            if !matches!(access, "f64" | "i64") {
                out.namespaces.remove(&TypeId::local(access));
            }
        }
    }
}

fn lower_source_declarations(
    program: &Program,
    tables: &PartialCheckerTables<'_>,
    out: &mut HirResolution,
) {
    let canonical = |name: &str| match tables.normalize(&Type::Named(name.to_owned())) {
        Type::Named(ref name) => TypeId::from_source_name(name),
        _ => TypeId::from_source_name(name),
    };
    let signature = |params: &[crate::parser::ast::Param],
                     result: &Type,
                     is_static,
                     is_async,
                     bound: &[String]| HirSignature {
        params: params
            .iter()
            .map(|p| tables.normalize_declared(&p.ty, bound).into())
            .collect(),
        param_modes: params.iter().map(|p| p.mode.clone()).collect(),
        return_type: tables.normalize_declared(result, bound).into(),
        is_static,
        is_async,
    };
    // Preserve declaration order for builtin enum discriminants too.
    static PRELUDE: std::sync::OnceLock<Program> = std::sync::OnceLock::new();
    let prelude = PRELUDE.get_or_init(|| {
        let tokens = crate::lexer::Lexer::new(crate::prelude::PRELUDE_SOURCE)
            .tokenize()
            .expect("prelude lexes");
        let (prelude, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "prelude parses: {errors:?}");
        prelude
    });
    for item in prelude.items.iter().chain(&program.items) {
        match item {
            Item::Enum(e) => {
                out.enums.insert(
                    canonical(&e.name),
                    HirEnumInfo {
                        type_params: e
                            .type_params
                            .iter()
                            .map(|name| TypeId::from_source_name(name))
                            .collect(),
                        variants: e
                            .variants
                            .iter()
                            .enumerate()
                            .map(|(tag, variant)| HirEnumVariant {
                                name: variant.name.clone(),
                                tag: tag as i64,
                                payloads: variant
                                    .payload
                                    .iter()
                                    .map(|ty| tables.normalize_declared(ty, &e.type_params).into())
                                    .collect(),
                            })
                            .collect(),
                    },
                );
            }
            Item::Function(f) => {
                out.functions.insert(
                    FunctionId::free_from_source_name(&f.name),
                    signature(&f.params, &f.return_type, true, f.is_async, &[]),
                );
            }
            Item::Class(c) => {
                if c.public && c.is_open {
                    out.open_dispatch_types.insert(canonical(&c.name));
                }
                let mut info = HirClassInfo {
                    implements: c
                        .implements
                        .iter()
                        .map(|ty| tables.normalize(ty).into())
                        .collect(),
                    base: c
                        .base_class
                        .as_ref()
                        .map(|base| canonical(&base.spelling())),
                    ..HirClassInfo::default()
                };
                for field in &c.fields {
                    let ty = tables.normalize(&field.ty).into();
                    if field.is_static {
                        info.static_fields.insert(field.name.clone(), ty);
                    } else {
                        info.fields.push((field.name.clone(), ty));
                    }
                }
                info.constructor = c
                    .constructors
                    .first()
                    .map(|ctor| signature(&ctor.params, &Type::Void, false, false, &[]));
                for method in &c.methods {
                    info.methods.insert(
                        method.name.clone(),
                        signature(
                            &method.params,
                            &method.return_type,
                            method.is_static,
                            method.is_async,
                            &[],
                        ),
                    );
                }
                out.classes.insert(canonical(&c.name), info);
            }
            Item::Interface(interface) => {
                if interface.public {
                    out.open_dispatch_types.insert(canonical(&interface.name));
                }
                out.interfaces.insert(
                    canonical(&interface.name),
                    HirInterfaceInfo {
                        type_params: interface
                            .type_params
                            .iter()
                            .map(|name| TypeId::from_source_name(name))
                            .collect(),
                        extends: interface
                            .extends
                            .iter()
                            .map(|name| canonical(name))
                            .collect(),
                        methods: interface
                            .methods
                            .iter()
                            .map(|method| {
                                (
                                    method.name.clone(),
                                    signature(
                                        &method.params,
                                        &method.return_type,
                                        method.is_static,
                                        false,
                                        &interface
                                            .type_params
                                            .iter()
                                            .cloned()
                                            .chain(std::iter::once("Self".to_string()))
                                            .collect::<Vec<_>>(),
                                    ),
                                )
                            })
                            .collect(),
                    },
                );
            }
        }
    }
}

fn lower_checked_declarations(tables: &PartialCheckerTables<'_>, out: &mut HirResolution) {
    if let Some(enums) = tables.enums {
        for (key, info) in enums {
            insert_declared(
                &mut out.enums,
                key,
                TypeId::from_source_name(&info.name),
                HirEnumInfo {
                    type_params: info
                        .type_params
                        .iter()
                        .map(|name| TypeId::from_source_name(name))
                        .collect(),
                    variants: info
                        .variants
                        .iter()
                        .map(|variant| HirEnumVariant {
                            name: variant.name.clone(),
                            tag: variant.tag,
                            payloads: variant
                                .payload_types
                                .iter()
                                .map(|ty| tables.normalize_declared(ty, &info.type_params).into())
                                .collect(),
                        })
                        .collect(),
                },
            );
        }
    }
    lower_checked_symbols(tables, out);
}

// A unit's tables also hold entries under alias keys: `$type$mono$`
// aliases for the types its generic instances name (the entry's `Get` in
// `util::id<Get>`) and module access paths. An alias's type can share its
// bare name with a type declared under that very key (the unit's own
// `Get`), which the HIR spells bare; an alias entry therefore never
// replaces it (which one won used to depend on hash order).
fn insert_declared<V>(
    table: &mut std::collections::HashMap<TypeId, V>,
    key: &TypeId,
    name: TypeId,
    info: V,
) {
    if *key == name {
        table.insert(name, info);
    } else {
        table.entry(name).or_insert(info);
    }
}

fn lower_checked_symbols(tables: &PartialCheckerTables<'_>, out: &mut HirResolution) {
    let canonical = |name: &str| match tables.normalize(&Type::Named(name.to_owned())) {
        Type::Named(ref name) => TypeId::from_source_name(name),
        _ => TypeId::from_source_name(name),
    };
    let checked_signature = |params: &[Type],
                             infos: &[symbols::ParamInfo],
                             result: &Type,
                             is_static,
                             is_async,
                             bound: &[String]| {
        HirSignature {
            params: params
                .iter()
                .map(|ty| tables.normalize_declared(ty, bound).into())
                .collect(),
            param_modes: params
                .iter()
                .enumerate()
                .map(|(i, _)| infos.get(i).map_or(ParamMode::Value, |p| p.mode.clone()))
                .collect(),
            return_type: tables.normalize_declared(result, bound).into(),
            is_static,
            is_async,
        }
    };
    if let Some(symbols) = tables.symbols {
        for (access, module) in symbols.module_accesses() {
            let functions = module
                .functions
                .ids()
                .map(|id| {
                    let function = module
                        .functions
                        .get_id(id)
                        .expect("declared module function");
                    (
                        id.name().to_string(),
                        checked_signature(
                            &function.params,
                            &function.param_infos,
                            &function.return_type,
                            true,
                            function.is_async,
                            &[],
                        ),
                    )
                })
                .collect();
            out.modules
                .insert(TypeId::from_source_name(access), functions);
        }
        for (id, function) in &symbols.functions {
            out.functions.insert(
                *id,
                checked_signature(
                    &function.params,
                    &function.param_infos,
                    &function.return_type,
                    true,
                    function.is_async,
                    &[],
                ),
            );
        }
        for (key, interface) in &symbols.interfaces {
            if interface.public {
                out.open_dispatch_types.insert(*key);
                out.open_dispatch_types
                    .insert(TypeId::from_source_name(&interface.name));
            }
            insert_declared(
                &mut out.interfaces,
                key,
                TypeId::from_source_name(&interface.name),
                HirInterfaceInfo {
                    type_params: interface
                        .type_params
                        .iter()
                        .map(|name| TypeId::from_source_name(name))
                        .collect(),
                    extends: interface
                        .extends
                        .iter()
                        .map(|name| canonical(name))
                        .collect(),
                    methods: interface
                        .methods
                        .iter()
                        .map(|(name, method)| {
                            (
                                name.clone(),
                                checked_signature(
                                    &method.params,
                                    &method.param_infos,
                                    &method.return_type,
                                    method.is_static,
                                    false,
                                    &interface
                                        .type_params
                                        .iter()
                                        .cloned()
                                        .chain(std::iter::once("Self".to_string()))
                                        .collect::<Vec<_>>(),
                                ),
                            )
                        })
                        .collect(),
                },
            );
        }
        for (key, class) in &symbols.classes {
            if class.public && class.is_open {
                out.open_dispatch_types.insert(*key);
                out.open_dispatch_types
                    .insert(TypeId::from_source_name(&class.name));
            }
            insert_declared(
                &mut out.classes,
                key,
                TypeId::from_source_name(&class.name),
                HirClassInfo {
                    base: class.base_class.as_ref().map(|name| canonical(name)),
                    implements: class
                        .implements
                        .iter()
                        .map(|ty| tables.normalize(ty).into())
                        .collect(),
                    fields: class
                        .instance_field_order
                        .iter()
                        .map(|(name, ty)| (name.clone(), tables.normalize(ty).into()))
                        .collect(),
                    static_fields: class
                        .static_props
                        .iter()
                        .map(|(name, field)| (name.clone(), tables.normalize(&field.ty).into()))
                        .collect(),
                    constructor: class.constructor.as_ref().map(|ctor| {
                        checked_signature(
                            &ctor.params,
                            &ctor.param_infos,
                            &Type::Void,
                            false,
                            false,
                            &[],
                        )
                    }),
                    methods: class
                        .methods
                        .iter()
                        .map(|(name, method)| {
                            (
                                name.clone(),
                                checked_signature(
                                    &method.params,
                                    &method.param_infos,
                                    &method.return_type,
                                    method.is_static,
                                    method.is_async,
                                    &[],
                                ),
                            )
                        })
                        .collect(),
                },
            );
        }
    }
}

fn inherit_static_fields(out: &mut HirResolution) {
    // A static property keeps its ancestor's storage while remaining visible
    // through a derived class. Preserve own-property shadowing in the snapshot.
    let declared = out.classes.clone();
    for (identity, info) in &mut out.classes {
        let mut visited = std::collections::HashSet::from([*identity]);
        let mut base = info.base;
        while let Some(parent) = base {
            if !visited.insert(parent) {
                break;
            }
            let Some(parent_info) = declared.get(&parent) else {
                break;
            };
            for (name, ty) in &parent_info.static_fields {
                info.static_fields
                    .entry(name.clone())
                    .or_insert_with(|| ty.clone());
            }
            base = parent_info.base;
        }
    }
}
