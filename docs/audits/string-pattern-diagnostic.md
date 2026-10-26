# String literal pattern diagnostic audit

Date: 2026-10-26

## Scope and result

The parser now reports E0102 with "string literal patterns are not supported"
and help "use if/else with string equality (==) instead". String pattern
matching remains unsupported. This is a diagnostic-only change; no language
semantics, runtime ABI, or backend lowering was added.

Minimal reproducer:

```willow
fn main() {
    let s = "a";
    match s { "a" => println(1), _ => println(0), }
}
```

Before the change, the new regression test failed because the message was
"expected pattern". After the change, it passes. The executable alternative
is in `example/string_pattern_if_else.wi`; its output is 1, 2, 0 on separate
lines for first-branch, second-branch, and fallback inputs.

## Test perspectives and reproduction

The parser matrix explicitly covers these 20 combinations:

| Literal | First match arm | Later arm after guard | if let | while let |
| --- | --- | --- | --- | --- |
| Empty | checked | checked | checked | checked |
| ASCII | checked | checked | checked | checked |
| Spaces | checked | checked | checked | checked |
| Unicode | checked | checked | checked | checked |
| Escape sequence | checked | checked | checked | checked |

Each checks one diagnostic, E0102, the specific message, actionable help,
the complete literal byte span, and recovery preserving a following async
function. Existing nested-constructor and literal-payload diagnostics are
also exercised. The CLI test checks rendered code/message/help and runs
the workaround in debug and release. The existing match-guard test covers
supported pattern selection and fallthrough in debug and release.

Commands run successfully:

```sh
rtk cargo fmt --all -- --check
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk proxy cargo test --lib parser::pattern::tests -- --nocapture
rtk cargo test --test integration string_pattern_cli_diagnostic_and_if_else_workaround -- --nocapture
rtk cargo test --test integration match_guards_select_and_fall_through -- --nocapture
```

Results: five parser tests and two focused integration tests passed.
The standalone new diagnostic filter also passed both new parser tests.

## Implementation and complexity audit

Reviewed `parse_pattern`, `Parser::err`, match-arm parsing,
`parse_let_head`, tuple-binding entry, statement recovery, and the parser
continuation macro's call rewriting. The new branch retains the current
token and returns through the existing error/recovery path. No recursive
parser call or new continuation is introduced. Tuple bindings and variant
payloads retain their existing dedicated paths and diagnostics.

Independent dimensions are source/token count N, preceding arm count A,
and literal byte length L. The new diagnostic construction has constant
message/help size and constant work, with no scans or caller multiplication.
The existing token-kind clone at dispatch may copy L bytes; this was already
present on the generic-error path and is unchanged. Lexing must read the
literal's L bytes. The whole parser's existing AST/token storage is unchanged;
the diagnostic adds only constant storage. No new input-dependent generated
code, runtime work, allocations in generated programs, or backend dispatch
paths are introduced. These are source-inspection conclusions, not native
code-size measurements.

The deterministic scaling test generates A copies of `0 => 0,` before one
invalid string arm, resets PARSER_TOKEN_READS after lexing, and checks a
linear upper bound. The checked-in test is the reproducible generator:

`src/parser/pattern.rs::tests::string_pattern_diagnostic_linear_token_reads`

| A | Measured token reads |
| ---: | ---: |
| 16 | 387 |
| 64 | 1443 |
| 256 | 5667 |

For these inputs the observed count is exactly 22A + 35. Together with the
single forward arm loop and shared error return, this supports O(A) parsing
for this shape, with no rescan of preceding arms. Reading preceding arms
requires Omega(A) work. This is not a claim about all existing parser shapes.
The change adds no avoidable input-dependent work.

## Limits

Validation ran on x86_64 Linux. macOS (arm64 and Intel) and Windows native
execution were not performed. No platform-specific, filesystem, thread,
runtime, or ABI implementation changed, so additional cross-target Clippy
was not applicable. Full suites and the full example audit were intentionally
omitted under local test policy. CI was not inspected.
