//! Syntax-only assertion marker. Willow validates the project review separately.
extern crate proc_macro;
use proc_macro::{TokenStream, TokenTree};

#[proc_macro_attribute]
pub fn assume(attribute: TokenStream, item: TokenStream) -> TokenStream {
    let tokens = attribute.into_iter().map(|token| match token {
        TokenTree::Ident(name) => match name.to_string().as_str() {
            "no_block" => ClaimToken::Effect(1),
            "no_io" => ClaimToken::Effect(2),
            _ => ClaimToken::Invalid,
        },
        TokenTree::Punct(punct) if punct.as_char() == ',' => ClaimToken::Comma,
        _ => ClaimToken::Invalid,
    });
    if !valid_assumptions(tokens) {
        return "compile_error!(\"expected unique no_block and/or no_io in willow::assume(...)\");"
            .parse()
            .unwrap();
    }
    item
}

#[derive(Clone, Copy)]
enum ClaimToken {
    Effect(u8),
    Comma,
    Invalid,
}

/// One pass over the attribute, with constant storage for supported effects.
fn valid_assumptions(tokens: impl IntoIterator<Item = ClaimToken>) -> bool {
    let mut seen = 0;
    let mut expect_effect = true;
    for token in tokens {
        match token {
            ClaimToken::Effect(bit) if expect_effect && seen & bit == 0 => {
                seen |= bit;
                expect_effect = false;
            }
            ClaimToken::Comma if !expect_effect => expect_effect = true,
            _ => return false,
        }
    }
    seen != 0
}

#[cfg(test)]
mod tests {
    use super::{ClaimToken::*, valid_assumptions};

    #[test]
    fn assumption_list_accepts_each_effect_orders_and_trailing_commas() {
        for tokens in [
            vec![Effect(1)],
            vec![Effect(2)],
            vec![Effect(1), Comma],
            vec![Effect(2), Comma],
            vec![Effect(1), Comma, Effect(2)],
            vec![Effect(2), Comma, Effect(1)],
            vec![Effect(1), Comma, Effect(2), Comma],
            vec![Effect(2), Comma, Effect(1), Comma],
        ] {
            assert!(valid_assumptions(tokens));
        }
    }

    #[test]
    fn assumption_list_rejects_duplicates_missing_names_and_invalid_tokens() {
        for tokens in [
            vec![],
            vec![Comma],
            vec![Invalid],
            vec![Comma, Effect(1)],
            vec![Effect(1), Effect(2)],
            vec![Effect(1), Comma, Comma],
            vec![Effect(1), Comma, Effect(1)],
            vec![Effect(2), Comma, Effect(2)],
            vec![Effect(1), Comma, Effect(2), Comma, Effect(1)],
            vec![Effect(1), Invalid],
            vec![Effect(2), Comma, Invalid],
        ] {
            assert!(!valid_assumptions(tokens));
        }
    }
}
