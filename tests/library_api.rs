use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use willow_compiler::{BuildMode, CodegenOptions, compile, lexer, parser};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_path(label: &str) -> std::path::PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "willow_library_api_{}_{}_{}",
        label,
        std::process::id(),
        id
    ))
}

#[test]
fn public_options_expose_debug_and_release_profiles() {
    let debug = CodegenOptions::debug();
    assert_eq!(debug.build_mode, BuildMode::Debug);
    assert!(debug.emit_debug_info);

    let release = CodegenOptions::release();
    assert_eq!(release.build_mode, BuildMode::Release);
    assert!(!release.emit_source_map);
}

#[test]
fn public_frontend_modules_can_lex_and_parse() {
    let tokens = lexer::Lexer::new("fn main() { println(42); }")
        .tokenize()
        .expect("source should lex");
    let (program, diagnostics) = parser::Parser::new(tokens).parse();
    assert!(diagnostics.is_empty());
    assert_eq!(program.items.len(), 1);
}

#[test]
fn public_compile_api_builds_a_runnable_program() {
    let source_path = temp_path("source").with_extension("wi");
    let binary_path = temp_path("binary");
    fs::write(&source_path, "fn main() { println(42); }").unwrap();

    let result = compile(
        source_path.to_str().unwrap(),
        binary_path.to_str().unwrap(),
        &CodegenOptions::debug(),
        None,
    );
    assert!(result.is_ok(), "library compile API failed: {result:?}");

    let output = Command::new(&binary_path).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n");

    let _ = fs::remove_file(source_path);
    let _ = fs::remove_file(&binary_path);
    let _ = fs::remove_file(format!("{}.wsmap", binary_path.to_string_lossy()));
}
