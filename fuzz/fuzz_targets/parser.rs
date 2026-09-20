#![no_main]

use libfuzzer_sys::fuzz_target;
use willow_compiler::{lexer::Lexer, parser::Parser};

fuzz_target!(|data: &[u8]| {
    // Compiler source is UTF-8. Reject invalid bytes without allocating a lossy copy.
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    // Lexical and syntax diagnostics are expected outcomes. Panics, sanitizer
    // failures, hangs, and failures while dropping the AST remain visible.
    if let Ok(tokens) = Lexer::new(source).tokenize() {
        let _ = Parser::new(tokens).parse();
    }
});
