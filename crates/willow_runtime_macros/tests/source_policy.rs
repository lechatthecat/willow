//! Parse all runtime source, including inactive platform branches. Keep raw
//! unwrap/expect inside a generated catch boundary, never in unguarded C code.
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("test")
            || (a.path().is_ident("cfg")
                && a.parse_args::<syn::Path>()
                    .is_ok_and(|p| p.is_ident("test")))
    })
}

#[derive(Default)]
struct Scan {
    path: PathBuf,
    failures: Vec<String>,
    guarded: usize,
    handlers: usize,
    raw_unwraps: usize,
}
impl<'ast> Visit<'ast> for Scan {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !test_only(&item.attrs) {
            visit::visit_item_mod(self, item);
        }
    }
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        if test_only(&function.attrs) {
            return;
        }
        if let Some(abi) = &function.sig.abi {
            let name = function.sig.ident.to_string();
            let abi = abi
                .name
                .as_ref()
                .map(syn::LitStr::value)
                .unwrap_or_default();
            if abi == "C" || abi == "system" {
                let guarded = function.attrs.iter().any(|a| {
                    a.path()
                        .segments
                        .iter()
                        .map(|s| s.ident.to_string())
                        .collect::<Vec<_>>()
                        == ["willow_runtime_macros", "ffi_boundary"]
                });
                let handler = self.path.ends_with("stack_overflow.rs")
                    && ((abi == "C" && name == "signal_handler")
                        || (abi == "system" && name == "exception_handler"));
                let mut risky = Risky::default();
                risky.visit_block(&function.block);
                self.raw_unwraps += risky.unwraps;
                if guarded {
                    self.guarded += 1;
                } else if handler && risky.unwraps == 0 && risky.panic_macros == 0 {
                    self.handlers += 1;
                } else {
                    self.failures.push(format!(
                        "{}::{name}: unguarded {abi} function ({} unwrap/expect, {} panic macros)",
                        self.path.display(),
                        risky.unwraps,
                        risky.panic_macros
                    ));
                }
            }
        }
        // Also find C callbacks nested inside ordinary Rust functions.
        visit::visit_item_fn(self, function);
    }
    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        if test_only(&item.attrs) {
            return;
        }
        // Macro templates cannot be parsed as ItemFn until metavariables are
        // expanded. Check each C declaration's immediately preceding token
        // prefix; a preceding function body cannot supply its guard.
        let tokens = quote::quote!(#item).to_string();
        let mut previous = 0;
        for (offset, _) in tokens.match_indices("extern \"C\" fn") {
            let prefix = &tokens[previous..offset];
            let prefix = prefix.rsplit('}').next().unwrap();
            if !prefix.contains("[willow_runtime_macros :: ffi_boundary]") {
                self.failures.push(format!(
                    "{}: unguarded macro C function",
                    self.path.display()
                ));
            }
            previous = offset + "extern \"C\" fn".len();
        }
    }
}
#[derive(Default)]
struct Risky {
    unwraps: usize,
    panic_macros: usize,
}
impl<'ast> Visit<'ast> for Risky {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "unwrap" || call.method == "expect" {
            self.unwraps += 1;
        }
        visit::visit_expr_method_call(self, call);
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac.path.segments.last().is_some_and(|s| {
            matches!(
                s.ident.to_string().as_str(),
                "panic"
                    | "assert"
                    | "assert_eq"
                    | "assert_ne"
                    | "unreachable"
                    | "todo"
                    | "unimplemented"
            )
        }) {
            self.panic_macros += 1;
        }
        visit::visit_macro(self, mac);
    }
}
fn walk(path: &Path, scan: &mut Scan) {
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, scan);
        } else if path.extension().is_some_and(|e| e == "rs")
            && !path
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .ends_with("tests")
        {
            let source = std::fs::read_to_string(&path).unwrap();
            let file = syn::parse_file(&source).unwrap();
            scan.path = path;
            scan.visit_file(&file);
        }
    }
}
#[test]
fn runtime_c_boundaries_are_guarded_including_unwrap_and_expect() {
    let mut scan = Scan::default();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../willow_runtime/src"),
        &mut scan,
    );
    assert!(scan.failures.is_empty(), "{}", scan.failures.join("\n"));
    assert!(
        scan.guarded > 300,
        "source scan must not silently skip runtime files"
    );
    assert_eq!(
        scan.handlers, 2,
        "only the two audited native handlers are exempt"
    );
    println!(
        "guarded_functions={} native_handlers={} contained_unwrap_expect={}",
        scan.guarded, scan.handlers, scan.raw_unwraps
    );
}
#[test]
fn scanner_rejects_unguarded_functions_and_skips_test_modules() {
    let source = r#"
extern "C" fn bad() { Some(1).unwrap(); }
unsafe extern "C" fn bad_expect() { Some(1).expect("x"); }
extern "system" fn also_bad() { panic!("x"); }
fn rust() { extern "C" fn nested() {} }
#[cfg(test)] mod tests { extern "C" fn ignored() { None::<u8>.unwrap(); } }
#[test] fn ignored_test() { extern "C" fn ignored() {} }
#[willow_runtime_macros::ffi_boundary] extern "C" fn good() { Some(1).unwrap(); }
macro_rules! bad_macro { () => { extern "C" fn $name() {} }; }
"#;
    let mut scan = Scan::default();
    scan.visit_file(&syn::parse_file(source).unwrap());
    assert_eq!(scan.failures.len(), 5, "{:?}", scan.failures);
    assert_eq!(scan.guarded, 1);
    assert_eq!(scan.raw_unwraps, 3);
}
