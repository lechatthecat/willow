//! `std::collections` (`Array`/`Map`) name-normalization pass for the Cranelift
//! backend (extracted from `mod.rs`). Rewrites imported/aliased collection
//! references to their canonical builtin form before codegen.

use std::collections::{HashMap, HashSet};

use crate::module::std_registry;
use crate::parser::ast::*;

pub(crate) fn normalize_std_collection_program(program: &Program) -> Program {
    let imports = std_collection_imports(program);
    let mut program = program.clone();
    for item in &mut program.items {
        normalize_std_collection_item(item, &imports);
    }
    program
}

pub(crate) struct StdCollectionImports {
    modules: HashSet<String>,
    aliases: HashMap<String, String>,
}

pub(crate) fn std_collection_imports(program: &Program) -> StdCollectionImports {
    let mut modules = HashSet::new();
    let mut aliases = HashMap::new();
    for import in &program.imports {
        if !std_registry::is_std_path(&import.path) {
            continue;
        }
        match std_registry::resolve_std_import(&import.path, import.span) {
            Ok(std_registry::StdImport::Module { module })
                if module == "collections" && import.alias.is_none() =>
            {
                modules.insert("collections".to_string());
            }
            Ok(std_registry::StdImport::Item { module, item })
                if module == "collections" && matches!(item.as_str(), "Array" | "Map") =>
            {
                aliases.insert(import.alias.clone().unwrap_or_else(|| item.clone()), item);
            }
            _ => {}
        }
    }
    StdCollectionImports { modules, aliases }
}

pub(crate) fn normalize_std_collection_item(item: &mut Item, imports: &StdCollectionImports) {
    match item {
        Item::Function(function) => normalize_std_collection_function(function, imports),
        Item::Class(class) => {
            for field in &mut class.fields {
                normalize_std_collection_type(&mut field.ty, imports);
            }
            for method in &mut class.methods {
                normalize_std_collection_method(method, imports);
            }
        }
        Item::Enum(en) => {
            for variant in &mut en.variants {
                for ty in &mut variant.payload {
                    normalize_std_collection_type(ty, imports);
                }
            }
        }
        Item::Interface(interface) => {
            for method in &mut interface.methods {
                for param in &mut method.params {
                    normalize_std_collection_type(&mut param.ty, imports);
                }
                normalize_std_collection_type(&mut method.return_type, imports);
            }
        }
    }
}

pub(crate) fn normalize_std_collection_function(function: &mut FunctionDecl, imports: &StdCollectionImports) {
    for param in &mut function.params {
        normalize_std_collection_type(&mut param.ty, imports);
    }
    normalize_std_collection_type(&mut function.return_type, imports);
    normalize_std_collection_block(&mut function.body, imports);
}

pub(crate) fn normalize_std_collection_method(method: &mut MethodDecl, imports: &StdCollectionImports) {
    for param in &mut method.params {
        normalize_std_collection_type(&mut param.ty, imports);
    }
    normalize_std_collection_type(&mut method.return_type, imports);
    normalize_std_collection_block(&mut method.body, imports);
}

pub(crate) fn normalize_std_collection_block(block: &mut Block, imports: &StdCollectionImports) {
    for stmt in &mut block.stmts {
        normalize_std_collection_stmt(stmt, imports);
    }
}

pub(crate) fn normalize_std_collection_stmt(stmt: &mut Stmt, imports: &StdCollectionImports) {
    match stmt {
        Stmt::Let(s) => {
            if let Some(ty) = &mut s.ty {
                normalize_std_collection_type(ty, imports);
            }
            normalize_std_collection_expr(&mut s.init, imports);
        }
        Stmt::Assign(s) => normalize_std_collection_expr(&mut s.value, imports),
        Stmt::SuperInit(s) => {
            for arg in &mut s.args {
                normalize_std_collection_call_arg(arg, imports);
            }
        }
        Stmt::StaticFieldAssign(s) => normalize_std_collection_expr(&mut s.value, imports),
        Stmt::FieldAssign(s) => {
            normalize_std_collection_expr(&mut s.object, imports);
            normalize_std_collection_expr(&mut s.value, imports);
        }
        Stmt::IndexAssign(s) => {
            normalize_std_collection_expr(&mut s.array, imports);
            normalize_std_collection_expr(&mut s.index, imports);
            normalize_std_collection_expr(&mut s.value, imports);
        }
        Stmt::If(s) => {
            normalize_std_collection_expr(&mut s.cond, imports);
            normalize_std_collection_block(&mut s.then_block, imports);
            if let Some(block) = &mut s.else_block {
                normalize_std_collection_block(block, imports);
            }
        }
        Stmt::While(s) => {
            normalize_std_collection_expr(&mut s.cond, imports);
            normalize_std_collection_block(&mut s.body, imports);
        }
        Stmt::For(s) => {
            normalize_std_collection_expr(&mut s.iterable, imports);
            normalize_std_collection_block(&mut s.body, imports);
        }
        Stmt::Return(s) => {
            if let Some(value) = &mut s.value {
                normalize_std_collection_expr(value, imports);
            }
        }
        Stmt::Expr(s) => normalize_std_collection_expr(&mut s.expr, imports),
    }
}

pub(crate) fn normalize_std_collection_expr(expr: &mut Expr, imports: &StdCollectionImports) {
    match expr {
        Expr::StaticField(_) => {}
        Expr::Binary(binary) => {
            normalize_std_collection_expr(&mut binary.lhs, imports);
            normalize_std_collection_expr(&mut binary.rhs, imports);
        }
        Expr::Unary(unary) => normalize_std_collection_expr(&mut unary.expr, imports),
        Expr::Call(call) => {
            for arg in &mut call.args {
                normalize_std_collection_call_arg(arg, imports);
            }
        }
        Expr::FieldAccess(object, _, _) => {
            normalize_std_collection_expr(object, imports);
        }
        Expr::MethodCall(call) => {
            normalize_std_collection_expr(&mut call.object, imports);
            for arg in &mut call.args {
                normalize_std_collection_call_arg(arg, imports);
            }
        }
        Expr::StaticCall(call) => {
            if let Some(item) = std_collection_item_name(&call.class, imports) {
                call.class = item.to_string();
            }
            for ty in &mut call.type_args {
                normalize_std_collection_type(ty, imports);
            }
            for arg in &mut call.args {
                normalize_std_collection_call_arg(arg, imports);
            }
        }
        Expr::New(n) => {
            for ty in &mut n.type_args {
                normalize_std_collection_type(ty, imports);
            }
            for arg in &mut n.args {
                normalize_std_collection_call_arg(arg, imports);
            }
        }
        Expr::ObjectLiteral(object) => {
            for field in &mut object.fields {
                normalize_std_collection_expr(&mut field.value, imports);
            }
        }
        Expr::Await(await_expr) => {
            normalize_std_collection_expr(&mut await_expr.expr, imports);
        }
        Expr::Print(arg, _, _) => normalize_std_collection_expr(arg, imports),
        Expr::Ternary(ternary) => {
            normalize_std_collection_expr(&mut ternary.condition, imports);
            normalize_std_collection_expr(&mut ternary.then_expr, imports);
            normalize_std_collection_expr(&mut ternary.else_expr, imports);
        }
        Expr::Range(range) => {
            normalize_std_collection_expr(&mut range.start, imports);
            normalize_std_collection_expr(&mut range.end, imports);
        }
        Expr::Lambda(lambda) => {
            for param in &mut lambda.params {
                if let Some(ty) = &mut param.ty {
                    normalize_std_collection_type(ty, imports);
                }
            }
            if let Some(ty) = &mut lambda.return_type {
                normalize_std_collection_type(ty, imports);
            }
            match &mut lambda.body {
                LambdaBody::Expr(body) => normalize_std_collection_expr(body, imports),
                LambdaBody::Block(block) => normalize_std_collection_block(block, imports),
            }
        }
        Expr::Match(match_expr) => {
            normalize_std_collection_expr(&mut match_expr.scrutinee, imports);
            for arm in &mut match_expr.arms {
                match &mut arm.body {
                    MatchBody::Expr(body) => normalize_std_collection_expr(body, imports),
                    MatchBody::Block(block) => normalize_std_collection_block(block, imports),
                }
            }
        }
        Expr::TryPropagate(inner, _) => normalize_std_collection_expr(inner, imports),
        Expr::ArrayLiteral(elements, _) => {
            for element in elements {
                normalize_std_collection_expr(element, imports);
            }
        }
        Expr::Index(array, index, _) => {
            normalize_std_collection_expr(array, imports);
            normalize_std_collection_expr(index, imports);
        }
        Expr::Select(s) => {
            for case in &mut s.cases {
                match &mut case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        normalize_std_collection_expr(channel, imports)
                    }
                    SelectCaseKind::Send { channel, value } => {
                        normalize_std_collection_expr(channel, imports);
                        normalize_std_collection_expr(value, imports);
                    }
                    SelectCaseKind::Default => {}
                }
                normalize_std_collection_block(&mut case.body, imports);
            }
        }
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::String(_, _)
        | Expr::Var(_, _) => {}
    }
}

pub(crate) fn normalize_std_collection_call_arg(arg: &mut CallArg, imports: &StdCollectionImports) {
    normalize_std_collection_expr(&mut arg.expr, imports);
}

pub(crate) fn normalize_std_collection_type(ty: &mut Type, imports: &StdCollectionImports) {
    match ty {
        Type::Array(element) => normalize_std_collection_type(element, imports),
        Type::Generic(name, args) => {
            for arg in args.iter_mut() {
                normalize_std_collection_type(arg, imports);
            }
            match std_collection_item_name(name, imports) {
                Some("Array") if args.len() == 1 => {
                    let element = args.remove(0);
                    *ty = Type::Array(Box::new(element));
                }
                Some("Map") => {
                    *name = "Map".to_string();
                }
                Some("Option") => {
                    *name = "Option".to_string();
                }
                Some("Result") => {
                    *name = "Result".to_string();
                }
                _ => {}
            }
        }
        Type::Nullable(inner) => normalize_std_collection_type(inner, imports),
        Type::Fn(params, ret) => {
            for param in params {
                normalize_std_collection_type(param, imports);
            }
            normalize_std_collection_type(ret, imports);
        }
        Type::I64
        | Type::F64
        | Type::Bool
        | Type::String
        | Type::Void
        | Type::Nil
        | Type::Named(_)
        | Type::Never => {}
    }
}

pub(crate) fn std_collection_item_name<'a>(
    qualified_name: &'a str,
    imports: &'a StdCollectionImports,
) -> Option<&'a str> {
    if let Some(item) = imports.aliases.get(qualified_name) {
        return Some(item.as_str());
    }
    match qualified_name {
        "std::collections::Array" => return Some("Array"),
        "std::collections::Map" => return Some("Map"),
        "std::option::Option" => return Some("Option"),
        "std::result::Result" => return Some("Result"),
        _ => {}
    }
    let (module, item) = qualified_name.split_once("::")?;
    if imports.modules.contains(module) && matches!(item, "Array" | "Map") {
        Some(item)
    } else {
        None
    }
}
