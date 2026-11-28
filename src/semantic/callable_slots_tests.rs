use super::*;

fn parse(source: &str) -> Program {
    let tokens = crate::lexer::Lexer::new(source).tokenize().expect("lex");
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "parse errors: {errors:?}");
    program
}

fn class<'a>(program: &'a Program, name: &str) -> &'a ClassDecl {
    program
        .items
        .iter()
        .find_map(|item| match item {
            Item::Class(class) if class.name == name => Some(class),
            _ => None,
        })
        .unwrap_or_else(|| panic!("class {name}"))
}

/// Whether `ty` is the shareable kind at its top level.
fn shareable(ty: &Type) -> bool {
    match ty {
        Type::Fn(..) => true,
        Type::Closure(..) => false,
        other => panic!("not a function type: {other:?}"),
    }
}

fn field(source: &str, class_name: &str, field: &str) -> bool {
    let program = parse(source);
    let decl = class(&program, class_name);
    let field = decl.fields.iter().find(|f| f.name == field).expect("field");
    shareable(&field.ty)
}

fn param(source: &str, class_name: &str, index: usize) -> bool {
    let program = parse(source);
    shareable(&class(&program, class_name).constructors[0].params[index].ty)
}

fn payload(source: &str, variant: &str) -> bool {
    let program = parse(source);
    let ty = program
        .items
        .iter()
        .find_map(|item| match item {
            Item::Enum(decl) => decl.variants.iter().find(|v| v.name == variant),
            _ => None,
        })
        .map(|v| v.payload[0].clone())
        .expect("variant");
    shareable(&ty)
}

const STAGE: &str = "fn inc(x: i64) -> i64 { return x + 1; }
class Stage { pub run: fn(i64) -> i64; }
";

fn stage(body: &str) -> bool {
    field(&format!("{STAGE}fn main() {{ {body} }}"), "Stage", "run")
}

#[test]
fn shareable_stores_keep_a_plain_field_shareable() {
    for body in [
        "",
        "let s = new Stage(inc);",
        "let s = new Stage(|x: i64| x * 2);",
        "let k = 3; let s = new Stage(|x: i64| x + k);",
        "let k: String = \"a\"; let s = new Stage(|x: i64| x + k.len());",
        "let s = new Stage(inc); s.run = |x: i64| x;",
        "let g = inc; let s = new Stage(g);",
        "let s = Stage { run: inc };",
        "let k = 2; let s = new Stage(if k > 1 { inc } else { |x: i64| x + k });",
    ] {
        assert!(stage(body), "{body}");
    }
}

#[test]
fn task_local_stores_make_a_plain_field_task_local() {
    for body in [
        "let mut n = 0; let s = new Stage(|x: i64| { n = n + x; return n; });",
        "let mut n = 0; let s = new Stage(inc); s.run = |x: i64| { n = n + x; return n; };",
        "let mut k = 1; let s = new Stage(|x: i64| x + k);",
        // A nested lambda's capture makes the stored outer lambda task-local.
        "let mut n = 0; let s = new Stage(|x: i64| { let g = |y: i64| { n = y; return y; }; return g(x); });",
        // A capture of unknown kind, a call result and an alias of one.
        "let a = [1]; let s = new Stage(|x: i64| x + a[0]);",
        "let s = new Stage(make());",
        "let f = make(); let s = Stage { run: f };",
        "let fs = [inc]; let s = new Stage(fs[0]);",
    ] {
        let source =
            format!("{STAGE}fn make() -> fn(i64) -> i64 {{ return inc; }}\nfn main() {{ {body} }}");
        assert!(!field(&source, "Stage", "run"), "{body}");
    }
}

#[test]
fn parameters_and_awaited_results_follow_their_signatures() {
    // A plain function's parameter may be task-local; an async one's is not.
    let source = format!("{STAGE}fn keep(f: fn(i64) -> i64) {{ let s = new Stage(f); }}");
    assert!(!field(&source, "Stage", "run"));
    let source = format!("{STAGE}async fn keep(f: fn(i64) -> i64) {{ let s = new Stage(f); }}");
    assert!(field(&source, "Stage", "run"));
    let source = format!(
        "{STAGE}async fn get() -> fn(i64) -> i64 {{ return inc; }}\nasync fn main() {{ let s = new Stage(await get()); }}"
    );
    assert!(field(&source, "Stage", "run"));
    // A method capturing `self` in a stored lambda.
    let source = format!(
        "{STAGE}class Owner {{ pub n: i64; pub fn keep(self) {{ let s = new Stage(|x: i64| x + self.n); }} }}"
    );
    assert!(!field(&source, "Stage", "run"));
}

#[test]
fn constructor_parameters_flow_into_fields() {
    let declarations = "fn inc(x: i64) -> i64 { return x + 1; }
class W { pub f: fn(i64) -> i64; pub init(self, g: fn(i64) -> i64) { self.f = g; } }
";
    let shareable_use = format!("{declarations}fn main() {{ let w = new W(inc); }}");
    assert!(param(&shareable_use, "W", 0));
    assert!(field(&shareable_use, "W", "f"));
    let task_local_use = format!(
        "{declarations}fn main() {{ let mut n = 0; let w = new W(|x: i64| {{ n = x; return x; }}); }}"
    );
    assert!(!param(&task_local_use, "W", 0));
    assert!(!field(&task_local_use, "W", "f"));
}

#[test]
fn copies_between_fields_propagate_task_local_kinds() {
    let declarations = "fn inc(x: i64) -> i64 { return x + 1; }
class A { pub f: fn(i64) -> i64; }
class B { pub g: fn(i64) -> i64; }
class C { pub h: fn(i64) -> i64; }
";
    // A.f is task-local, B.g copies it, C.h copies B.g.
    let source = format!(
        "{declarations}fn main() {{ let mut n = 0; let a = new A(|x: i64| {{ n = x; return x; }}); let b = new B(a.f); let c = new C(b.g); }}"
    );
    assert!(!field(&source, "A", "f"));
    assert!(!field(&source, "B", "g"));
    assert!(!field(&source, "C", "h"));
    let source = format!(
        "{declarations}fn main() {{ let a = new A(inc); let b = new B(a.f); let c = new C(b.g); }}"
    );
    assert!(field(&source, "C", "h"));
}

#[test]
fn unresolved_receivers_taint_every_field_of_that_name() {
    let source = format!(
        "{STAGE}class Other {{ pub run: fn(i64) -> i64; }}\nclass Keep {{ pub keep: fn(i64) -> i64; }}\nfn main() {{ let mut n = 0; let xs = [new Stage(inc)]; xs[0].run = |x: i64| {{ n = x; return x; }}; }}"
    );
    assert!(!field(&source, "Stage", "run"));
    assert!(!field(&source, "Other", "run"));
    assert!(field(&source, "Keep", "keep"));
}

#[test]
fn enum_payloads_follow_their_constructions() {
    let declarations = "fn inc(x: i64) -> i64 { return x + 1; }
enum Op { Apply(fn(i64) -> i64), Nothing }
";
    for body in ["let o = Op::Apply(inc);", "let o = Apply(inc);", ""] {
        assert!(
            payload(&format!("{declarations}fn main() {{ {body} }}"), "Apply"),
            "{body}"
        );
    }
    for body in [
        "let mut n = 0; let o = Op::Apply(|x: i64| { n = x; return x; });",
        "let mut n = 0; let o = Apply(|x: i64| { n = x; return x; });",
    ] {
        assert!(
            !payload(&format!("{declarations}fn main() {{ {body} }}"), "Apply"),
            "{body}"
        );
    }
}

#[test]
fn generic_class_fields_follow_their_stores() {
    let declarations = "fn inc(x: i64) -> i64 { return x + 1; }
class G<T> { pub f: fn(T) -> T; pub v: T; }
";
    let source = format!("{declarations}fn main() {{ let g = new G<i64>(inc, 1); }}");
    assert!(field(&source, "G", "f"));
    let source = format!(
        "{declarations}fn main() {{ let mut n = 0; let g = new G<i64>(|x: i64| {{ n = x; return x; }}, 1); }}"
    );
    assert!(!field(&source, "G", "f"));
}

#[test]
fn implicit_and_base_constructors_map_arguments_to_slots() {
    // The implicit constructor takes base fields first.
    let declarations = "fn inc(x: i64) -> i64 { return x + 1; }
open class Base { pub a: i64; }
class Derived extends Base { pub f: fn(i64) -> i64; }
open class P { pub g: fn(i64) -> i64; pub init(self, g: fn(i64) -> i64) { self.g = g; } }
class Q extends P { pub init(self) { let mut n = 0; super.init(|x: i64| { n = x; return x; }); } }
";
    let source = format!(
        "{declarations}fn main() {{ let mut n = 0; let d = new Derived(1, |x: i64| {{ n = x; return x; }}); }}"
    );
    assert!(!field(&source, "Derived", "f"));
    assert!(!param(&source, "P", 0));
    assert!(!field(&source, "P", "g"));
    let clean = "fn inc(x: i64) -> i64 { return x + 1; }
open class Base { pub a: i64; }
class Derived extends Base { pub f: fn(i64) -> i64; }
fn main() { let d = new Derived(1, inc); }";
    assert!(field(clean, "Derived", "f"));
}

#[test]
fn promised_classes_are_shareable_at_every_depth() {
    for declarations in [
        "class C implements Sync { pub f: fn(i64) -> i64; pub g: Array<fn(i64) -> i64>; }",
        "interface A extends Send {} interface B extends A {} class C implements B { pub f: fn(i64) -> i64; pub g: Array<fn(i64) -> i64>; }",
        "interface S extends Sync {} open class Base implements S {} class C extends Base { pub f: fn(i64) -> i64; pub g: Array<fn(i64) -> i64>; }",
    ] {
        // A task-local store does not change a promised slot; the checker
        // rejects that store instead.
        let program = parse(&format!(
            "{declarations}\nfn main() {{ let mut n = 0; let c = new C(|x: i64| {{ n = x; return x; }}, []); }}"
        ));
        let decl = class(&program, "C");
        assert!(shareable(&decl.fields[0].ty), "{declarations}");
        assert!(
            matches!(&decl.fields[1].ty, Type::Generic(_, args) if matches!(args[0], Type::Fn(..))),
            "{declarations}: {:?}",
            decl.fields[1].ty
        );
    }
}

#[test]
fn decisions_are_reapplied_from_parsed_kinds() {
    // Applying twice gives the same declarations, and a class that stops
    // being promised loses the nested shareable kinds.
    let mut program = parse(
        "interface S extends Sync {} class C implements S { pub g: Array<fn(i64) -> i64>; } fn main() {}",
    );
    apply_program(&mut program);
    let Item::Class(decl) = &mut program.items[1] else {
        panic!("class");
    };
    assert!(
        matches!(&decl.fields[0].ty, Type::Generic(_, args) if matches!(args[0], Type::Fn(..)))
    );
    decl.implements.clear();
    apply_program(&mut program);
    let Item::Class(decl) = &program.items[1] else {
        panic!("class");
    };
    assert!(
        matches!(&decl.fields[0].ty, Type::Generic(_, args) if matches!(args[0], Type::Closure(..)))
    );
}

/// Deterministic work counts grow linearly in every input dimension,
/// including deep inheritance, copy chains and by-name fan-out.
#[test]
fn resolution_work_is_linear() {
    fn stats(source: String) -> SlotStats {
        let mut program = parse(&source);
        apply_program(&mut program)
    }
    let deep = |n: usize| {
        let mut source = String::from("open class C0 { pub f: fn(i64) -> i64; }\n");
        for i in 1..n {
            source.push_str(&format!("open class C{i} extends C{} {{}}\n", i - 1));
        }
        source.push_str("fn main() { let mut m = 0;");
        for i in 0..n {
            source.push_str(&format!(
                " let c{i} = new C{i}(|x: i64| x); c{i}.f = |x: i64| {{ m = x; return x; }};"
            ));
        }
        source.push_str(" }");
        source
    };
    let chain = |n: usize| {
        let mut source = String::new();
        for i in 0..n {
            source.push_str(&format!("class K{i} {{ pub f: fn(i64) -> i64; }}\n"));
        }
        source
            .push_str("fn main() { let mut m = 0; let k0 = new K0(|x: i64| { m = x; return x; });");
        for i in 1..n {
            source.push_str(&format!(" let k{i} = new K{i}(k{}.f);", i - 1));
        }
        source.push_str(" }");
        source
    };
    let fan_out = |n: usize| {
        let mut source = String::new();
        for i in 0..n {
            source.push_str(&format!("class S{i} {{ pub run: fn(i64) -> i64; }}\n"));
        }
        source.push_str("fn main() { let mut m = 0; let xs = [1];");
        for _ in 0..n {
            source.push_str(" xs[0].run = |x: i64| { m = x; return x; };");
        }
        for i in 0..n {
            source.push_str(&format!(
                " let s{i} = new S{i}(|x: i64| x); let r{i} = s{i}.run;"
            ));
        }
        source.push_str(" }");
        source
    };
    for (name, build) in [
        ("deep", &deep as &dyn Fn(usize) -> String),
        ("chain", &chain),
        ("fan_out", &fan_out),
    ] {
        let small = stats(build(50));
        let large = stats(build(100));
        for (what, a, b) in [
            ("stores", small.stores, large.stores),
            ("nodes", small.nodes, large.nodes),
            ("edges", small.edges, large.edges),
            ("visits", small.visits, large.visits),
        ] {
            // Doubling the input at most doubles the work (+ a constant).
            println!("{name} {what}: n=50 {a}, n=100 {b}");
            assert!(b <= 2 * a + 4, "{name} {what}: {a} -> {b}");
        }
    }
    // Every class in the deep chain resolves the inherited field.
    let program = parse(&deep(50));
    assert!(!shareable(&class(&program, "C0").fields[0].ty));
}

#[test]
fn new_construction_scan_preserves_capture_slots() {
    for declaration in [
        "class Holder { pub callback: fn(i64) -> i64; }",
        "class Holder { pub callback: fn(i64) -> i64; pub init(self, f: fn(i64) -> i64) { self.callback = f; } }",
    ] {
        let source = format!(
            "{declaration} fn main() {{ let xs = [1]; let h = new Holder(|x: i64| x + xs[0]); let callback = h.callback; println(callback(2)); }}"
        );
        let program = parse(&source);
        assert!(
            !shareable(&class(&program, "Holder").fields[0].ty),
            "{source}"
        );
        let mut checker = crate::semantic::TypeChecker::new();
        checker.check_program(&program);
        assert!(checker.errors.is_empty(), "{source}: {:?}", checker.errors);
    }
    let source = "class Box<T> { pub f: fn(T) -> T; pub value: T; } fn main() { let xs = [1]; let b = new Box<i64>(|x: i64| x + xs[0], 1); }";
    assert!(!field(source, "Box", "f"));
    let source = format!(
        "{STAGE} class Other {{ pub run: fn(i64) -> i64; }} fn main() {{ let s = new Stage(inc); let alias = s; let xs = [1]; alias.run = |x: i64| x + xs[0]; }}"
    );
    assert!(!field(&source, "Stage", "run"));
    assert!(field(&source, "Other", "run"));
}

#[test]
fn call_scan_never_treats_a_call_as_construction() {
    // Only `new` constructs: a call spelled like the class (rejected by the
    // checker), a function or a local binding with that name stores nothing
    // into the class's constructor slots.
    for (declaration, binding) in [
        ("", ""),
        ("fn Stage(f: fn(i64) -> i64) -> i64 { 0 }", ""),
        ("", "let Stage = |f: fn(i64) -> i64| 0;"),
    ] {
        let source = format!(
            "{STAGE} {declaration} fn main() {{ {binding} let xs = [1]; Stage(|x: i64| x + xs[0]); }}"
        );
        assert!(field(&source, "Stage", "run"), "{source}");
    }
}

#[test]
fn new_construction_scan_resolves_imports_and_scales() {
    for call in ["model::Holder", "m::Holder", "Container"] {
        let mut model = parse("pub class Holder { pub callback: fn(i64) -> i64; }");
        let source = format!(
            "import model; import model as m; import model::Holder as Container; fn main() {{ let xs = [1]; let h = new {call}(|x: i64| x + xs[0]); }}"
        );
        let (mut main, errors) = crate::parser::Parser::new(
            crate::lexer::Lexer::with_file_id(&source, FileId(1))
                .tokenize()
                .unwrap(),
        )
        .parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut stores = program_stores(&model);
        stores.extend(program_stores(&main));
        apply_build(
            &mut [("model", &mut model), ("main", &mut main)],
            None,
            &stores,
        );
        assert!(
            !shareable(&class(&model, "Holder").fields[0].ty),
            "{source}"
        );
    }
    for count in [8, 32, 128] {
        let declarations = (0..count)
            .map(|i| format!("class Holder{i} {{ pub f: fn(i64) -> i64; }}"))
            .collect::<String>();
        let calls = (0..count)
            .map(|i| format!("new Holder{i}(|x: i64| x + xs[0]);"))
            .collect::<String>();
        let mut program = parse(&format!(
            "{declarations} fn main() {{ let xs = [1]; {calls} }}"
        ));
        let stores = program_stores(&program);
        assert_eq!(stores.len(), count);
        let stats = apply_build(&mut [("", &mut program)], None, &stores);
        assert_eq!(stats.stores, count);
        assert_eq!(stats.visits, count);
        eprintln!(
            "new construction classes={count} stores={} visits={}",
            stats.stores, stats.visits
        );
    }
}

#[test]
fn call_scan_indexes_qualified_variants_once() {
    for count in [8, 32, 128] {
        let variants = (0..count)
            .map(|i| format!("V{i}(fn(i64) -> i64),"))
            .collect::<String>();
        let calls = (0..count)
            .map(|i| format!("Op::V{i}(|x: i64| x + xs[0]);"))
            .collect::<String>();
        let mut program = parse(&format!(
            "enum Op {{ {variants} }} fn main() {{ let xs = [1]; {calls} }}"
        ));
        let stores = program_stores(&program);
        let stats = apply_build(&mut [("", &mut program)], None, &stores);
        assert_eq!(stats.variant_lookups, count);
        assert_eq!(stats.stores, count);
        assert_eq!(stats.visits, count);
        eprintln!(
            "qualified variants={count} lookups={} stores={} visits={}",
            stats.variant_lookups, stats.stores, stats.visits
        );
    }
}

#[test]
fn construction_scan_indexes_arity_and_shared_inherited_layouts() {
    for count in [8, 32, 128] {
        let chain = (1..=count)
            .map(|i| {
                format!(
                    "open class C{i} extends C{} {{ pub value{i}: i64; }}",
                    i - 1
                )
            })
            .collect::<String>();
        let program = parse(&format!(
            "open class C0 {{ pub f: fn(i64) -> i64; }} {chain} fn main() {{}}"
        ));
        let mut index = Index::new(&[("", &program)], None);
        let expected = index.constructor_slot(0, 1, 0).unwrap();
        for _ in 0..4 {
            for class in (0..=count).rev() {
                assert_eq!(index.constructor_slot(class, class + 1, 0), Some(expected));
            }
        }
        assert_eq!(index.graph.stats.constructor_steps, count + 1);
        assert_eq!(index.graph.stats.implicit_layout_steps, count + 1);
        assert_eq!(index.graph.stats.implicit_field_steps, count + 1);
        eprintln!(
            "implicit chain={count} calls={} owner_steps={} layouts={} fields={}",
            4 * (count + 1),
            index.graph.stats.constructor_steps,
            index.graph.stats.implicit_layout_steps,
            index.graph.stats.implicit_field_steps
        );

        let constructors = (0..count)
            .map(|arity| {
                let extras = (0..arity)
                    .map(|i| format!(", p{i}: i64"))
                    .collect::<String>();
                format!("pub init(self, f: fn(i64) -> i64{extras}) {{}}")
            })
            .collect::<String>();
        let program = parse(&format!(
            "open class C0 {{ {constructors} }} {chain} fn main() {{}}"
        ));
        let mut index = Index::new(&[("", &program)], None);
        for _ in 0..4 {
            for arity in (1..=count).rev() {
                assert!(index.constructor_slot(count, arity, 0).is_some());
            }
        }
        assert_eq!(index.graph.stats.constructor_steps, count + 1);
        assert_eq!(index.graph.stats.constructor_arity_lookups, 4 * count);
        assert_eq!(index.graph.stats.implicit_layout_steps, 0);
        eprintln!(
            "explicit chain={count} arities={count} calls={} owner_steps={} arity_lookups={}",
            4 * count,
            index.graph.stats.constructor_steps,
            index.graph.stats.constructor_arity_lookups
        );
    }
}
