//! Mechanical panic containment for runtime-owned C entry points.
use proc_macro::TokenStream;
use quote::quote;

/// Keep the public ABI and place the entire implementation inside a Rust
/// closure, so unwinding is caught *before* reaching the C boundary.
#[proc_macro_attribute]
pub fn ffi_boundary(args: TokenStream, input: TokenStream) -> TokenStream {
    if !args.is_empty() {
        return syn::Error::new(
            proc_macro::Span::call_site().into(),
            "no arguments expected",
        )
        .to_compile_error()
        .into();
    }
    let function = syn::parse_macro_input!(input as syn::ItemFn);
    match expand(function) {
        Ok(function) => quote!(#function).into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand(mut function: syn::ItemFn) -> syn::Result<syn::ItemFn> {
    let is_c = function
        .sig
        .abi
        .as_ref()
        .is_some_and(|abi| abi.name.as_ref().is_some_and(|name| name.value() == "C"));
    if !is_c {
        return Err(syn::Error::new_spanned(
            &function.sig,
            "ffi_boundary requires extern C",
        ));
    }
    let name = function.sig.ident.to_string();
    let body = function.block;
    function.block = syn::parse_quote!({ crate::failure::ffi_boundary(#name, || #body) });
    Ok(function)
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::visit::{self, Visit};

    #[derive(Default)]
    struct Counts {
        closures: usize,
        boundaries: usize,
        returns: usize,
    }
    impl<'ast> Visit<'ast> for Counts {
        fn visit_expr_closure(&mut self, expression: &'ast syn::ExprClosure) {
            self.closures += 1;
            visit::visit_expr_closure(self, expression);
        }
        fn visit_expr_call(&mut self, expression: &'ast syn::ExprCall) {
            if let syn::Expr::Path(path) = &*expression.func
                && path
                    .path
                    .segments
                    .last()
                    .is_some_and(|s| s.ident == "ffi_boundary")
            {
                self.boundaries += 1;
            }
            visit::visit_expr_call(self, expression);
        }
        fn visit_expr_return(&mut self, expression: &'ast syn::ExprReturn) {
            self.returns += 1;
            visit::visit_expr_return(self, expression);
        }
    }

    #[test]
    fn expansion_keeps_one_body_and_one_boundary_across_many_exits() {
        for exits in [1, 16, 64, 256, 1024] {
            let branches = (0..exits)
                .map(|i| format!("if n == {i} {{ return {i}; }}"))
                .collect::<String>();
            let input: syn::ItemFn = syn::parse_str(&format!(
                "#[unsafe(no_mangle)] pub unsafe extern \"C\" fn probe(n: usize) -> usize {{ {branches} n }}"
            )).unwrap();
            let signature = &input.sig;
            let signature = quote!(#signature).to_string();
            let output = expand(input).unwrap();
            let result_signature = &output.sig;
            assert_eq!(signature, quote!(#result_signature).to_string());
            assert!(output.attrs[0].path().is_ident("unsafe"));
            let mut counts = Counts::default();
            counts.visit_item_fn(&output);
            assert_eq!(counts.closures, 1);
            assert_eq!(counts.boundaries, 1);
            assert_eq!(counts.returns, exits);
            println!(
                "exits={exits} emitted_returns={} closures={} boundaries={}",
                counts.returns, counts.closures, counts.boundaries
            );
        }
    }

    #[test]
    fn rejects_non_c_functions() {
        for source in [
            "fn f() {}",
            "extern \"system\" fn f() {}",
            "extern \"C-unwind\" fn f() {}",
        ] {
            assert!(expand(syn::parse_str(source).unwrap()).is_err());
        }
    }
}
