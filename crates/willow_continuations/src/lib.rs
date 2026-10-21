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
                "then" => {
                    // `(!cond).then(..)` must not keep its now-redundant parentheses.
                    let condition = match &**receiver {
                        Expr::Paren(inner) => &*inner.expr,
                        condition => condition,
                    };
                    *expression = parse_quote!(if #condition {Some(#body)} else {None});
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
                    if let syn::ReceiverKind::Reference(_, lifetime, _) = &mut receiver.kind {
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

/// Parser methods lowered to continuations. Every method on a `Parser` call
/// cycle must be listed: an unlisted one re-enters the synchronous wrapper, so
/// each nesting level starts a fresh executor on the native stack.
const PARSER_METHODS: &str = "parse, parse_add, parse_and, parse_assign, parse_await, parse_bitwise, parse_block, parse_block_inner, parse_call_arg, parse_call_args_after_lparen, parse_class, parse_cmp, parse_constructor, parse_control_head, parse_enum_decl, parse_expr, parse_expr_before_as, parse_expr_stmt, parse_field, parse_field_assign, parse_fn, parse_for, parse_grouped_import, parse_if, parse_if_expr, parse_if_statement_block, parse_if_value, parse_import, parse_interface, parse_interface_method, parse_item, parse_lambda, parse_lambda_inner, parse_let, parse_let_head, parse_lock, parse_match_expr, parse_method, parse_module_decl, parse_module_path, parse_mul, parse_new, parse_object_literal_fields, parse_or, parse_param, parse_pattern, parse_postfix, parse_pow, parse_primary, parse_range, parse_receiver_direct_assign, parse_return, parse_select, parse_static_call, parse_std_qualified_expr, parse_stmt, parse_super_init, parse_ternary, parse_tuple_binding, parse_type, parse_type_path, parse_unary, parse_while, try_parse_generic_static_call";

/// Shared continuation boundary for the parser call graph.
#[proc_macro_attribute]
pub fn parser(_: TokenStream, input: TokenStream) -> TokenStream {
    methods(
        PARSER_METHODS.parse().expect("static continuation names"),
        input,
    )
}

/// Type checker methods lowered to continuations.
const CHECKER_METHODS: &str = "absorb_body, block_diverges, check_all_paths_return, check_array_literal, check_array_literal_expecting, check_array_literal_type, check_array_method_call, check_async_capture, check_async_task_send, check_atomic_method_call, check_block, check_block_scope, check_call_arg_against_param, check_call_args_against_param_infos, check_class, check_class_implements, check_class_inheritance, check_concurrency_method_call, check_constructor, check_deferred_lambdas_plainly, check_expr, check_expr_expecting, check_expr_expecting_inner, check_expr_inner, check_fn_arg_with_param_context, check_format_call, check_frozen_map_method_call, check_function, check_generic_call, check_generic_call_parts, check_generic_enum_argument, check_index, check_interface, check_interface_default_body, check_interpolation, check_lambda, check_lambda_body, check_lambda_expecting, check_lambda_for_callable, check_lambda_in_context, check_lambda_scoped, check_lambda_with_context, check_lambda_with_context_inner, check_lambda_with_param_context, check_lock_method_call, check_lock_stmt, check_map_method_call, check_match_body, check_match_body_inner, check_match_expr, check_match_guard, check_method, check_method_call_expecting, check_module_program, check_new, check_object_literal, check_operator_tree, check_option_result_method_call, check_panic_interpolation, check_program, check_program_items, check_range, check_receiver_method_call, check_reference_argument, check_select, check_source_type_access, check_static_field_assign, check_static_forward_references, check_static_property_initializers, check_stmt, check_super_init, check_super_method_call, check_ternary_expecting, check_try_propagate, check_value_arg_type, check_value_call_args, class_info_from_decl, construct_static_variant_call, construct_variant_call, expr_diverges, instantiate_class, instantiation_bindings, is_send, is_sync, is_task_send, marker_holds, marker_holds_with_reason, named_marker_holds, normalize_decl_param_infos, normalize_decl_type, normalize_param_infos, normalize_param_types, normalize_type, normalize_type_inner, reference_place_info, register_class, register_enum, register_interface, register_module, register_module_impl, register_module_type_signatures, register_module_with_id, register_prelude_enum, register_prelude_interface, reject_constant_call, resolve_fully_qualified_std_module_call, resolve_i64_overflow_method, resolve_interface_method, resolve_method, resolve_static_call, stmt_diverges, validate_type";

/// Shared continuation boundary for the checker call graph.
#[proc_macro_attribute]
pub fn checker(_: TokenStream, input: TokenStream) -> TokenStream {
    methods(
        CHECKER_METHODS.parse().expect("static continuation names"),
        input,
    )
}

#[cfg(test)]
mod tests {
    //! Guard the continuation name lists against the call graphs they lower.
    use super::{CHECKER_METHODS, PARSER_METHODS};
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};
    use syn::parse::Parser as _;
    use syn::visit_mut::{self, VisitMut};
    use syn::{Expr, ImplItem};

    /// `self.m(..)` and `Self::m(..)` callees, including inside macro
    /// arguments that parse as expressions (a superset of the macros the
    /// lowering rewrites, so the graph never misses a call edge).
    struct Callees(BTreeSet<String>);
    impl VisitMut for Callees {
        fn visit_expr_method_call_mut(&mut self, call: &mut syn::ExprMethodCall) {
            if matches!(&*call.receiver, Expr::Path(p) if p.path.is_ident("self")) {
                self.0.insert(call.method.to_string());
            }
            visit_mut::visit_expr_method_call_mut(self, call);
        }
        fn visit_expr_call_mut(&mut self, call: &mut syn::ExprCall) {
            if let Expr::Path(p) = &*call.func
                && let [owner, method] = &p.path.segments.iter().collect::<Vec<_>>()[..]
                && owner.ident == "Self"
            {
                self.0.insert(method.ident.to_string());
            }
            visit_mut::visit_expr_call_mut(self, call);
        }
        fn visit_macro_mut(&mut self, mac: &mut syn::Macro) {
            let parser = syn::punctuated::Punctuated::<Expr, syn::Token![,]>::parse_terminated;
            if let Ok(arguments) = parser.parse2(mac.tokens.clone()) {
                for mut argument in arguments {
                    self.visit_expr_mut(&mut argument);
                }
            }
        }
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Method -> callees over every `impl <ty>` block below `dir`.
    fn call_graph(dir: &str, ty: &str) -> BTreeMap<String, BTreeSet<String>> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(dir);
        let mut files = Vec::new();
        rust_files(&root, &mut files);
        let mut graph = BTreeMap::<String, BTreeSet<String>>::new();
        for path in files {
            let file = syn::parse_file(&std::fs::read_to_string(&path).unwrap())
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            for item in file.items {
                let syn::Item::Impl(implementation) = item else {
                    continue;
                };
                if !matches!(&*implementation.self_ty, syn::Type::Path(p) if p.path.is_ident(ty)) {
                    continue;
                }
                for item in implementation.items {
                    let ImplItem::Fn(mut method) = item else {
                        continue;
                    };
                    let mut callees = Callees(BTreeSet::new());
                    callees.visit_block_mut(&mut method.block);
                    graph
                        .entry(method.sig.ident.to_string())
                        .or_default()
                        .extend(callees.0);
                }
            }
        }
        graph
    }

    /// Unlisted methods sharing a strongly connected component with a listed
    /// one. Such a method calls a listed method's synchronous wrapper, so each
    /// trip around the cycle nests another executor on the native stack.
    /// Tarjan's algorithm keeps this O(methods + calls).
    fn boundary_leaks(
        graph: &BTreeMap<String, BTreeSet<String>>,
        listed: &str,
    ) -> BTreeSet<String> {
        let listed: BTreeSet<&str> = listed.split(',').map(str::trim).collect();
        let names: Vec<&String> = graph.keys().collect();
        let index_of: BTreeMap<&str, usize> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();
        let edges: Vec<Vec<usize>> = names
            .iter()
            .map(|n| {
                graph[*n]
                    .iter()
                    .filter_map(|c| index_of.get(c.as_str()).copied())
                    .collect()
            })
            .collect();
        let (mut index, mut low, mut on_stack) = (
            vec![usize::MAX; names.len()],
            vec![0; names.len()],
            vec![false; names.len()],
        );
        let (mut next, mut stack, mut leaks) = (0, Vec::new(), BTreeSet::new());
        for start in 0..names.len() {
            if index[start] != usize::MAX {
                continue;
            }
            // Explicit (node, next edge) frames instead of recursion.
            let mut frames = vec![(start, 0)];
            index[start] = next;
            low[start] = next;
            next += 1;
            stack.push(start);
            on_stack[start] = true;
            while let Some(&mut (node, ref mut edge)) = frames.last_mut() {
                if let Some(&child) = edges[node].get(*edge) {
                    *edge += 1;
                    if index[child] == usize::MAX {
                        index[child] = next;
                        low[child] = next;
                        next += 1;
                        stack.push(child);
                        on_stack[child] = true;
                        frames.push((child, 0));
                    } else if on_stack[child] {
                        low[node] = low[node].min(index[child]);
                    }
                    continue;
                }
                frames.pop();
                if let Some(&(parent, _)) = frames.last() {
                    low[parent] = low[parent].min(low[node]);
                }
                if low[node] == index[node] {
                    let mut component = Vec::new();
                    loop {
                        let member = stack.pop().unwrap();
                        on_stack[member] = false;
                        component.push(names[member].as_str());
                        if member == node {
                            break;
                        }
                    }
                    if component.iter().any(|m| listed.contains(m)) {
                        leaks.extend(
                            component
                                .into_iter()
                                .filter(|m| !listed.contains(m))
                                .map(str::to_owned),
                        );
                    }
                }
            }
        }
        leaks
    }

    #[test]
    fn parser_call_cycles_are_fully_lowered() {
        // willow-w1tl: parse_if_statement_block and parse_let_head were
        // missing, costing a fresh executor per nested `if` level.
        let leaks = boundary_leaks(&call_graph("src/parser", "Parser"), PARSER_METHODS);
        assert!(leaks.is_empty(), "add to PARSER_METHODS: {leaks:?}");
    }

    #[test]
    fn checker_call_cycles_are_fully_lowered() {
        // willow-5gke: generic calls, match guards, callable lambdas and
        // their helpers each re-entered a synchronous wrapper per level.
        let leaks = boundary_leaks(&call_graph("src/semantic", "TypeChecker"), CHECKER_METHODS);
        assert!(leaks.is_empty(), "add to CHECKER_METHODS: {leaks:?}");
    }

    #[test]
    fn boundary_leaks_reports_only_unlisted_members_of_listed_cycles() {
        let graph: BTreeMap<String, BTreeSet<String>> = [
            ("a", vec!["helper"]),
            ("helper", vec!["a"]),
            ("b", vec!["b"]),
            ("x", vec!["y"]),
            ("y", vec!["x", "a"]),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.into_iter().map(str::to_owned).collect()))
        .collect();
        let leaks = boundary_leaks(&graph, "a, b");
        assert_eq!(leaks, BTreeSet::from(["helper".to_owned()]));
    }
}
