//! Closed reviewed Rust inputs. Both policies share standard expansion and
//! shadow protection. Dependency trees additionally support ordinary modules
//! and literal data includes; project roots permit inline modules and the
//! injected assertion marker, with no additional file-loading expansion.
use anyhow::{Context, Result, bail, ensure};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use std::path::{Component, Path, PathBuf};

fn normalized(path: &Path) -> Option<PathBuf> {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                output.file_name()?;
                output.pop();
            }
            _ => output.push(component.as_os_str()),
        }
    }
    Some(output)
}

/// Lexical containment only. The inventory separately rejects symlink entries.
pub(super) fn contained(root: &Path, path: &Path) -> bool {
    let Some(root) = normalized(root) else {
        return false;
    };
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    };
    let Ok(relative) = path.strip_prefix(&root) else {
        return false;
    };
    if relative.components().any(|component| {
        matches!(component, Component::Normal(name) if name.to_str().is_none_or(super::excluded_dependency_name))
    }) {
        return false;
    }
    normalized(&path).is_some_and(|path| path.starts_with(root))
}

fn standard_macro(name: &str) -> bool {
    matches!(
        name,
        "vec"
            | "format"
            | "format_args"
            | "assert"
            | "assert_eq"
            | "assert_ne"
            | "debug_assert"
            | "debug_assert_eq"
            | "debug_assert_ne"
            | "panic"
            | "todo"
            | "unimplemented"
            | "unreachable"
            | "print"
            | "println"
            | "eprint"
            | "eprintln"
            | "write"
            | "writeln"
            | "matches"
            | "dbg"
            | "stringify"
            | "concat"
            | "concat_bytes"
            | "cfg"
    )
}

fn standard_derive(name: &str) -> bool {
    matches!(
        name,
        "Clone" | "Copy" | "Debug" | "Default" | "Eq" | "PartialEq" | "Ord" | "PartialOrd" | "Hash"
    )
}

fn derives(tokens: TokenStream) -> bool {
    let mut expect_name = true;
    let mut found = false;
    for token in tokens {
        match token {
            TokenTree::Ident(name) if expect_name && standard_derive(&name.to_string()) => {
                expect_name = false;
                found = true;
            }
            TokenTree::Punct(punct) if !expect_name && punct.as_char() == ',' => expect_name = true,
            _ => return false,
        }
    }
    found
}

fn attribute(tokens: TokenStream) -> bool {
    let mut tokens = tokens.into_iter();
    let Some(TokenTree::Ident(name)) = tokens.next() else {
        return false;
    };
    if name == "derive" {
        return matches!(tokens.next(), Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis && derives(group.stream()))
            && tokens.next().is_none();
    }
    if !matches!(
        name.to_string().as_str(),
        "cfg"
            | "doc"
            | "allow"
            | "warn"
            | "deny"
            | "forbid"
            | "repr"
            | "inline"
            | "cold"
            | "must_use"
            | "non_exhaustive"
            | "deprecated"
            | "test"
            | "ignore"
            | "should_panic"
            | "no_mangle"
            | "export_name"
            | "link_name"
            | "link_section"
            | "used"
            | "unsafe"
    ) {
        return false;
    }
    let next = tokens.next();
    if name == "unsafe" {
        let Some(TokenTree::Group(group)) = next else {
            return false;
        };
        let mut nested = group.stream().into_iter();
        return matches!(nested.next(), Some(TokenTree::Ident(name)) if matches!(name.to_string().as_str(), "no_mangle" | "export_name" | "link_section"));
    }
    matches!(next, None | Some(TokenTree::Group(_)))
        || matches!(next, Some(TokenTree::Punct(punct)) if punct.as_char() == '=')
}

/// Validate and count this bounded marker without scanning its tokens again.
fn assumption_attribute(tokens: TokenStream) -> Option<usize> {
    let mut tokens = tokens.into_iter();
    if !matches!(tokens.next(), Some(TokenTree::Ident(name)) if name == "willow")
        || !matches!(tokens.next(), Some(TokenTree::Punct(punct)) if punct.as_char() == ':')
        || !matches!(tokens.next(), Some(TokenTree::Punct(punct)) if punct.as_char() == ':')
        || !matches!(tokens.next(), Some(TokenTree::Ident(name)) if name == "assume")
    {
        return None;
    }
    let Some(TokenTree::Group(group)) = tokens.next() else {
        return None;
    };
    if group.delimiter() != Delimiter::Parenthesis || tokens.next().is_some() {
        return None;
    }
    let mut count = 5;
    let mut seen = 0;
    let mut expect_name = true;
    for token in group.stream() {
        count += 1;
        match token {
            TokenTree::Ident(name) if expect_name => {
                let bit = match name.to_string().as_str() {
                    "no_block" => 1,
                    "no_io" => 2,
                    _ => return None,
                };
                if seen & bit != 0 {
                    return None;
                }
                seen |= bit;
                expect_name = false;
            }
            TokenTree::Punct(punct) if !expect_name && punct.as_char() == ',' => expect_name = true,
            _ => return None,
        }
    }
    (seen != 0).then_some(count)
}

struct Frame {
    tokens: proc_macro2::token_stream::IntoIter,
    in_use: bool,
    attribute: bool,
    identifier: Option<String>,
    qualified: bool,
    colons: usize,
    data_include: bool,
    module_name: bool,
    inline_module: bool,
    super_glob: Option<u8>,
}
impl Frame {
    fn new(tokens: TokenStream, in_use: bool) -> Self {
        Self {
            tokens: tokens.into_iter(),
            in_use,
            attribute: false,
            identifier: None,
            qualified: false,
            colons: 0,
            data_include: false,
            module_name: false,
            inline_module: false,
            super_glob: None,
        }
    }
}

#[derive(Default)]
struct DataInputs {
    canonical_root: Option<PathBuf>,
    checked: std::collections::HashSet<String>,
}

fn data_input(
    root: &Path,
    source: &Path,
    tokens: TokenStream,
    inputs: &mut DataInputs,
) -> Result<()> {
    let mut tokens = tokens.into_iter();
    let Some(TokenTree::Literal(literal)) = tokens.next() else {
        bail!("data include requires a direct string literal");
    };
    let literal: syn::LitStr = syn::parse_str(&literal.to_string())
        .context("data include requires a direct string literal")?;
    if let Some(token) = tokens.next() {
        ensure!(
            matches!(token, TokenTree::Punct(punct) if punct.as_char() == ',')
                && tokens.next().is_none(),
            "data include requires one literal argument"
        );
    }
    let value = literal.value();
    if inputs.checked.contains(&value) {
        return Ok(());
    }
    let path = Path::new(&value);
    ensure!(
        !value.contains('\\')
            && !path.is_absolute()
            && !value.contains('\0')
            && !value.contains(':'),
        "data include path must be relative and portable"
    );
    let path = source
        .parent()
        .context("dependency source parent missing")?
        .join(path);
    ensure!(
        contained(root, &path),
        "data include escapes reviewed dependency inputs"
    );
    if inputs.canonical_root.is_none() {
        inputs.canonical_root =
            Some(std::fs::canonicalize(root).context("dependency root is unavailable")?);
    }
    #[cfg(test)]
    INPUT_VALIDATIONS.set(INPUT_VALIDATIONS.get() + 1);
    let canonical_path = std::fs::canonicalize(&path).context("data include is unavailable")?;
    ensure!(
        contained(inputs.canonical_root.as_ref().unwrap(), &canonical_path),
        "data include alias escapes reviewed dependency inputs"
    );
    ensure!(
        std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()),
        "data include is not a regular reviewed file"
    );
    inputs.checked.insert(value);
    Ok(())
}

fn inspect(root: Option<&Path>, path: &Path, tokens: TokenStream) -> Result<usize> {
    let mut visits = 0;
    let mut inputs = DataInputs::default();
    let mut frames = vec![Frame::new(tokens, false)];
    while let Some(frame) = frames.last_mut() {
        let Some(token) = frame.tokens.next() else {
            ensure!(!frame.data_include, "data include argument missing");
            ensure!(
                root.is_some() || !(frame.module_name || frame.inline_module),
                "root modules must be inline"
            );
            frames.pop();
            continue;
        };
        visits += 1;
        if frame.inline_module {
            ensure!(
                matches!(&token, TokenTree::Group(group) if group.delimiter() == Delimiter::Brace),
                "root modules must be inline"
            );
            frame.inline_module = false;
        }
        if frame.data_include {
            let TokenTree::Group(group) = token else {
                bail!("data include argument missing");
            };
            data_input(
                root.context("root data includes are unsupported")?,
                path,
                group.stream(),
                &mut inputs,
            )?;
            frame.data_include = false;
            frame.identifier = None;
            continue;
        }
        match token {
            TokenTree::Ident(name) => {
                let name = name.to_string();
                let canonical = name.strip_prefix("r#").unwrap_or(&name);
                ensure!(
                    root.is_some() || canonical != "willow",
                    "the injected assertion namespace cannot be shadowed"
                );
                ensure!(
                    root.is_some() || !frame.in_use || canonical != "assume",
                    "assertion macro imports cannot be reviewed"
                );
                if frame.module_name {
                    ensure!(
                        !super::excluded_dependency_name(canonical),
                        "module loads an excluded dependency directory"
                    );
                    frame.inline_module = root.is_none();
                }
                frame.module_name = name == "mod";
                ensure!(canonical != "macro", "macro definitions cannot be reviewed");
                if matches!(
                    canonical,
                    "include" | "include_str" | "include_bytes" | "env" | "option_env"
                ) {
                    ensure!(
                        !frame.in_use && !name.starts_with("r#"),
                        "imported or raw input macros cannot be reviewed"
                    );
                }
                ensure!(
                    !frame.in_use || !(standard_macro(canonical) || standard_derive(canonical)),
                    "imports can shadow reviewed standard macros or derives"
                );
                frame.qualified = frame.colons == 2;
                frame.colons = 0;
                if name == "use" {
                    frame.in_use = true;
                    frame.super_glob = Some(0);
                } else if frame.in_use {
                    frame.super_glob =
                        (frame.super_glob == Some(0) && name == "super").then_some(1);
                }
                frame.identifier = if matches!(
                    name.as_str(),
                    "if" | "while" | "return" | "break" | "yield" | "match" | "in" | "else"
                ) {
                    None
                } else {
                    Some(name)
                };
            }
            TokenTree::Punct(punct) if punct.as_char() == ':' => {
                frame.colons += 1;
                frame.super_glob = match frame.super_glob {
                    Some(1) => Some(2),
                    Some(2) => Some(3),
                    _ => None,
                };
            }
            TokenTree::Punct(punct) if punct.as_char() == '#' => {
                frame.attribute = true;
                frame.identifier = None;
                frame.colons = 0;
            }
            TokenTree::Punct(punct)
                if punct.as_char() == '!' && punct.spacing() == proc_macro2::Spacing::Alone =>
            {
                if let Some(name) = frame.identifier.take() {
                    ensure!(
                        !frame.qualified
                            && ((root.is_some()
                                && matches!(name.as_str(), "include_str" | "include_bytes"))
                                || standard_macro(&name)),
                        "only unqualified reviewed standard macros and literal data includes are supported"
                    );
                    frame.data_include = matches!(name.as_str(), "include_str" | "include_bytes");
                }
                frame.colons = 0;
            }
            TokenTree::Punct(punct) if punct.as_char() == '*' && frame.in_use => {
                ensure!(
                    frame.super_glob == Some(3),
                    "glob imports can shadow input macros"
                );
                frame.super_glob = None;
            }
            TokenTree::Punct(punct) if punct.as_char() == ';' => {
                frame.in_use = false;
                frame.identifier = None;
                frame.colons = 0;
                frame.super_glob = None;
            }
            TokenTree::Group(group) => {
                let marker = (frame.attribute && root.is_none())
                    .then(|| assumption_attribute(group.stream()))
                    .flatten();
                if frame.attribute {
                    ensure!(
                        group.delimiter() == Delimiter::Bracket
                            && (marker.is_some() || attribute(group.stream())),
                        "transforming or unsupported attributes cannot be reviewed"
                    );
                    frame.attribute = false;
                }
                if let Some(count) = marker {
                    visits += count;
                    frame.identifier = None;
                    frame.colons = 0;
                    frame.super_glob = None;
                    continue;
                }
                let child = Frame::new(group.stream(), frame.in_use);
                frame.super_glob = None;
                frame.identifier = None;
                frame.colons = 0;
                frames.push(child);
            }
            _ => {
                frame.identifier = None;
                frame.colons = 0;
                frame.super_glob = None;
            }
        }
    }
    Ok(visits)
}

fn source_tokens(path: &Path, source: &str) -> Result<TokenStream> {
    source.parse::<TokenStream>().map_err(|error| {
        anyhow::anyhow!(
            "rust_review_source_layout: {}: invalid reviewed Rust source: {error}",
            path.display()
        )
    })
}

pub(super) fn validate(root: &Path, path: &Path, source: &str) -> Result<()> {
    inspect(Some(root), path, source_tokens(path, source)?)
        .with_context(|| format!("rust_review_source_layout: {}", path.display()))?;
    Ok(())
}

/// Root review performs one token parse and one shared policy scan, with no IO.
/// The visit count is useful for deterministic scaling checks.
pub(super) fn validate_root(path: &Path, source: &str) -> Result<usize> {
    inspect(None, path, source_tokens(path, source)?)
        .with_context(|| format!("rust_review_source_layout: {}", path.display()))
}

#[cfg(test)]
thread_local! {
    static INPUT_VALIDATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "willow-reviewed-inputs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::create_dir_all(root.join("assets")).unwrap();
            std::fs::write(root.join("src/data.txt"), "reviewed").unwrap();
            std::fs::write(root.join("assets/data.bin"), "reviewed").unwrap();
            Self(root)
        }
        fn check(&self, source: &str) -> Result<()> {
            validate(&self.0, &self.0.join("src/lib.rs"), source)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn dependency_input_layout_allows_closed_modules_and_literal_data() {
        let fixture = Fixture::new();
        for source in [
            "mod child; pub mod nested { mod child; }",
            "#[cfg(windows)] mod windows; #[cfg(unix)] mod unix;",
            "const DATA: &str = include_str!(\"data.txt\");",
            "const DATA: &[u8] = include_bytes!(\"../assets/data.bin\",);",
            "#![allow(dead_code)] #[inline] pub fn f() {}",
            "#[unsafe(no_mangle)] pub fn f() {}",
            "#[doc = include_str!(\"data.txt\")] pub fn f() {}",
            "use std::{collections::BTreeMap, path::Path};",
            "pub fn negate(value: bool) -> bool { if !value { return !value; } !value }",
            "#[derive(Default, Clone, Debug, PartialEq, Eq)] struct Value;",
            "pub fn f() { let values = vec![1, 2]; let text = format!(\"{}\", values.len()); assert_eq!(text, \"2\"); assert!(true); println!(\"{}\", text); }",
            "#[cfg(test)] mod tests { use super::*; #[test] fn f() { assert!(true); } }",
            "format!(\"{}\", include_str!(\"data.txt\"));",
        ] {
            fixture
                .check(source)
                .unwrap_or_else(|error| panic!("{source}: {error:#}"));
        }
    }
    #[test]
    fn dependency_input_layout_rejects_escape_and_expansion_inputs() {
        let fixture = Fixture::new();
        for path in [
            "../outside",
            "requirements/input",
            "Requirements/input",
            "target/input",
            "TARGET/input",
            ".git/config",
            ".Git/config",
            ".willow/input",
            ".Willow/input",
            ".willow-edits/input",
            ".Willow-Edits/input",
            "src/../../outside",
        ] {
            assert!(
                !contained(&fixture.0, &fixture.0.join(path)),
                "contained {path}"
            );
        }
        assert!(contained(
            &fixture.0,
            &fixture.0.join("src/../assets/data.bin")
        ));
        for source in [
            "include!(\"child.rs\");",
            "include_str!(\"../../outside\");",
            "include_bytes!(\"/absolute\");",
            "include_str!(\"..\\\\outside\");",
            "include_str!(\"../requirements/private\");",
            "include_str!(\"../target/generated\");",
            "include_str!(\"../.git/config\");",
            "include_str!(\"../.willow/generated\");",
            "include_str!(\"../.willow-edits/input\");",
            "include_str!(\"missing\");",
            "include_str!(concat!(\"data\", \".txt\"));",
            "include_str!(name);",
            "include_str!(b\"data.txt\");",
            "include_str!(\"data.txt\", \"other\");",
            "use std::include_str as read;",
            "use std::{include_bytes};",
            "use std::*;",
            "use external as include_str;",
            "r#include_str!(\"data.txt\");",
            "external::include_str!(\"data.txt\");",
            "std::include_str!(\"data.txt\");",
            "::include_str!(\"data.txt\");",
            "core::include_bytes!(\"data.txt\");",
            "macro_rules! load { () => { mod child; } }",
            "external! { mod child; }",
            "external!();",
            "format!(\"{}\", include!(\"outside.rs\"));",
            "vec![include_str!(\"../../outside\")];",
            "assert!(external!());",
            "format!(\"{}\", env!(\"INPUT\"));",
            "use external::format; format!(\"input\");",
            "use external::load as println; println!(\"input\");",
            "use external::{assert_eq};",
            "use external::r#format;",
            "use external::Default; #[derive(Default)] struct Value;",
            "use external::Hidden as Clone; #[derive(Clone)] struct Value;",
            "use super::nested::*;",
            "use super::{*};",
            "use crate::*;",
            "#[derive(Default, External)] struct Value;",
            "#[derive(std::clone::Clone)] struct Value;",
            "std::format!(\"input\");",
            "env!(\"INPUT\");",
            "option_env!(\"INPUT\");",
            "#[path=\"../../outside.rs\"] mod child;",
            "#[cfg_attr(windows,path=\"outside.rs\")] mod child;",
            "#[macro_use] extern crate external;",
            "#[derive(External)] struct Value;",
            "#[external::attribute] fn f() {}",
            "#[external] fn f() {}",
            "#[allow::external] fn f() {}",
            "#[unsafe(external)] fn f() {}",
            "#[link(name = \"/outside/libpayload.a\", kind = \"static\", modifiers = \"+verbatim\")] unsafe extern \"C\" {}",
            "#[link(name = \"../payload\", kind = \"static\")] unsafe extern \"C\" {}",
            "#[link(name = \"payload\", kind = \"static\")] unsafe extern \"C\" {}",
            "#[cfg(any())] mod hidden { external!(); }",
            "mod target;",
            "mod requirements;",
            "mod r#target;",
            "mod r#requirements;",
            "pub mod target {}",
            "mod Target;",
            "mod r#Requirements;",
            "include_str!(\"../Requirements/private\");",
            "include_str!(\"../TARGET/generated\");",
            "include_str!(\"../.Git/config\");",
            "include_str!(\"../.Willow/generated\");",
            "include_str!(\"../.Willow-Edits/input\");",
        ] {
            assert!(fixture.check(source).is_err(), "accepted {source}");
        }
    }
    #[test]
    fn dependency_input_layout_visits_scale_linearly() {
        let fixture = Fixture::new();
        let unit = "mod child; const DATA: &str = include_str!(\"data.txt\");";
        let one = inspect(
            Some(&fixture.0),
            &fixture.0.join("src/lib.rs"),
            unit.parse().unwrap(),
        )
        .unwrap();
        for count in [8, 32, 128, 512] {
            let tokens = unit.repeat(count).parse().unwrap();
            INPUT_VALIDATIONS.set(0);
            let visits = inspect(Some(&fixture.0), &fixture.0.join("src/lib.rs"), tokens).unwrap();
            assert_eq!(visits, one * count);
            assert_eq!(INPUT_VALIDATIONS.get(), 1);
        }
        for depth in [8, 32, 96] {
            let source = format!("{}{}{}", "{".repeat(depth), unit, "}".repeat(depth));
            let visits = inspect(
                Some(&fixture.0),
                &fixture.0.join("src/lib.rs"),
                source.parse().unwrap(),
            )
            .unwrap();
            assert_eq!(visits, one + depth);
        }
    }

    #[test]
    #[cfg(unix)]
    fn dependency_input_layout_rejects_symlink_parent_escape() {
        let fixture = Fixture::new();
        let source_root = fixture.0.join("src");
        std::os::unix::fs::symlink(fixture.0.join("assets"), source_root.join("alias")).unwrap();
        assert!(
            validate(
                &source_root,
                &source_root.join("lib.rs"),
                "include_bytes!(\"alias/data.bin\");"
            )
            .is_err()
        );
        std::fs::remove_file(source_root.join("alias")).unwrap();
        std::fs::create_dir(fixture.0.join("target")).unwrap();
        std::fs::write(fixture.0.join("target/data.bin"), "not reviewed").unwrap();
        std::os::unix::fs::symlink(fixture.0.join("target"), source_root.join("alias")).unwrap();
        assert!(
            fixture
                .check("include_bytes!(\"alias/data.bin\");")
                .is_err()
        );
    }

    #[test]
    fn root_input_layout_supports_injected_markers_and_closed_standard_expansions() {
        let path = Path::new("nonexistent-reviewed-root/native.rs");
        for source in [
            "#[willow::assume(no_block)] pub fn answer() -> i64 { 42 }",
            "#[willow::assume(no_io, no_block,)] pub struct Handle; impl Handle { #[willow::assume(no_io)] pub fn read(&self) -> i64 { 42 } }",
            "#[derive(Default)] struct Handle; mod child { #[cfg(test)] mod tests { use super::*; #[test] fn check() { assert_eq!(1, 1); } } }",
            "pub fn f() { let values = vec![1, 2]; println!(\"{}\", format!(\"{}\", values.len())); }",
            "fn include() {} fn env() {} fn asm() {} fn global_asm() {} fn main() { include(); env(); asm(); global_asm(); }",
        ] {
            assert!(validate_root(path, source).is_ok(), "rejected {source}");
        }
    }

    #[test]
    fn root_input_layout_rejects_assembly_loaders_and_marker_shadowing() {
        let path = Path::new("nonexistent-reviewed-root/native.rs");
        for source in [
            "include!(\"input.rs\");",
            "include_str!(\"input.txt\");",
            "include_bytes!(\"input.bin\");",
            "env!(\"INPUT\");",
            "option_env!(\"INPUT\");",
            "asm!(\".incbin other\");",
            "global_asm!(\".include other\");",
            "naked_asm!(\".incbin other\");",
            "llvm_asm!(\".include other\");",
            "core::arch::global_asm!(\".incbin other\");",
            "r#global_asm!(\".incbin other\");",
            "use core::arch::asm as machine; fn f() { machine!(\".include other\"); }",
            "#[cfg(any())] mod hidden { global_asm!(\".incbin other\"); }",
            "stringify!(asm!(\".incbin other\"));",
            "macro_rules! emit { () => { global_asm!(\".include other\"); } }",
            "external!();",
            "external! { mod child; }",
            "format!(\"{}\", include_str!(\"input.txt\"));",
            "mod child;",
            "#[cfg(any())] mod child;",
            "mod inline { mod child; }",
            "#[path=\"outside.rs\"] mod child;",
            "#[cfg_attr(any(), path=\"outside.rs\")] mod child;",
            "#[link(name=\"outside\", kind=\"static\")] unsafe extern \"C\" {}",
            "#[willow::assume()] pub fn f() {}",
            "#[willow::assume(no_alloc)] pub fn f() {}",
            "#[willow::assume(no_io,no_io)] pub fn f() {}",
            "#[willow::assume(no_io no_block)] pub fn f() {}",
            "#[willow::other(no_io)] pub fn f() {}",
            "#[r#willow::assume(no_io)] pub fn f() {}",
            "#[assume(no_io)] pub fn f() {}",
            "use external as willow; #[willow::assume(no_io)] fn f() {}",
            "extern crate external as willow; #[willow::assume(no_io)] fn f() {}",
            "mod willow { pub use external::assume; }",
            "mod r#willow {}",
            "use external::assume;",
            "use external::r#assume;",
            "use external as r#willow;",
            "use external::format; format!(\"input\");",
            "use external::Default; #[derive(Default)] struct Handle;",
        ] {
            let error = validate_root(path, source).expect_err(source);
            assert!(error.to_string().contains("rust_review_source_layout"));
        }
    }

    #[test]
    fn root_input_layout_visits_scale_linearly_without_filesystem_inputs() {
        let path = Path::new("nonexistent-reviewed-root/native.rs");
        let unit = "#[willow::assume(no_io)] fn answer() -> i64 { 42 }";
        let one = validate_root(path, unit).unwrap();
        for count in [8, 32, 128, 512] {
            INPUT_VALIDATIONS.set(0);
            assert_eq!(
                validate_root(path, &unit.repeat(count)).unwrap(),
                one * count
            );
            assert_eq!(INPUT_VALIDATIONS.get(), 0);
        }
        for depth in [8, 32, 96] {
            let source = format!(
                "{}{}{}",
                "mod child {".repeat(depth),
                unit,
                "}".repeat(depth)
            );
            assert_eq!(validate_root(path, &source).unwrap(), one + depth * 3);
        }
    }
}
