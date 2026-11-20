use super::*;

fn interface(source: &str) -> Program {
    let (program, errors) = parse(Path::new("src/native.rs"), source, FileId(7));
    assert!(errors.is_empty(), "{errors:#?}");
    program
}

fn function(program: &Program, name: &str) -> RustBridgeSymbol {
    program
        .items
        .iter()
        .find_map(|item| match item {
            Item::Function(f) if f.name == name => f.rust_bridge.as_deref().cloned(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing function {name}"))
}

macro_rules! scalar_case {
    ($name:ident, $rust:literal, $scalar:expr) => {
        #[test]
        fn $name() {
            let program = interface(concat!("pub fn echo(v: ", $rust, ") -> ", $rust, " { v }"));
            let symbol = function(&program, "echo");
            assert_eq!(symbol.input_types, vec![$scalar]);
            assert_eq!(symbol.output_type, $scalar);
            assert_ne!(
                symbol.effects & willow_abi::RuntimeEffects::MAY_BLOCK.bits(),
                0
            );
        }
    };
}
scalar_case!(signed_byte, "i8", Scalar::I8);
scalar_case!(signed_short, "i16", Scalar::I16);
scalar_case!(signed_word, "i32", Scalar::I32);
scalar_case!(signed_long, "i64", Scalar::I64);
scalar_case!(unsigned_byte, "u8", Scalar::U8);
scalar_case!(unsigned_short, "u16", Scalar::U16);
scalar_case!(unsigned_word, "u32", Scalar::U32);
scalar_case!(unsigned_long, "u64", Scalar::U64);
scalar_case!(unicode_char, "char", Scalar::Char);
scalar_case!(double_float, "f64", Scalar::F64);
scalar_case!(boolean, "bool", Scalar::Bool);
scalar_case!(owned_string, "String", Scalar::String);
scalar_case!(owned_bytes, "Vec<u8>", Scalar::Bytes);
scalar_case!(
    integer_array,
    "Vec<i64>",
    Scalar::Array(Box::new(Scalar::I64))
);
scalar_case!(
    string_array,
    "Vec<String>",
    Scalar::Array(Box::new(Scalar::String))
);
scalar_case!(
    optional_scalar,
    "Option<i64>",
    Scalar::Option(Box::new(Scalar::I64))
);
scalar_case!(
    result_pair,
    "Result<i64, bool>",
    Scalar::Result(Box::new(Scalar::I64), Box::new(Scalar::Bool))
);
scalar_case!(
    nested_optional,
    "Option<Option<String>>",
    Scalar::Option(Box::new(Scalar::Option(Box::new(Scalar::String))))
);

#[test]
fn borrowed_strings_bytes_and_unit_returns() {
    let program = interface(
        "pub fn text(v: &str) -> String { v.into() } pub fn bytes(v: &[u8]) -> Vec<u8> { v.to_vec() } pub fn touch() {} pub fn unit() -> () {}",
    );
    assert_eq!(function(&program, "text").input_types, [Scalar::String]);
    assert_eq!(function(&program, "bytes").input_types, [Scalar::Bytes]);
    assert_eq!(function(&program, "touch").output_type, Scalar::Void);
    assert_eq!(function(&program, "unit").output_type, Scalar::Void);
}

#[test]
fn nested_records_resolve_forward_declarations() {
    let program = interface(
        "pub struct Outer { pub inner: Inner, pub label: String } pub struct Inner { pub number: i64 } pub fn echo(v: Outer) -> Outer { v }",
    );
    let symbol = function(&program, "echo");
    assert_eq!(symbol.records.len(), 2);
    assert!(matches!(&symbol.input_types[0], Scalar::Record(name) if name.ends_with("_Outer")));
    assert!(symbol.native.unwrap().adapter.contains("to_native_"));
}

#[test]
fn opaque_handles_have_close_and_private_representation() {
    let program = interface(
        "pub struct Handle { secret: i64 } pub fn make() -> Box<Handle> { Box::new(Handle { secret: 42 }) } pub fn read(v: &Handle) -> i64 { v.secret }",
    );
    assert!(matches!(
        function(&program, "make").output_type,
        Scalar::Opaque(_)
    ));
    assert!(matches!(
        &function(&program, "read").input_types[0],
        Scalar::Opaque(_)
    ));
    assert!(function(&program, "Handle_close").close_handle);
    let captures = program.items.iter().filter(|item| matches!(item, Item::Function(f) if f.rust_bridge.as_ref().unwrap().native.as_ref().unwrap().source.is_some())).count();
    assert_eq!(captures, 1);
}

#[test]
fn asynchronous_interfaces_keep_conservative_effects() {
    let symbol = function(
        &interface("pub async fn echo(v: String) -> String { v }"),
        "echo",
    );
    assert!(symbol.is_async);
    assert_ne!(
        symbol.effects & willow_abi::RuntimeEffects::MAY_BLOCK.bits(),
        0
    );
    assert_ne!(
        symbol.effects & willow_abi::RuntimeEffects::MAY_SUSPEND.bits(),
        0
    );
    assert!(symbol.nonblocking.is_none());
}

#[test]
fn restricted_and_private_items_are_not_exported() {
    let program = interface(
        "fn hidden<T>(x:T)->T{x} pub(crate) fn internal(x: usize)->usize{x} pub(super) struct Hidden<T>(T); pub fn visible()->i64{42}",
    );
    assert_eq!(program.items.len(), 1);
    assert_eq!(function(&program, "visible").output_type, Scalar::I64);
}

macro_rules! rejects {
    ($name:ident, $source:literal, $needle:literal) => {
        #[test]
        fn $name() {
            let source = concat!("// interface\n", $source);
            let (_, errors) = parse(Path::new("src/native.rs"), source, FileId(7));
            assert!(!errors.is_empty(), "silently accepted {source}");
            let label = &errors[0].labels[0];
            assert_eq!(label.span.file_id, FileId(7));
            assert!(label.span.line >= 2, "{label:?}");
            assert!(label.span.end <= source.len(), "{label:?}");
            assert!(
                source[label.span.start..label.span.end].contains($needle),
                "{}: {label:?}",
                errors[0].message
            );
        }
    };
}
rejects!(unsupported_usize, "pub fn bad(v: usize) {}", "usize");
rejects!(unsupported_float32, "pub fn bad(v: f32) {}", "f32");
rejects!(generic_function, "pub fn bad<T>(v: T) {}", "T");
rejects!(lifetime_function, "pub fn bad<'a>(v: &'a str) {}", "'a");
rejects!(
    explicit_reference_lifetime,
    "pub fn bad(v: &'static str) {}",
    "'static"
);
rejects!(
    impl_trait_parameter,
    "pub fn bad(v: impl Send) {}",
    "impl Send"
);
rejects!(
    impl_trait_result,
    "pub fn bad() -> impl Send { 1 }",
    "impl Send"
);
rejects!(generic_struct, "pub struct Bad<T> { pub value: T }", "T");
rejects!(mutable_reference, "pub fn bad(v: &mut str) {}", "&mut str");
rejects!(borrowed_result, "pub fn bad() -> &str { \"text\" }", "&str");
rejects!(tuple_record, "pub struct Bad(pub i64);", "i64");
rejects!(
    private_record_field,
    "pub struct Bad { value: i64 }",
    "value"
);
rejects!(
    unsupported_array_element,
    "pub fn bad(v: Vec<Option<i64>>) {}",
    "Vec"
);
rejects!(conditional_interface, "#[cfg(unix)] pub fn bad() {}", "cfg");
rejects!(
    recursive_record,
    "pub struct Bad { pub child: Option<Bad> }",
    "Bad"
);

#[test]
fn syntax_error_has_rust_source_location() {
    let (_, errors) = parse(
        Path::new("src/broken.rs"),
        "// heading\npub fn broken( {",
        FileId(19),
    );
    assert!(!errors.is_empty());
    assert_eq!(errors[0].labels[0].span.file_id, FileId(19));
    assert_eq!(errors[0].labels[0].span.line, 2);
}

#[test]
fn content_unchanged_reuses_derived_interface() {
    use crate::compiler_db::incremental::SyntaxQueries;
    let path = Path::new("native.rs");
    let source = "pub fn answer() -> i64 { 42 }";
    let mut accepted = SyntaxQueries::default().candidate().unwrap();
    let (_, errors, _) = accepted.parse(path, FileId(1), source).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    let mut next = accepted.candidate().unwrap();
    let (program, errors, _) = next.parse(path, FileId(1), source).unwrap();
    assert!(errors.is_empty());
    assert_eq!(function(&program, "answer").output_type, Scalar::I64);
    assert_eq!(next.stats().recomputed, 0);
}

#[test]
fn shared_rust_import_is_read_once_for_increasing_fan_out() {
    let root =
        std::env::temp_dir().join(format!("willow-rust-module-fanout-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("native.rs"), "pub fn answer() -> i64 { 42 }").unwrap();
    for count in [1, 8, 32] {
        let mut source = String::new();
        for index in 0..count {
            writeln!(source, "import client{index};").unwrap();
            std::fs::write(
                root.join(format!("client{index}.wi")),
                "import native; pub fn answer() -> i64 { return native::answer(); }",
            )
            .unwrap();
        }
        source.push_str("fn main() {}");
        let (entry, errors) =
            crate::parser::Parser::new(crate::lexer::Lexer::new(&source).tokenize().unwrap())
                .parse();
        assert!(errors.is_empty());
        let resolution = crate::module::resolve_imports(&entry, &root);
        assert!(
            resolution.diagnostics.is_empty(),
            "{:?}",
            resolution.diagnostics
        );
        assert_eq!(resolution.graph.rust_source_loads, 1);
        assert_eq!(resolution.graph.source_loads, count + 1);
        println!(
            "importers={count} rust_reads={} total_source_reads={}",
            resolution.graph.rust_source_loads, resolution.graph.source_loads
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn reserved_struct_names_are_source_errors() {
    for name in [
        "String", "Vec", "Option", "Result", "Array", "i64", "Future",
    ] {
        let source = format!("pub struct {name} {{ pub value: i64 }}");
        let (_, errors) = parse(Path::new("src/native.rs"), &source, FileId(7));
        assert!(!errors.is_empty(), "reserved name {name} was accepted");
        assert_eq!(errors[0].labels[0].span.file_id, FileId(7));
    }
}

#[test]
fn repeated_record_signatures_share_one_definition_closure() {
    for count in [1, 8, 32] {
        let mut source = String::from("pub struct Packet { pub value: i64, pub label: String }");
        for index in 0..count {
            write!(source, "pub fn echo{index}(v: Packet) -> Packet {{ v }}").unwrap();
        }
        let program = interface(&source);
        let first = function(&program, "echo0");
        for index in 0..count {
            let current = function(&program, &format!("echo{index}"));
            assert!(first.records.shares_storage(&current.records));
        }
        let definitions: usize = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Function(f) => f
                    .rust_bridge
                    .as_ref()?
                    .native
                    .as_ref()
                    .map(|n| n.definitions.len()),
                _ => None,
            })
            .sum();
        assert_eq!(definitions, 1);
        println!("equivalent_signatures={count} shared_record_definitions={definitions}");
    }
}

#[test]
fn nested_conversion_output_scales_linearly_and_scalar_arrays_are_moves() {
    for depth in [8, 32, 128] {
        let mut ty = NativeType::String { borrowed: false };
        for _ in 0..depth {
            ty = NativeType::Option(Box::new(ty));
        }
        CONVERSION_VISITS.set(0);
        let conversion = ty.convert("input", "native", true);
        assert_eq!(CONVERSION_VISITS.get(), 2 * (depth + 1));
        assert_eq!(conversion.matches(".map(|v|").count(), depth);
        assert_eq!(conversion.matches(".to_owned()").count(), 1);
        assert!(conversion.len() <= depth * 20 + 30);
        println!(
            "option_depth={depth} conversion_maps={depth} generated_bytes={} visits={}",
            conversion.len(),
            CONVERSION_VISITS.get()
        );
        let mut owned = NativeType::String { borrowed: true };
        for _ in 0..depth {
            owned = NativeType::Option(Box::new(owned));
        }
        CONVERSION_VISITS.set(0);
        assert!(owned.needs_owned_conversion());
        let mut conversion = String::new();
        owned.append_owned_conversion(&mut conversion, "&mut input", "native");
        assert_eq!(CONVERSION_VISITS.get(), 2 * (depth + 1));
        assert_eq!(conversion.matches(".map(|v|").count(), depth);
        assert!(conversion.len() <= depth * 32 + 50);
        println!(
            "async_option_depth={depth} conversion_maps={depth} generated_bytes={} visits={}",
            conversion.len(),
            CONVERSION_VISITS.get()
        );
    }
    let array = NativeType::Array(Box::new(NativeType::Value(Scalar::I64)));
    assert_eq!(array.convert("input", "native", true), "input");
}

#[test]
fn assume_no_block_attribute_validation_perspectives() {
    // 1-9: valid free function, method, opaque close, independent claims,
    // retained allocation/panic/foreign effects, async rejection, no pure claim.
    let program = interface("#[willow::assume(no_block)] pub fn f() {} pub fn g() {}");
    assert!(function(&program, "f").nonblocking.is_some());
    assert!(function(&program, "g").nonblocking.is_none());
    // 10-22: malformed effect/path/list, duplicates, unsupported placements,
    // async assertions, mutable/by-value/typed receivers and name collisions.
    for source in [
        "#[willow::assume] pub fn f() {}",
        "#[willow::assume()] pub fn f() {}",
        "#[willow::assume(no_io)] pub fn f() {}",
        "#[willow::assume(no_block, no_io)] pub fn f() {}",
        "#[willow::assume(\"no_block\")] pub fn f() {}",
        "#[willow::assume(no_block)] #[willow::assume(no_block)] pub fn f() {}",
        "#[willow::assume(no_block)] pub struct Record { pub x: i64 }",
        "pub fn f(#[willow::assume(no_block)] x: i64) {}",
        "#[willow::assume(no_block)] pub async fn f() {}",
        "pub struct Handle(i64); impl Handle { pub fn f(&mut self) {} }",
        "pub struct Handle(i64); impl Handle { pub fn f(self) {} }",
        "pub struct Handle(i64); impl Handle { pub fn f(self: &Self) {} }",
        "pub struct Handle(i64); impl Handle { pub fn close(&self) {} }",
    ] {
        let (_, errors) = parse(Path::new("src/native.rs"), source, FileId(7));
        assert!(!errors.is_empty(), "{source}");
    }
}

#[test]
fn assume_no_block_generated_work_is_linear() {
    for count in [1, 16, 128, 512] {
        let source: String = (0..count)
            .map(|i| {
                format!("#[willow::assume(no_block)] pub fn operation_{i}() -> i64 {{ {i} }}\n")
            })
            .collect();
        let program = interface(&source);
        let symbols: Vec<_> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Function(f) => f.rust_bridge.as_deref().cloned(),
                _ => None,
            })
            .collect();
        assert_eq!(symbols.len(), count);
        assert_eq!(
            symbols
                .iter()
                .filter(|s| s.native.as_ref().unwrap().source.is_some())
                .count(),
            1
        );
        let wrappers = crate::rust_bridge_ir::wrappers(&symbols, Path::new("abi.rs"), 1);
        assert_eq!(
            wrappers
                .matches("willow_rust_bridge_enter_nonblocking(1)")
                .count(),
            count
        );
        assert_eq!(
            symbols
                .iter()
                .map(|s| s
                    .native
                    .as_ref()
                    .unwrap()
                    .adapter
                    .matches("let result = ")
                    .count())
                .sum::<usize>(),
            count
        );
    }
}

#[test]
fn assume_no_block_methods_preserve_private_helpers_and_raw_owners() {
    let program = interface(
        "struct Hidden; impl Hidden { pub fn generic<T>(&self, v: T) -> T { v } } #[willow::assume(no_block)] pub fn answer() -> i64 { 42 }",
    );
    assert_eq!(program.items.len(), 1);
    let program = interface(
        "#[willow::assume(no_block)] pub struct r#struct(i64); impl r#struct { #[willow::assume(no_block)] pub fn read(&self) -> i64 { self.0 } }",
    );
    assert!(function(&program, "struct_read").nonblocking.is_some());
}
