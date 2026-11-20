//! Human-owned native review record, deliberately outside lock regeneration.
use super::*;
use crate::rust_bridge_ir::RustBridgeSymbol;
use std::collections::BTreeMap;

pub const RECORD: &str = "rust-review.toml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Review {
    fingerprint: String,
}

pub fn recorded(root: &Path) -> Result<Option<String>> {
    let path = root.join(RECORD);
    match fs::read_to_string(&path) {
        Ok(source) => {
            let review: Review = toml::from_str(&source).context("invalid rust-review.toml")?;
            anyhow::ensure!(
                review.fingerprint.len() == 64
                    && review
                        .fingerprint
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
                "invalid Rust review fingerprint"
            );
            Ok(Some(review.fingerprint))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn required<'a>(root: &Path, symbols: impl IntoIterator<Item = &'a RustBridgeSymbol>) -> bool {
    root.join(RECORD).exists()
        || symbols
            .into_iter()
            .any(|s| s.native.is_some() && s.nonblocking.is_some())
}

pub fn validate(root: &Path, symbols: &[RustBridgeSymbol], current: &str) -> Result<()> {
    if required(root, symbols) && recorded(root)?.as_deref() != Some(current) {
        bail!(
            "rust_nonblocking_review_required: review all Rust modules and dependencies, then explicitly write fingerprint = \"{current}\" to {RECORD}"
        );
    }
    Ok(())
}

pub(super) fn fingerprint(
    root: &Path,
    symbols: &[RustBridgeSymbol],
    lock: &str,
    input: &str,
    legacy: Option<&str>,
) -> Result<String> {
    // Captured modules avoid duplicate source reads; discover unimported modules
    // as well so check, build and query share one project-wide review boundary.
    let mut sources = BTreeMap::new();
    for native in symbols.iter().filter_map(|s| s.native.as_ref()) {
        if let Some(source) = &native.source {
            sources.insert(
                review_module_path(root, &native.source_path),
                reviewed_source_hash(&native.source_path, source)?,
            );
        }
    }
    let mut pending = vec![root.join("src")];
    while let Some(directory) = pending.pop() {
        if !directory.exists() {
            continue;
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if entry.file_name() != ".willow-edits" {
                    pending.push(path);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let key = review_module_path(root, &path);
                if let std::collections::btree_map::Entry::Vacant(entry) = sources.entry(key) {
                    entry.insert(reviewed_file_hash(&path)?);
                }
            }
        }
    }
    Ok(hash(&serde_json::to_vec(&(
        "native-review-v1",
        lock,
        input,
        legacy,
        sources,
    ))?))
}

/// Review has a closed source boundary: every Rust module must be inline or
/// independently loaded by Willow. Never issue a fingerprint for an unchecked
/// Rust file loader, even if cfg disables it on this host.
pub(super) fn reviewed_file_hash(path: &Path) -> Result<String> {
    reviewed_source_hash(path, &fs::read_to_string(path)?)
}

fn reviewed_source_hash(path: &Path, source: &str) -> Result<String> {
    let tokens = source
        .parse::<proc_macro2::TokenStream>()
        .map_err(|error| {
            anyhow::anyhow!("rust_review_source_layout: {}: {error}", path.display())
        })?;
    if let Some(span) = unsupported_source(tokens) {
        bail!(
            "rust_review_source_layout: {}:{}: external Rust modules, include macros and generated file-loading modules cannot be reviewed; use inline modules or independently imported Willow Rust modules; {RECORD} was not updated",
            path.display(),
            span.start().line
        );
    }
    Ok(hash_source_bytes(source.as_bytes()))
}

fn include_name(name: &proc_macro2::Ident) -> bool {
    name == "include"
        || name == "r#include"
        || name == "include_str"
        || name == "r#include_str"
        || name == "include_bytes"
        || name == "r#include_bytes"
}

struct SourceTokens {
    tokens: proc_macro2::token_stream::IntoIter,
    in_use: bool,
    opaque: bool,
    // mod keyword followed by its name, then an inline brace group.
    module: Option<(proc_macro2::Span, bool)>,
    include: Option<proc_macro2::Span>,
    previous_ident: bool,
    macro_group: bool,
}
impl SourceTokens {
    fn new(tokens: proc_macro2::TokenStream, in_use: bool, opaque: bool) -> Self {
        Self {
            tokens: tokens.into_iter(),
            in_use,
            opaque,
            module: None,
            include: None,
            previous_ident: false,
            macro_group: false,
        }
    }
}

// Inspect every group, including inactive cfg branches and opaque macro bodies.
// Explicit iterator frames avoid both a second recursive syn AST and repeated
// subtree scans. No caller-dependent or host-dependent source layout is trusted.
fn unsupported_source(tokens: proc_macro2::TokenStream) -> Option<proc_macro2::Span> {
    use proc_macro2::{Delimiter, TokenTree};
    let mut frames = vec![SourceTokens::new(tokens, false, false)];
    while let Some(frame) = frames.last_mut() {
        let Some(token) = frame.tokens.next() else {
            if let Some((span, _)) = frame.module {
                return Some(span);
            }
            frames.pop();
            continue;
        };
        #[cfg(test)]
        LAYOUT_VISITS.set(LAYOUT_VISITS.get() + 1);
        if let Some((span, has_name)) = frame.module.take() {
            match (&token, has_name) {
                (TokenTree::Ident(_), false) => frame.module = Some((span, true)),
                (TokenTree::Group(group), true) if group.delimiter() == Delimiter::Brace => {}
                _ => return Some(span),
            }
        }
        let previous_include = frame.include.take();
        let previous_ident = frame.previous_ident;
        frame.previous_ident = false;
        match token {
            TokenTree::Ident(name) => {
                if name == "mod" {
                    frame.module = Some((name.span(), false));
                }
                if name == "use" {
                    frame.in_use = true;
                }
                if include_name(&name) {
                    if frame.in_use || frame.opaque {
                        return Some(name.span());
                    }
                    frame.include = Some(name.span());
                }
                frame.previous_ident = name != "if"
                    && name != "while"
                    && name != "return"
                    && name != "break"
                    && name != "yield"
                    && name != "match"
                    && name != "in"
                    && name != "else";
            }
            TokenTree::Punct(punct) if punct.as_char() == '!' => {
                if previous_include.is_some() && punct.spacing() == proc_macro2::Spacing::Alone {
                    return previous_include;
                }
                frame.macro_group |=
                    previous_ident && punct.spacing() == proc_macro2::Spacing::Alone;
            }
            TokenTree::Punct(punct) if punct.as_char() == ';' => {
                frame.in_use = false;
                frame.macro_group = false;
            }
            TokenTree::Group(group) => {
                let child = SourceTokens::new(
                    group.stream(),
                    frame.in_use,
                    frame.opaque || frame.macro_group,
                );
                frame.macro_group = false;
                frames.push(child);
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
thread_local! {
    static LAYOUT_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust_module::NativeBridge;

    #[test]
    fn native_review_source_layout_rejects_file_loading_in_all_contexts() {
        for source in [
            "mod helper;",
            "#[path = \"../helper.rs\"] mod helper;",
            "#[cfg(windows)] #[path = \"../helper.rs\"] mod helper;",
            "#[cfg_attr(windows, path = \"../helper.rs\")] mod helper;",
            "mod inline { mod child; }",
            "fn f() { #[path = \"../helper.rs\"] mod local; }",
            "include!(\"../helper.rs\");",
            "r#include!(\"../helper.rs\");",
            "use std::r#include as load; load!(\"../helper.rs\");",
            "fn f() { let _ = include_str!(\"../helper.rs\"); }",
            "fn f() { let _ = ::std::include_bytes!(\"../data.bin\"); }",
            "use std::include as load; load!(\"../helper.rs\");",
            "use std::{include_str as load}; fn f() { let _ = load!(\"../helper.rs\"); }",
            "macro_rules! load { ($name:ident) => { mod $name; }; } load!(helper);",
            "macro_rules! load { () => { include!(\"../helper.rs\"); }; } load!();",
            "fn f() { mod local { mod deeper; } }",
        ] {
            let error = reviewed_source_hash(Path::new("native.rs"), source).unwrap_err();
            assert!(
                error.to_string().contains("rust_review_source_layout"),
                "{source}: {error:#}"
            );
        }
        for source in [
            "mod helper { pub fn answer() -> i64 { 42 } }",
            "fn f() { mod local { pub fn value() {} } local::value(); }",
            "fn f() { println!(\"mod helper; include!(file)\"); let _ = vec![1, 2]; }",
            "macro_rules! value { ($x:expr) => { $x + 1 }; } pub fn f() -> i64 { value!(41) }",
            "fn f() { let include = 0; if include != 1 { let include = 2; } if !false { let include = 3; } }",
            "macro_rules! inline { () => { mod child { pub fn f() {} } }; } inline!();",
            "// mod helper;\nconst TEXT: &str = \"include!(file)\";",
        ] {
            assert_eq!(
                reviewed_source_hash(Path::new("native.rs"), source).unwrap(),
                hash_source_bytes(source.as_bytes())
            );
        }
    }

    #[test]
    fn native_review_source_layout_visits_scale_with_syntax() {
        for count in [1, 16, 128, 512] {
            let source: String = (0..count)
                .map(|i| format!("mod helper_{i} {{ pub fn answer() -> i64 {{ 42 }} }}\n"))
                .collect();
            LAYOUT_VISITS.set(0);
            reviewed_source_hash(Path::new("native.rs"), &source).unwrap();
            assert_eq!(LAYOUT_VISITS.get(), 12 * count);
            let source = format!(
                "macro_rules! values {{ () => {{ [{}] }}; }}",
                "0,".repeat(count)
            );
            LAYOUT_VISITS.set(0);
            reviewed_source_hash(Path::new("native.rs"), &source).unwrap();
            assert_eq!(LAYOUT_VISITS.get(), 2 * count + 10);
            println!(
                "layout count={count}: inline tokens={}, macro tokens={}",
                12 * count,
                2 * count + 10
            );
        }
    }

    #[test]
    fn native_review_source_layout_nested_modules_visit_once() {
        for depth in [1, 16, 64] {
            let source = format!(
                "{}fn leaf() {{}}{}",
                "mod nested {".repeat(depth),
                "}".repeat(depth)
            );
            LAYOUT_VISITS.set(0);
            reviewed_source_hash(Path::new("native.rs"), &source).unwrap();
            assert_eq!(LAYOUT_VISITS.get(), 3 * depth + 4);
            println!("layout depth={depth}: inline tokens={}", 3 * depth + 4);
        }
    }

    #[test]
    fn native_review_platform_paths_line_endings_and_order_are_portable() {
        let mut expected = None;
        // Model all four CI environments without executing foreign binaries.
        for (root, slash, newline) in [
            ("/nonexistent-linux/project", "/", "\n"),
            (r"Z:\nonexistent-windows\project", "\\", "\r\n"),
            ("/nonexistent-macos-arm/project", "/", "\n"),
            ("/nonexistent-macos-intel/project", "/", "\n"),
        ] {
            let symbols: Vec<_> = ["z", "a"]
                .iter()
                .map(|name| {
                    let mut s = RustBridgeSymbol::new(
                        (*name).into(),
                        vec![],
                        crate::rust_bridge_ir::Scalar::Void,
                    );
                    s.native = Some(NativeBridge {
                        module: format!("{root}_{name}"),
                        source_path: PathBuf::from(format!("{root}{slash}src{slash}{name}.rs")),
                        source: Some(format!(
                            "#[willow::assume(no_block)]{newline}pub fn f() {{}}{newline}"
                        )),
                        adapter: String::new(),
                        target: String::new(),
                        definitions: vec![],
                    });
                    s
                })
                .collect();
            let value = fingerprint(
                Path::new(root),
                &symbols,
                "same Cargo lock",
                "same inputs",
                None,
            )
            .unwrap();
            assert_eq!(
                value,
                fingerprint(
                    Path::new(root),
                    &symbols.iter().rev().cloned().collect::<Vec<_>>(),
                    "same Cargo lock",
                    "same inputs",
                    None
                )
                .unwrap()
            );
            if let Some(expected) = &expected {
                assert_eq!(&value, expected);
            } else {
                expected = Some(value);
            }
        }
    }
}
