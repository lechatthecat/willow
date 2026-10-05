use super::support::*;

#[test]
fn lexer_diag_integer_overflow_e0052() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = 99999999999999999999;
    println(x);
}
"#,
        &["error[E0052]", "out of range for `i64`"],
    );
}

// End-to-end: an unterminated block comment surfaces as E0053.
#[test]
fn lexer_diag_unterminated_block_comment_e0053() {
    assert_compile_error_contains(
        r#"
fn main() {
    /* this comment never closes
    println(1);
}
"#,
        &["error[E0053]", "unterminated block comment"],
    );
}

// End-to-end: a valid (nested) block comment compiles and runs.
#[test]
fn lexer_diag_block_comment_compiles_and_runs() {
    let (out, ok) = compile_and_run(
        r#"
/* header /* nested */ comment */
fn main() {
    let a = 10; /* inline */ let b = 20;
    println(a + b);
}
"#,
    );
    assert!(ok, "block comments should compile");
    assert_eq!(out, "30\n");
}

// ── Radix prefixes and digit separators (willow-jz15.55) ────────────────────

// End-to-end: prefixed and separated literals reach every literal position —
// expressions, `const` initializers, (negated) match patterns, compound
// assignment and call arguments — with their decimal values.
#[test]
fn lexer_radix_literals_compile_and_run() {
    let (out, ok) = compile_and_run(
        r#"
const MASK: i64 = 0xFF_00;
const BIG: i64 = 1_000_000;

fn classify(x: i64) -> String {
    return match x {
        0x10 => "sixteen",
        -0b1 => "minus one",
        0o777 => "five-eleven",
        _ => "other",
    };
}

fn main() {
    println(0xFF + 0o17 + 0b1010);
    println(0x1234 & MASK);
    println(BIG + 2_500);
    println(-0x7FFF_FFFF_FFFF_FFFF);
    println(0x7FFF_FFFF_FFFF_FFFF);
    println(classify(16));
    println(classify(-1));
    println(classify(511));
    println(classify(0b0));
    let mut flags = 0b0000;
    flags |= 0b0101;
    println(flags);
    println(1_000.25 + 0.5);
    println(!0x0 == -1);
}
"#,
    );
    assert!(ok, "radix literals should compile: {out}");
    assert_eq!(
        out,
        "280\n4608\n1002500\n-9223372036854775807\n9223372036854775807\n\
         sixteen\nminus one\nfive-eleven\nother\n5\n1000.75\ntrue\n"
    );
}

// End-to-end: an invalid digit surfaces as E0054 with the help line.
#[test]
fn lexer_diag_invalid_binary_digit_e0054() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(0b1012);
}
"#,
        &[
            "error[E0054]",
            "invalid digit `2` in binary literal `0b1012`",
            "binary digits are `0` and `1`",
        ],
    );
}

// End-to-end: a misplaced separator surfaces as E0054.
#[test]
fn lexer_diag_misplaced_separator_e0054() {
    assert_compile_error_contains(
        "fn main() { println(1__000); }\n",
        &["error[E0054]", "misplaced `_` in numeric literal `1__000`"],
    );
}

// End-to-end: a hex literal with the sign bit set is E0052, not `-1`.
#[test]
fn lexer_diag_hex_sign_bit_e0052() {
    assert_compile_error_contains(
        "fn main() { println(0xFFFF_FFFF_FFFF_FFFF); }\n",
        &[
            "error[E0052]",
            "integer literal `0xFFFF_FFFF_FFFF_FFFF` out of range for `i64`",
            "`!0`",
        ],
    );
}
