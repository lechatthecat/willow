//! Concrete instantiations discovered by semantic checking, before persistent
//! body queries are registered. The final pipeline checks the expanded program.
use super::precheck_journal::{Map, Set};
use super::*;
use crate::parser::ownership::NodeMut;
use crate::semantic::symbols::{EnumInfo, FuncInfo, InstantiationType, InterfaceInfo};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::rc::Rc;

type InstantiationKey = (u8, String, Vec<Type>);
type ImportedTypes = Vec<(String, Rc<InstantiationType>)>;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct InstantiatedClass {
    info: ClassInfo,
    types: ImportedTypes,
}

/// Concrete metadata shared by independently requested method bodies.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct DeferredClass {
    template: String,
    shell: ClassDecl,
    bindings: HashMap<String, Type>,
}

/// One owned template per source declaration across every checker in a build.
pub(crate) struct TemplateRegistry {
    pub capture_source_references: bool,
    pub module_paths: Rc<Map<crate::diagnostics::FileId, String>>,
    requested: Map<InstantiationKey, ()>,
    nominal_types: Map<String, Rc<InstantiationType>>,
    instantiated_classes: Map<InstantiationKey, Rc<InstantiatedClass>>,
    requested_methods: Map<(crate::diagnostics::FileId, String, String), ()>,
    deferred_classes: Map<(crate::diagnostics::FileId, String), Rc<DeferredClass>>,
    method_templates: Map<(crate::diagnostics::FileId, String, String), Rc<MethodDecl>>,
    functions: Map<(crate::diagnostics::FileId, String), Rc<FunctionDecl>>,
    // Read-only declaration identities are shared with abstract checking; its
    // speculative instances still use a separate registry.
    classes: Rc<Map<(crate::diagnostics::FileId, String), Rc<ClassDecl>>>,
    /// Generic interfaces with default bodies, by declaring file and name: a
    /// class in another unit receives their defaults (willow-s71l).
    interfaces: Map<(crate::diagnostics::FileId, String), Rc<InterfaceDecl>>,
    /// Each concrete instantiation of such an interface, checked once in its
    /// declaring unit and shared by every receiving class.
    interface_instances: Map<InstantiationKey, Rc<InterfaceDecl>>,
    #[cfg(test)]
    interface_specializations: usize,
    #[cfg(test)]
    function_specializations: usize,
    #[cfg(test)]
    class_specializations: usize,
    #[cfg(test)]
    class_requests: usize,
    #[cfg(test)]
    function_copies: usize,
    #[cfg(test)]
    class_copies: usize,
    /// Template provenance of instantiated bodies and field initializers,
    /// instance id -> template id. Revision tracking binds each generated
    /// body to its template's source syntax.
    pub origins: GeneratedOrigins,
    /// Instance segment -> source spelling with concrete type arguments.
    /// Shared across units; keys keep linker identity out of runtime reports.
    pub runtime_names: Map<String, String>,
}
impl Default for TemplateRegistry {
    fn default() -> Self {
        Self {
            capture_source_references: Default::default(),
            module_paths: Rc::new(Map::new("registry.module_paths")),
            requested: Map::new("registry.requested"),
            nominal_types: Map::new("registry.nominal_types"),
            instantiated_classes: Map::new("registry.instantiated_classes"),
            requested_methods: Map::new("registry.requested_methods"),
            deferred_classes: Map::new("registry.deferred_classes"),
            method_templates: Map::new("registry.method_templates"),
            functions: Map::new("registry.functions"),
            classes: Rc::new(Map::new("registry.classes")),
            interfaces: Map::new("registry.interfaces"),
            interface_instances: Map::new("registry.interface_instances"),
            #[cfg(test)]
            interface_specializations: Default::default(),
            #[cfg(test)]
            function_specializations: Default::default(),
            #[cfg(test)]
            class_specializations: Default::default(),
            #[cfg(test)]
            class_requests: Default::default(),
            #[cfg(test)]
            function_copies: Default::default(),
            #[cfg(test)]
            class_copies: Default::default(),
            origins: Default::default(),
            runtime_names: Map::new("registry.runtime_names"),
        }
    }
}

pub(crate) struct GeneratedOrigins {
    pub bodies: Map<BodyId, BodyId>,
    pub initializers: Map<ExprId, ExprId>,
}
impl Default for GeneratedOrigins {
    fn default() -> Self {
        Self {
            bodies: Map::new("origins.bodies"),
            initializers: Map::new("origins.initializers"),
        }
    }
}

impl TemplateRegistry {
    fn reserve(&mut self, key: &InstantiationKey) -> Result<bool, ()> {
        if self.requested.contains_key(key) {
            return Ok(false);
        }
        if self.requested.raw().len() >= 4096 {
            let _ = self.requested.len();
            return Err(());
        }
        self.requested.insert(key.clone(), ());
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

    /// Retain a generic interface whose defaults other units may instantiate.
    pub(crate) fn intern_interface(&mut self, declaration: &InterfaceDecl) {
        self.interfaces
            .entry((declaration.span.file_id, declaration.name.clone()))
            .or_insert_with(|| Rc::new(declaration.clone()));
    }

    pub(crate) fn intern_class(&mut self, declaration: &ClassDecl) -> Rc<ClassDecl> {
        let key = (declaration.span.file_id, declaration.name.clone());
        if let Some(template) = self.classes.get(&key) {
            return Rc::clone(template);
        }
        for method in &declaration.methods {
            self.method_templates.insert(
                (
                    declaration.span.file_id,
                    declaration.name.clone(),
                    method.name.clone(),
                ),
                Rc::new(method.clone()),
            );
        }
        Rc::make_mut(&mut self.classes)
            .entry(key)
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

pub(crate) struct Generics {
    pub registry: Rc<RefCell<TemplateRegistry>>,
    pub abstract_check: bool,
    /// Opaque parameters of the template currently being validated.
    pub parameter_names: HashSet<String>,
    /// Source calls rejected on an opaque declaration parameter. Concrete
    /// checking reuses the rejection instead of exposing specialization types.
    pub rejected_parameter_methods: Set<Span>,
    pub source_symbols: crate::semantic::analysis_symbols::Facts,
    pub functions: Map<String, std::rc::Rc<FunctionDecl>>,
    pub classes: Map<String, std::rc::Rc<ClassDecl>>,
    pub instances: Map<InstantiationKey, String>,
    pub pending: std::collections::VecDeque<Instantiation>,
    pub imported_types: Map<String, Rc<InstantiationType>>,
    pub emitted: Vec<Item>,
    pub logical_types: Map<String, Type>,
    /// Instantiation nominal aliases (`$type$mono$...`) to the module-qualified
    /// identity they hash, so analysis can spell an instance's types.
    pub nominal_aliases: Map<String, String>,
    pub active_class_depth: usize,
    pub calls: Map<ExprId, String>,
    pub function_paths: Map<String, String>,
    pub class_paths: Map<String, String>,
    pub objects: Map<ExprId, String>,
    /// Interface defaults this checker's classes received from a generic
    /// interface another unit declares, by (class, method): each is replaced
    /// by the default at that method index of the instance checked in that
    /// unit (willow-s71l).
    pub default_copies: Map<String, HashMap<String, (Rc<InterfaceDecl>, usize)>>,
}
impl Default for Generics {
    fn default() -> Self {
        Self {
            registry: Default::default(),
            abstract_check: Default::default(),
            parameter_names: Default::default(),
            rejected_parameter_methods: Set::new("generic.rejected_parameter_methods"),
            source_symbols: Default::default(),
            functions: Map::new("generic.functions"),
            classes: Map::new("generic.classes"),
            instances: Map::new("generic.instances"),
            pending: Default::default(),
            imported_types: Map::new("generic.imported_types"),
            emitted: Default::default(),
            logical_types: Map::new("generic.logical_types"),
            nominal_aliases: Map::new("generic.nominal_aliases"),
            active_class_depth: Default::default(),
            calls: Map::new("generic.calls"),
            function_paths: Map::new("generic.function_paths"),
            class_paths: Map::new("generic.class_paths"),
            objects: Map::new("generic.objects"),
            default_copies: Map::new("generic.default_copies"),
        }
    }
}

impl Generics {
    /// Each generic class template this checker can name, by spelling, to its
    /// declaration span. Analysis needs it after expansion drops templates.
    pub(crate) fn class_template_spans(&self) -> HashMap<String, Span> {
        self.classes
            .iter()
            .map(|(name, template)| (name.clone(), template.span))
            .collect()
    }
}

enum InstantiationTypeRef<'a> {
    Class(&'a ClassInfo),
    Enum(&'a EnumInfo),
    Interface(&'a InterfaceInfo),
}

#[cfg(test)]
thread_local! {
    pub(super) static GENERIC_ENUM_ARGUMENT_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static GENERIC_CALL_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static EXPECTED_RESULT_HINTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
thread_local! {
    static CONSTRAINED_BODY_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
    fn constrained_method_body_work_scales_with_selected_bodies() {
        for methods in [1, 8, 32] {
            for statements in [1, 8, 32] {
                let bodies: String = (0..methods)
                    .map(|i| {
                        format!(
                            "pub fn m{i}() -> i64 where T: i64 | String {{ {} return 1; }}",
                            "let x = 1;".repeat(statements)
                        )
                    })
                    .collect();
                // Distinct names keep the body valid while preserving its shape.
                let bodies = bodies.replace("let x = 1;", "1;");
                let program = parse(&format!("class Buffer<T> {{ {bodies} }}"));
                CONSTRAINED_BODY_VISITS.with(|count| count.set(0));
                let mut checker = TypeChecker::new();
                checker.check_program(&program);
                assert!(
                    checker.errors.iter().all(|d| d.severity != Severity::Error),
                    "{:?}",
                    checker.errors
                );
                let visits = CONSTRAINED_BODY_VISITS.with(|count| count.get());
                assert_eq!(visits, 2 * methods * (statements + 1));
                eprintln!(
                    "methods={methods} statements={statements} substituted_expressions={visits}"
                );
            }
        }
    }

    #[test]
    fn generic_diagnostic_class_name_index_scaling() {
        for templates in [1, 16, 256] {
            for parameters in [1, 4, 16] {
                let names = (0..parameters)
                    .map(|i| format!("P{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let program = parse(&format!("class Heap<{names}> {{}}"));
                let Item::Class(class) = &program.items[0] else {
                    unreachable!()
                };
                let mut checker = TypeChecker::new();
                checker.register_class(class);
                let template = checker.generics.registry.borrow_mut().intern_class(class);
                checker.generics.classes.insert("Heap".into(), template);
                for i in 1..templates {
                    let mut other = class.clone();
                    other.name = format!("Other{i}");
                    checker.generics.registry.borrow_mut().intern_class(&other);
                }
                // Force the file/name index fallback for a canonical instance.
                let canonical = format!("$local::module::Heap$mono${:064x}", 7);
                let mut info = checker.symbols.lookup_class("Heap").unwrap().clone();
                info.name = canonical.clone();
                checker.symbols.define_class(canonical.clone(), info);
                let mut queries = 0;
                for _ in 0..64 {
                    assert_eq!(
                        checker.diagnostic_class_name("Heap"),
                        format!("Heap<{names}>")
                    );
                    assert_eq!(
                        checker.diagnostic_class_name(&canonical),
                        format!("$local::module::Heap<{names}>")
                    );
                    queries += 2;
                }
                assert_eq!(queries, 128);
                assert_eq!(checker.generics.registry.borrow().classes.len(), templates);
                eprintln!(
                    "generic name index: templates={templates} parameters={parameters} queries={queries} retained_templates={templates}"
                );
            }
        }
    }

    #[test]
    fn generic_diagnostic_declaration_index_is_shared_without_speculative_state() {
        let program = parse("class Heap<T> {} class Other<U> {}");
        let Item::Class(heap) = &program.items[0] else {
            unreachable!()
        };
        let Item::Class(other) = &program.items[1] else {
            unreachable!()
        };
        let mut source = TemplateRegistry::default();
        source.intern_class(heap);
        let mut abstract_registry = TemplateRegistry {
            classes: Rc::clone(&source.classes),
            ..Default::default()
        };
        for _ in 0..64 {
            source.intern_class(heap);
            assert!(Rc::ptr_eq(&source.classes, &abstract_registry.classes));
        }
        assert_eq!(source.class_copies, 1);
        abstract_registry
            .requested
            .insert((1, "speculative".into(), vec![Type::I64]), ());
        assert!(source.requested.is_empty());
        abstract_registry.intern_class(other);
        assert_eq!(source.classes.len(), 1);
        assert_eq!(abstract_registry.classes.len(), 2);
        assert!(!Rc::ptr_eq(&source.classes, &abstract_registry.classes));
    }

    #[test]
    fn abstract_source_references_are_opt_in_and_resolved_once() {
        for n in [1, 16, 256] {
            let source = format!(
                "fn fold<T>(x: T) -> T {{ return x; }} fn wrapper<T>(x: T) -> T {{ {} return x; }} fn main() {{}}",
                "fold(x);".repeat(n),
            );
            let program = parse(&source);
            let Item::Function(fold) = &program.items[0] else {
                unreachable!()
            };
            for enabled in [false, true] {
                let mut checker = TypeChecker::new();
                checker
                    .generics
                    .registry
                    .borrow_mut()
                    .capture_source_references = enabled;
                checker.check_program(&program);
                assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                let references = &checker.generics.source_symbols.references;
                let calls: Vec<_> = references
                    .iter()
                    .filter(|r| r.role == "call" && r.target.kind == "function")
                    .collect();
                assert_eq!(calls.len(), if enabled { n } else { 0 });
                assert!(
                    calls
                        .iter()
                        .all(|r| r.written == "fold" && r.target.span == fold.span)
                );
                assert!(checker.generics.pending.is_empty());
                assert!(checker.generics.emitted.is_empty());
                println!(
                    "abstract_calls={n} enabled={enabled} resolved_references={} queued_instances=0",
                    calls.len()
                );
            }
        }
    }
    #[test]
    fn nested_deferred_lambdas_check_each_generic_call_once() {
        // Each level's lambda is typed after its sibling argument and holds the
        // next generic call, so re-checking a deferred lambda would grow the
        // count exponentially with depth (willow-jz15.64).
        for depth in [1, 4, 8, 16, 32] {
            let mut expression = format!("x{} + 1", depth - 1);
            for level in (0..depth).rev() {
                let input = if level == 0 {
                    "1".to_string()
                } else {
                    format!("x{}", level - 1)
                };
                expression = format!("apply({input}, |x{level}| {expression})");
            }
            let source = format!(
                "fn apply<T, U>(x: T, f: closure(T) -> U) -> U {{ return f(x); }} fn main() {{ let n: i64 = {expression}; println(n); }}"
            );
            let mut checker = TypeChecker::new();
            GENERIC_CALL_CHECKS.with(|count| count.set(0));
            checker.check_program(&parse(&source));
            assert!(checker.errors.is_empty(), "{:?}", checker.errors);
            let visits = GENERIC_CALL_CHECKS.with(|count| count.get());
            assert_eq!(visits, depth);
            println!(
                "WILLOW_GENERIC_AUDIT {}",
                serde_json::json!({"case":"nested_deferred_lambda", "depth":depth, "generic_call_checks":visits})
            );
        }
    }
    #[test]
    fn generic_caller_chains_check_each_generic_call_once() {
        // Every link is a generic function that calls `fold` and the previous
        // link with unannotated lambdas typed from its own `T` (willow-jz15.97).
        for depth in [1, 4, 16, 64] {
            let mut source = "import std::collections::Array; fn fold<T, A>(xs: Array<T>, init: A, f: closure(A, T) -> A) -> A { let mut acc = init; for x in xs { acc = f(acc, x); } return acc; }".to_string();
            for link in 0..depth {
                let previous = if link == 0 {
                    "0".to_string()
                } else {
                    format!("c{}(xs, |y| p(y))", link - 1)
                };
                source.push_str(&format!(
                    " fn c{link}<T>(xs: Array<T>, p: closure(T) -> bool) -> i64 {{ return fold(xs, 0, |n, x| if p(x) {{ n + 1 }} else {{ n }}) + {previous}; }}"
                ));
            }
            source.push_str(&format!(
                " fn main() {{ println(c{}([1, 2], |v| v > 1)); }}",
                depth - 1
            ));
            let mut checker = TypeChecker::new();
            GENERIC_CALL_CHECKS.with(|count| count.set(0));
            checker.check_program(&parse(&source));
            assert!(checker.errors.is_empty(), "{:?}", checker.errors);
            let visits = GENERIC_CALL_CHECKS.with(|count| count.get());
            // `fold` in every link, the previous link in all but the first,
            // and the call in `main`.
            assert_eq!(visits, 2 * depth);
            println!(
                "WILLOW_GENERIC_AUDIT {}",
                serde_json::json!({"case":"generic_caller_chain", "depth":depth, "generic_call_checks":visits})
            );
        }
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
    fn reachable_method_specialization_counts_ignore_unused_body_width() {
        for methods in [1, 8, 32] {
            for width in [1, 16, 64] {
                let unused: String = (0..methods)
                    .map(|n| format!("pub fn unused{n}() {{ {} }}", "println(0);".repeat(width)))
                    .collect();
                let source = format!(
                    "class Cell<T> {{ pub value: T; pub fn get() -> T {{ return self.value; }} {unused} }} fn main() {{ let a = new Cell<i64>(7); println(a.get()); println(a.get()); }}"
                );
                let mut checker = TypeChecker::new();
                checker.check_program(&parse(&source));
                let mut bodies = 0;
                while let Some(instance) = checker.generics.pending.pop_front() {
                    bodies += usize::from(instance.method_only);
                    checker.check_generic_instance(instance);
                }
                assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                assert_eq!(bodies, 1, "methods={methods} width={width}");
                let emitted: usize = checker
                    .generics
                    .emitted
                    .iter()
                    .map(|item| {
                        if let Item::Class(c) = item {
                            c.methods.len()
                        } else {
                            0
                        }
                    })
                    .sum();
                assert_eq!(emitted, 1);
                println!(
                    "methods={methods} body_width={width} specialized={bodies} emitted={emitted}"
                );
            }
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
    fn generic_lambda_annotations_reuse_class_specializations() {
        for uses in [1, 8, 32] {
            for depth in [1, 4, 8] {
                let mut ty = "i64".to_string();
                for _ in 0..depth {
                    ty = format!("Wrap<{ty}>");
                }
                let mut body = String::new();
                for index in 0..uses {
                    body.push_str(&format!("let f{index} = |x: {ty}| x;"));
                }
                let source = format!("class Wrap<T> {{ pub v: T; }} fn main() {{ {body} }}");
                let mut checker = TypeChecker::new();
                checker.check_program(&parse(&source));
                assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                let registry = checker.generics.registry.borrow();
                assert_eq!(registry.class_requests, uses * depth);
                assert_eq!(registry.class_specializations, depth);
                // `Wrap` and the bundled `Array` template are each copied once.
                assert_eq!(registry.class_copies, 2);
                println!(
                    "lambda annotations: uses={uses} depth={depth} requests={} specializations={} copies={}",
                    registry.class_requests, registry.class_specializations, registry.class_copies
                );
            }
        }
    }

    #[test]
    fn imported_member_access_scaling_reuses_specialization() {
        for width in [1, 16, 64] {
            let template = parse(&format!(
                "pub class Box<T> {{ pub value: T; pub fn get(self) -> T {{ {} return self.value; }} }} pub fn make() -> Box<i64> {{ return new Box<i64>(1); }}",
                "println(1);".repeat(width)
            ));
            for consumers in [1, 8, 32] {
                for uses in [1, 16, 64] {
                    let registry = Rc::new(RefCell::new(TemplateRegistry::default()));
                    let mut pending = 0;
                    for _ in 0..consumers {
                        let mut checker = TypeChecker::new();
                        checker.generics.registry = registry.clone();
                        checker.register_module("boxes", "boxes", "boxes.wi", &template);
                        let entry = parse(&format!(
                            "fn main() {{ {} }}",
                            "println(boxes::make().value);".repeat(uses)
                        ));
                        checker.check_program(&entry);
                        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                        pending += checker.generics.pending.len();
                    }
                    let registry = registry.borrow();
                    assert_eq!(registry.class_requests, consumers * uses);
                    assert_eq!(registry.class_specializations, 1);
                    // `Box` and the bundled `Array` template are each copied once.
                    assert_eq!(registry.class_copies, 2);
                    assert_eq!(pending, 1);
                    println!(
                        "WILLOW_GENERIC_MEMBER_AUDIT {}",
                        serde_json::json!({
                            "body_width": width, "consumers": consumers, "uses_per_consumer": uses,
                            "class_requests": registry.class_requests,
                            "specializations": registry.class_specializations,
                            "template_copies": registry.class_copies, "queued_bodies": pending
                        })
                    );
                }
            }
        }
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
                assert_eq!(pending, 3, "width={width} consumers={count}");
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
    fn iflh_default_body_visits_are_not_added_by_import_collection() {
        use super::super::check_decls::DEFAULT_BODY_CHECKS;
        use crate::parser::import_usage::DEFAULT_BODY_PARSES;
        for width in [1, 8, 32] {
            for templates in [1, 8, 32] {
                for instantiated in [false, true] {
                    DEFAULT_BODY_PARSES.with(|v| v.set(0));
                    DEFAULT_BODY_CHECKS.with(|v| v.set(0));
                    let source = format!(
                        "import std::fs; {}",
                        (0..templates)
                            .map(|n| format!(
                                "interface I{n}<T> {{ fn f(self) {{ {} }} }}",
                                (0..width)
                                    .map(|i| format!("let text{i} = fs::read_to_string(\"x\");"))
                                    .collect::<String>()
                            ))
                            .collect::<String>()
                    );
                    let mut program = parse(&source);
                    assert_eq!(DEFAULT_BODY_PARSES.with(|v| v.get()), templates);
                    let mut checker = TypeChecker::new();
                    checker.check_program(&program);
                    assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                    assert_eq!(DEFAULT_BODY_CHECKS.with(|v| v.get()), 0);
                    if instantiated {
                        for item in &program.items {
                            let Item::Interface(interface) = item else {
                                unreachable!()
                            };
                            let interface = Rc::new(interface.clone());
                            // Repeated equivalent requests must share existing checks.
                            for _ in 0..8 {
                                let instance = checker
                                    .instantiate_interface_template(
                                        &interface,
                                        &[Type::I64],
                                        interface.span,
                                    )
                                    .unwrap();
                                assert!(instance.default_import_uses.is_empty());
                            }
                        }
                        while let Some(instance) = checker.generics.pending.pop_front() {
                            checker.check_generic_instance(instance);
                        }
                        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                    }
                    let checks = DEFAULT_BODY_CHECKS.with(|v| v.get());
                    assert_eq!(checks, if instantiated { templates } else { 0 });
                    assert_eq!(DEFAULT_BODY_PARSES.with(|v| v.get()), templates);
                    checker.expand_generics(&mut program, &mut FreshIds::default());
                    let mut final_checker = TypeChecker::new();
                    final_checker.source_imports_checked = true;
                    final_checker.check_program(&program);
                    assert!(
                        final_checker.errors.is_empty(),
                        "{:?}",
                        final_checker.errors
                    );
                    let total = DEFAULT_BODY_CHECKS.with(|v| v.get());
                    assert_eq!(total, 2 * checks);
                    assert_eq!(DEFAULT_BODY_PARSES.with(|v| v.get()), templates);
                    println!(
                        "iflh defaults={templates} width={width} instantiated={instantiated} parses={templates} generic_body_checks={checks} total_body_checks={total}"
                    );
                }
            }
        }
    }

    #[test]
    fn foreign_default_receivers_share_one_interface_instance() {
        // Every receiver of one instantiation reuses the provider's single
        // checked instance; only a new type argument adds one (willow-s71l).
        for width in [1, 16, 256] {
            let provider = parse(&format!(
                "pub interface Get<T> {{ fn raw(self) -> T; fn get(self) -> i64 {{ {} return 1; }} }}",
                "println(1);".repeat(width)
            ));
            let Item::Interface(template) = &provider.items[0] else {
                unreachable!()
            };
            let default = template.methods[1].default_body.clone().unwrap();
            for receivers in [1, 16, 256] {
                let registry = Rc::new(RefCell::new(TemplateRegistry::default()));
                registry.borrow_mut().intern_interface(template);
                let mut pending = 0;
                for consumer in 0..receivers {
                    let mut checker = TypeChecker::new();
                    checker.generics.registry = registry.clone();
                    checker.register_module("util", "util", "util.wi", &provider);
                    let arg = if consumer % 2 == 0 { "i64" } else { "String" };
                    let program = parse(&format!(
                        "class C implements util::Get<{arg}> {{ pub fn raw(self) -> {arg} {{ return self.raw(); }} pub fn get(self) -> i64 {{ return 0; }} }}"
                    ));
                    let Item::Class(class) = &program.items[0] else {
                        unreachable!()
                    };
                    let mut class = class.clone();
                    class.span.file_id = FileId(1);
                    let copy = &mut class.methods[1];
                    copy.body = default.clone();
                    copy.is_interface_default = true;
                    copy.is_default_injected = false;
                    let handled = checker.instantiate_foreign_defaults(&class);
                    assert!(handled.contains("get"), "{:?}", checker.errors);
                    pending += checker.generics.pending.len();
                }
                let distinct = receivers.min(2);
                let registry = registry.borrow();
                assert_eq!(registry.interface_specializations, distinct);
                assert_eq!(registry.interface_instances.len(), distinct);
                assert_eq!(pending, distinct);
                println!(
                    "WILLOW_GENERIC_AUDIT {}",
                    serde_json::json!({"case":"foreign_default_receivers", "body_width":width, "receivers":receivers, "interface_specializations":registry.interface_specializations, "queued_bodies":pending})
                );
            }
        }
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
                    checker.instantiation_bindings(Span::dummy(), &bindings);
                assert_eq!(substituted.len(), count);
                assert_eq!(metadata.len(), 1);
                let one = HashMap::from([("T".to_string(), Type::Named("Payload".into()))]);
                for _ in 0..count {
                    let (_, next) = checker.instantiation_bindings(Span::dummy(), &one);
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

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Instantiation {
    pub item: Item,
    pub classes: ImportedTypes,
    /// A class item containing one requested method, without a declaration
    /// header. Its class signature is registered by an earlier queue item.
    #[serde(default)]
    pub method_only: bool,
}

#[derive(Default)]
pub(crate) struct FreshIds {
    bodies: HashMap<BodyId, BodyId>,
    expressions: HashMap<ExprId, ExprId>,
    patterns: HashMap<PatternId, PatternId>,
}
impl FreshIds {
    /// Original id -> fresh id of every renumbered block.
    pub(crate) fn bodies(&self) -> &HashMap<BodyId, BodyId> {
        &self.bodies
    }
    /// Original id -> fresh id of every renumbered expression.
    pub(crate) fn expressions(&self) -> &HashMap<ExprId, ExprId> {
        &self.expressions
    }
    fn record_bodies(&self, origins: &mut GeneratedOrigins) {
        origins.bodies.extend(
            self.bodies
                .iter()
                .map(|(&template, &instance)| (instance, template)),
        );
    }
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
pub(super) fn class_signature(class: &ClassDecl) -> ClassDecl {
    let empty = |body: &Block| Block {
        id: body.id,
        stmts: Vec::new(),
        span: body.span,
    };
    ClassDecl {
        abi_storage: class.abi_storage.clone(),
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
                effect_contracts: m.effect_contracts.clone(),
                array_body_optimization: m.array_body_optimization,
                constraint: m.constraint.clone(),
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
    mentions(ty, |name| parameters.contains(name))
}

/// Whether any name in `ty`, a generic head included, satisfies `matches`.
pub(super) fn mentions(ty: &Type, matches: impl Fn(&str) -> bool) -> bool {
    let mut pending = vec![ty];
    while let Some(ty) = pending.pop() {
        match ty {
            Type::Named(name) if matches(name) => return true,
            Type::Array(element) | Type::FixedArray(element, _) => pending.push(element),
            Type::Generic(name, args) => {
                if matches(name) {
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
    format!("{name}$mono${:x}", crate::hex::Hex(&digest.finalize()))
}

#[willow_continuations::checker]
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
                Type::Array(element) | Type::FixedArray(element, _) => pending.push(element),
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
        span: Span,
        bindings: &HashMap<String, Type>,
    ) -> (HashMap<String, Type>, ImportedTypes) {
        let mut classes = HashMap::new();
        let bindings = bindings
            .iter()
            .map(|(param, ty)| {
                let ty = self.normalize_type(ty, span);
                let ty = ty.map_names(|name| {
                    // Builtins have stable identities in every unit. Aliasing
                    // Option/Result would hide their intrinsic methods when
                    // the concrete signature is checked again.
                    if matches!(
                        crate::semantic::builtin_types::BuiltinTypeId::from_name(name),
                        Some(
                            crate::semantic::builtin_types::BuiltinTypeId::Option
                                | crate::semantic::builtin_types::BuiltinTypeId::Result
                        )
                    ) {
                        return name.clone();
                    }
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
                    // Cached signatures can be consumed outside their owner.
                    // Carry every nominal argument through the shared type
                    // table instead of leaving an owner-local bare name.
                    let canonical = self.generic_nominal_identity(identity, declaration_span);
                    let alias = symbol("$type", &[Type::Named(canonical.clone())]);
                    // The alias hashes `canonical`, so every unit records the
                    // same module-qualified value for it.
                    if !self.generics.nominal_aliases.contains_key(&alias) {
                        self.generics
                            .nominal_aliases
                            .insert(alias.clone(), canonical);
                    }
                    classes.entry(alias.clone()).or_insert_with(|| {
                        let mut registry = self.generics.registry.borrow_mut();
                        if let Some(existing) = registry.nominal_types.get(&alias) {
                            return Rc::clone(existing);
                        }
                        let value = Rc::new(info.into_owned());
                        registry
                            .nominal_types
                            .insert(alias.clone(), Rc::clone(&value));
                        value
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

    /// A type in generic-inference form: nominal names by declaration
    /// identity, remembering the first spelling of each so bindings can be
    /// reported back in the caller's terms.
    fn canonical_generic_type(&self, ty: &Type, spellings: &mut HashMap<String, String>) -> Type {
        // Inference compares declaration shapes without instantiating symbolic
        // user classes. Source-backed ABI declarations must nevertheless use
        // the same storage shape as checked values. Their declaration owns
        // this substitution; source access was checked at the written type site.
        let mut storage = ty.clone();
        let mut pending = vec![&mut storage];
        while let Some(node) = pending.pop() {
            if let Type::Generic(name, args) = node {
                let template = self.generics.classes.get(name).or_else(|| {
                    let resolved = self.resolve_static_call_class_quiet(name)?;
                    self.generics
                        .classes
                        .get(&format!("$std::collections::{resolved}"))
                });
                if let Some(template) = template
                    && let Some(Type::Array(element)) = &template.abi_storage
                    && let Type::Named(parameter) = element.as_ref()
                    && args.len() == template.type_params.len()
                    && let Some(index) = template
                        .type_params
                        .iter()
                        .position(|name| name == parameter)
                {
                    // Move the subtree: cloning it at each nested source array
                    // would make normalization quadratic in nesting depth.
                    *node = Type::Array(Box::new(args.swap_remove(index)));
                }
            }
            match node {
                Type::Array(element) | Type::FixedArray(element, _) => pending.push(element),
                Type::Generic(_, args) => pending.extend(args.iter_mut()),
                Type::Fn(args, result) | Type::Closure(args, result) => {
                    pending.extend(args.iter_mut());
                    pending.push(result);
                }
                _ => {}
            }
        }
        storage.map_names(|name| {
            let identity = self.generic_type_identity(name);
            spellings
                .entry(identity.clone())
                .or_insert_with(|| name.clone());
            identity
        })
    }

    /// The deferred lambdas of a call rejected before inference starts still
    /// need their bodies checked and typed.
    fn check_deferred_lambdas_plainly(&mut self, call_args: &[CallArg], deferred: &[bool]) {
        for (arg, _) in call_args.iter().zip(deferred).filter(|(_, d)| **d) {
            if let Expr::Lambda(lambda) = &arg.expr {
                let ty = self.check_lambda(lambda);
                self.expr_types.insert(arg.expr.id(), ty);
            }
        }
    }

    /// An argument of a generic call inside an interface default, as
    /// inference sees it (willow-dnia). The receiver is unknown there, so no
    /// instantiation may be keyed by `Self`: a whole `self` is inferred as
    /// the interface it converts to, and `Self` inside another type, which no
    /// conversion reaches, is reported (`None`).
    fn generic_self_argument(&mut self, ty: Type, callee: &str, span: Span) -> Option<Type> {
        let Some(interface) = self.local.self_interface.clone() else {
            return Some(ty);
        };
        if matches!(&ty, Type::Named(name) if name == "Self") {
            return Some(interface);
        }
        if !mentions(&ty, |name| name == "Self") {
            return Some(ty);
        }
        let interface = type_name(&interface);
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0201,
                format!(
                    "`{}` cannot be passed to generic function `{callee}`: its `Self` is not a whole value that converts to `{interface}`",
                    type_name(&ty)
                ),
            )
            .with_label(Label::primary(span, "the implementing class is unknown here"))
            .with_help(format!(
                "convert `self` first (`let it: {interface} = self;`) and build the argument from `it`"
            )),
        );
        None
    }

    pub(super) fn generic_error(&mut self, span: Span, message: impl Into<String>) {
        self.push(
            Diagnostic::new(Severity::Error, ErrorCode::E0201, message)
                .with_label(Label::primary(span, "generic instantiation")),
        );
    }

    /// Preserve the enum head as context while its payload parameters are still
    /// unknown. Each payload is checked once; absent constraints remain holes
    /// until all call arguments have contributed to inference.
    pub(super) fn check_generic_enum_argument(
        &mut self,
        expr: &Expr,
        expected: &Type,
        parameters: &HashSet<&str>,
    ) -> Type {
        #[cfg(test)]
        GENERIC_ENUM_ARGUMENT_CHECKS.with(|count| count.set(count.get() + 1));
        if !uses_parameters(expected, parameters) {
            return self.check_expr_expecting(expr, expected);
        }
        if let (Expr::ArrayLiteral(elements, _, _), Type::FixedArray(element, _)) = (expr, expected)
        {
            let mut actual = crate::semantic::generic_inference::enum_hole();
            for item in elements {
                let ty = self.check_generic_enum_argument(item, element, parameters);
                if crate::semantic::generic_inference::is_enum_hole(&actual) {
                    actual = ty;
                } else if crate::semantic::generic_inference::has_enum_hole(&actual)
                    || crate::semantic::generic_inference::has_enum_hole(&ty)
                {
                    if let Some(merged) =
                        crate::semantic::generic_inference::merge_enum_holes(&actual, &ty)
                    {
                        actual = merged;
                    } else {
                        self.generic_error(
                            item.span(),
                            "fixed array elements must have the same type",
                        );
                    }
                } else if !self.types_compatible(&actual, &ty) {
                    self.generic_error(item.span(), "fixed array elements must have the same type");
                }
            }
            let ty = Type::FixedArray(Box::new(actual), elements.len() as u64);
            self.expr_types.insert(expr.id(), ty.clone());
            return ty;
        }
        if matches!(expected, Type::Generic(..) | Type::FixedArray(..))
            && matches!(expr, Expr::Ternary(_) | Expr::Match(_))
        {
            let context = expected.substitute_names(|name| {
                parameters
                    .contains(name.as_str())
                    .then(crate::semantic::generic_inference::enum_hole)
            });
            let ty = match expr {
                Expr::Ternary(t) => self.check_ternary_expecting(t, &context),
                Expr::Match(m) => {
                    let previous = self.local.match_expected.replace(context);
                    let ty = self.check_match_expr(m);
                    self.local.match_expected = previous;
                    ty
                }
                _ => unreachable!(),
            };
            self.expr_types.insert(expr.id(), ty.clone());
            return ty;
        }
        let Type::Generic(enum_name, expected_args) = expected else {
            return self.check_expr(expr);
        };
        // Only enum constructors produce inference holes. Ordinary values keep
        // their own type, even when their formal parameter is a generic enum.
        let (variant, args) = match expr {
            Expr::Call(call)
                if self.prelude_variant_name_is_unshadowed(&call.callee)
                    && self.enum_nameable_bare(enum_name) =>
            {
                (call.callee.as_str(), call.args.as_slice())
            }
            Expr::Var(name, _, _)
                if self.prelude_variant_name_is_unshadowed(name)
                    && self.enum_nameable_bare(enum_name) =>
            {
                (name.as_str(), &[][..])
            }
            Expr::StaticCall(call)
                if call.type_args.is_empty()
                    && self.static_call_enum_key(&call.class).as_deref()
                        == Some(enum_name.as_str()) =>
            {
                (call.method.as_str(), call.args.as_slice())
            }
            _ => return self.check_expr(expr),
        };
        let Some(info) = self.symbols.lookup_enum(enum_name) else {
            return self.check_expr(expr);
        };
        let Some(definition) = info.variants.iter().find(|v| v.name == variant) else {
            return self.check_expr(expr);
        };
        let type_params = info.type_params.clone();
        let payload_templates = definition.payload_types.clone();
        let bindings: HashMap<_, _> = type_params.iter().zip(expected_args).collect();
        let payloads: Vec<_> = payload_templates
            .iter()
            .map(|ty| ty.substitute_names(|name| bindings.get(name).map(|ty| (*ty).clone())))
            .collect();
        let omitted_void_payload = args.is_empty() && matches!(payloads.as_slice(), [Type::Void]);
        if payloads.len() != args.len() && !omitted_void_payload {
            self.generic_error(
                expr.span(),
                format!(
                    "enum variant `{enum_name}::{variant}` takes {} argument(s), got {}",
                    payloads.len(),
                    args.len()
                ),
            );
            for arg in args {
                self.check_expr(&arg.expr);
            }
            return Self::error_type();
        }
        let actual: Vec<_> = args
            .iter()
            .zip(&payloads)
            .map(|(arg, ty)| self.check_generic_enum_argument(&arg.expr, ty, parameters))
            .collect();
        let mut payload_inference =
            crate::semantic::generic_inference::Inference::new(&type_params, &[])
                .expect("registered enum parameters");
        let constraints = payload_templates
            .iter()
            .zip(&actual)
            .try_for_each(|(formal, actual)| payload_inference.constrain_partial(formal, actual));
        let inferred = match constraints.and_then(|()| payload_inference.finish(true)) {
            Ok(inferred) => inferred,
            Err(message) => {
                self.generic_error(expr.span(), message);
                return Self::error_type();
            }
        };
        let result = Type::Generic(
            enum_name.clone(),
            type_params
                .iter()
                .zip(expected_args)
                .map(|(name, expected)| {
                    let actual = &inferred[name];
                    if *actual == Type::Void {
                        expected.substitute_names(|name| {
                            parameters
                                .contains(name.as_str())
                                .then(crate::semantic::generic_inference::enum_hole)
                        })
                    } else {
                        actual.clone()
                    }
                })
                .collect(),
        );
        self.record_variant_use(enum_name, variant, expr.span());
        if matches!(expr, Expr::Call(_) | Expr::Var(..)) {
            self.enum_variant_resolutions
                .insert(expr.id(), enum_name.clone());
        }
        if let Expr::StaticCall(call) = expr {
            self.record_type_use(&call.class, &result, call.span);
            self.check_source_type_name(&call.class, call.span);
            self.static_call_classes.insert(
                call.id,
                crate::semantic::ids::TypeId::from_source_name(enum_name),
            );
        }
        self.expr_types.insert(expr.id(), result.clone());
        result
    }

    fn is_inferred_enum_constructor(&self, expr: &Expr) -> bool {
        self.enum_variant_resolutions.contains_key(&expr.id())
            || matches!(expr, Expr::StaticCall(call)
                if call.type_args.is_empty() && self.static_call_enum_key(&call.class).is_some())
    }

    /// Complete constructor metadata after successful structural inference,
    /// without checking payloads again. Each nested constructor owns a type
    /// entry, including holes below more than one generic enum head.
    fn resolve_generic_argument(&mut self, expr: &Expr, expected: &Type) {
        let mut pending = vec![(expr, expected.clone())];
        while let Some((expr, expected)) = pending.pop() {
            match expr {
                Expr::Ternary(t) => {
                    self.expr_types.insert(expr.id(), expected.clone());
                    pending.push((&t.then_expr, expected.clone()));
                    pending.push((&t.else_expr, expected));
                    continue;
                }
                Expr::Match(m) => {
                    self.expr_types.insert(expr.id(), expected.clone());
                    pending.extend(m.arms.iter().filter_map(|arm| match &arm.body {
                        MatchBody::Expr(expr) => Some((expr.as_ref(), expected.clone())),
                        _ => None,
                    }));
                    continue;
                }
                _ if !self.is_inferred_enum_constructor(expr) => continue,
                _ => {}
            }
            self.expr_types.insert(expr.id(), expected.clone());
            let (variant, args) = match expr {
                Expr::Call(call) => (call.callee.as_str(), call.args.as_slice()),
                Expr::StaticCall(call) => (call.method.as_str(), call.args.as_slice()),
                _ => continue,
            };
            if let Some((_, payloads, _)) = self.expected_qualified_variant(variant, &expected) {
                pending.extend(args.iter().zip(payloads).map(|(arg, ty)| (&arg.expr, ty)));
            }
        }
    }

    /// Per value parameter, the formal with its type parameters taken from the
    /// expected result (`expected` matched against `declared`, the return type),
    /// or `None` when the formal does not mention a type parameter or the match
    /// leaves one of them unbound. Explicit type arguments win over the hint.
    /// One walk over `declared` and `expected`, then one substitution per
    /// formal: O(|declared| + |expected| + sum |formal|).
    fn expected_result_param_hints(
        &mut self,
        type_params: &[String],
        declared: &Type,
        params: &[Type],
        explicit: &HashMap<String, Type>,
        expected: &Type,
        span: Span,
    ) -> Vec<Option<Type>> {
        #[cfg(test)]
        EXPECTED_RESULT_HINTS.with(|count| count.set(count.get() + 1));
        if Self::contains_error_type(expected) {
            return Vec::new();
        }
        let Ok(mut inference) =
            crate::semantic::generic_inference::Inference::new(type_params, &[])
        else {
            return Vec::new();
        };
        // An expected generic class arrives as its instance; match its logical
        // application, as argument types are.
        let expected =
            expected.substitute_names(|name| self.generics.logical_types.get(name).cloned());
        let mut spellings = HashMap::new();
        let formal = self.canonical_generic_type(declared, &mut HashMap::new());
        let expected = self.canonical_generic_type(&expected, &mut spellings);
        if inference.constrain(&formal, &expected).is_err() {
            return Vec::new();
        }
        let parameter_names: HashSet<_> = type_params.iter().map(String::as_str).collect();
        let mut hints = Vec::with_capacity(params.len());
        for formal in params {
            hints.push(if !uses_parameters(formal, &parameter_names) {
                None
            } else {
                let mut unbound = false;
                let hint = formal.substitute_names(|name| {
                    if !parameter_names.contains(name.as_str()) {
                        return None;
                    }
                    let bound = explicit.get(name).cloned().or_else(|| {
                        inference.binding(name).map(|bound| {
                            bound.map_names(|identity| {
                                spellings
                                    .get(identity)
                                    .cloned()
                                    .unwrap_or_else(|| identity.clone())
                            })
                        })
                    });
                    unbound |= bound.is_none();
                    bound
                });
                (!unbound).then(|| self.normalize_type(&hint, span))
            });
        }
        hints
    }

    pub(super) fn check_generic_call(
        &mut self,
        c: &CallExpr,
        expected: Option<&Type>,
    ) -> Option<Type> {
        self.check_generic_call_parts(&c.callee, &c.type_args, &c.args, c.span, c.id, expected)
    }

    pub(super) fn check_generic_call_parts(
        &mut self,
        callee: &str,
        type_args: &[Type],
        call_args: &[CallArg],
        span: Span,
        id: ExprId,
        expected: Option<&Type>,
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
                // The test runs on the formal: an explicit argument may spell
                // the caller's own parameter under a callee parameter's name
                // (willow-jz15.97).
                let expected = substituted.as_ref().unwrap_or(ty);
                (fully_explicit || !uses_parameters(ty, &parameter_names))
                    .then(|| self.normalize_type(expected, span))
            })
            .collect();
        // The expected result type, matched against the declared return type,
        // gives a parameter-mentioning formal a context for contextual forms
        // (`let h: Box<Array<i64>> = wrap([])`, willow-jz15.96). It is only a
        // checking context: inference still binds from the checked arguments,
        // so an ordinary value keeps its own type.
        let hinted_params = match expected {
            // An async call yields `Task<R>` and has no generic form to match.
            Some(expected) if !fully_explicit && !template.is_async => {
                let declared = imported
                    .as_ref()
                    .map_or(&template.return_type, |info| &info.return_type);
                self.expected_result_param_hints(
                    &template.type_params,
                    declared,
                    &params,
                    &explicit_bindings,
                    expected,
                    span,
                )
            }
            _ => Vec::new(),
        };
        // A lambda whose formal still mentions a type parameter is typed after
        // the other arguments: its parameter types come from what they bind
        // (`map(xs, |x| x * 2)`), and its result then binds the rest.
        let deferred: Vec<bool> = call_args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                matches!(a.expr, Expr::Lambda(_))
                    && a.mode == CallArgMode::Value
                    && template
                        .params
                        .get(i)
                        .is_some_and(|p| p.mode == ParamMode::Value)
                    && matches!(params.get(i), Some(Type::Fn(..) | Type::Closure(..)))
                    && matches!(contextual_params.get(i), Some(None))
            })
            .collect();
        let mut reference_places = Vec::with_capacity(call_args.len());
        let mut actual: Vec<Option<Type>> = call_args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                if deferred[i] {
                    reference_places.push(None);
                    return None;
                }
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
                } else if let Some(Some(hint)) = hinted_params.get(i) {
                    self.check_expr_expecting(&a.expr, hint)
                } else if let Some(expected) = params.get(i) {
                    self.check_generic_enum_argument(&a.expr, expected, &parameter_names)
                } else {
                    self.check_expr(&a.expr)
                };
                reference_places.push(place);
                Some(result.substitute_names(|n| self.generics.logical_types.get(n).cloned()))
            })
            .collect();
        let mut spellings = HashMap::new();
        let inferred_explicit: Vec<_> = explicit
            .iter()
            .map(|ty| self.canonical_generic_type(ty, &mut spellings))
            .collect();
        // Bindings are subtrees of the explicit and actual types only, so only
        // their spellings are reported back; the template's own spellings of
        // the same declarations must not displace the caller's.
        let inferred_params: Vec<_> = params
            .iter()
            .map(|ty| self.canonical_generic_type(ty, &mut HashMap::new()))
            .collect();
        let mut inference = match crate::semantic::generic_inference::Inference::new(
            &template.type_params,
            &inferred_explicit,
        ) {
            Ok(inference) => inference,
            Err(message) => {
                self.check_deferred_lambdas_plainly(call_args, &deferred);
                self.generic_error(span, message);
                return Some(Self::error_type());
            }
        };
        // A wrong argument count is reported first, but the arguments that do
        // have a formal still bind, so a lambda among them is typed normally.
        let mut failure = (params.len() != call_args.len()).then(|| {
            format!(
                "expected {} value arguments, got {}",
                params.len(),
                call_args.len()
            )
        });
        let mut mismatched = false;
        let mut failure_argument = None;
        // An argument whose type already carries the recovery marker has been
        // reported; matching it would only add a derived mismatch, and every
        // later call fed this call's result would add another (willow-jz15.64).
        let mut poisoned = false;
        for (i, ty) in actual.iter().enumerate() {
            let Some(ty) = ty else { continue };
            let fixed = contextual_params
                .get(i)
                .and_then(Option::as_ref)
                .filter(|_| template.params[i].mode == ParamMode::Value);
            let ty = if let Some(expected) = fixed
                && let normalized = self.normalize_type(ty, span)
                && self.types_compatible(expected, &normalized)
            {
                expected.substitute_names(|name| self.generics.logical_types.get(name).cloned())
            } else {
                ty.clone()
            };
            let Some(ty) = self.generic_self_argument(ty, &template.name, call_args[i].span) else {
                poisoned = true;
                continue;
            };
            let ty = self.canonical_generic_type(&ty, &mut spellings);
            if mentions(&ty, Self::is_error_name) {
                poisoned = true;
            } else if !mismatched
                && let Some(formal) = inferred_params.get(i)
                && let Err(message) = inference.constrain_partial(formal, &ty)
            {
                mismatched = true;
                if failure.is_none() {
                    failure_argument = Some(i);
                }
                failure.get_or_insert_with(|| {
                    format!(
                        "argument {} (`{}`): expected `{}`, found `{}`; {message}",
                        i + 1,
                        template.params[i].name,
                        type_name(
                            &params[i].substitute_names(|name| inference.binding(name).cloned())
                        ),
                        type_name(&ty)
                    )
                });
            }
        }
        for i in (0..call_args.len()).filter(|&i| deferred[i]) {
            let Expr::Lambda(lambda) = &call_args[i].expr else {
                unreachable!("deferred arguments are lambdas")
            };
            let (Type::Fn(formal_params, formal_ret) | Type::Closure(formal_params, formal_ret)) =
                &params[i]
            else {
                unreachable!("deferred formals are callable")
            };
            // What the arguments so far bound, in this unit's spellings. An
            // unbound parameter leaves its type unknown, unless a poisoned
            // argument means it never will be bound: it is then the recovery
            // marker, so the body is checked without a derived E1001.
            let unbound = poisoned.then(Self::error_type);
            // Knownness is decided on the FORMAL: a binding may itself spell
            // the caller's own type parameter, whose name can coincide with
            // one of the callee's (`fold` called from `count_where<T>` binds
            // the callee's `T` to the caller's `T`), so the substituted type
            // says nothing about which callee parameters are still unbound
            // (willow-jz15.97).
            let known = |ty: &Type| {
                if mentions(ty, |name| {
                    parameter_names.contains(name)
                        && unbound.is_none()
                        && inference.binding(name).is_none()
                }) {
                    return None;
                }
                Some(ty.substitute_names(|name| {
                    if !parameter_names.contains(name.as_str()) {
                        return None;
                    }
                    inference
                        .binding(name)
                        .map(|bound| {
                            bound.map_names(|identity| {
                                spellings
                                    .get(identity)
                                    .cloned()
                                    .unwrap_or_else(|| identity.clone())
                            })
                        })
                        .or_else(|| unbound.clone())
                }))
            };
            let lambda_ty = if formal_params.len() == lambda.params.len() {
                let expected_params: Option<Vec<_>> = formal_params.iter().map(known).collect();
                let expected_ret = known(formal_ret);
                let expected_params = expected_params.map(|types| {
                    types
                        .iter()
                        .map(|ty| self.normalize_type(ty, span))
                        .collect::<Vec<_>>()
                });
                let expected_ret = expected_ret.map(|ty| self.normalize_type(&ty, span));
                // The help for a rejected capture names the declared
                // signature, which is what the user would change.
                self.check_lambda_for_callable(
                    lambda,
                    &params[i],
                    expected_params.as_deref(),
                    expected_ret.as_ref(),
                )
                .0
            } else {
                self.check_lambda(lambda)
            };
            self.expr_types
                .insert(call_args[i].expr.id(), lambda_ty.clone());
            let lambda_ty =
                lambda_ty.substitute_names(|n| self.generics.logical_types.get(n).cloned());
            let canonical = match self.generic_self_argument(
                lambda_ty.clone(),
                &template.name,
                call_args[i].span,
            ) {
                Some(ty) => self.canonical_generic_type(&ty, &mut spellings),
                None => Self::error_type(),
            };
            if mentions(&canonical, Self::is_error_name) {
                poisoned = true;
            } else if !mismatched
                && let Err(message) = inference.constrain(&inferred_params[i], &canonical)
            {
                mismatched = true;
                if failure.is_none() {
                    failure_argument = Some(i);
                }
                failure.get_or_insert_with(|| {
                    format!(
                        "argument {} (`{}`): expected `{}`, found `{}`; {message}",
                        i + 1,
                        template.params[i].name,
                        type_name(
                            &params[i].substitute_names(|name| inference.binding(name).cloned())
                        ),
                        type_name(&lambda_ty)
                    )
                });
            }
            actual[i] = Some(lambda_ty);
        }
        let actual: Vec<Type> = actual.into_iter().map(Option::unwrap).collect();
        if let Some(message) = failure {
            if let Some(i) = failure_argument {
                let diagnostic =
                    Diagnostic::new(Severity::Error, ErrorCode::E0201, message).with_label(
                        Label::primary(call_args[i].expr.span(), "argument type mismatch"),
                    );
                self.push(Self::with_fn_closure_help(
                    diagnostic,
                    &params[i],
                    &actual[i],
                    &call_args[i].expr,
                ));
            } else {
                self.generic_error(span, message);
            }
            return Some(Self::error_type());
        }
        if poisoned {
            return Some(Self::error_type());
        }
        let bindings = match inference.finish(false) {
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
        let actual: Vec<_> = actual
            .into_iter()
            .zip(call_args)
            .zip(&params)
            .map(|((actual, arg), formal)| {
                let expected = formal.substitute_names(|name| bindings.get(name).cloned());
                let expected = self.normalize_type(&expected, arg.span);
                if self.is_inferred_enum_constructor(&arg.expr)
                    || matches!(arg.expr, Expr::Ternary(_) | Expr::Match(_))
                {
                    self.resolve_generic_argument(&arg.expr, &expected);
                    expected
                } else {
                    actual
                }
            })
            .collect();
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
            if self.capture_call_sites {
                self.analysis_symbols.reference(
                    span,
                    callee,
                    crate::semantic::analysis_symbols::Declaration::new(
                        &template.name,
                        "function",
                        template.span,
                        None,
                    ),
                    "call",
                    false,
                );
            }
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
            self.record_runtime_instance_name(&name, &template.name, &args);
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
                let (bindings, classes) = self.instantiation_bindings(span, &bindings);
                let mut item = Item::Function(f);
                substitute_item(&mut item, &bindings)
                    .record_bodies(&mut self.generics.registry.borrow_mut().origins);
                self.generics.pending.push_back(Instantiation {
                    item,
                    classes,
                    method_only: false,
                });
            }
            name
        };
        // Template origin is canonical, but the generated source call still
        // resolves in the consumer's scope. Preserve its written prefix so
        // source-use diagnostics do not lose a used module import.
        let path = self.generics.function_paths.get(callee).map(|origin| {
            callee
                .rsplit_once("::")
                .map_or(origin.as_str(), |(written, _)| written)
        });
        let target = path.map_or(name.clone(), |path| format!("{path}::{name}"));
        self.generics.calls.insert(id, target);
        Some(if template.is_async {
            Type::Generic("Task".into(), vec![result])
        } else {
            result
        })
    }

    fn record_runtime_instance_name(&self, symbol: &str, source: &str, args: &[Type]) {
        if self
            .generics
            .registry
            .borrow()
            .runtime_names
            .contains_key(symbol)
        {
            return;
        }
        let args = args
            .iter()
            .map(|arg| {
                let logical =
                    arg.substitute_names(|name| self.generics.logical_types.get(name).cloned());
                let canonical = logical.map_names(|name| {
                    let name = self.generics.nominal_aliases.get(name).unwrap_or(name);
                    self.generic_type_identity(name)
                });
                self.diagnostic_type_name(&canonical)
            })
            .collect::<Vec<_>>()
            .join(", ");
        self.generics
            .registry
            .borrow_mut()
            .runtime_names
            .insert(symbol.to_owned(), format!("{source}<{args}>"));
    }

    /// Resolve nominal instantiation aliases only in type slots of diagnostics.
    pub(super) fn diagnostic_type_name(&self, ty: &Type) -> String {
        type_name_with(ty, |name| {
            self.generics.nominal_aliases.get(name).unwrap_or(name)
        })
    }

    /// Display a class using its declared generic parameters, so a diagnostic
    /// in a template body has the same spelling in every concrete instance.
    /// Only class-name slots use this helper; member names and fix text must
    /// retain their original spelling even when they match a generic class.
    pub(super) fn diagnostic_class_name(&self, name: &str) -> String {
        let source = name.split_once("$mono$").map_or(name, |(base, _)| base);
        if let Some(template) = self.generics.classes.get(source) {
            return format!("{source}<{}>", template.type_params.join(", "));
        }
        // A shared instance may carry the canonical defining-module spelling
        // rather than this checker's import alias. Resolve by declaration file
        // and source name, without searching every template or import.
        if let Some(class) = self.symbols.lookup_class(name) {
            let local = source.rsplit("::").next().unwrap_or(source);
            let registry = self.generics.registry.borrow();
            if let Some(template) = registry
                .classes
                .get(&(class.declaration_span.file_id, local.to_owned()))
            {
                return format!("{source}<{}>", template.type_params.join(", "));
            }
        }
        name.to_owned()
    }

    /// Queue one concrete body. The class signature is already available, so
    /// recursion and calls discovered in other units share the same request.
    pub(super) fn request_class_method(&mut self, owner: &str, method: &str) {
        if self.generics.abstract_check {
            return;
        }
        let Some(info) = self.symbols.lookup_class(owner) else {
            return;
        };
        if !info.methods.contains_key(method) {
            return;
        }
        let local = info
            .name
            .rsplit("::")
            .next()
            .unwrap_or(&info.name)
            .to_string();
        let file = info.declaration_span.file_id;
        let deferred = self
            .generics
            .registry
            .borrow()
            .deferred_classes
            .get(&(file, local.clone()))
            .cloned();
        let Some(deferred) = deferred else {
            return;
        };
        let key = (file, local.clone(), method.to_string());
        {
            let mut registry = self.generics.registry.borrow_mut();
            if registry.requested_methods.contains_key(&key) {
                return;
            }
            registry.requested_methods.insert(key, ());
        }
        let template = self
            .generics
            .registry
            .borrow()
            .method_templates
            .get(&(file, deferred.template.clone(), method.to_string()))
            .cloned();
        let Some(template) = template else {
            return;
        };
        let mut class = deferred.shell.clone();
        let mut method = (*template).clone();
        method.constraint = None;
        class.methods.push(method);
        let mut item = Item::Class(class);
        let mut ids = FreshIds::default();
        visit_item(
            &mut item,
            &mut |ty| crate::semantic::generic_inference::substitute(ty, &deferred.bindings),
            &mut |_| {},
            true,
            true,
            Some((&deferred.template, &local)),
            &mut ids,
        );
        ids.record_bodies(&mut self.generics.registry.borrow_mut().origins);
        self.generics.pending.push_back(Instantiation {
            item,
            // The signature request precedes every method request and installs
            // nominal aliases once in the declaring checker.
            classes: Vec::new(),
            method_only: true,
        });
    }

    pub(super) fn instantiate_class(
        &mut self,
        name: &str,
        args: &[Type],
        span: Span,
    ) -> Option<Type> {
        let template = self.generics.classes.get(name)?.clone();
        #[cfg(test)]
        {
            self.generics.registry.borrow_mut().class_requests += 1;
        }
        if !self.signature_metadata && !template.public && template.span.file_id != span.file_id {
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
        // ABI source receivers are inferred from already checked values. Import
        // access is checked at written type sites, not at literal method calls.
        if template.abi_storage.is_none() {
            for ty in &args {
                self.validate_type(ty, span);
            }
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
        let bindings: HashMap<String, Type> = template
            .type_params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        let template_access = name.to_string();
        let local_name = symbol(&template.name, &identity_args);
        self.record_runtime_instance_name(&local_name, &template.name, &args);
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
        let mut class = class_signature(&template);
        if !self.generics.abstract_check {
            class.fields = template.fields.clone();
            class.constructors = template.constructors.clone();
        }
        class.methods.retain(|m| {
            m.constraint.as_ref().is_none_or(|bound| {
                bindings
                    .get(&bound.parameter)
                    .is_some_and(|arg| bound.alternatives.contains(arg))
            })
        });
        for method in &mut class.methods {
            method.constraint = None;
        }
        class.name = name.clone();
        class.type_params.clear();
        let (bindings, classes) = self.instantiation_bindings(span, &bindings);
        self.register_shared_instantiation_types(&classes);
        let mut item = Item::Class(class);
        let mut ids = FreshIds::default();
        visit_item(
            &mut item,
            &mut |ty| crate::semantic::generic_inference::substitute(ty, &bindings),
            &mut |_| {},
            true,
            true,
            Some((&template.name, &local_name)),
            &mut ids,
        );
        {
            let mut registry = self.generics.registry.borrow_mut();
            ids.record_bodies(&mut registry.origins);
            for field in &template.fields {
                if let Some(initializer) = &field.initializer
                    && let Some(&instance) = ids.expressions.get(&initializer.id())
                {
                    registry
                        .origins
                        .initializers
                        .insert(instance, initializer.id());
                }
            }
        }
        let Item::Class(mut class) = item else {
            unreachable!()
        };
        let generation = self.error_generation;
        if template_access == template.name {
            self.register_class(&class);
        } else {
            // Registration happens in the consumer, while the queued body is
            // checked in the provider. Only the consumer's signature copy may
            // use its import path for the template's own receiver type.
            let mut signature = class_signature(&template);
            signature.methods.retain(|m| {
                m.constraint.as_ref().is_none_or(|bound| {
                    bindings
                        .get(&bound.parameter)
                        .is_some_and(|arg| bound.alternatives.contains(arg))
                })
            });
            for method in &mut signature.methods {
                method.constraint = None;
            }
            signature.name = name.clone();
            signature.type_params.clear();
            let mut signature = Item::Class(signature);
            visit_item(
                &mut signature,
                &mut |ty| {
                    let qualified = ty.map_names(|n| {
                        if n == &template.name {
                            template_access.clone()
                        } else {
                            n.clone()
                        }
                    });
                    crate::semantic::generic_inference::substitute(&qualified, &bindings)
                },
                &mut |_| {},
                false,
                true,
                None,
                &mut FreshIds::default(),
            );
            let Item::Class(signature) = signature else {
                unreachable!()
            };
            self.register_class(&signature);
        }
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
            class.name = local_name.clone();
            // Method checking only needs the receiver and owner identity.
            // Do not copy fields, constructor bodies, interfaces or the entire
            // method-signature table into each queued method fragment.
            let shell = ClassDecl {
                abi_storage: class.abi_storage.clone(),
                type_params: Vec::new(),
                name: class.name.clone(),
                public: class.public,
                is_open: class.is_open,
                base_class: None,
                implements: Vec::new(),
                source_implements_len: 0,
                fields: Vec::new(),
                methods: Vec::new(),
                constructors: Vec::new(),
                span: class.span,
            };
            self.generics.registry.borrow_mut().deferred_classes.insert(
                (class.span.file_id, local_name),
                Rc::new(DeferredClass {
                    template: template.name.clone(),
                    shell,
                    bindings,
                }),
            );
            self.generics.pending.push_back(Instantiation {
                item: Item::Class(class),
                classes,
                method_only: false,
            });
        }
        Some(Type::Named(name))
    }

    /// Request, from the unit that declares each generic interface `c`
    /// implements, the concrete instance whose defaults `c`'s copies reuse
    /// (willow-s71l). A copy of another unit's generic default must name that
    /// unit's items, so it is checked there, once per distinct interface
    /// instantiation, as a non-generic interface; the class's copy is then
    /// rebound to that body when generics are expanded and lowered in that
    /// unit's scope like any other foreign default (willow-rvpp). Returns the
    /// copies handled this way, which `c` must not check itself.
    pub(super) fn instantiate_foreign_defaults<'c>(
        &mut self,
        c: &'c ClassDecl,
    ) -> HashSet<&'c str> {
        let copies: Vec<&MethodDecl> = c
            .methods
            .iter()
            .filter(|m| {
                m.is_interface_default
                    && !m.is_default_injected
                    && m.body.span.file_id != c.span.file_id
            })
            .collect();
        let mut handled = HashSet::new();
        if copies.is_empty() {
            return handled;
        }
        // A template's copies still name its own parameters; each concrete
        // instance of the class requests its own interface instance.
        if self.generics.abstract_check {
            handled.extend(copies.iter().map(|m| m.name.as_str()));
            return handled;
        }
        let copy_names: HashSet<&'c str> = copies.iter().map(|m| m.name.as_str()).collect();
        // Each default method name to the first implemented interface that
        // provides it, in the order desugaring chose the copies in.
        let mut providers: HashMap<&str, (Rc<InterfaceDecl>, &'c [Type], usize)> = HashMap::new();
        for ty in &c.implements {
            let Type::Generic(name, args) = ty else {
                continue;
            };
            let Some(info) = self.symbols.lookup_interface(name) else {
                continue;
            };
            let local = info.name.rsplit("::").next().unwrap_or(&info.name);
            let key = (info.declaration_span.file_id, local.to_string());
            let Some(template) = self
                .generics
                .registry
                .borrow()
                .interfaces
                .get(&key)
                .cloned()
            else {
                continue;
            };
            for (index, m) in template.methods.iter().enumerate() {
                if m.default_body.is_none() {
                    continue;
                }
                if let Some(&name) = copy_names.get(m.name.as_str()) {
                    providers
                        .entry(name)
                        .or_insert_with(|| (Rc::clone(&template), args.as_slice(), index));
                }
            }
        }
        let mut instances: HashMap<*const InterfaceDecl, Rc<InterfaceDecl>> = HashMap::new();
        for m in copies {
            let Some((template, args, index)) = providers.get(m.name.as_str()) else {
                continue;
            };
            // Spans survive class instantiation; body ids are renumbered.
            let declared = &template.methods[*index];
            let provides = declared
                .default_body
                .as_ref()
                .is_some_and(|b| b.span == m.body.span);
            if !provides {
                continue;
            }
            let instance = match instances.get(&Rc::as_ptr(template)) {
                Some(instance) => Rc::clone(instance),
                None => {
                    let Some(instance) =
                        self.instantiate_interface_template(template, args, c.span)
                    else {
                        continue;
                    };
                    instances.insert(Rc::as_ptr(template), Rc::clone(&instance));
                    instance
                }
            };
            self.generics
                .default_copies
                .entry(c.name.clone())
                .or_default()
                .insert(m.name.clone(), (instance, *index));
            handled.insert(m.name.as_str());
        }
        handled
    }

    /// The non-generic instance of generic interface `template` at `args`,
    /// queued once per build for the declaring unit's checker.
    fn instantiate_interface_template(
        &mut self,
        template: &Rc<InterfaceDecl>,
        args: &[Type],
        span: Span,
    ) -> Option<Rc<InterfaceDecl>> {
        if args.len() != template.type_params.len() {
            return None;
        }
        let args: Vec<_> = args.iter().map(|t| self.normalize_type(t, span)).collect();
        if args.iter().any(|ty| mentions(ty, Self::is_error_name))
            || !self.check_instantiation_size(&args, span)
        {
            return None;
        }
        let identity_args: Vec<_> = args
            .iter()
            .map(|ty| ty.map_names(|n| self.generic_type_identity(n)))
            .collect();
        let key = (
            2,
            format!("{}::{}", template.span.file_id.0, template.name),
            identity_args,
        );
        let cached = self
            .generics
            .registry
            .borrow()
            .interface_instances
            .get(&key)
            .cloned();
        if let Some(instance) = cached {
            return Some(instance);
        }
        if self.reserve_instantiation(&key, span) != Some(true) {
            return None;
        }
        #[cfg(test)]
        {
            self.generics
                .registry
                .borrow_mut()
                .interface_specializations += 1;
        }
        let bindings = template.type_params.iter().cloned().zip(args).collect();
        let (bindings, classes) = self.instantiation_bindings(span, &bindings);
        // Source import summaries belong to the template, not each concrete
        // instance. Clone only the methods that substitution already needs.
        let instance = InterfaceDecl {
            default_import_uses: Vec::new(),
            name: symbol(&template.name, &key.2),
            public: template.public,
            type_params: Vec::new(),
            extends: Vec::new(), // inherited methods are already composed
            methods: template.methods.clone(),
            // The opaque receiver keeps the written instantiation.
            instance_of: Some(Type::Generic(
                template.name.clone(),
                template
                    .type_params
                    .iter()
                    .map(|p| Type::Named(p.clone()))
                    .collect(),
            )),
            span: template.span,
        };
        let mut item = Item::Interface(instance);
        substitute_item(&mut item, &bindings)
            .record_bodies(&mut self.generics.registry.borrow_mut().origins);
        let Item::Interface(instance) = item else {
            unreachable!()
        };
        let instance = Rc::new(instance);
        self.generics
            .registry
            .borrow_mut()
            .interface_instances
            .insert(key, Rc::clone(&instance));
        self.generics.pending.push_back(Instantiation {
            method_only: false,
            item: Item::Interface((*instance).clone()),
            classes,
        });
        Some(instance)
    }

    pub(super) fn check_constrained_method(&mut self, method: &MethodDecl, class: &ClassDecl) {
        let bound = method.constraint.as_ref().expect("constrained method");
        if !class.type_params.contains(&bound.parameter) {
            self.generic_error(
                method.span,
                "method constraint must name an enclosing type parameter",
            );
            return;
        }
        // Each alternative is checked at declaration time, including alternatives
        // that no program ever instantiates. No dynamic dispatch is introduced.
        let mut seen = HashSet::new();
        for alternative in &bound.alternatives {
            if !matches!(
                alternative,
                Type::I64 | Type::F64 | Type::Bool | Type::String
            ) {
                self.generic_error(
                    method.span,
                    "method constraint alternatives must be scalar types (i64, f64, bool, String)",
                );
                continue;
            }
            if !seen.insert(alternative.clone()) {
                self.generic_error(method.span, "duplicate method constraint alternative");
                continue;
            }
            let bindings = [(bound.parameter.clone(), alternative.clone())]
                .into_iter()
                .collect();
            let receiver =
                crate::semantic::generic_inference::substitute(&class.receiver_type(), &bindings);
            let receiver = self.normalize_type(&receiver, method.span);
            let copy = ClassDecl {
                abi_storage: None,
                type_params: Vec::new(),
                name: class.name.clone(),
                public: class.public,
                is_open: false,
                base_class: None,
                implements: Vec::new(),
                source_implements_len: 0,
                fields: Vec::new(),
                methods: vec![method.clone()],
                constructors: Vec::new(),
                span: class.span,
            };
            let mut item = Item::Class(copy);
            visit_item(
                &mut item,
                &mut |ty| crate::semantic::generic_inference::substitute(ty, &bindings),
                &mut |_| {
                    #[cfg(test)]
                    CONSTRAINED_BODY_VISITS.with(|count| count.set(count.get() + 1));
                },
                true,
                false,
                None,
                &mut FreshIds::default(),
            );
            let Item::Class(copy) = item else {
                unreachable!()
            };
            self.check_method(&copy.methods[0], &copy, &receiver);
        }
    }

    pub(crate) fn validate_generic_templates(
        &mut self,
        file: crate::diagnostics::FileId,
        program: &Program,
    ) {
        // Abstract checking has a separate symbol/output epoch: no symbolic
        // type or speculative body can escape into the concrete artifact set.
        let mut checker = TypeChecker::new();
        checker.capture_call_sites = self.generics.registry.borrow().capture_source_references;
        *checker.symbols = self.symbols.clone();
        checker.resolution = self.resolution.clone();
        checker.generics.functions = self.generics.functions.clone();
        checker.generics.classes = self.generics.classes.clone();
        checker.generics.function_paths = self.generics.function_paths.clone();
        checker.generics.class_paths = self.generics.class_paths.clone();
        checker.generics.registry.borrow_mut().module_paths =
            Rc::clone(&self.generics.registry.borrow().module_paths);
        checker.generics.registry.borrow_mut().classes =
            Rc::clone(&self.generics.registry.borrow().classes);
        checker.generics.abstract_check = true;
        checker.prechecks = self.prechecks.clone();
        checker.precheck_context = self.precheck_context.clone();
        let contracts = program.items.iter().any(|item| match item {
            Item::Function(f) => !f.type_params.is_empty() && !f.effect_contracts.is_empty(),
            Item::Class(c) => {
                !c.type_params.is_empty()
                    && (c.base_class.is_some()
                        || !c.implements.is_empty()
                        || c.methods.iter().any(|m| !m.effect_contracts.is_empty()))
            }
            Item::Interface(i) => {
                !i.type_params.is_empty()
                    && (!i.extends.is_empty()
                        || i.methods.iter().any(|m| !m.effect_contracts.is_empty()))
            }
            _ => false,
        });
        if contracts {
            // Abstract effect evidence must be collected, not replayed from the
            // diagnostic-only template cache. It stays in this isolated checker.
            checker.prechecks = None;
            checker.effect_index = self.effect_index.clone();
            checker.effect_queries = self.effect_queries.clone();
            checker.indirect_lock = self.indirect_lock.clone();
            checker.expr_types = self.expr_types.clone();
            checker.resolved_calls = self.resolved_calls.clone();
            checker.effect_inputs.direct = self.effect_inputs.direct.clone();
            checker.effect_inputs.edges = self.effect_inputs.edges.clone();
        }
        let functions: Vec<_> = self.generics.functions.values().cloned().collect();
        let classes: Vec<_> = self.generics.classes.values().cloned().collect();
        for f in functions {
            if f.span.file_id != file {
                continue;
            }
            checker.generics.parameter_names = f.type_params.iter().cloned().collect();
            checker.local.declared_type_params = f.type_params.clone();
            checker.precheck_operation(
                format!("abstract_function:{}:{}", file.0, f.name),
                f.body.id,
                serde_json::to_value(&f).unwrap(),
                |state| {
                    if let Some(cache) = &state.prechecks {
                        cache.templates.set(cache.templates.get() + 1);
                    }
                    for name in &f.type_params {
                        if name == &f.name
                            || state.symbols.lookup_class(name).is_some()
                            || state.symbols.lookup_enum(name).is_some()
                            || state.symbols.lookup_interface(name).is_some()
                            || state.generics.classes.contains_key(name)
                        {
                            state.generic_error(
                        f.span,
                        format!("type parameter `{name}` shadows an existing type or declaration"),
                    );
                        }
                    }
                    state.check_function(&f);
                    state.generics.pending.clear();
                },
            );
        }
        let mut checked_classes = HashSet::new();
        for c in classes {
            if c.span.file_id != file || !checked_classes.insert(c.span) {
                continue;
            }
            checker.generics.parameter_names = c.type_params.iter().cloned().collect();
            checker.local.declared_type_params = c.type_params.clone();
            checker.precheck_operation(
                format!("abstract_class:{}:{}", file.0, c.name),
                BodyId::fresh(),
                serde_json::to_value(&c).unwrap(),
                |state| {
                    if let Some(cache) = &state.prechecks {
                        cache.templates.set(cache.templates.get() + 1);
                    }
                    for name in &c.type_params {
                        if name == &c.name
                            || state.symbols.lookup_class(name).is_some()
                            || state.symbols.lookup_enum(name).is_some()
                            || state.symbols.lookup_interface(name).is_some()
                            || state.generics.classes.contains_key(name)
                        {
                            state.generic_error(
                        c.span,
                        format!("type parameter `{name}` shadows an existing type or declaration"),
                    );
                        }
                    }
                    state.register_class(&c);
                    state.check_class(&c);
                    state.generics.pending.clear();
                },
            );
        }
        // Annotated generic defaults need checking even without an instance.
        // Check their sibling defaults too, so self-helper edges retain evidence.
        for interface in program.items.iter().filter_map(|item| match item {
            Item::Interface(i)
                if !i.type_params.is_empty()
                    && i.span.file_id == file
                    && (!i.extends.is_empty()
                        || i.methods.iter().any(|m| {
                            m.default_body.is_some() && !m.effect_contracts.is_empty()
                        })) =>
            {
                Some(i)
            }
            _ => None,
        }) {
            checker.generics.parameter_names = interface.type_params.iter().cloned().collect();
            checker.local.declared_type_params = interface.type_params.clone();
            for method in &interface.methods {
                if method.default_body.is_some() {
                    let id = interface_default_effect_id(&interface.name, &method.name);
                    checker.effect_index.callables.insert(id, false);
                    checker
                        .effect_inputs
                        .edges
                        .entry(FunctionId::method(
                            TypeId::from_source_name(&interface.name),
                            &method.name,
                        ))
                        .or_default()
                        .insert(id);
                }
            }
            for method in &interface.methods {
                if method.default_body.is_some() {
                    checker.check_interface_default_body(method, interface);
                    checker.generics.pending.clear();
                }
            }
        }
        if contracts {
            use crate::semantic::call_graph::{CallGraph, CallSites};
            let mut graph = CallGraph::default();
            for &id in checker.effect_index.callables.keys() {
                graph.merge(id, CallSites::default());
            }
            for (&id, sites) in &checker.resolved_calls {
                graph.merge(id, sites.clone());
            }
            for (&id, targets) in &checker.effect_inputs.edges {
                graph.merge(
                    id,
                    CallSites {
                        targets: targets.iter().copied().collect(),
                        ..Default::default()
                    },
                );
            }
            let effects = crate::compiler_db::effects::solve_unit(
                program,
                &graph,
                &checker.expr_types,
                None,
                &checker.effect_index.callables,
                &checker.effect_inputs.direct,
                |target| {
                    self.effect_queries.as_ref().map_or_else(
                        || {
                            crate::compiler_db::effects::intrinsic_effects(target)
                                .unwrap_or(crate::semantic::effects::RuntimeEffects::MAY_PANIC)
                        },
                        |(queries, unit)| {
                            queries.external(*unit, target, &self.effect_index.imports)
                        },
                    )
                },
            );
            checker.report_effect_contracts(program, &graph, &effects.facts, true);
        }
        self.analysis_symbols
            .used_imports
            .extend(std::mem::take(&mut checker.analysis_symbols.used_imports));
        if checker.capture_call_sites {
            self.generics
                .source_symbols
                .extend(checker.analysis_symbols);
        }
        self.generics
            .rejected_parameter_methods
            .extend(checker.generics.rejected_parameter_methods);
        self.errors.extend(checker.errors);
    }

    pub(crate) fn check_generic_instance(&mut self, instance: Instantiation) {
        if let Some(cache) = self.prechecks.clone()
            && !super::precheck_journal::is_active()
        {
            let (name, file, id) = match &instance.item {
                Item::Function(f) => (f.name.clone(), f.span.file_id, f.body.id),
                Item::Class(c) => (
                    if instance.method_only {
                        format!("{}::{}", c.name, c.methods[0].name)
                    } else {
                        c.name.clone()
                    },
                    c.span.file_id,
                    c.methods.first().map_or_else(BodyId::fresh, |m| m.body.id),
                ),
                Item::Interface(i) => (i.name.clone(), i.span.file_id, BodyId::fresh()),
                Item::Enum(_) => unreachable!(),
            };
            let syntax = serde_json::to_value(&instance).unwrap();
            self.precheck_operation(
                format!("instance:{}:{name}", file.0),
                id,
                syntax,
                |checker| {
                    cache.instances.set(cache.instances.get() + 1);
                    checker.check_generic_instance(instance);
                },
            );
            return;
        }

        self.register_shared_instantiation_types(&instance.classes);
        let item = instance.item;
        match &item {
            Item::Function(f) => {
                let params = self.normalize_param_types(&f.params);
                let param_infos = self.normalize_param_infos(&f.params);
                let return_type = self.normalize_type(
                    &f.return_type,
                    f.return_type_span.as_deref().copied().unwrap_or(f.span),
                );
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
            Item::Class(c) if instance.method_only => {
                let receiver = c.receiver_type();
                let method = &c.methods[0];
                let foreign_default = self
                    .generics
                    .default_copies
                    .get(&c.name)
                    .is_some_and(|copies| copies.contains_key(&method.name));
                if !foreign_default && !method.is_default_injected {
                    self.check_method(method, c, &receiver);
                }
            }
            Item::Class(c) => {
                self.register_class(c);
                let receiver = self.cached_class_validation(c);
                self.check_static_property_initializers(c);
                self.cached_class_defaults(c);
                for method in &c.methods {
                    if !method.is_static && (method.is_open || method.is_override) {
                        self.request_class_method(&c.name, &method.name);
                    }
                }
                for ctor in &c.constructors {
                    self.check_constructor(ctor, c, &receiver);
                }
            }
            Item::Interface(i) => {
                self.register_interface(i, None);
                self.check_interface(i);
            }
            Item::Enum(_) => unreachable!(),
        }
        let mut item = item;
        if !instance.method_only
            && let Item::Class(class) = &mut item
        {
            class.methods.clear();
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
        // A signature item precedes its body fragments in the work queue.
        // Merge once with an index, never scan all classes for each method.
        let mut classes = HashMap::new();
        for item in std::mem::take(&mut self.generics.emitted) {
            if let Item::Class(mut class) = item {
                if let Some(&index) = classes.get(&class.name) {
                    let Item::Class(existing) = &mut program.items[index] else {
                        unreachable!()
                    };
                    existing.methods.append(&mut class.methods);
                } else {
                    classes.insert(class.name.clone(), program.items.len());
                    program.items.push(Item::Class(class));
                }
            } else {
                program.items.push(item);
            }
        }
        if !self.generics.default_copies.is_empty() {
            for item in &mut program.items {
                let Item::Class(c) = item else { continue };
                let Some(copies) = self.generics.default_copies.get(&c.name) else {
                    continue;
                };
                for m in &mut c.methods {
                    let Some((instance, index)) = copies.get(&m.name) else {
                        continue;
                    };
                    let Some(body) = instance.methods[*index].default_body.as_ref() else {
                        continue;
                    };
                    // The copy now shares the instance's body, checked and
                    // lowered in the interface's unit.
                    m.body = body.clone();
                    m.is_default_injected = true;
                }
            }
        }
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
                    let normalized = self
                        .normalized_types
                        .get(ty)
                        .cloned()
                        .unwrap_or_else(|| ty.clone());
                    // Keep a fully qualified collection spelling: the final
                    // check accepts it without an import, a bare name not.
                    let mut qualified = Vec::new();
                    ty.map_names(|name| {
                        if let Some(item) = name.strip_prefix("std::collections::") {
                            qualified.push((item.to_owned(), name.clone()));
                        }
                    });
                    if qualified.is_empty() {
                        return normalized;
                    }
                    normalized.map_names(|name| {
                        qualified
                            .iter()
                            .find(|(item, _)| item == name)
                            .map_or_else(|| name.clone(), |(_, path)| path.clone())
                    })
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
                None,
                ids,
            );
        }
    }
}

fn substitute_item(item: &mut Item, bindings: &HashMap<String, Type>) -> FreshIds {
    let mut ids = FreshIds::default();
    visit_item(
        item,
        &mut |ty| crate::semantic::generic_inference::substitute(ty, bindings),
        &mut |_| {},
        true,
        true,
        None,
        &mut ids,
    );
    ids
}

fn visit_item(
    item: &mut Item,
    ty: &mut impl FnMut(&Type) -> Type,
    expr: &mut impl FnMut(&mut Expr),
    fresh: bool,
    preserve_interface_defaults: bool,
    static_receiver: Option<(&str, &str)>,
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
            if let Some(storage) = &mut c.abi_storage {
                *storage = ty(storage);
            }
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
                for p in &mut m.params {
                    p.ty = ty(&p.ty);
                }
                m.return_type = ty(&m.return_type);
                if !(preserve_interface_defaults && m.is_default_injected) {
                    blocks.push(&mut m.body);
                }
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
            if let Some(t) = &mut i.instance_of {
                *t = ty(t);
            }
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
                        if c.type_args.is_empty()
                            && let Some((template, receiver)) = static_receiver
                            && c.class == template
                        {
                            c.class = receiver.to_string();
                        }
                        for t in &mut c.type_args {
                            *t = ty(t);
                        }
                    }
                    Expr::StaticField(c) => {
                        if let Some((template, receiver)) = static_receiver
                            && c.class == template
                        {
                            c.class = receiver.to_string();
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
                if let Stmt::StaticFieldAssign(assign) = s
                    && let Some((template, receiver)) = static_receiver
                    && assign.class == template
                {
                    assign.class = receiver.to_string();
                }
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

#[cfg(test)]
mod semantic_fact_identity_tests {
    use super::*;
    use crate::{
        lexer::Lexer,
        parser::{
            Parser,
            iter::{AstEvent, AstWalk},
        },
    };

    #[test]
    fn semantic_facts_specialization_never_reuses_template_node_ids() {
        let source = "fn identity<T>(x: T) -> T { let f = |v: T| v; return match true { true => f(x), false => x }; }";
        let (program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
        assert!(errors.is_empty());
        let collect = |item: &Item| {
            let Item::Function(function) = item else {
                panic!()
            };
            let mut exprs = HashSet::new();
            let mut patterns = HashSet::new();
            let mut bodies = HashSet::new();
            for event in AstWalk::new(AstEvent::Block(&function.body)) {
                match event {
                    AstEvent::Expr(expr) => {
                        assert!(exprs.insert(expr.id()));
                    }
                    AstEvent::Pattern(pattern) => {
                        assert!(patterns.insert(pattern.id()));
                    }
                    AstEvent::Block(block) => {
                        assert!(bodies.insert(block.id));
                    }
                    _ => {}
                }
            }
            (exprs, patterns, bodies)
        };
        let template = &program.items[0];
        let (mut exprs, mut patterns, mut bodies) = collect(template);
        for ty in [Type::I64, Type::Bool, Type::String] {
            let mut instance = template.clone();
            substitute_item(&mut instance, &HashMap::from([("T".into(), ty)]));
            let (new_exprs, new_patterns, new_bodies) = collect(&instance);
            assert!(exprs.is_disjoint(&new_exprs));
            assert!(patterns.is_disjoint(&new_patterns));
            assert!(bodies.is_disjoint(&new_bodies));
            exprs.extend(new_exprs);
            patterns.extend(new_patterns);
            bodies.extend(new_bodies);
        }
    }
}

impl Generics {
    pub(super) fn journal_value(
        &self,
        key: &super::precheck_journal::Key,
        raw: bool,
    ) -> serde_json::Value {
        let registry = self.registry.borrow();
        match key.field.as_str() {
            "generic.rejected_parameter_methods" => {
                if raw {
                    self.rejected_parameter_methods.output_value(key)
                } else {
                    self.rejected_parameter_methods.read_value(key)
                }
            }
            "registry.module_paths" => {
                if raw {
                    registry.module_paths.output_value(key)
                } else {
                    registry.module_paths.read_value(key)
                }
            }
            "registry.requested" => {
                if raw {
                    registry.requested.output_value(key)
                } else {
                    registry.requested.read_value(key)
                }
            }
            "registry.nominal_types" => {
                if raw {
                    registry.nominal_types.output_value(key)
                } else {
                    registry.nominal_types.read_value(key)
                }
            }
            "registry.requested_methods" => {
                if raw {
                    registry.requested_methods.output_value(key)
                } else {
                    registry.requested_methods.read_value(key)
                }
            }
            "registry.deferred_classes" => {
                if raw {
                    registry.deferred_classes.output_value(key)
                } else {
                    registry.deferred_classes.read_value(key)
                }
            }
            "registry.method_templates" => {
                if raw {
                    registry.method_templates.output_value(key)
                } else {
                    registry.method_templates.read_value(key)
                }
            }
            "registry.instantiated_classes" => {
                if raw {
                    registry.instantiated_classes.output_value(key)
                } else {
                    registry.instantiated_classes.read_value(key)
                }
            }
            "registry.functions" => {
                if raw {
                    registry.functions.output_value(key)
                } else {
                    registry.functions.read_value(key)
                }
            }
            "registry.classes" => {
                if raw {
                    registry.classes.output_value(key)
                } else {
                    registry.classes.read_value(key)
                }
            }
            "registry.interfaces" => {
                if raw {
                    registry.interfaces.output_value(key)
                } else {
                    registry.interfaces.read_value(key)
                }
            }
            "registry.interface_instances" => {
                if raw {
                    registry.interface_instances.output_value(key)
                } else {
                    registry.interface_instances.read_value(key)
                }
            }
            "registry.runtime_names" => {
                if raw {
                    registry.runtime_names.output_value(key)
                } else {
                    registry.runtime_names.read_value(key)
                }
            }
            "origins.bodies" => {
                if raw {
                    registry.origins.bodies.output_value(key)
                } else {
                    registry.origins.bodies.read_value(key)
                }
            }
            "origins.initializers" => {
                if raw {
                    registry.origins.initializers.output_value(key)
                } else {
                    registry.origins.initializers.read_value(key)
                }
            }
            "generic.functions" => {
                if raw {
                    self.functions.output_value(key)
                } else {
                    self.functions.read_value(key)
                }
            }
            "generic.classes" => {
                if raw {
                    self.classes.output_value(key)
                } else {
                    self.classes.read_value(key)
                }
            }
            "generic.instances" => {
                if raw {
                    self.instances.output_value(key)
                } else {
                    self.instances.read_value(key)
                }
            }
            "generic.imported_types" => {
                if raw {
                    self.imported_types.output_value(key)
                } else {
                    self.imported_types.read_value(key)
                }
            }
            "generic.logical_types" => {
                if raw {
                    self.logical_types.output_value(key)
                } else {
                    self.logical_types.read_value(key)
                }
            }
            "generic.nominal_aliases" => {
                if raw {
                    self.nominal_aliases.output_value(key)
                } else {
                    self.nominal_aliases.read_value(key)
                }
            }
            "generic.calls" => {
                if raw {
                    self.calls.output_value(key)
                } else {
                    self.calls.read_value(key)
                }
            }
            "generic.function_paths" => {
                if raw {
                    self.function_paths.output_value(key)
                } else {
                    self.function_paths.read_value(key)
                }
            }
            "generic.class_paths" => {
                if raw {
                    self.class_paths.output_value(key)
                } else {
                    self.class_paths.read_value(key)
                }
            }
            "generic.objects" => {
                if raw {
                    self.objects.output_value(key)
                } else {
                    self.objects.read_value(key)
                }
            }
            "generic.default_copies" => {
                if raw {
                    self.default_copies.output_value(key)
                } else {
                    self.default_copies.read_value(key)
                }
            }
            _ => serde_json::Value::Null,
        }
    }
    pub(super) fn validate_journal_key(
        &self,
        key: &super::precheck_journal::Key,
    ) -> anyhow::Result<()> {
        let registry = self.registry.borrow();
        match key.field.as_str() {
            "generic.rejected_parameter_methods" => {
                self.rejected_parameter_methods.validate_key(key)
            }
            "registry.module_paths" => registry.module_paths.validate_key(key),
            "registry.requested" => registry.requested.validate_key(key),
            "registry.nominal_types" => registry.nominal_types.validate_key(key),
            "registry.requested_methods" => registry.requested_methods.validate_key(key),
            "registry.deferred_classes" => registry.deferred_classes.validate_key(key),
            "registry.method_templates" => registry.method_templates.validate_key(key),
            "registry.instantiated_classes" => registry.instantiated_classes.validate_key(key),
            "registry.functions" => registry.functions.validate_key(key),
            "registry.classes" => registry.classes.validate_key(key),
            "registry.interfaces" => registry.interfaces.validate_key(key),
            "registry.interface_instances" => registry.interface_instances.validate_key(key),
            "registry.runtime_names" => registry.runtime_names.validate_key(key),
            "origins.bodies" => registry.origins.bodies.validate_key(key),
            "origins.initializers" => registry.origins.initializers.validate_key(key),
            "generic.functions" => self.functions.validate_key(key),
            "generic.classes" => self.classes.validate_key(key),
            "generic.instances" => self.instances.validate_key(key),
            "generic.imported_types" => self.imported_types.validate_key(key),
            "generic.logical_types" => self.logical_types.validate_key(key),
            "generic.nominal_aliases" => self.nominal_aliases.validate_key(key),
            "generic.calls" => self.calls.validate_key(key),
            "generic.function_paths" => self.function_paths.validate_key(key),
            "generic.class_paths" => self.class_paths.validate_key(key),
            "generic.objects" => self.objects.validate_key(key),
            "generic.default_copies" => self.default_copies.validate_key(key),
            _ => anyhow::bail!("unknown generic dependency field"),
        }
    }
    pub(super) fn replay_budget_allows(
        &self,
        writes: &[(super::precheck_journal::Key, serde_json::Value)],
    ) -> bool {
        let registry = self.registry.borrow();
        let extra = writes
            .iter()
            .filter(|(key, value)| {
                key.field == "registry.requested"
                    && value.get(0) == Some(&serde_json::Value::Bool(true))
            })
            .filter(|(key, _)| {
                key.key
                    .as_ref()
                    .and_then(|k| serde_json::from_value::<InstantiationKey>(k.clone()).ok())
                    .is_some_and(|key| !registry.requested.raw().contains_key(&key))
            })
            .count();
        registry.requested.raw().len() + extra <= 4096
    }
    pub(super) fn restore_journal(
        &mut self,
        key: &super::precheck_journal::Key,
        value: serde_json::Value,
    ) -> anyhow::Result<()> {
        let mut registry = self.registry.borrow_mut();
        match key.field.as_str() {
            "generic.rejected_parameter_methods" => {
                self.rejected_parameter_methods.restore_value(key, value)?
            }
            "registry.module_paths" => {
                Rc::make_mut(&mut registry.module_paths).restore_value(key, value)?
            }
            "registry.requested" => registry.requested.restore_value(key, value)?,
            "registry.nominal_types" => registry.nominal_types.restore_value(key, value)?,
            "registry.instantiated_classes" => {
                registry.instantiated_classes.restore_value(key, value)?
            }
            "registry.deferred_classes" => registry.deferred_classes.restore_value(key, value)?,
            "registry.method_templates" => registry.method_templates.restore_value(key, value)?,
            "registry.requested_methods" => registry.requested_methods.restore_value(key, value)?,
            "registry.functions" => registry.functions.restore_value(key, value)?,
            "registry.classes" => Rc::make_mut(&mut registry.classes).restore_value(key, value)?,
            "registry.interfaces" => registry.interfaces.restore_value(key, value)?,
            "registry.interface_instances" => {
                registry.interface_instances.restore_value(key, value)?
            }
            "registry.runtime_names" => registry.runtime_names.restore_value(key, value)?,
            "origins.bodies" => registry.origins.bodies.restore_value(key, value)?,
            "origins.initializers" => registry.origins.initializers.restore_value(key, value)?,
            "generic.functions" => self.functions.restore_value(key, value)?,
            "generic.classes" => self.classes.restore_value(key, value)?,
            "generic.instances" => self.instances.restore_value(key, value)?,
            "generic.imported_types" => self.imported_types.restore_value(key, value)?,
            "generic.logical_types" => self.logical_types.restore_value(key, value)?,
            "generic.nominal_aliases" => self.nominal_aliases.restore_value(key, value)?,
            "generic.calls" => self.calls.restore_value(key, value)?,
            "generic.function_paths" => self.function_paths.restore_value(key, value)?,
            "generic.class_paths" => self.class_paths.restore_value(key, value)?,
            "generic.objects" => self.objects.restore_value(key, value)?,
            "generic.default_copies" => self.default_copies.restore_value(key, value)?,
            _ => anyhow::bail!("unknown generic replay field"),
        }
        Ok(())
    }
}
