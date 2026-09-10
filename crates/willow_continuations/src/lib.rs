//! Preserve synchronous compiler APIs while lowering recursive methods to
//! borrowed heap continuations. The explicit name list is the call-graph
//! boundary: only listed methods and calls become resumable.
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use std::collections::HashSet;
use syn::parse::Parser as _;
use syn::visit_mut::{self, VisitMut};
use syn::{Expr, FnArg, ImplItem, ItemImpl, Pat, ReturnType, parse_macro_input, parse_quote};

struct AwaitFinder(bool);
impl VisitMut for AwaitFinder {
    fn visit_expr_await_mut(&mut self, _: &mut syn::ExprAwait) {
        self.0 = true;
    }
    fn visit_expr_macro_mut(&mut self, expression: &mut syn::ExprMacro) {
        fn contains(tokens: proc_macro2::TokenStream) -> bool {
            tokens.into_iter().any(|token| match token {
                proc_macro2::TokenTree::Ident(i) => i == "await",
                proc_macro2::TokenTree::Group(g) => contains(g.stream()),
                _ => false,
            })
        }
        self.0 |= contains(expression.mac.tokens.clone());
    }
}
fn has_await(expression: &Expr) -> bool {
    let mut finder = AwaitFinder(false);
    finder.visit_expr_mut(&mut expression.clone());
    finder.0
}
struct Calls<'a>(&'a HashSet<String>);
impl VisitMut for Calls<'_> {
    fn visit_expr_macro_mut(&mut self, expression: &mut syn::ExprMacro) {
        let Some(name) = expression.mac.path.segments.last() else {
            return;
        };
        if !matches!(
            name.ident.to_string().as_str(),
            "format"
                | "format_args"
                | "vec"
                | "write"
                | "writeln"
                | "assert"
                | "assert_eq"
                | "assert_ne"
                | "debug_assert"
                | "debug_assert_eq"
                | "debug_assert_ne"
        ) {
            return;
        }
        let parser = syn::punctuated::Punctuated::<Expr, syn::Token![,]>::parse_terminated;
        if let Ok(mut arguments) = parser.parse2(expression.mac.tokens.clone()) {
            for argument in &mut arguments {
                self.visit_expr_mut(argument);
            }
            expression.mac.tokens = quote!(#arguments);
        }
    }

    fn visit_expr_mut(&mut self, expression: &mut Expr) {
        // Iterator callbacks containing child computations must execute in
        // the surrounding continuation, preserving sequence/short-circuiting.
        if let Expr::MethodCall(collect) = expression
            && collect.method == "collect"
        {
            let collection_type: syn::Type = collect
                .turbofish
                .as_ref()
                .and_then(|args| args.args.first())
                .and_then(|arg| {
                    if let syn::GenericArgument::Type(ty) = arg {
                        Some(ty.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| parse_quote!(_));
            if let Expr::MethodCall(map) = &mut *collect.receiver
                && map.method == "map"
                && map.args.len() == 1
                && let Some(Expr::Closure(callback)) = map.args.first_mut()
            {
                self.visit_expr_mut(&mut callback.body);
                if has_await(&callback.body) {
                    self.visit_expr_mut(&mut map.receiver);
                    let receiver = &map.receiver;
                    let Expr::Closure(callback) = map.args.first().unwrap() else {
                        unreachable!()
                    };
                    let pattern = &callback.inputs;
                    let original_body = &callback.body;
                    let body = quote!((async { #original_body }).await);
                    *expression = parse_quote!({let mut __continuation_values=crate::compiler_stack::Collector::<#collection_type, _>::new(); for #pattern in #receiver {if __continuation_values.push(#body).is_break() {break;}} __continuation_values.finish()});
                    return;
                }
            }
        }
        visit_mut::visit_expr_mut(self, expression);
        if let Expr::MethodCall(call) = expression
            && call.args.len() == 1
            && let Some(Expr::Closure(callback)) = call.args.first()
            && has_await(&callback.body)
        {
            let receiver = &call.receiver;
            let pattern = &callback.inputs;
            let original_body = &callback.body;
            let body = quote!((async { #original_body }).await);
            match call.method.to_string().as_str() {
                "map" => {
                    *expression =
                        parse_quote!(match #receiver {Some(#pattern)=>Some(#body),None=>None});
                    return;
                }
                "is_some_and" => {
                    *expression = parse_quote!(match #receiver {Some(#pattern)=>#body,None=>false});
                    return;
                }
                "is_none_or" => {
                    *expression = parse_quote!(match #receiver {Some(#pattern)=>#body,None=>true});
                    return;
                }
                "all" => {
                    *expression = parse_quote!({let mut __continuation_result=true;for #pattern in #receiver {if !(#body) {__continuation_result=false;break;}}__continuation_result});
                    return;
                }
                "any" => {
                    *expression = parse_quote!({let mut __continuation_result=false;for #pattern in #receiver {if #body {__continuation_result=true;break;}}__continuation_result});
                    return;
                }
                _ => {}
            }
        }

        if let Expr::Call(call) = expression
            && let Expr::Path(path) = &mut *call.func
            && let Some(segment) = path.path.segments.last_mut()
            && self.0.contains(&segment.ident.to_string())
        {
            segment.ident = format_ident!("{}_cont", segment.ident);
            *expression = parse_quote! { #call.await };
            return;
        }
        if let Expr::MethodCall(call) = expression
            && self.0.contains(&call.method.to_string())
        {
            call.method = format_ident!("{}_cont", call.method);
            *expression = parse_quote! { #call.await };
        }
    }
}
struct BorrowLifetime;
impl VisitMut for BorrowLifetime {
    fn visit_lifetime_mut(&mut self, lifetime: &mut syn::Lifetime) {
        if lifetime.ident == "_" {
            *lifetime = parse_quote!('stack);
        }
    }

    fn visit_type_reference_mut(&mut self, reference: &mut syn::TypeReference) {
        if reference.lifetime.is_none() {
            reference.lifetime = Some(parse_quote!('stack));
        }
        visit_mut::visit_type_reference_mut(self, reference);
    }
}
#[proc_macro_attribute]
pub fn methods(arguments: TokenStream, input: TokenStream) -> TokenStream {
    let names = parse_macro_input!(arguments with syn::punctuated::Punctuated::<syn::Ident, syn::Token![,]>::parse_terminated);
    let names: HashSet<String> = names.iter().map(ToString::to_string).collect();
    let mut implementation = parse_macro_input!(input as ItemImpl);
    let mut generated = Vec::new();
    for item in &mut implementation.items {
        let ImplItem::Fn(method) = item else {
            continue;
        };
        if !names.contains(&method.sig.ident.to_string()) {
            continue;
        }
        let mut continuation = method.clone();
        continuation.sig.ident = format_ident!("{}_cont", method.sig.ident);
        continuation
            .sig
            .generics
            .params
            .insert(0, parse_quote!('stack));
        let output: syn::Type = match &method.sig.output {
            ReturnType::Default => parse_quote!(()),
            ReturnType::Type(_, ty) => *ty.clone(),
        };
        for argument in &mut continuation.sig.inputs {
            match argument {
                FnArg::Receiver(receiver) => {
                    if let Some((_, lifetime)) = &mut receiver.reference {
                        *lifetime = Some(parse_quote!('stack));
                    }
                }
                FnArg::Typed(argument) => BorrowLifetime.visit_type_mut(&mut argument.ty),
            }
        }
        continuation.sig.output =
            parse_quote!(-> crate::compiler_stack::Continuation<'stack, #output>);
        Calls(&names).visit_block_mut(&mut continuation.block);
        let body = continuation.block;
        continuation.block = parse_quote!({
            let __compiler_future = async move #body;
            // SAFETY: compiler state machines await their directly owned
            // child continuations; suspended parents retain each child.
            unsafe { crate::compiler_stack::Continuation::new(__compiler_future) }
        });
        let arguments: Vec<_> = method
            .sig
            .inputs
            .iter()
            .filter_map(|argument| match argument {
                FnArg::Receiver(_) => None,
                FnArg::Typed(argument) => match &*argument.pat {
                    Pat::Ident(name) => Some(name.ident.clone()),
                    _ => None,
                },
            })
            .collect();
        let name = &continuation.sig.ident;
        method.block = parse_quote!({ crate::compiler_stack::run(self.#name(#(#arguments),*)) });
        // A method can be used solely by another continuation; preserve the
        // synchronous API for tests and independently-entered compiler stages.
        method.attrs.push(parse_quote!(#[allow(dead_code)]));
        generated.push(ImplItem::Fn(continuation));
    }
    implementation.items.extend(generated);
    quote!(#implementation).into()
}

#[proc_macro_attribute]
pub fn function(arguments: TokenStream, input: TokenStream) -> TokenStream {
    let names = parse_macro_input!(arguments with syn::punctuated::Punctuated::<syn::Ident, syn::Token![,]>::parse_terminated);
    let names: HashSet<String> = names.iter().map(ToString::to_string).collect();
    let mut function = parse_macro_input!(input as syn::ItemFn);
    let mut continuation = function.clone();
    continuation.sig.ident = format_ident!("{}_cont", function.sig.ident);
    let existing: Vec<_> = continuation
        .sig
        .generics
        .lifetimes()
        .map(|p| p.lifetime.clone())
        .collect();
    continuation
        .sig
        .generics
        .params
        .insert(0, parse_quote!('stack));
    for lifetime in existing {
        continuation
            .sig
            .generics
            .make_where_clause()
            .predicates
            .push(parse_quote!(#lifetime: 'stack));
    }
    for argument in &mut continuation.sig.inputs {
        if let FnArg::Typed(argument) = argument {
            BorrowLifetime.visit_type_mut(&mut argument.ty);
        }
    }
    let output: syn::Type = match &function.sig.output {
        ReturnType::Default => parse_quote!(()),
        ReturnType::Type(_, ty) => *ty.clone(),
    };
    continuation.sig.output = parse_quote!(-> crate::compiler_stack::Continuation<'stack, #output>);
    Calls(&names).visit_block_mut(&mut continuation.block);
    let body = continuation.block;
    continuation.block = parse_quote!({
        let __compiler_future = async move #body;
        // SAFETY: compiler state machines retain directly awaited children.
        unsafe { crate::compiler_stack::Continuation::new(__compiler_future) }
    });
    let arguments: Vec<_> = function
        .sig
        .inputs
        .iter()
        .filter_map(|a| match a {
            FnArg::Typed(a) => match &*a.pat {
                Pat::Ident(p) => Some(p.ident.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let name = &continuation.sig.ident;
    *function.block = parse_quote!({crate::compiler_stack::run(#name(#(#arguments),*))});
    function.attrs.push(parse_quote!(#[allow(dead_code)]));
    quote!(#function #continuation).into()
}

/// Shared continuation boundary for the parser call graph.
#[proc_macro_attribute]
pub fn parser(_: TokenStream, input: TokenStream) -> TokenStream {
    methods("parse, parse_add, parse_and, parse_assign, parse_await, parse_block, parse_call_arg, parse_call_args_after_lparen, parse_class, parse_cmp, parse_constructor, parse_enum_decl, parse_expr, parse_expr_stmt, parse_field, parse_field_assign, parse_fn, parse_for, parse_grouped_import, parse_if, parse_import, parse_interface, parse_interface_method, parse_item, parse_lambda, parse_let, parse_lock, parse_match_expr, parse_method, parse_module_decl, parse_module_path, parse_mul, parse_new, parse_object_literal_fields, parse_or, parse_param, parse_pattern, parse_postfix, parse_pow, parse_primary, parse_range, parse_receiver_direct_assign, parse_return, parse_select, parse_static_call, parse_std_qualified_expr, parse_stmt, parse_super_init, parse_ternary, parse_type, parse_type_path, parse_unary, parse_while, try_parse_generic_static_call".parse().expect("static continuation names"), input)
}

/// Shared continuation boundary for the checker call graph.
#[proc_macro_attribute]
pub fn checker(_: TokenStream, input: TokenStream) -> TokenStream {
    methods("block_diverges, check_all_paths_return, check_array_literal, check_array_literal_expecting, check_array_literal_type, check_array_method_call, check_async_capture, check_async_task_send, check_atomic_method_call, check_block, check_call_arg_against_param, check_call_args_against_param_infos, check_class, check_class_implements, check_class_inheritance, check_concurrency_method_call, check_constructor, check_expr, check_expr_expecting, check_expr_expecting_inner, check_expr_inner, check_fn_arg_with_param_context, check_format_call, check_frozen_map_method_call, check_function, check_index, check_interface, check_interface_default_body, check_interpolation, check_lambda, check_lambda_expecting, check_lambda_with_context, check_lambda_with_context_inner, check_lambda_with_param_context, check_lock_method_call, check_lock_stmt, check_map_method_call, check_match_body, check_match_body_inner, check_match_expr, check_method, check_module_program, check_new, check_object_literal, check_operator_tree, check_option_result_method_call, check_panic_interpolation, check_program, check_program_items, check_range, check_reference_argument, check_select, check_source_type_access, check_static_field_assign, check_static_forward_references, check_static_property_initializers, check_stmt, check_super_init, check_ternary_expecting, check_try_propagate, check_value_arg_type, check_value_call_args, class_info_from_decl, construct_static_variant_call, construct_variant_call, expr_diverges, is_send, is_sync, is_task_send, marker_holds, named_marker_holds, normalize_decl_param_infos, normalize_decl_type, normalize_param_infos, normalize_param_types, normalize_type, normalize_type_inner, reference_place_info, register_class, register_enum, register_interface, register_module, register_module_impl, register_module_type_signatures, register_module_with_id, register_prelude_enum, register_prelude_interface, resolve_fully_qualified_std_module_call, resolve_interface_method, resolve_method, resolve_static_call, stmt_diverges, validate_type".parse().expect("static continuation names"), input)
}
