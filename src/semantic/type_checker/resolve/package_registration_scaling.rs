use super::*;
#[test]
fn signature_only_source_spelling_index_deduplicates_and_preserves_visible_imports() {
    let program = crate::parser::Parser::new(
        crate::lexer::Lexer::new("pub class Cell {}")
            .tokenize()
            .unwrap(),
    )
    .parse()
    .0;
    for modules in [16, 64, 256, 1024] {
        let mut checker = TypeChecker::new();
        for module in 0..modules {
            let name = format!("m{module}");
            for _ in 0..4 {
                checker.register_signature_only_spelling(&name);
            }
        }
        assert_eq!(checker.signature_only_modules.len(), modules);
        let module = format!("m{}", modules - 1);
        let ty = format!("{module}::Cell");
        assert!(!checker.check_source_type_name(&ty, Span::dummy()));
        checker.register_module(
            &module,
            &format!("$local::{module}"),
            "visible.wi",
            &program,
        );
        assert!(checker.check_source_type_name(&ty, Span::dummy()));
        assert_eq!(checker.signature_only_modules.len(), modules);
        println!(
            "source scope modules={modules} inserts={} unique_spellings={modules}",
            4 * modules
        );
    }
}

#[test]
fn enum_identity_qualification_looks_up_only_signature_names() {
    for modules in [16, 64, 256, 1024] {
        let source = "pub enum Kind { Value } pub fn kind() -> Kind { return Kind::Value; }";
        let program =
            crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
                .parse()
                .0;
        let mut checker = TypeChecker::new();
        ENUM_IDENTITY_LOOKUPS.with(|count| count.set(0));
        for module in 0..modules {
            let name = format!("m{module}");
            checker.register_module(&name, &name, "scaling.wi", &program);
        }
        let lookups = ENUM_IDENTITY_LOOKUPS.with(|count| count.get());
        assert_eq!(lookups, modules);
        assert_eq!(checker.symbols.enums.len(), modules);
        eprintln!("enum registration modules={modules} lookups={lookups}");
    }
}
