//! Syntax-only assertion marker. Willow validates the project review separately.
extern crate proc_macro;
use proc_macro::{TokenStream, TokenTree};

#[proc_macro_attribute]
pub fn assume(attribute: TokenStream, item: TokenStream) -> TokenStream {
    let mut tokens = attribute.into_iter();
    if !matches!(tokens.next(), Some(TokenTree::Ident(name)) if name.to_string() == "no_block")
        || tokens.next().is_some()
    {
        return "compile_error!(\"expected willow::assume(no_block)\");"
            .parse()
            .unwrap();
    }
    item
}
