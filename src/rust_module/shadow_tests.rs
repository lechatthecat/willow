use super::*;

fn parse_source(source: &str) -> (Program, Vec<Diagnostic>) {
    parse(Path::new("src/native.rs"), source, FileId(19))
}
fn accepted(source: &str) -> Program {
    let (program, errors) = parse_source(source);
    assert!(errors.is_empty(), "{errors:#?}");
    program
}
fn rejected_binding(source: &str) {
    let (_, errors) = parse_source(source);
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert!(
        errors[0].message.contains("local type binding"),
        "{errors:#?}"
    );
    assert_eq!(errors[0].labels[0].span.file_id, FileId(19));
    assert_eq!(errors[0].labels[0].span.line, 2);
    assert_eq!(errors[0].labels.len(), 2);
}

#[test]
fn private_type_declarations_cannot_impersonate_standard_strings() {
    for declaration in [
        "type String = usize;",
        "type String<T = usize> = T;",
        "struct String;",
        "enum String { Value }",
        "union String { value: usize }",
        "trait String {}",
    ] {
        rejected_binding(&format!(
            "{declaration}\npub fn exported(value: String) -> String {{ value }}"
        ));
    }
}

#[test]
fn custom_named_renamed_and_grouped_imports_cannot_impersonate_builtins() {
    for import in [
        "use custom::String;",
        "use custom::Other as String;",
        "use custom::{nested::{Other as String}, Unrelated};",
        "use custom::String as String;",
    ] {
        rejected_binding(&format!(
            "{import}\npub fn exported(value: String) -> String {{ value }}"
        ));
    }
}

#[test]
fn standard_import_does_not_hide_a_conflicting_local_binding() {
    rejected_binding(
        "type String = usize; use ::std::string::String;\npub fn exported(value: String) {}",
    );
}

#[test]
fn private_generic_aliases_remain_implementation_details() {
    accepted(
        "type String<T> = T; fn hidden(value: String<usize>) {} pub fn exported(value: i64) -> i64 { value }",
    );
}

#[test]
fn explicit_standard_imports_keep_original_bridge_mappings() {
    let program = accepted(
        r#"
use std::{string::String, vec::Vec, option::Option, result::Result, boxed::Box};
pub struct Handle { value: i64 }
pub fn exported(value: Option<Vec<String>>) -> Result<Box<Handle>, String> { todo!() }
"#,
    );
    let symbol = program
        .items
        .iter()
        .find_map(|item| match item {
            Item::Function(f) if f.name == "exported" => f.rust_bridge.as_ref(),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        symbol.input_types,
        vec![Scalar::Option(Box::new(Scalar::Array(Box::new(
            Scalar::String
        ))))]
    );
    assert!(matches!(symbol.output_type, Scalar::Result(_, _)));
}

#[test]
fn core_alloc_and_absolute_imports_are_canonical() {
    accepted(
        r#"
use ::alloc::{string::String, vec::Vec, boxed::Box};
use ::core::{option::Option, result::Result, primitive::i64};
pub struct Handle { value: i64 }
pub fn exported(value: Option<Vec<String>>) -> Result<Box<Handle>, String> { todo!() }
"#,
    );
}

#[test]
fn standard_aliases_under_new_names_are_not_guessed() {
    let (_, errors) =
        parse_source("use std::string::String as Text; pub fn exported(value: Text) {}");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("unsupported Rust bridge type"));
}

#[test]
fn unknown_wildcard_imports_require_explicit_standard_paths() {
    rejected_binding("use custom::*;\npub fn exported(value: i64) -> i64 { value }");
    accepted(
        "use custom::*; pub fn exported(value: ::core::primitive::i64) -> ::alloc::vec::Vec<::core::primitive::u8> { todo!() }",
    );
}

#[test]
fn a_wildcard_does_not_hide_explicitly_declared_records() {
    accepted(
        "use custom::*; pub struct Record { pub value: ::core::primitive::i64 } pub fn exported(value: Record) -> Record { value }",
    );
}

#[test]
fn shadowed_standard_roots_require_absolute_paths() {
    rejected_binding("mod std {}\npub fn exported(value: std::string::String) {}");
    rejected_binding(
        "use custom::Namespace as std;\npub fn exported(value: std::string::String) {}",
    );
    rejected_binding("mod std {} use std::string::String;\npub fn exported(value: String) {}");
    accepted(
        "mod std {} pub fn exported(value: ::std::string::String) -> ::std::string::String { value }",
    );
}

#[test]
fn canonical_borrowed_strings_and_bytes_bypass_local_shadows() {
    accepted(
        "type str = usize; type u8 = usize; pub fn exported(text: &::core::primitive::str, data: &[::core::primitive::u8]) -> ::std::string::String { todo!() }",
    );
    rejected_binding("type u8 = usize;\npub fn exported(data: &[u8]) {}");
    rejected_binding("type str = usize;\npub fn exported(text: &str) {}");
}

#[test]
fn qualified_standard_box_marks_a_local_opaque_type() {
    let program = accepted(
        "pub struct Handle { value: i64 } pub fn exported() -> ::std::boxed::Box<Handle> { todo!() }",
    );
    assert!(
        program
            .items
            .iter()
            .any(|item| matches!(item, Item::Enum(e) if e.name == "Handle"))
    );
}

#[test]
fn standard_types_work_in_public_record_fields() {
    accepted(
        "pub struct Record { pub text: ::alloc::string::String, pub values: ::std::vec::Vec<::core::primitive::i64> } pub fn exported(value: Record) -> Record { value }",
    );
}

#[test]
fn absolute_standard_imports_override_uncertain_wildcard_bindings() {
    accepted(
        "use custom::*; use ::std::string::String; use ::core::primitive::i64; pub fn exported(value: String) -> i64 { todo!() }",
    );
}

#[test]
fn unspellable_public_names_have_rust_source_diagnostics() {
    for source in [
        "\npub fn r#async() {}",
        "\npub struct r#class;",
        "\npub struct Record { pub r#await: i64 }",
        "\npub fn this() {}",
    ] {
        let (_, errors) = parse_source(source);
        assert_eq!(errors.len(), 1, "{source}: {errors:#?}");
        assert!(errors[0].message.contains("Willow identifier"));
        assert_eq!(errors[0].labels[0].span.file_id, FileId(19));
        assert_eq!(errors[0].labels[0].span.line, 2);
    }
}

#[test]
fn raw_rust_names_and_contextual_new_remain_callable() {
    let program = accepted(
        "pub struct Record { pub r#type: i64 } pub fn r#type(value: Record) -> Record { value } pub fn new() {}",
    );
    assert!(
        program
            .items
            .iter()
            .any(|item| matches!(item, Item::Function(f) if f.name == "type"))
    );
    assert!(
        program
            .items
            .iter()
            .any(|item| matches!(item, Item::Function(f) if f.name == "new"))
    );
}

#[test]
fn private_and_positional_parameter_names_need_no_willow_spelling() {
    accepted("fn r#async() {} pub fn exported(r#await: i64) -> i64 { r#await }");
}
