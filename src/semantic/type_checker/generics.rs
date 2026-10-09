//! Concrete instantiations discovered by semantic checking, before persistent
//! body queries are registered. The final pipeline checks the expanded program.
use super::*;
use crate::parser::ownership::NodeMut;
use crate::semantic::symbols::{EnumInfo, FuncInfo, InstantiationType, InterfaceInfo};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::rc::Rc;

type InstantiationKey = (u8, String, Vec<Type>);
type ImportedTypes = Vec<(String, Rc<InstantiationType>)>;
struct InstantiatedClass {
    info: ClassInfo,
    types: ImportedTypes,
}

/// One owned template per source declaration across every checker in a build.
#[derive(Default)]
pub(crate) struct TemplateRegistry {
    pub module_paths: Rc<HashMap<crate::diagnostics::FileId, String>>,
    requested: HashSet<InstantiationKey>,
    nominal_types: HashMap<String, Rc<InstantiationType>>,
    instantiated_classes: HashMap<InstantiationKey, Rc<InstantiatedClass>>,
    functions: HashMap<(crate::diagnostics::FileId, String), Rc<FunctionDecl>>,
    classes: HashMap<(crate::diagnostics::FileId, String), Rc<ClassDecl>>,
    #[cfg(test)]
    function_specializations: usize,
    #[cfg(test)]
    class_specializations: usize,
    #[cfg(test)]
    function_copies: usize,
    #[cfg(test)]
    class_copies: usize,
}

impl TemplateRegistry {
    fn reserve(&mut self, key: &InstantiationKey) -> Result<bool, ()> {
        if self.requested.contains(key) {
            return Ok(false);
        }
        if self.requested.len() >= 4096 {
            return Err(());
        }
        self.requested.insert(key.clone());
        Ok(true)
    }

    pub(crate) fn intern_function(&mut self, declaration: &FunctionDecl) -> Rc<FunctionDecl> {
        self.functions
            .entry((declaration.span.file_id, declaration.name.clone()))
            .or_insert_with(|| {
                #[cfg(test)]
                {
                    self.function_copies += 1;
                }
                Rc::new(declaration.clone())
            })
            .clone()
    }

    pub(crate) fn intern_class(&mut self, declaration: &ClassDecl) -> Rc<ClassDecl> {
        self.classes
            .entry((declaration.span.file_id, declaration.name.clone()))
            .or_insert_with(|| {
                #[cfg(test)]
                {
                    self.class_copies += 1;
                }
                Rc::new(declaration.clone())
            })
            .clone()
    }
}

#[derive(Default)]
pub(crate) struct Generics {
    pub registry: Rc<RefCell<TemplateRegistry>>,
    pub abstract_check: bool,
    pub functions: HashMap<String, std::rc::Rc<FunctionDecl>>,
    pub classes: HashMap<String, std::rc::Rc<ClassDecl>>,
    pub instances: HashMap<InstantiationKey, String>,
    pub pending: std::collections::VecDeque<Instantiation>,
    pub imported_types: HashMap<String, Rc<InstantiationType>>,
    pub emitted: Vec<Item>,
    pub logical_types: HashMap<String, Type>,
    pub active_class_depth: usize,
    pub calls: HashMap<ExprId, String>,
    pub function_paths: HashMap<String, String>,
    pub class_paths: HashMap<String, String>,
    pub objects: HashMap<ExprId, String>,
}

enum InstantiationTypeRef<'a> {
    Class(&'a ClassInfo),
    Enum(&'a EnumInfo),
    Interface(&'a InterfaceInfo),
}

#[cfg(test)]
thread_local! {
    static GENERIC_CALL_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static INSTANTIATION_METADATA_COPIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl InstantiationTypeRef<'_> {
    fn identity(&self) -> (&str, Span) {
        match self {
            Self::Class(info) => (&info.name, info.declaration_span),
            Self::Enum(info) => (&info.name, info.declaration_span),
            Self::Interface(info) => (&info.name, info.declaration_span),
        }
    }

    fn into_owned(self) -> InstantiationType {
        #[cfg(test)]
        INSTANTIATION_METADATA_COPIES.with(|copies| copies.set(copies.get() + 1));
        match self {
            Self::Class(info) => InstantiationType::Class(info.clone()),
            Self::Enum(info) => InstantiationType::Enum(info.clone()),
            Self::Interface(info) => InstantiationType::Interface(info.clone()),
        }
    }
}

#[cfg(test)]
mod template_registry_tests {
    use super::*;
    use crate::diagnostics::FileId;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> Program {
        let (program, diagnostics) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        program
    }

    #[test]
    fn nested_reference_inference_checks_each_generic_call_once() {
        for depth in [1, 4, 8, 16] {
            let mut expression = "0".to_string();
            for _ in 0..depth {
                expression = format!("index(&a[{expression}])");
            }
            let source = format!(
                "import std::collections::Array; fn index<T>(x: &T) -> i64 {{ return 0; }} fn main() {{ let a: Array<i64> = [1]; println({expression}); }}"
            );
            let mut checker = TypeChecker::new();
            GENERIC_CALL_CHECKS.with(|count| count.set(0));
            checker.check_program(&parse(&source));
            assert!(checker.errors.is_empty(), "{:?}", checker.errors);
            let visits = GENERIC_CALL_CHECKS.with(|count| count.get());
            assert_eq!(visits, depth);
            println!(
                "WILLOW_GENERIC_AUDIT {}",
                serde_json::json!({"case":"nested_reference", "depth":depth, "generic_call_checks":visits})
            );
        }
    }

    #[test]
    fn specialization_reservations_enforce_limit_before_body_cloning() {
        let mut registry = TemplateRegistry::default();
        for index in 0..4096 {
            assert_eq!(
                registry.reserve(&(0, index.to_string(), vec![Type::I64])),
                Ok(true)
            );
        }
        assert_eq!(
            registry.reserve(&(0, "extra".into(), vec![Type::I64])),
            Err(())
        );
        assert_eq!(
            registry.reserve(&(0, "0".into(), vec![Type::I64])),
            Ok(false)
        );
        assert_eq!(registry.requested.len(), 4096);
    }

    #[test]
    fn consumer_fanout_specializes_each_body_once() {
        for width in [1, 16, 256] {
            let templates = parse(&format!(
                "pub fn identity<T>(x: T) -> T {{ {} return x; }} pub class Box<T> {{ pub value: T; pub fn get(self) -> T {{ {} return self.value; }} }}",
                "println(1);".repeat(width),
                "println(1);".repeat(width)
            ));
            let Item::Function(f) = &templates.items[0] else {
                unreachable!()
            };
            let Item::Class(c) = &templates.items[1] else {
                unreachable!()
            };
            for count in [1, 16, 256] {
                let registry = Rc::new(RefCell::new(TemplateRegistry::default()));
                let f = registry.borrow_mut().intern_function(f);
                let c = registry.borrow_mut().intern_class(c);
                let mut pending = 0;
                for _ in 0..count {
                    let mut checker = TypeChecker::new();
                    checker.generics.registry = registry.clone();
                    checker
                        .generics
                        .functions
                        .insert("identity".into(), f.clone());
                    checker.generics.classes.insert("Box".into(), c.clone());
                    let program = parse(
                        "fn main() { identity(1); let b = new Box<i64>(1); println(b.get()); }",
                    );
                    checker.check_program(&program);
                    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                    pending += checker.generics.pending.len();
                }
                let registry = registry.borrow();
                assert_eq!(pending, 2, "width={width} consumers={count}");
                assert_eq!(registry.function_specializations, 1);
                assert_eq!(registry.class_specializations, 1);
                println!(
                    "WILLOW_GENERIC_AUDIT {}",
                    serde_json::json!({"case":"consumer_fanout", "body_width":width, "consumers":count, "function_specializations":registry.function_specializations, "class_specializations":registry.class_specializations, "queued_bodies":pending})
                );
            }
        }
    }

    #[test]
    fn repeated_interning_copies_each_template_body_once() {
        for width in [1, 16, 256] {
            let source = format!(
                "fn identity<T>(x: T) -> T {{ {} return x; }} class Box<T> {{ pub value: T; pub fn get(self) -> T {{ {} return self.value; }} }}",
                "println(1);".repeat(width),
                "println(1);".repeat(width)
            );
            let program = parse(&source);
            let Item::Function(function) = &program.items[0] else {
                unreachable!()
            };
            let Item::Class(class) = &program.items[1] else {
                unreachable!()
            };
            for count in [1, 16, 256] {
                let shared = Rc::new(RefCell::new(TemplateRegistry::default()));
                let first_function = shared.borrow_mut().intern_function(function);
                let first_class = shared.borrow_mut().intern_class(class);
                for _ in 0..count {
                    // A distinct checker-local handle shares the build registry.
                    let registry = Rc::clone(&shared);
                    assert!(Rc::ptr_eq(
                        &first_function,
                        &registry.borrow_mut().intern_function(function)
                    ));
                    assert!(Rc::ptr_eq(
                        &first_class,
                        &registry.borrow_mut().intern_class(class)
                    ));
                }
                assert_eq!(
                    shared.borrow().function_copies,
                    1,
                    "width={width}, uses={count}"
                );
                assert_eq!(
                    shared.borrow().class_copies,
                    1,
                    "width={width}, uses={count}"
                );
                println!(
                    "WILLOW_GENERIC_AUDIT {}",
                    serde_json::json!({"case":"template_interning", "body_width":width, "uses":count, "function_copies":shared.borrow().function_copies, "class_copies":shared.borrow().class_copies})
                );
            }
        }
    }

    #[test]
    fn same_named_declarations_in_different_files_are_distinct() {
        let program =
            parse("fn identity<T>(x: T) -> T { return x; } class Box<T> { pub value: T; }");
        let Item::Function(function) = &program.items[0] else {
            unreachable!()
        };
        let Item::Class(class) = &program.items[1] else {
            unreachable!()
        };
        let mut other_function = function.clone();
        other_function.span.file_id = FileId(1);
        let mut other_class = class.clone();
        other_class.span.file_id = FileId(1);
        let mut registry = TemplateRegistry::default();
        assert!(!Rc::ptr_eq(
            &registry.intern_function(function),
            &registry.intern_function(&other_function)
        ));
        assert!(!Rc::ptr_eq(
            &registry.intern_class(class),
            &registry.intern_class(&other_class)
        ));
        assert_eq!(registry.function_copies, 2);
        assert_eq!(registry.class_copies, 2);
    }

    #[test]
    fn repeated_nominal_bindings_copy_metadata_once() {
        for width in [1, 16, 256] {
            let fields: String = (0..width)
                .map(|index| format!("pub field{index}: i64;"))
                .collect();
            let program = parse(&format!("class Payload {{ {fields} }}"));
            let Item::Class(class) = &program.items[0] else {
                unreachable!()
            };
            for count in [1, 16, 256] {
                let mut checker = TypeChecker::new();
                checker.register_class(class);
                let bindings = (0..count)
                    .map(|index| (format!("T{index}"), Type::Named("Payload".into())))
                    .collect();
                INSTANTIATION_METADATA_COPIES.with(|copies| copies.set(0));
                let (substituted, metadata) =
                    checker.instantiation_bindings(FileId(1), Span::dummy(), &bindings);
                assert_eq!(substituted.len(), count);
                assert_eq!(metadata.len(), 1);
                let one = HashMap::from([("T".to_string(), Type::Named("Payload".into()))]);
                for _ in 0..count {
                    let (_, next) = checker.instantiation_bindings(FileId(1), Span::dummy(), &one);
                    assert!(Rc::ptr_eq(&metadata[0].1, &next[0].1));
                }
                assert_eq!(
                    INSTANTIATION_METADATA_COPIES.with(|copies| copies.get()),
                    1,
                    "width={width}, uses={count}"
                );
                println!(
                    "WILLOW_GENERIC_AUDIT {}",
                    serde_json::json!({"case":"nominal_metadata", "fields":width, "uses":count, "metadata_copies":INSTANTIATION_METADATA_COPIES.with(|copies| copies.get())})
                );
            }
        }
    }
}

pub(crate) struct Instantiation {
    pub item: Item,
    pub classes: ImportedTypes,
}

#[derive(Default)]
pub(crate) struct FreshIds {
    bodies: HashMap<BodyId, BodyId>,
    expressions: HashMap<ExprId, ExprId>,
    patterns: HashMap<PatternId, PatternId>,
}
impl FreshIds {
    fn body(&mut self, id: BodyId) -> BodyId {
        *self.bodies.entry(id).or_insert_with(BodyId::fresh)
    }
    fn expr(&mut self, id: ExprId) -> ExprId {
        *self.expressions.entry(id).or_insert_with(ExprId::fresh)
    }
    fn pattern(&mut self, id: PatternId) -> PatternId {
        *self.patterns.entry(id).or_insert_with(PatternId::fresh)
    }
}

/// Abstract class use needs field/method signatures, never executable bodies.
fn class_signature(class: &ClassDecl) -> ClassDecl {
    let empty = |body: &Block| Block {
        id: body.id,
        stmts: Vec::new(),
        span: body.span,
    };
    ClassDecl {
        type_params: class.type_params.clone(),
        name: class.name.clone(),
        public: class.public,
        is_open: class.is_open,
        base_class: class.base_class.clone(),
        implements: class.implements.clone(),
        source_implements_len: class.source_implements_len,
        span: class.span,
        fields: class
            .fields
            .iter()
            .map(|f| FieldDecl {
                name: f.name.clone(),
                ty: f.ty.clone(),
                public: f.public,
                protected: f.protected,
                is_static: f.is_static,
                is_mut: f.is_mut,
                initializer: None,
                span: f.span,
            })
            .collect(),
        methods: class
            .methods
            .iter()
            .map(|m| MethodDecl {
                name: m.name.clone(),
                public: m.public,
                protected: m.protected,
                is_async: m.is_async,
                is_open: m.is_open,
                is_override: m.is_override,
                is_static: m.is_static,
                params: m.params.clone(),
                return_type: m.return_type.clone(),
                body: empty(&m.body),
                span: m.span,
                is_default_injected: m.is_default_injected,
                is_interface_default: m.is_interface_default,
            })
            .collect(),
        constructors: class
            .constructors
            .iter()
            .map(|c| ConstructorDecl {
                public: c.public,
                protected: c.protected,
                params: c.params.clone(),
                body: empty(&c.body),
                span: c.span,
            })
            .collect(),
    }
}

fn uses_parameters(ty: &Type, parameters: &HashSet<&str>) -> bool {
    let mut pending = vec![ty];
    while let Some(ty) = pending.pop() {
        match ty {
            Type::Named(name) if parameters.contains(name.as_str()) => return true,
            Type::Array(element) => pending.push(element),
            Type::Generic(name, args) => {
                if parameters.contains(name.as_str()) {
                    return true;
                }
                pending.extend(args);
            }
            Type::Fn(args, result) | Type::Closure(args, result) => {
                pending.extend(args);
                pending.push(result);
            }
            _ => {}
        }
    }
    false
}

fn symbol(name: &str, args: &[Type]) -> String {
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(args).expect("type serialization"));
    format!("{name}$mono${:x}", digest.finalize())
}

impl TypeChecker {
    fn check_instantiation_size(&mut self, args: &[Type], span: Span) -> bool {
        let mut nodes = 0usize;
        let mut pending: Vec<_> = args.iter().collect();
        while let Some(ty) = pending.pop() {
            nodes += 1;
            if nodes > 4096 {
                self.generic_error(
                    span,
                    "generic type argument size exceeded (possible non-finite recursive expansion)",
                );
                return false;
            }
            match ty {
                Type::Array(element) => pending.push(element),
                Type::Generic(_, args) => pending.extend(args),
                Type::Fn(args, result) | Type::Closure(args, result) => {
                    pending.extend(args);
                    pending.push(result);
                }
                _ => {}
            }
        }
        true
    }

    fn reserve_instantiation(&mut self, key: &InstantiationKey, span: Span) -> Option<bool> {
        let result = self.generics.registry.borrow_mut().reserve(key);
        match result {
            Ok(fresh) => Some(fresh),
            Err(()) => {
                self.generic_error(span, "generic instantiation limit exceeded (possible non-finite recursive expansion)");
                None
            }
        }
    }

    fn generic_type_identity(&self, name: &str) -> String {
        let identity = self
            .symbols
            .lookup_class(name)
            .map(|i| (i.name.as_str(), i.declaration_span))
            .or_else(|| {
                self.symbols
                    .lookup_enum(name)
                    .map(|i| (i.name.as_str(), i.declaration_span))
            })
            .or_else(|| {
                self.symbols
                    .lookup_interface(name)
                    .map(|i| (i.name.as_str(), i.declaration_span))
            })
            .or_else(|| {
                self.generics
                    .classes
                    .get(name)
                    .map(|i| (i.name.as_str(), i.span))
            });
        let Some((name, span)) = identity else {
            return name.to_string();
        };
        self.generic_nominal_identity(name, span)
    }

    fn generic_nominal_identity(&self, name: &str, span: Span) -> String {
        self.generics
            .registry
            .borrow()
            .module_paths
            .get(&span.file_id)
            .map_or_else(
                || name.to_string(),
                |module| format!("{module}::{}", name.rsplit("::").next().unwrap_or(name)),
            )
    }

    fn instantiation_bindings(
        &mut self,
        owner: crate::diagnostics::FileId,
        span: Span,
        bindings: &HashMap<String, Type>,
    ) -> (HashMap<String, Type>, ImportedTypes) {
        let mut classes = HashMap::new();
        let bindings = bindings
            .iter()
            .map(|(param, ty)| {
                let ty = self.normalize_type(ty, span);
                let ty = ty.map_names(|name| {
                    let Some(info) = self
                        .symbols
                        .lookup_class(name)
                        .map(InstantiationTypeRef::Class)
                        .or_else(|| {
                            self.symbols
                                .lookup_enum(name)
                                .map(InstantiationTypeRef::Enum)
                        })
                        .or_else(|| {
                            self.symbols
                                .lookup_interface(name)
                                .map(InstantiationTypeRef::Interface)
                        })
                    else {
                        return name.clone();
                    };
                    let (identity, declaration_span) = info.identity();
                    if declaration_span.file_id == owner && !name.contains("::") {
                        return name.clone();
                    }
                    let canonical = self.generic_nominal_identity(identity, declaration_span);
                    let alias = symbol("$type", &[Type::Named(canonical)]);
                    classes.entry(alias.clone()).or_insert_with(|| {
                        self.generics
                            .registry
                            .borrow_mut()
                            .nominal_types
                            .entry(alias.clone())
                            .or_insert_with(|| Rc::new(info.into_owned()))
                            .clone()
                    });
                    alias
                });
                (param.clone(), ty)
            })
            .collect();
        (bindings, classes.into_iter().collect())
    }

    pub(crate) fn register_instantiation_types(&mut self, types: &[(String, InstantiationType)]) {
        for (alias, info) in types {
            if !self.generics.imported_types.contains_key(alias) {
                self.register_instantiation_type(alias, Rc::new(info.clone()));
            }
        }
    }

    fn register_shared_instantiation_types(&mut self, types: &ImportedTypes) {
        for (alias, info) in types {
            if !self.generics.imported_types.contains_key(alias) {
                self.register_instantiation_type(alias, Rc::clone(info));
            }
        }
    }

    fn register_instantiation_type(&mut self, alias: &str, info: Rc<InstantiationType>) {
        match info.as_ref() {
            InstantiationType::Class(info) => {
                self.symbols.define_class(alias.to_string(), info.clone())
            }
            InstantiationType::Enum(info) => {
                self.symbols.define_enum(alias.to_string(), info.clone())
            }
            InstantiationType::Interface(info) => self
                .symbols
                .define_interface(alias.to_string(), info.clone()),
        }
        self.generics.imported_types.insert(alias.to_string(), info);
    }

    pub(super) fn generic_error(&mut self, span: Span, message: impl Into<String>) {
        self.push(
            Diagnostic::new(Severity::Error, ErrorCode::E0201, message)
                .with_label(Label::primary(span, "generic instantiation")),
        );
    }

    pub(super) fn check_generic_call(&mut self, c: &CallExpr) -> Option<Type> {
        self.check_generic_call_parts(&c.callee, &c.type_args, &c.args, c.span, c.id)
    }

    pub(super) fn check_generic_call_parts(
        &mut self,
        callee: &str,
        type_args: &[Type],
        call_args: &[CallArg],
        span: Span,
        id: ExprId,
    ) -> Option<Type> {
        let template = self.generics.functions.get(callee)?.clone();
        #[cfg(test)]
        GENERIC_CALL_CHECKS.with(|count| count.set(count.get() + 1));
        if !template.public && template.span.file_id != span.file_id {
            self.generic_error(span, format!("generic function `{}` is private", callee));
            return Some(Self::error_type());
        }
        let explicit: Vec<_> = type_args
            .iter()
            .map(|t| {
                let ty = self.normalize_type(t, span);
                ty.substitute_names(|n| self.generics.logical_types.get(n).cloned())
            })
            .collect();
        for ty in &explicit {
            self.validate_type(ty, span);
        }
        let imported = self
            .generics
            .function_paths
            .get(callee)
            .and_then(|path| self.symbols.lookup_module_func(path, &template.name))
            .cloned();
        let params: Vec<_> = imported.as_ref().map_or_else(
            || template.params.iter().map(|p| p.ty.clone()).collect(),
            |info| info.params.clone(),
        );
        let explicit_bindings: HashMap<_, _> = template
            .type_params
            .iter()
            .cloned()
            .zip(explicit.iter().cloned())
            .collect();
        let parameter_names: HashSet<_> = template.type_params.iter().map(String::as_str).collect();
        let fully_explicit = !explicit.is_empty() && explicit.len() == template.type_params.len();
        // A fixed value parameter may require an ordinary upcast or interface
        // box. Explicit arguments also fix parameters that contain T, so use
        // their substituted expected types for both context and inference.
        let contextual_params: Vec<_> = params
            .iter()
            .map(|ty| {
                let substituted = fully_explicit
                    .then(|| ty.substitute_names(|name| explicit_bindings.get(name).cloned()));
                let expected = substituted.as_ref().unwrap_or(ty);
                (!uses_parameters(expected, &parameter_names))
                    .then(|| self.normalize_type(expected, span))
            })
            .collect();
        let mut reference_places = Vec::with_capacity(call_args.len());
        let actual: Vec<_> = call_args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let is_reference = matches!(
                    template.params.get(i).map(|p| &p.mode),
                    Some(ParamMode::Reference { .. })
                ) && matches!(a.mode, CallArgMode::Reference { .. });
                let place = if is_reference {
                    self.reference_place_info(&a.expr, a.span)
                } else {
                    None
                };
                let result = if is_reference {
                    place
                        .as_ref()
                        .map_or_else(Self::error_type, |place| place.ty.clone())
                } else if let Some(Some(expected)) = contextual_params.get(i) {
                    self.check_expr_expecting(&a.expr, expected)
                } else {
                    self.check_expr(&a.expr)
                };
                reference_places.push(place);
                result.substitute_names(|n| self.generics.logical_types.get(n).cloned())
            })
            .collect();
        let inference_actual: Vec<_> = actual
            .iter()
            .enumerate()
            .map(|(i, ty)| {
                if let Some(Some(expected)) = contextual_params.get(i)
                    && template.params[i].mode == ParamMode::Value
                {
                    let actual = self.normalize_type(ty, span);
                    if self.types_compatible(expected, &actual) {
                        return expected.substitute_names(|name| {
                            self.generics.logical_types.get(name).cloned()
                        });
                    }
                }
                ty.clone()
            })
            .collect();
        let mut spellings = HashMap::new();
        let mut canonicalize = |ty: &Type| {
            ty.map_names(|name| {
                let identity = self.generic_type_identity(name);
                spellings
                    .entry(identity.clone())
                    .or_insert_with(|| name.clone());
                identity
            })
        };
        let inferred_explicit: Vec<_> = explicit.iter().map(&mut canonicalize).collect();
        let inferred_actual: Vec<_> = inference_actual.iter().map(&mut canonicalize).collect();
        let inferred_params: Vec<_> = params.iter().map(&mut canonicalize).collect();
        let bindings = match crate::semantic::generic_inference::infer(
            &template.type_params,
            &inferred_params,
            &inferred_actual,
            &inferred_explicit,
        ) {
            Ok(bindings) => bindings
                .into_iter()
                .map(|(name, ty)| {
                    (
                        name,
                        ty.map_names(|identity| {
                            spellings
                                .get(identity)
                                .cloned()
                                .unwrap_or_else(|| identity.clone())
                        }),
                    )
                })
                .collect::<HashMap<_, _>>(),
            Err(message) => {
                self.generic_error(span, message);
                return Some(Self::error_type());
            }
        };
        let param_infos: Vec<_> = template
            .params
            .iter()
            .zip(&actual)
            .map(|(param, ty)| ParamInfo {
                ty: self.normalize_type(ty, param.type_span),
                mode: param.mode.clone(),
                span: param.span,
                type_span: param.type_span,
            })
            .collect();
        for (((param, arg), actual), place) in param_infos
            .iter()
            .zip(call_args)
            .zip(&actual)
            .zip(reference_places)
        {
            match (&param.mode, &arg.mode) {
                (ParamMode::Value, CallArgMode::Reference { .. }) => {
                    self.push_unexpected_reference_arg(&param.ty, actual, arg.span)
                }
                (ParamMode::Reference { .. }, CallArgMode::Value) => {
                    self.push_missing_reference_arg(arg)
                }
                (ParamMode::Reference { mutable, .. }, CallArgMode::Reference { .. }) => {
                    if let Some(place) = place {
                        self.check_reference_argument_place(param, arg, *mutable, place);
                    }
                }
                (ParamMode::Value, CallArgMode::Value) => {}
            }
        }
        self.check_mut_reference_aliases(&param_infos, call_args);
        self.check_async_call_captures(&param_infos, call_args, template.is_async);
        let result = imported
            .as_ref()
            .map_or(&template.return_type, |info| &info.return_type)
            .substitute_names(|n| bindings.get(n).cloned());
        let result = self.normalize_type(&result, span);
        if self.generics.abstract_check {
            return Some(if template.is_async {
                Type::Generic("Task".into(), vec![result])
            } else {
                result
            });
        }
        let args: Vec<_> = template
            .type_params
            .iter()
            .map(|p| bindings[p].clone())
            .collect();
        if !self.check_instantiation_size(&args, span) {
            return Some(Self::error_type());
        }
        let identity_args: Vec<_> = args
            .iter()
            .map(|ty| ty.map_names(|n| self.generic_type_identity(n)))
            .collect();
        let key = (
            0,
            format!("{}::{}", template.span.file_id.0, template.name),
            identity_args.clone(),
        );
        let name = if let Some(name) = self.generics.instances.get(&key) {
            name.clone()
        } else {
            let name = symbol(&template.name, &identity_args);
            self.generics.instances.insert(key.clone(), name.clone());
            let Some(fresh) = self.reserve_instantiation(&key, span) else {
                return Some(Self::error_type());
            };
            if fresh {
                #[cfg(test)]
                {
                    self.generics.registry.borrow_mut().function_specializations += 1;
                }
                let mut f = (*template).clone();
                f.name = name.clone();
                f.type_params.clear();
                let (bindings, classes) =
                    self.instantiation_bindings(template.span.file_id, span, &bindings);
                let mut item = Item::Function(f);
                substitute_item(&mut item, &bindings);
                self.generics
                    .pending
                    .push_back(Instantiation { item, classes });
            }
            name
        };
        let target = self
            .generics
            .function_paths
            .get(callee)
            .map_or(name.clone(), |path| format!("{path}::{name}"));
        self.generics.calls.insert(id, target);
        Some(if template.is_async {
            Type::Generic("Task".into(), vec![result])
        } else {
            result
        })
    }

    pub(super) fn instantiate_class(
        &mut self,
        name: &str,
        args: &[Type],
        span: Span,
    ) -> Option<Type> {
        let template = self.generics.classes.get(name)?.clone();
        if !template.public && template.span.file_id != span.file_id {
            self.generic_error(span, format!("generic class `{name}` is private"));
            return Some(Self::error_type());
        }
        if args.len() != template.type_params.len() {
            self.generic_error(
                span,
                format!(
                    "generic class `{name}` expects {} type argument(s), got {}",
                    template.type_params.len(),
                    args.len()
                ),
            );
            return Some(Self::error_type());
        }
        let args: Vec<_> = args.iter().map(|t| self.normalize_type(t, span)).collect();
        for ty in &args {
            self.validate_type(ty, span);
        }
        if let Err(message) =
            crate::semantic::generic_inference::infer(&template.type_params, &[], &[], &args)
        {
            self.generic_error(span, message);
            return Some(Self::error_type());
        }
        if !self.check_instantiation_size(&args, span) {
            return Some(Self::error_type());
        }
        let identity_args: Vec<_> = args
            .iter()
            .map(|ty| ty.map_names(|n| self.generic_type_identity(n)))
            .collect();
        let key = (
            1,
            format!("{}::{}", template.span.file_id.0, template.name),
            identity_args.clone(),
        );
        if let Some(name) = self.generics.instances.get(&key) {
            return Some(Type::Named(name.clone()));
        }
        // Repeated concrete keys above terminate ordinary recursive layouts.
        // Bound changing recursion by resources rather than rejecting every
        // re-entry of a template: Link<String> -> Link<i64> can be finite.
        if self.generics.active_class_depth >= 64 {
            self.generic_error(span, "generic class instantiation depth exceeded (possible non-finite recursive expansion)");
            return Some(Self::error_type());
        }
        self.generics.active_class_depth += 1;
        let logical_name = self
            .symbols
            .lookup_class(name)
            .map_or_else(|| name.to_string(), |info| info.name.clone());
        let bindings = template
            .type_params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        let local_name = symbol(&template.name, &identity_args);
        let name = self
            .generics
            .class_paths
            .get(name)
            .map_or(local_name.clone(), |path| format!("{path}::{local_name}"));
        self.generics.instances.insert(key.clone(), name.clone());
        self.generics.logical_types.insert(
            name.clone(),
            Type::Generic(
                logical_name,
                args.iter()
                    .map(|arg| {
                        arg.substitute_names(|n| self.generics.logical_types.get(n).cloned())
                    })
                    .collect(),
            ),
        );
        let cached = self
            .generics
            .registry
            .borrow()
            .instantiated_classes
            .get(&key)
            .cloned();
        if let Some(cached) = cached {
            self.register_shared_instantiation_types(&cached.types);
            let mut info = cached.info.clone();
            info.name = name.clone();
            self.symbols.define_class(name.clone(), info);
            self.generics.active_class_depth -= 1;
            return Some(Type::Named(name));
        }
        if self.reserve_instantiation(&key, span) != Some(true) {
            self.generics.active_class_depth -= 1;
            return Some(Self::error_type());
        }
        #[cfg(test)]
        {
            self.generics.registry.borrow_mut().class_specializations += 1;
        }
        let mut class = if self.generics.abstract_check {
            class_signature(&template)
        } else {
            (*template).clone()
        };
        class.name = name.clone();
        class.type_params.clear();
        let (bindings, classes) =
            self.instantiation_bindings(template.span.file_id, span, &bindings);
        self.register_shared_instantiation_types(&classes);
        let mut item = Item::Class(class);
        substitute_item(&mut item, &bindings);
        let Item::Class(mut class) = item else {
            unreachable!()
        };
        let generation = self.error_generation;
        self.register_class(&class);
        self.generics.active_class_depth -= 1;
        if self.error_generation != generation {
            return Some(Self::error_type());
        }
        let info = self
            .symbols
            .lookup_class(&name)
            .expect("registered generic class")
            .clone();
        self.generics
            .registry
            .borrow_mut()
            .instantiated_classes
            .insert(
                key,
                Rc::new(InstantiatedClass {
                    info,
                    types: classes.clone(),
                }),
            );
        if !self.generics.abstract_check {
            class.name = local_name;
            self.generics.pending.push_back(Instantiation {
                item: Item::Class(class),
                classes,
            });
        }
        Some(Type::Named(name))
    }

    pub(crate) fn validate_generic_templates(&mut self, file: crate::diagnostics::FileId) {
        // Abstract checking has a separate symbol/output epoch: no symbolic
        // type or speculative body can escape into the concrete artifact set.
        let mut checker = TypeChecker::new();
        *checker.symbols = self.symbols.clone();
        checker.resolution = self.resolution.clone();
        checker.generics.functions = self.generics.functions.clone();
        checker.generics.classes = self.generics.classes.clone();
        checker.generics.function_paths = self.generics.function_paths.clone();
        checker.generics.class_paths = self.generics.class_paths.clone();
        checker.generics.registry.borrow_mut().module_paths =
            Rc::clone(&self.generics.registry.borrow().module_paths);
        checker.generics.abstract_check = true;
        let functions: Vec<_> = self.generics.functions.values().cloned().collect();
        let classes: Vec<_> = self.generics.classes.values().cloned().collect();
        for f in functions {
            if f.span.file_id != file {
                continue;
            }
            for name in &f.type_params {
                if name == &f.name
                    || checker.symbols.lookup_class(name).is_some()
                    || checker.symbols.lookup_enum(name).is_some()
                    || checker.symbols.lookup_interface(name).is_some()
                    || checker.generics.classes.contains_key(name)
                {
                    checker.generic_error(
                        f.span,
                        format!("type parameter `{name}` shadows an existing type or declaration"),
                    );
                }
            }
            checker.local.declared_type_params = f.type_params.clone();
            checker.check_function(&f);
            checker.generics.pending.clear();
        }
        for c in classes {
            if c.span.file_id != file {
                continue;
            }
            for name in &c.type_params {
                if name == &c.name
                    || checker.symbols.lookup_class(name).is_some()
                    || checker.symbols.lookup_enum(name).is_some()
                    || checker.symbols.lookup_interface(name).is_some()
                    || checker.generics.classes.contains_key(name)
                {
                    checker.generic_error(
                        c.span,
                        format!("type parameter `{name}` shadows an existing type or declaration"),
                    );
                }
            }
            checker.local.declared_type_params = c.type_params.clone();
            checker.register_class(&c);
            checker.check_class(&c);
            checker.generics.pending.clear();
        }
        self.errors.extend(checker.errors);
    }

    pub(crate) fn check_generic_instance(&mut self, instance: Instantiation) {
        self.register_shared_instantiation_types(&instance.classes);
        let item = instance.item;
        match &item {
            Item::Function(f) => {
                let params = self.normalize_param_types(&f.params);
                let param_infos = self.normalize_param_infos(&f.params);
                let return_type = self.normalize_type(&f.return_type, f.span);
                self.symbols.define_func(
                    f.name.clone(),
                    FuncInfo {
                        foreign: false,
                        params,
                        param_infos,
                        return_type,
                        public: f.public,
                        is_async: f.is_async,
                        declaration_span: f.span,
                        module_path: None,
                        constant: None,
                    },
                );
                self.check_function(f);
            }
            Item::Class(c) => {
                self.register_class(c);
                self.check_class(c);
            }
            _ => unreachable!(),
        }
        self.generics.emitted.push(item);
    }

    pub(crate) fn expand_generics(&mut self, program: &mut Program, ids: &mut FreshIds) {
        let mut imports = Vec::new();
        let mut modules = HashSet::new();
        for import in std::mem::take(&mut program.imports) {
            let local = import
                .alias
                .as_deref()
                .unwrap_or_else(|| import.path.rsplit("::").next().unwrap_or(&import.path));
            let path = self
                .generics
                .function_paths
                .get(local)
                .or_else(|| self.generics.class_paths.get(local));
            if let Some(path) = path
                && let Some((module, _)) = import.path.rsplit_once("::")
            {
                if modules.insert(path.clone()) {
                    imports.push(ImportDecl {
                        path: module.to_string(),
                        alias: Some(path.clone()),
                        span: import.span,
                    });
                }
            } else {
                imports.push(import);
            }
        }
        program.imports = imports;
        program.items.retain(|item| match item {
            Item::Function(f) => f.type_params.is_empty(),
            Item::Class(c) => c.type_params.is_empty(),
            _ => true,
        });
        program.items.append(&mut self.generics.emitted);
        program.instantiation_types = self
            .generics
            .imported_types
            .iter()
            .map(|(name, info)| (name.clone(), info.as_ref().clone()))
            .collect();
        program.instantiation_types.sort_by(|a, b| a.0.cmp(&b.0));
        for item in &mut program.items {
            visit_item(
                item,
                &mut |ty| {
                    self.normalized_types
                        .get(ty)
                        .cloned()
                        .unwrap_or_else(|| ty.clone())
                },
                &mut |expr| {
                    if let Some(name) = self.generics.calls.get(&expr.id()) {
                        if let Expr::Call(c) = expr {
                            if let Some((class, method)) = name.rsplit_once("::") {
                                *expr = Expr::StaticCall(Box::new(StaticCallExpr {
                                    id: c.id,
                                    class: class.into(),
                                    method: method.into(),
                                    type_args: Vec::new(),
                                    args: std::mem::take(&mut c.args),
                                    span: c.span,
                                    method_span: c.span,
                                }));
                            } else {
                                c.callee = name.clone();
                                c.type_args.clear();
                            }
                        } else if let Expr::StaticCall(c) = expr
                            && let Some((class, method)) = name.rsplit_once("::")
                        {
                            c.class = class.into();
                            c.method = method.into();
                            c.type_args.clear();
                        }
                    }
                    if let Some(name) = self.generics.objects.get(&expr.id()) {
                        match expr {
                            Expr::ObjectLiteral(c) => {
                                c.class = name.clone();
                                c.type_args.clear();
                            }
                            Expr::New(c) => {
                                c.class_name = name.clone();
                                c.type_args.clear();
                            }
                            Expr::StaticCall(c) => {
                                c.class = name.clone();
                                c.type_args.clear();
                            }
                            _ => {}
                        }
                    }
                },
                true,
                false,
                ids,
            );
        }
    }
}

fn substitute_item(item: &mut Item, bindings: &HashMap<String, Type>) {
    visit_item(
        item,
        &mut |ty| crate::semantic::generic_inference::substitute(ty, bindings),
        &mut |_| {},
        true,
        true,
        &mut FreshIds::default(),
    );
}

fn visit_item(
    item: &mut Item,
    ty: &mut impl FnMut(&Type) -> Type,
    expr: &mut impl FnMut(&mut Expr),
    fresh: bool,
    preserve_interface_defaults: bool,
    ids: &mut FreshIds,
) {
    let mut blocks = Vec::new();
    let mut initializers = Vec::new();
    match item {
        Item::Function(f) => {
            for p in &mut f.params {
                p.ty = ty(&p.ty);
            }
            f.return_type = ty(&f.return_type);
            blocks.push(&mut f.body);
        }
        Item::Class(c) => {
            for t in &mut c.implements {
                *t = ty(t);
            }
            for f in &mut c.fields {
                f.ty = ty(&f.ty);
                if let Some(e) = &mut f.initializer {
                    initializers.push(e);
                }
            }
            for m in &mut c.methods {
                // Nongeneric interface defaults retain their defining body and scope.
                // The final program-wide ID pass remaps all shared copies together.
                if preserve_interface_defaults && m.is_default_injected {
                    continue;
                }
                for p in &mut m.params {
                    p.ty = ty(&p.ty);
                }
                m.return_type = ty(&m.return_type);
                blocks.push(&mut m.body);
            }
            for c in &mut c.constructors {
                for p in &mut c.params {
                    p.ty = ty(&p.ty);
                }
                blocks.push(&mut c.body);
            }
        }
        Item::Enum(e) => {
            for v in &mut e.variants {
                for t in &mut v.payload {
                    *t = ty(t);
                }
            }
        }
        Item::Interface(i) => {
            for m in &mut i.methods {
                for p in &mut m.params {
                    p.ty = ty(&p.ty);
                }
                m.return_type = ty(&m.return_type);
                if let Some(b) = &mut m.default_body {
                    blocks.push(b);
                }
            }
        }
    }
    let mut nodes: Vec<_> = initializers.into_iter().map(NodeMut::Expr).collect();
    for b in blocks {
        if fresh {
            b.id = ids.body(b.id);
        }
        nodes.extend(b.stmts.iter_mut().map(NodeMut::Stmt));
    }
    while let Some(node) = nodes.pop() {
        match node {
            NodeMut::Expr(e) => {
                expr(e);
                match e {
                    Expr::Call(c) => {
                        for t in &mut c.type_args {
                            *t = ty(t);
                        }
                    }
                    Expr::StaticCall(c) => {
                        for t in &mut c.type_args {
                            *t = ty(t);
                        }
                    }
                    Expr::New(c) => {
                        for t in &mut c.type_args {
                            *t = ty(t);
                        }
                    }
                    Expr::ObjectLiteral(c) => {
                        for t in &mut c.type_args {
                            *t = ty(t);
                        }
                    }
                    Expr::Lambda(l) => {
                        for p in &mut l.params {
                            if let Some(t) = &mut p.ty {
                                *t = ty(t);
                            }
                        }
                        if let Some(t) = &mut l.return_type {
                            *t = ty(t);
                        }
                        if fresh && let LambdaBody::Block(b) = &mut l.body {
                            b.id = ids.body(b.id);
                        }
                    }
                    _ => {}
                }
                if fresh {
                    fresh_expr(e, ids);
                }
                NodeMut::Expr(e).for_each_child(|n| nodes.push(n));
            }
            NodeMut::Stmt(s) => {
                if let Stmt::Let(l) = s
                    && let Some(t) = &mut l.ty
                {
                    *t = ty(t);
                }
                if fresh {
                    match s {
                        Stmt::If(v) => {
                            v.then_block.id = ids.body(v.then_block.id);
                            if let Some(b) = &mut v.else_block {
                                b.id = ids.body(b.id);
                            }
                        }
                        Stmt::While(v) => v.body.id = ids.body(v.body.id),
                        Stmt::For(v) => v.body.id = ids.body(v.body.id),
                        Stmt::Lock(v) => v.body.id = ids.body(v.body.id),
                        Stmt::Defer(v) => {
                            if let DeferBody::Block(b) = &mut v.body {
                                b.id = ids.body(b.id);
                            }
                        }
                        _ => {}
                    }
                }
                NodeMut::Stmt(s).for_each_child(|n| nodes.push(n));
            }
        }
    }
}
fn fresh_expr(e: &mut Expr, ids: &mut FreshIds) {
    let slot = match e {
        Expr::Integer(_, _, id)
        | Expr::Float(_, _, id)
        | Expr::Bool(_, _, id)
        | Expr::String(_, _, id)
        | Expr::Var(_, _, id)
        | Expr::FieldAccess(_, _, _, id)
        | Expr::Print(_, _, _, id)
        | Expr::TryPropagate(_, _, id)
        | Expr::ArrayLiteral(_, _, id)
        | Expr::Index(_, _, _, id) => id,
        Expr::Binary(v) => &mut v.id,
        Expr::Unary(v) => &mut v.id,
        Expr::Call(v) => &mut v.id,
        Expr::MethodCall(v) => &mut v.id,
        Expr::StaticCall(v) => &mut v.id,
        Expr::StaticField(v) => &mut v.id,
        Expr::New(v) => &mut v.id,
        Expr::ObjectLiteral(v) => &mut v.id,
        Expr::Await(v) => &mut v.id,
        Expr::Ternary(v) => &mut v.id,
        Expr::Range(v) => &mut v.id,
        Expr::Lambda(v) => &mut v.id,
        Expr::Select(v) => {
            for c in &mut v.cases {
                c.body.id = ids.body(c.body.id);
            }
            &mut v.id
        }
        Expr::Match(v) => {
            for a in &mut v.arms {
                let id = match &mut a.pattern {
                    Pattern::Wildcard(_, id)
                    | Pattern::LiteralBool(_, _, id)
                    | Pattern::LiteralInt(_, _, id)
                    | Pattern::Binding { id, .. }
                    | Pattern::EnumVariant { id, .. }
                    | Pattern::EnumVariantTuple { id, .. }
                    | Pattern::ClassDowncast { id, .. } => id,
                };
                *id = ids.pattern(*id);
                if let MatchBody::Block(b) = &mut a.body {
                    b.id = ids.body(b.id);
                }
            }
            &mut v.id
        }
    };
    *slot = ids.expr(*slot);
}
