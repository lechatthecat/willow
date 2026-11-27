//! Syntax-only Rust module interfaces and adapters to the existing bridge ABI.
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use proc_macro2::Span as RustSpan;
use sha2::{Digest, Sha256};
use syn::{spanned::Spanned, visit::Visit};

use crate::diagnostics::{Diagnostic, ErrorCode, FileId, Label, Severity, Span};
use crate::parser::ast::*;
use crate::rust_bridge_ir::{Record, RustBridgeSymbol, Scalar};

/// Captured source is attached only to the first declaration in a module.
/// The remaining declarations carry only their own adapter and module identity.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NativeBridge {
    pub module: String,
    pub source_path: PathBuf,
    pub source: Option<String>,
    pub adapter: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub definitions: Vec<Record>,
}

#[cfg(test)]
thread_local! {
    static CONVERSION_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone)]
enum NativeType {
    Value(Scalar),
    String { borrowed: bool },
    Bytes { borrowed: bool },
    Record(String),
    Opaque(String),
    OwnedOpaque(String),
    Array(Box<Self>),
    Option(Box<Self>),
    Result(Box<Self>, Box<Self>),
}
impl NativeType {
    fn scalar(&self, module: &str) -> Scalar {
        match self {
            Self::Value(s) => s.clone(),
            Self::String { .. } => Scalar::String,
            Self::Bytes { .. } => Scalar::Bytes,
            Self::Record(n) => Scalar::Record(format!("{module}_{n}")),
            Self::Opaque(n) | Self::OwnedOpaque(n) => Scalar::Opaque(format!("{module}_{n}")),
            Self::Array(t) => Scalar::Array(Box::new(t.scalar(module))),
            Self::Option(t) => Scalar::Option(Box::new(t.scalar(module))),
            Self::Result(a, b) => {
                Scalar::Result(Box::new(a.scalar(module)), Box::new(b.scalar(module)))
            }
        }
    }
    fn willow(&self) -> Type {
        match self {
            Self::String { .. } => Type::String,
            Self::Bytes { .. } => Type::Array(Box::new(Type::U8)),
            Self::Record(n) | Self::Opaque(n) | Self::OwnedOpaque(n) => Type::Named(n.clone()),
            Self::Array(t) => Type::Array(Box::new(t.willow())),
            Self::Option(t) => Type::Generic("Option".into(), vec![t.willow()]),
            Self::Result(a, b) => Type::Generic("Result".into(), vec![a.willow(), b.willow()]),
            Self::Value(s) => match s {
                Scalar::I8 => Type::I8,
                Scalar::I16 => Type::I16,
                Scalar::I32 => Type::I32,
                Scalar::I64 => Type::I64,
                Scalar::U8 => Type::U8,
                Scalar::U16 => Type::U16,
                Scalar::U32 => Type::U32,
                Scalar::U64 => Type::U64,
                Scalar::Char => Type::Char,
                Scalar::F64 => Type::F64,
                Scalar::Bool => Type::Bool,
                Scalar::Void => Type::Void,
                _ => unreachable!(),
            },
        }
    }
    fn needs_conversion(&self, input: bool) -> bool {
        #[cfg(test)]
        CONVERSION_VISITS.set(CONVERSION_VISITS.get() + 1);
        match self {
            Self::String { borrowed } | Self::Bytes { borrowed } => input && !borrowed,
            Self::Record(_) | Self::OwnedOpaque(_) => true,
            Self::Array(t) | Self::Option(t) => t.needs_conversion(input),
            Self::Result(a, b) => a.needs_conversion(input) || b.needs_conversion(input),
            _ => false,
        }
    }
    fn convert(&self, value: &str, module: &str, input: bool) -> String {
        if !self.needs_conversion(input) {
            return value.to_owned();
        }
        let mut out = String::new();
        self.append_conversion(&mut out, value, module, input);
        out
    }
    fn append_conversion(&self, out: &mut String, value: &str, module: &str, input: bool) {
        #[cfg(test)]
        CONVERSION_VISITS.set(CONVERSION_VISITS.get() + 1);
        // Each type node appends once; nested Option/Result never recopy an
        // already-generated suffix. Identity Vec/scalar paths remain moves.
        match self {
            Self::OwnedOpaque(_) if input => write!(out, "*({value})").unwrap(),
            Self::OwnedOpaque(_) => write!(out, "Box::new({value})").unwrap(),
            Self::String { borrowed: false } if input => {
                write!(out, "({value}).to_owned()").unwrap()
            }
            Self::Bytes { borrowed: false } if input => write!(out, "({value}).to_vec()").unwrap(),
            Self::Record(n) => write!(
                out,
                "{}_{module}_{n}({value})",
                if input { "to_native" } else { "from_native" }
            )
            .unwrap(),
            Self::Array(t) if t.needs_conversion(input) => {
                write!(out, "({value}).into_iter().map(|v| ").unwrap();
                t.append_conversion(out, "v", module, input);
                out.push_str(").collect()");
            }
            Self::Option(t) => {
                write!(out, "({value}).map(|v| ").unwrap();
                t.append_conversion(out, "v", module, input);
                out.push(')');
            }
            Self::Result(a, b) => {
                write!(out, "({value}).map(|v| ").unwrap();
                a.append_conversion(out, "v", module, input);
                out.push_str(").map_err(|e| ");
                b.append_conversion(out, "e", module, input);
                out.push(')');
            }
            _ => out.push_str(value),
        }
    }
    fn needs_owned_conversion(&self) -> bool {
        #[cfg(test)]
        CONVERSION_VISITS.set(CONVERSION_VISITS.get() + 1);
        match self {
            Self::String { borrowed } | Self::Bytes { borrowed } => *borrowed,
            Self::Record(_) | Self::Opaque(_) => true,
            Self::OwnedOpaque(_) => false,
            Self::Array(t) | Self::Option(t) => t.needs_owned_conversion(),
            Self::Result(a, b) => a.needs_owned_conversion() || b.needs_owned_conversion(),
            _ => false,
        }
    }

    /// Convert a mutable reference to owned task input. Borrowed native leaves
    /// retain their storage across await; owned leaves move without cloning.
    fn append_owned_conversion(&self, out: &mut String, value: &str, module: &str) {
        #[cfg(test)]
        CONVERSION_VISITS.set(CONVERSION_VISITS.get() + 1);
        match self {
            Self::Value(_) => write!(out, "*({value})").unwrap(),
            Self::OwnedOpaque(_) => unreachable!("owned handles move directly"),
            Self::String { borrowed: true } => write!(out, "({value}).as_str()").unwrap(),
            Self::Bytes { borrowed: true } => write!(out, "({value}).as_slice()").unwrap(),
            Self::String { borrowed: false } | Self::Bytes { borrowed: false } => {
                write!(out, "std::mem::take({value})").unwrap();
            }
            Self::Opaque(_) => write!(out, "&**({value})").unwrap(),
            Self::Record(name) => write!(out, "to_native_owned_{module}_{name}({value})").unwrap(),
            // Array elements are limited to scalars and strings, so this
            // predicate does not rescan a nested container subtree.
            Self::Array(t) if t.needs_owned_conversion() => {
                write!(out, "({value}).iter_mut().map(|v| ").unwrap();
                t.append_owned_conversion(out, "v", module);
                out.push_str(").collect()");
            }
            Self::Array(_) => write!(out, "std::mem::take({value})").unwrap(),
            Self::Option(t) => {
                write!(out, "({value}).as_mut().map(|v| ").unwrap();
                t.append_owned_conversion(out, "v", module);
                out.push(')');
            }
            Self::Result(a, b) => {
                write!(out, "({value}).as_mut().map(|v| ").unwrap();
                a.append_owned_conversion(out, "v", module);
                out.push_str(").map_err(|e| ");
                b.append_owned_conversion(out, "e", module);
                out.push(')');
            }
        }
    }
}

struct Deriver<'a> {
    source: &'a str,
    file: FileId,
    module: String,
    structs: HashMap<String, &'a syn::ItemStruct>,
    opaque: HashSet<String>,
    bindings: TypeBindings,
}
impl Deriver<'_> {
    fn span(&self, span: RustSpan) -> Span {
        let start = span.start();
        let bytes = span.byte_range();
        Span::in_file(
            self.file,
            bytes.start,
            bytes.end,
            start.line,
            start.column + 1,
        )
    }
    fn error(&self, span: RustSpan, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(
            Severity::Error,
            ErrorCode::E0102,
            format!("rust_module_signature: {}", message.into()),
        )
        .with_label(Label::primary(self.span(span), "Rust interface here"))
    }
    fn generics(&self, generics: &syn::Generics) -> Result<(), Diagnostic> {
        if !generics.params.is_empty() || generics.where_clause.is_some() {
            return Err(self.error(
                generics.span(),
                "generics and lifetimes are unsupported in public signatures",
            ));
        }
        Ok(())
    }
    fn export_name(&self, id: &syn::Ident) -> Result<String, Diagnostic> {
        use crate::lexer::token::TokenKind;
        let name = ident(id);
        let tokens = crate::lexer::Lexer::new(&name).tokenize();
        // Match Parser::expect_ident, including its contextual `new` exception.
        let valid = tokens.as_ref().is_ok_and(|tokens| {
            tokens.len() == 2
                && matches!(tokens[1].kind, TokenKind::Eof)
                && (matches!(&tokens[0].kind, TokenKind::Ident(n) if n == &name && n != "this")
                    || matches!(tokens[0].kind, TokenKind::New))
        });
        if !valid {
            return Err(self.error(
                id.span(),
                format!("public Rust name `{name}` cannot be written as a Willow identifier"),
            ));
        }
        Ok(name)
    }
    fn attributes(&self, attrs: &[syn::Attribute]) -> Result<(), Diagnostic> {
        self.assumptions(attrs, false).map(|_| ())
    }
    fn assumptions(
        &self,
        attrs: &[syn::Attribute],
        supported: bool,
    ) -> Result<Option<(bool, bool, Span)>, Diagnostic> {
        let mut assumed = None;
        for attr in attrs {
            if attr.path().segments.len() == 2
                && attr.path().segments[0].ident == "willow"
                && attr.path().segments[1].ident == "assume"
            {
                let error = || {
                    self.error(attr.span(), "expected one #[willow::assume(no_block, no_io)] with unique supported effect names on a function, method or opaque type")
                };
                if !supported || assumed.is_some() {
                    return Err(error());
                }
                let names = attr
                    .parse_args_with(
                        syn::punctuated::Punctuated::<syn::Ident, syn::Token![,]>::parse_terminated,
                    )
                    .map_err(|_| error())?;
                if names.is_empty() {
                    return Err(error());
                }
                let (mut no_block, mut no_io) = (false, false);
                for name in names {
                    match name.to_string().as_str() {
                        "no_block" if !no_block => no_block = true,
                        "no_io" if !no_io => no_io = true,
                        _ => return Err(error()),
                    }
                }
                assumed = Some((no_block, no_io, self.span(attr.span())));
                continue;
            }
            if !attr.path().is_ident("doc")
                && !attr.path().is_ident("allow")
                && !attr.path().is_ident("warn")
                && !attr.path().is_ident("deny")
                && !attr.path().is_ident("forbid")
                && !attr.path().is_ident("derive")
                && !attr.path().is_ident("inline")
                && !attr.path().is_ident("must_use")
                && !attr.path().is_ident("repr")
                && !attr.path().is_ident("deprecated")
            {
                return Err(self.error(
                    attr.span(),
                    "conditional or transforming attributes on public interfaces are unsupported",
                ));
            }
        }
        Ok(assumed)
    }
    fn ty(&self, ty: &syn::Type, input: bool) -> Result<NativeType, Diagnostic> {
        let fail = || self.error(ty.span(), "unsupported Rust bridge type or type arguments");
        if let syn::Type::Tuple(t) = ty
            && t.elems.is_empty()
        {
            return Ok(NativeType::Value(Scalar::Void));
        }
        if let syn::Type::Reference(r) = ty {
            if !input || r.lifetime.is_some() || r.mutability.is_some() {
                return Err(fail());
            }
            if let syn::Type::Path(p) = &*r.elem
                && p.qself.is_none()
            {
                let name = self.type_name(p)?;
                if name == "str"
                    && matches!(
                        p.path.segments.last().unwrap().arguments,
                        syn::PathArguments::None
                    )
                {
                    return Ok(NativeType::String { borrowed: true });
                }
                if self.opaque.contains(&name) && p.path.get_ident().is_some() {
                    return Ok(NativeType::Opaque(name));
                }
            }
            if let syn::Type::Slice(s) = &*r.elem
                && matches!(self.ty(&s.elem, input)?, NativeType::Value(Scalar::U8))
            {
                return Ok(NativeType::Bytes { borrowed: true });
            }
            return Err(fail());
        }
        let syn::Type::Path(p) = ty else {
            return Err(fail());
        };
        let n = self.type_name(p)?;
        let seg = p.path.segments.last().unwrap();
        if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
            return self.generic_ty(&n, args, ty.span(), input);
        }
        if !matches!(seg.arguments, syn::PathArguments::None) {
            return Err(fail());
        }
        let scalar = match n.as_str() {
            "i8" => Scalar::I8,
            "i16" => Scalar::I16,
            "i32" => Scalar::I32,
            "i64" => Scalar::I64,
            "u8" => Scalar::U8,
            "u16" => Scalar::U16,
            "u32" => Scalar::U32,
            "u64" => Scalar::U64,
            "char" => Scalar::Char,
            "f64" => Scalar::F64,
            "bool" => Scalar::Bool,
            "String" => return Ok(NativeType::String { borrowed: false }),
            _ if self.opaque.contains(&n) => return Ok(NativeType::OwnedOpaque(n)),
            _ if self.structs.contains_key(&n) && !self.opaque.contains(&n) => {
                return Ok(NativeType::Record(n));
            }
            _ => return Err(fail()),
        };
        Ok(NativeType::Value(scalar))
    }
    fn type_name(&self, ty: &syn::TypePath) -> Result<String, Diagnostic> {
        let fail = || self.error(ty.span(), "unsupported Rust bridge type path");
        if ty.qself.is_some() {
            return Err(fail());
        }
        let path = &ty.path;
        if path.leading_colon.is_none() && path.segments.len() == 1 {
            let name = ident(&path.segments[0].ident);
            if builtin_type_name(&name) {
                self.check_type_binding(&name, path.span())?;
            }
            return Ok(name);
        }
        let name = standard_path_name(path).ok_or_else(fail)?;
        if path.leading_colon.is_none() {
            self.check_type_binding(&ident(&path.segments[0].ident), path.span())?;
        }
        Ok(name)
    }

    fn check_type_binding(&self, name: &str, span: RustSpan) -> Result<(), Diagnostic> {
        let wildcard = (!self.bindings.canonical.contains(name))
            .then_some(self.bindings.wildcard.as_ref())
            .flatten();
        if let Some(binding) = self.bindings.shadows.get(name).or(wildcard) {
            return Err(self.error(span,
                format!("cannot derive builtin `{name}` through a local type binding or wildcard import; use an absolute canonical standard type path"))
                .with_label(Label::secondary(self.span(*binding), "type binding declared here")));
        }
        Ok(())
    }
    fn generic_ty(
        &self,
        name: &str,
        args: &syn::AngleBracketedGenericArguments,
        span: RustSpan,
        input: bool,
    ) -> Result<NativeType, Diagnostic> {
        let fail = || self.error(span, "unsupported Rust bridge type or type arguments");
        let args = args
            .args
            .iter()
            .map(|a| match a {
                syn::GenericArgument::Type(t) => Ok(t),
                _ => Err(fail()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        match (name, args.as_slice()) {
            ("Box", [syn::Type::Path(p)])
                if !input
                    && p.path
                        .get_ident()
                        .is_some_and(|n| self.opaque.contains(&ident(n))) =>
            {
                Ok(NativeType::Opaque(ident(p.path.get_ident().unwrap())))
            }
            ("Vec", [t]) => {
                let elem = self.ty(t, input)?;
                if matches!(elem, NativeType::Value(Scalar::U8)) {
                    return Ok(NativeType::Bytes { borrowed: false });
                }
                if !matches!(elem, NativeType::Value(ref s) if *s!=Scalar::Void)
                    && !matches!(elem, NativeType::String { .. })
                {
                    return Err(fail());
                }
                Ok(NativeType::Array(Box::new(elem)))
            }
            ("Option", [t]) => Ok(NativeType::Option(Box::new(self.ty(t, input)?))),
            ("Result", [a, b]) => Ok(NativeType::Result(
                Box::new(self.ty(a, input)?),
                Box::new(self.ty(b, input)?),
            )),
            _ => Err(fail()),
        }
    }
    fn function(&self, f: &syn::ItemFn, path: &Path) -> Result<FunctionDecl, Diagnostic> {
        self.generics(&f.sig.generics)?;
        let assumed = self.assumptions(&f.attrs, true)?;
        if matches!(f.sig.safety, syn::Safety::Unsafe(_))
            || f.sig.abi.is_some()
            || f.sig.variadic.is_some()
        {
            return Err(self.error(
                f.sig.span(),
                "unsafe, extern and variadic functions are unsupported",
            ));
        }
        let name = self.export_name(&f.sig.ident)?;
        let span = self.span(f.sig.span());
        let mut params = Vec::new();
        let mut types = Vec::new();
        for (i, arg) in f.sig.inputs.iter().enumerate() {
            let syn::FnArg::Typed(p) = arg else {
                return Err(self.error(arg.span(), "expected free function argument"));
            };
            self.attributes(&p.attrs)?;
            let ty = self.ty(&p.ty, true)?;
            if !matches!(ty, NativeType::OwnedOpaque(_)) {
                let mut pending = vec![&ty];
                while let Some(t) = pending.pop() {
                    match t {
                        NativeType::OwnedOpaque(_) => return Err(self.error(p.ty.span(), "owned handles in container arguments are unsupported; pass the handle directly")),
                        NativeType::Option(t) | NativeType::Array(t) => pending.push(t),
                        NativeType::Result(a, b) => { pending.push(a); pending.push(b); }
                        _ => {}
                    }
                }
            }

            if matches!(ty, NativeType::Value(Scalar::Void)) {
                return Err(self.error(p.ty.span(), "unit arguments are unsupported"));
            }
            params.push(Param {
                name: format!("a{i}"),
                ty: ty.willow(),
                mode: ParamMode::Value,
                sending: false,
                span: self.span(p.span()),
                type_span: self.span(p.ty.span()),
            });
            types.push(ty);
        }
        let output = match &f.sig.output {
            syn::ReturnType::Default => NativeType::Value(Scalar::Void),
            syn::ReturnType::Type(_, t) => self.ty(t, false)?,
        };
        let mut symbol = RustBridgeSymbol::new(
            name.clone(),
            types.iter().map(|t| t.scalar(&self.module)).collect(),
            output.scalar(&self.module),
        );
        if let Some((no_block, no_io, span)) = assumed {
            if no_block && f.sig.asyncness.is_some() {
                return Err(self.error(f.sig.span(), "async Rust bridge cannot declare no_block"));
            }
            // Native claims are bound to the single project review at validation.
            if no_block {
                symbol.declare_nonblocking(String::new());
            }
            symbol.no_io = no_io;
            symbol.assumption_span = Some(span);
        }
        symbol.consume_inputs = types
            .iter()
            .enumerate()
            .filter_map(|(i, ty)| matches!(ty, NativeType::OwnedOpaque(_)).then_some(i))
            .collect();
        symbol.is_async = f.sig.asyncness.is_some();
        if symbol.is_async {
            symbol.effects |= willow_abi::RuntimeEffects::MAY_SUSPEND.bits();
        }
        let (target, adapter) = self.function_adapter(&name, &symbol, &types, &output);
        symbol.native = Some(NativeBridge {
            module: self.module.clone(),
            source_path: path.to_owned(),
            source: None,
            adapter,
            target,
            definitions: vec![],
        });
        let mut modifiers = Modifiers::PUBLIC;
        modifiers.set(Modifiers::ASYNC, symbol.is_async);
        Ok(FunctionDecl {
            requires: vec![],
            ensures: vec![],
            effect_contracts: vec![],
            type_params: vec![],
            rust_bridge: Some(Box::new(symbol)),
            name,
            modifiers,
            params,
            return_type: output.willow(),
            return_type_span: match &f.sig.output {
                syn::ReturnType::Type(_, t) => Some(Box::new(self.span(t.span()))),
                _ => None,
            },
            body: Block {
                id: BodyId::fresh(),
                stmts: vec![],
                span,
            },
            span,
            constant: None,
        })
    }
    fn method(
        &self,
        owner: &syn::Ident,
        method: &syn::ImplItemFn,
        path: &Path,
    ) -> Result<FunctionDecl, Diagnostic> {
        let owner_name = ident(owner);
        let mut signature = method.sig.clone();
        syn::visit_mut::VisitMut::visit_signature_mut(&mut MethodSelf(owner), &mut signature);
        let method_name = self.export_name(&signature.ident)?;
        signature.ident = syn::Ident::new(
            &format!("{owner_name}_{method_name}"),
            signature.ident.span(),
        );
        for arg in &mut signature.inputs {
            if let syn::FnArg::Receiver(receiver) = arg {
                self.attributes(&receiver.attrs)?;
                if !self.opaque.contains(&owner_name)
                    || (receiver.mutability.is_some()
                        && !matches!(receiver.kind, syn::ReceiverKind::Value))
                {
                    return Err(self.error(
                        receiver.span(),
                        "&mut self is unsupported: opaque handles may alias; use an internal Mutex",
                    ));
                }
                *arg = match receiver.kind {
                    syn::ReceiverKind::Reference(_, None, None) => {
                        syn::parse_str(&format!("receiver: &{owner}")).unwrap()
                    }
                    syn::ReceiverKind::Value => {
                        syn::parse_str(&format!("receiver: {owner}")).unwrap()
                    }
                    _ => {
                        return Err(self.error(
                            receiver.span(),
                            "unsupported receiver; expected &self or self",
                        ));
                    }
                };
            }
        }
        let function = syn::ItemFn {
            modifiers: method.modifiers.clone(),
            attrs: method.attrs.clone(),
            vis: method.vis.clone(),
            sig: signature,
            block: Box::new(syn::parse_str("{}").unwrap()),
        };
        let mut derived = self.function(&function, path)?;
        let symbol = derived.rust_bridge.as_mut().unwrap();
        symbol.native_owner = Some(owner_name.clone());
        symbol.native_method = Some(method_name.clone());
        symbol.native_receiver = method.sig.receiver().is_some();
        let native = derived
            .rust_bridge
            .as_mut()
            .unwrap()
            .native
            .as_mut()
            .unwrap();
        native.adapter = native.adapter.replace(
            &format!("crate::{}::r#{}(", self.module, derived.name),
            &format!("crate::{}::r#{owner_name}::r#{method_name}(", self.module),
        );
        derived.span = self.span(method.sig.span());
        Ok(derived)
    }
    fn function_adapter(
        &self,
        name: &str,
        symbol: &RustBridgeSymbol,
        types: &[NativeType],
        output: &NativeType,
    ) -> (String, String) {
        let consumed: std::collections::HashSet<_> =
            symbol.consume_inputs.iter().copied().collect();
        let target = format!("{}_{name}", self.module);
        let mut adapter = format!(
            "pub {}fn {target}(",
            if symbol.is_async { "async " } else { "" }
        );
        let owned_conversions: Vec<_> = types
            .iter()
            .map(|ty| symbol.is_async && ty.needs_owned_conversion())
            .collect();
        for (i, ty) in symbol.input_types.iter().enumerate() {
            let mutable = if owned_conversions[i] { "mut " } else { "" };
            let rust_type = if consumed.contains(&i) {
                ty.rust_type(false)
            } else if symbol.is_async {
                ty.owned_rust_type()
            } else {
                ty.rust_type(true)
            };
            write!(adapter, "{mutable}a{i}: {rust_type},").unwrap();
        }
        write!(
            adapter,
            ") -> {} {{ let result = crate::{}::r#{name}(",
            symbol.output_type.rust_type(false),
            self.module
        )
        .unwrap();
        for (i, ty) in types.iter().enumerate() {
            if owned_conversions[i] {
                ty.append_owned_conversion(&mut adapter, &format!("&mut a{i}"), &self.module);
            } else if consumed.contains(&i) {
                write!(adapter, "*a{i}").unwrap();
            } else if symbol.is_async {
                write!(adapter, "a{i}").unwrap();
            } else {
                adapter.push_str(&ty.convert(&format!("a{i}"), &self.module, true));
            }
            adapter.push(',');
        }
        writeln!(
            adapter,
            "){}; {} }}",
            if symbol.is_async { ".await" } else { "" },
            output.convert("result", &self.module, false)
        )
        .unwrap();
        (target, adapter)
    }
}
fn ident(id: &syn::Ident) -> String {
    id.to_string().trim_start_matches("r#").to_owned()
}

fn primitive_type_name(name: &str) -> bool {
    matches!(
        name,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "char"
            | "f64"
            | "bool"
            | "str"
    )
}
fn builtin_type_name(name: &str) -> bool {
    primitive_type_name(name) || matches!(name, "String" | "Vec" | "Box" | "Option" | "Result")
}
fn standard_type_name<'a>(parts: &[&'a str]) -> Option<&'a str> {
    let [origin, module, name] = parts else {
        return None;
    };
    let accepted = match (*origin, *module, *name) {
        ("std" | "alloc", "string", "String")
        | ("std" | "alloc", "vec", "Vec")
        | ("std" | "alloc", "boxed", "Box")
        | ("std" | "core", "option", "Option")
        | ("std" | "core", "result", "Result") => true,
        ("std" | "core", "primitive", name) => primitive_type_name(name),
        _ => false,
    };
    accepted.then_some(*name)
}
fn standard_path_name(path: &syn::Path) -> Option<String> {
    if path.segments.len() != 3
        || path
            .segments
            .iter()
            .take(2)
            .any(|s| !matches!(s.arguments, syn::PathArguments::None))
    {
        return None;
    }
    let parts: Vec<_> = path.segments.iter().map(|s| ident(&s.ident)).collect();
    standard_type_name(&[&parts[0], &parts[1], &parts[2]]).map(str::to_owned)
}

#[derive(Default)]
struct TypeBindings {
    shadows: HashMap<String, RustSpan>,
    wildcard: Option<RustSpan>,
    canonical: HashSet<String>,
}
struct TypeImport {
    name: String,
    canonical_root: Option<String>,
    absolute: bool,
    span: RustSpan,
}
impl TypeBindings {
    fn collect(syntax: &syn::File) -> Self {
        let mut result = Self::default();
        let mut imports = Vec::new();
        for item in &syntax.items {
            let declaration = match item {
                syn::Item::Struct(item) => Some(&item.ident),
                syn::Item::Enum(item) => Some(&item.ident),
                syn::Item::Union(item) => Some(&item.ident),
                syn::Item::Type(item) => Some(&item.ident),
                syn::Item::Trait(item) => Some(&item.ident),
                syn::Item::TraitAlias(item) => Some(&item.ident),
                syn::Item::Mod(item) => Some(&item.ident),
                syn::Item::ExternCrate(item) => {
                    let name = item.rename.as_ref().map_or(&item.ident, |(_, name)| name);
                    if name == &item.ident
                        && matches!(ident(name).as_str(), "std" | "core" | "alloc")
                    {
                        None
                    } else {
                        Some(name)
                    }
                }
                syn::Item::Use(item) => {
                    result.collect_imports(
                        &item.tree,
                        item.leading_colon.is_some(),
                        &mut Vec::new(),
                        &mut imports,
                    );
                    None
                }
                _ => None,
            };
            if let Some(name) = declaration {
                result.shadows.insert(ident(name), name.span());
            }
        }
        let bound_names: HashSet<_> = result
            .shadows
            .keys()
            .cloned()
            .chain(imports.iter().map(|import| import.name.clone()))
            .collect();
        for import in imports {
            let root_is_known = import.canonical_root.as_ref().is_some_and(|root| {
                import.absolute || (result.wildcard.is_none() && !bound_names.contains(root))
            });
            if root_is_known {
                result.canonical.insert(import.name);
            } else {
                result.shadows.insert(import.name, import.span);
            }
        }
        result
    }

    fn collect_imports(
        &mut self,
        tree: &syn::UseTree,
        absolute: bool,
        path: &mut Vec<String>,
        imports: &mut Vec<TypeImport>,
    ) {
        match tree {
            syn::UseTree::Path(item) => {
                path.push(ident(&item.ident));
                self.collect_imports(&item.tree, absolute, path, imports);
                path.pop();
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    self.collect_imports(item, absolute, path, imports);
                }
            }
            syn::UseTree::Glob(glob) => {
                self.wildcard.get_or_insert(glob.span());
            }
            syn::UseTree::Name(item) => {
                let original = ident(&item.ident);
                let name = if item.ident == "self" {
                    path.last().cloned().unwrap_or_else(|| "self".into())
                } else {
                    ident(&item.ident)
                };
                imports.push(TypeImport {
                    canonical_root: canonical_import_root(path, &original, &name),
                    name,
                    absolute,
                    span: item.span(),
                });
            }
            syn::UseTree::Rename(item) => {
                if item.rename == "_" {
                    return;
                }
                let name = ident(&item.rename);
                imports.push(TypeImport {
                    canonical_root: canonical_import_root(path, &ident(&item.ident), &name),
                    name,
                    absolute,
                    span: item.span(),
                });
            }
        }
    }
}

/// Only a three-segment canonical import needs its prefix. Do not clone an
/// arbitrary shared use-tree prefix for every leaf of a grouped import.
fn canonical_import_root(path: &[String], original: &str, name: &str) -> Option<String> {
    let [origin, module] = path else {
        return None;
    };
    (standard_type_name(&[origin, module, original]) == Some(name)).then(|| origin.clone())
}

struct MethodSelf<'a>(&'a syn::Ident);
impl syn::visit_mut::VisitMut for MethodSelf<'_> {
    fn visit_type_path_mut(&mut self, path: &mut syn::TypePath) {
        if path.qself.is_none() && path.path.is_ident("Self") {
            path.path.segments[0].ident = self.0.clone();
        } else {
            syn::visit_mut::visit_type_path_mut(self, path);
        }
    }
}

struct HandleUses<'a>(&'a mut HashSet<String>);
impl<'ast> Visit<'ast> for HandleUses<'_> {
    fn visit_type_reference(&mut self, r: &'ast syn::TypeReference) {
        if let syn::Type::Path(p) = &*r.elem
            && let Some(n) = p.path.get_ident()
        {
            self.0.insert(ident(n));
        }
        syn::visit::visit_type_reference(self, r);
    }
    fn visit_type_path(&mut self, p: &'ast syn::TypePath) {
        if p.path.is_ident("Self") {
            self.0.insert("Self".into());
        }
        if (p
            .path
            .segments
            .first()
            .is_some_and(|s| p.path.segments.len() == 1 && s.ident == "Box")
            || standard_path_name(&p.path).as_deref() == Some("Box"))
            && let syn::PathArguments::AngleBracketed(a) =
                &p.path.segments.last().unwrap().arguments
            && let Some(syn::GenericArgument::Type(syn::Type::Path(p))) = a.args.first()
            && let Some(n) = p.path.get_ident()
        {
            self.0.insert(ident(n));
        }
        syn::visit::visit_type_path(self, p);
    }
}

/// Called at the same content-fingerprinted syntax boundary as Willow parsing.
/// Function bodies are parsed by syn, but never compiled or type-checked here.
pub(crate) fn parse(path: &Path, source: &str, file: FileId) -> (Program, Vec<Diagnostic>) {
    let mut program = Program {
        instantiation_types: vec![],
        type_uses: vec![],
        module: None,
        imports: vec![],
        items: vec![],
    };
    let mut d = Deriver {
        source,
        file,
        module: format!(
            "native_{:x}",
            crate::hex::Hex(&Sha256::digest(path.to_string_lossy().as_bytes()))
        ),
        structs: HashMap::new(),
        opaque: HashSet::new(),
        bindings: TypeBindings::default(),
    };
    let syntax = match syn::parse_file(source) {
        Ok(s) => s,
        Err(e) => return (program, vec![d.error(e.span(), e.to_string())]),
    };
    if let Err(e) = d.attributes(&syntax.attrs) {
        return (program, vec![e]);
    }
    d.bindings = TypeBindings::collect(&syntax);
    for item in &syntax.items {
        if let syn::Item::Struct(s) = item
            && matches!(s.vis, syn::Visibility::Public(_))
        {
            d.structs.insert(ident(&s.ident), s);
            for field in &s.fields {
                if matches!(field.vis, syn::Visibility::Public(_)) {
                    HandleUses(&mut d.opaque).visit_type(&field.ty);
                }
            }
        }
        if let syn::Item::Fn(f) = item
            && matches!(f.vis, syn::Visibility::Public(_))
        {
            HandleUses(&mut d.opaque).visit_signature(&f.sig);
        }
    }
    for item in &syntax.items {
        if let syn::Item::Impl(i) = item
            && let syn::Type::Path(p) = &*i.self_ty
            && let Some(owner) = p.path.get_ident()
        {
            for item in &i.items {
                if let syn::ImplItem::Fn(m) = item
                    && matches!(m.vis, syn::Visibility::Public(_))
                {
                    if m.sig.receiver().is_some()
                        || matches!(&m.sig.output, syn::ReturnType::Type(_, t) if matches!(&**t, syn::Type::Path(p) if p.path.is_ident("Self")))
                    {
                        d.opaque.insert(ident(owner));
                    }
                    HandleUses(&mut d.opaque).visit_signature(&m.sig);
                    if d.opaque.remove("Self") {
                        d.opaque.insert(ident(owner));
                    }
                }
            }
        }
    }
    d.opaque.retain(|n| d.structs.contains_key(n));
    let result = derive_items(&d, &syntax, path, &mut program);
    match result {
        Ok(()) => (program, vec![]),
        Err(e) => (program, vec![e]),
    }
}

fn derive_items(
    d: &Deriver<'_>,
    syntax: &syn::File,
    path: &Path,
    program: &mut Program,
) -> Result<(), Diagnostic> {
    let mut records = RecordInterfaces::default();
    for item in &syntax.items {
        if let syn::Item::Struct(s) = item
            && matches!(s.vis, syn::Visibility::Public(_))
        {
            records.add_struct(d, s, path, program)?;
        }
    }
    check_record_cycles(d, &records.records)?;
    for item in &syntax.items {
        if let syn::Item::Fn(f) = item
            && matches!(f.vis, syn::Visibility::Public(_))
        {
            program.items.push(Item::Function(d.function(f, path)?));
        }
    }
    for item in &syntax.items {
        if let syn::Item::Impl(implementation) = item
            && implementation.items.iter().any(|member| matches!(member, syn::ImplItem::Fn(m) if matches!(m.vis, syn::Visibility::Public(_))))
        {
            let syn::Type::Path(owner) = &*implementation.self_ty else { continue; };
            let Some(owner) = owner.path.get_ident().filter(|name| d.structs.contains_key(&ident(name))) else { continue; };
            d.attributes(&implementation.attrs)?;
            d.generics(&implementation.generics)?;
            if implementation.trait_.is_some() {
                return Err(d.error(implementation.span(), "only inherent public methods are supported"));
            }
            for member in &implementation.items {
                if let syn::ImplItem::Fn(method) = member && matches!(method.vis, syn::Visibility::Public(_)) {
                    program.items.push(Item::Function(d.method(owner, method, path)?));
                }
            }
        }
    }
    resolve_native_closers(program);
    let mut names = HashSet::new();
    for item in &program.items {
        if let Item::Function(f) = item
            && !names.insert(&f.name)
        {
            return Err(d.error(
                syntax.span(),
                format!("duplicate exported function `{}`", f.name),
            ));
        }
    }
    records.finish(d, program)
}

/// A consuming, argument-free synchronous `close(self)` returning unit is
/// also the resource cleanup operation. Other Rust close signatures keep a
/// distinct internal target, preserving the generated explicit Drop function.
fn resolve_native_closers(program: &mut Program) {
    let mut closers = HashMap::new();
    let mut disposal_names = HashSet::new();
    for item in &mut program.items {
        let Item::Function(function) = item else {
            continue;
        };
        let Some(bridge) = function.rust_bridge.as_mut() else {
            continue;
        };
        let method = bridge.native_method.as_deref();
        if bridge.native_receiver
            && method.is_some_and(|m| m == "dispose" || m.starts_with("dispose_"))
        {
            disposal_names.insert(function.name.clone());
        }
        if bridge.close_handle || method != Some("close") {
            continue;
        }
        let scope_cleanup = bridge.native_receiver
            && !bridge.is_async
            && function.params.len() == 1
            && function.return_type == Type::Void
            && bridge.consume_inputs == [0];
        if scope_cleanup {
            closers.insert(function.name.clone(), Some(function.clone()));
        } else {
            if bridge.native_receiver {
                closers.insert(function.name.clone(), None);
            }
            // Native member lookup uses metadata, so this unspellable suffix
            // needs no reserved user-visible name and cannot collide with one.
            function.name.push_str("$method");
            bridge.willow_function = function.name.clone();
            bridge.abi_symbol = function.name.clone();
        }
    }
    program.items.retain_mut(|item| {
        let Item::Function(function) = item else {
            return true;
        };
        let Some(bridge) = function.rust_bridge.as_mut().filter(|b| b.close_handle) else {
            return true;
        };
        let Some(owner) = bridge.native_owner.as_ref() else {
            return true;
        };
        let Some(closer) = closers.get(&format!("{owner}_close")) else {
            return true;
        };
        let Some(closer) = closer else {
            if !bridge.auto_close {
                // A native instance close owns that spelling. Give its Drop
                // operation a free instance name without adding an ABI wrapper.
                let mut alias = String::from("dispose");
                let mut suffix = 2;
                while disposal_names.contains(&format!("{owner}_{alias}")) {
                    alias = format!("dispose_{suffix}");
                    suffix += 1;
                }
                bridge.native_method = Some(alias);
            }
            return true;
        };
        if !bridge.auto_close {
            return false;
        }
        let name = function.name.clone();
        *function = closer.clone();
        function.name = name;
        function.body.id = BodyId::fresh();
        let bridge = function.rust_bridge.as_mut().unwrap();
        bridge.willow_function = function.name.clone();
        bridge.abi_symbol = function.name.clone();
        bridge.auto_close = true;
        bridge.native_method = Some("$use_close".into());
        // Both wrappers call the one adapter emitted by the public method.
        bridge.native.as_mut().unwrap().adapter.clear();
        true
    });
}

#[derive(Default)]
struct RecordInterfaces {
    records: HashMap<String, Record>,
    helpers: String,
    input_helpers: HashMap<String, String>,
    owned_input_helpers: HashMap<String, String>,
}
impl RecordInterfaces {
    fn add_struct(
        &mut self,
        d: &Deriver<'_>,
        s: &syn::ItemStruct,
        path: &Path,
        program: &mut Program,
    ) -> Result<(), Diagnostic> {
        d.generics(&s.generics)?;
        let name = ident(&s.ident);
        let assumed = d.assumptions(&s.attrs, d.opaque.contains(&name))?;
        if matches!(
            name.as_str(),
            "String"
                | "Vec"
                | "Box"
                | "Array"
                | "Option"
                | "Result"
                | "i8"
                | "i16"
                | "i32"
                | "i64"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "f64"
                | "bool"
                | "char"
                | "str"
                | "void"
        ) || crate::semantic::builtin_types::BuiltinTypeId::from_name(&name).is_some()
        {
            return Err(d.error(
                s.ident.span(),
                "public struct name conflicts with a bridge builtin type",
            ));
        }
        d.export_name(&s.ident)?;
        let span = d.span(s.span());
        if d.opaque.contains(&name) {
            self.add_opaque(d, name, span, path, program, assumed);
            Ok(())
        } else {
            self.add_record(d, s, name, span, program)
        }
    }

    fn add_opaque(
        &mut self,
        d: &Deriver<'_>,
        name: String,
        span: Span,
        path: &Path,
        program: &mut Program,
        assumed: Option<(bool, bool, Span)>,
    ) {
        program.items.push(Item::Enum(EnumDecl {
            name: name.clone(),
            public: true,
            type_params: vec![],
            variants: vec![EnumVariant {
                name: "$rust_handle".into(),
                payload: vec![Type::I64],
                span,
            }],
            span,
        }));
        writeln!(
            &mut self.helpers,
            "pub use crate::{}::r#{name} as {}_{name};",
            d.module, d.module
        )
        .unwrap();
        let mut symbol = RustBridgeSymbol::new(
            format!("{name}_close"),
            vec![Scalar::Opaque(format!("{}_{name}", d.module))],
            Scalar::Void,
        );
        symbol.close_handle = true;
        symbol.native_owner = Some(name.clone());
        symbol.native_method = Some("close".into());
        symbol.native_receiver = true;
        if let Some((no_block, no_io, span)) = assumed {
            if no_block {
                symbol.declare_nonblocking(String::new());
            }
            symbol.no_io = no_io;
            symbol.assumption_span = Some(span);
        }
        symbol.native = Some(NativeBridge {
            module: d.module.clone(),
            source_path: path.to_owned(),
            source: None,
            adapter: String::new(),
            target: String::new(),
            definitions: vec![],
        });
        program.items.push(Item::Function(FunctionDecl {
            requires: vec![],
            ensures: vec![],
            effect_contracts: vec![],
            type_params: vec![],
            name: format!("{name}_close"),
            modifiers: Modifiers::PUBLIC,
            params: vec![Param {
                name: "handle".into(),
                ty: Type::Named(name),
                mode: ParamMode::Value,
                sending: false,
                span,
                type_span: span,
            }],
            return_type: Type::Void,
            return_type_span: None,
            body: Block {
                id: BodyId::fresh(),
                stmts: vec![],
                span,
            },
            span,
            constant: None,
            rust_bridge: Some(Box::new(symbol)),
        }));
        let Item::Function(mut cleanup) = program.items.last().unwrap().clone() else {
            unreachable!()
        };
        cleanup.name.push_str("$scope");
        cleanup.body.id = BodyId::fresh();
        let bridge = cleanup.rust_bridge.as_mut().unwrap();
        bridge.willow_function = cleanup.name.clone();
        bridge.abi_symbol = cleanup.name.clone();
        bridge.auto_close = true;
        bridge.native_method = Some("$use_close".into());
        program.items.push(Item::Function(cleanup));
    }

    fn add_record(
        &mut self,
        d: &Deriver<'_>,
        s: &syn::ItemStruct,
        name: String,
        span: Span,
        program: &mut Program,
    ) -> Result<(), Diagnostic> {
        if !matches!(s.fields, syn::Fields::Named(_) | syn::Fields::Unit) {
            return Err(d.error(s.fields.span(), "records require named public fields"));
        }
        let mut fields = Vec::new();
        let mut bridge_fields = Vec::new();
        let mut input_fields = String::new();
        let mut output_fields = String::new();
        let mut owned_input_fields = String::new();
        for f in &s.fields {
            d.attributes(&f.attrs)?;
            if !matches!(f.vis, syn::Visibility::Public(_)) {
                return Err(d.error(
                    f.span(),
                    "value records require all fields public; use Box<T>/&T for opaque handles",
                ));
            }
            let n = d.export_name(f.ident.as_ref().unwrap())?;
            let ty = d.ty(&f.ty, false)?;
            if matches!(ty, NativeType::Value(Scalar::Void)) {
                return Err(d.error(f.ty.span(), "record fields require supported value types"));
            }
            fields.push(FieldDecl {
                name: n.clone(),
                ty: ty.willow(),
                modifiers: Modifiers::PUBLIC,
                initializer: None,
                span: d.span(f.span()),
            });
            bridge_fields.push((n.clone(), ty.scalar(&d.module)));
            write!(owned_input_fields, "r#{n}: ").unwrap();
            ty.append_owned_conversion(
                &mut owned_input_fields,
                &format!("&mut v.r#{n}"),
                &d.module,
            );
            owned_input_fields.push(',');
            write!(
                input_fields,
                "r#{n}: {},",
                ty.convert(&format!("v.r#{n}"), &d.module, true)
            )
            .unwrap();
            write!(
                output_fields,
                "r#{n}: {},",
                ty.convert(&format!("v.r#{n}"), &d.module, false)
            )
            .unwrap();
        }
        let unique = format!("{}_{name}", d.module);
        self.records.insert(
            unique.clone(),
            Record {
                name: unique.clone(),
                fields: bridge_fields,
            },
        );
        let native_construct = if matches!(s.fields, syn::Fields::Unit) {
            format!("crate::{}::r#{name}", d.module)
        } else {
            format!("crate::{}::r#{name} {{ {input_fields} }}", d.module)
        };
        let owned_construct = if matches!(s.fields, syn::Fields::Unit) {
            format!("crate::{}::r#{name}", d.module)
        } else {
            format!("crate::{}::r#{name} {{ {owned_input_fields} }}", d.module)
        };
        self.owned_input_helpers.insert(unique.clone(), format!("fn to_native_owned_{unique}(v: &mut crate::record_owned::{unique}) -> crate::{}::r#{name} {{ {owned_construct} }}\n",d.module));
        self.input_helpers.insert(unique.clone(), format!("fn to_native_{unique}(v: crate::record_inputs::{unique}<'_>) -> crate::{}::r#{name} {{ {native_construct} }}\n",d.module));
        writeln!(&mut self.helpers,"fn from_native_{unique}(v: crate::{}::r#{name}) -> crate::record_outputs::{unique} {{ crate::records::{unique} {{ {output_fields} }} }}",d.module).unwrap();
        program.items.push(Item::Class(ClassDecl {
            abi_storage: None,
            type_params: vec![],
            name,
            modifiers: Modifiers::PUBLIC,
            base_class: None,
            implements: vec![],
            source_implements_len: 0,
            fields,
            methods: vec![],
            constructors: vec![],
            span,
        }));
        Ok(())
    }

    fn input_record_names(
        &self,
        d: &Deriver<'_>,
        program: &Program,
        asynchronous: bool,
    ) -> Result<HashSet<String>, Diagnostic> {
        let mut input_records = HashSet::new();
        let mut pending: Vec<_> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Function(f) => f
                    .rust_bridge
                    .as_ref()
                    .filter(|symbol| symbol.is_async == asynchronous),
                _ => None,
            })
            .flat_map(|s| s.input_types.iter())
            .collect();
        while let Some(ty) = pending.pop() {
            match ty {
                Scalar::Record(name) if input_records.insert(name.clone()) => {
                    let record = &self.records[name];
                    let mut fields: Vec<_> = record.fields.iter().map(|(_, t)| t).collect();
                    while let Some(field) = fields.pop() {
                        match field {
                            Scalar::Opaque(_) => {
                                let name = name.strip_prefix(&format!("{}_", d.module)).unwrap();
                                return Err(d.error(d.structs[name].span(), "record inputs cannot consume owned opaque handles; pass &T as a function argument"));
                            }
                            Scalar::Record(_) => pending.push(field),
                            Scalar::Option(t) | Scalar::Array(t) => fields.push(t),
                            Scalar::Result(a, b) => {
                                fields.push(a);
                                fields.push(b);
                            }
                            _ => {}
                        }
                    }
                }
                Scalar::Option(t) | Scalar::Array(t) => pending.push(t),
                Scalar::Result(a, b) => {
                    pending.push(a);
                    pending.push(b);
                }
                _ => {}
            }
        }
        Ok(input_records)
    }

    fn finish(mut self, d: &Deriver<'_>, program: &mut Program) -> Result<(), Diagnostic> {
        let input_records = self.input_record_names(d, program, false)?;
        let owned_input_records = self.input_record_names(d, program, true)?;
        // Source declaration order makes generated text deterministic without a sort.
        for item in &program.items {
            if let Item::Class(c) = item {
                let name = format!("{}_{}", d.module, c.name);
                if input_records.contains(&name) {
                    self.helpers.push_str(&self.input_helpers[&name]);
                }
                if owned_input_records.contains(&name) {
                    self.helpers.push_str(&self.owned_input_helpers[&name]);
                }
            }
        }
        let definitions = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(c) => self
                    .records
                    .get(&format!("{}_{}", d.module, c.name))
                    .cloned(),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut definitions = Some(definitions);
        let mut record_groups: HashMap<Vec<String>, crate::rust_bridge_ir::RecordDefinitions> =
            HashMap::new();
        let mut first = true;
        for item in &mut program.items {
            if let Item::Function(f) = item
                && let Some(symbol) = &mut f.rust_bridge
            {
                let roots = record_roots(symbol);
                if let Some(shared) = record_groups.get(&roots) {
                    symbol.records = shared.clone();
                } else {
                    symbol.resolve_records(&self.records);
                    record_groups.insert(roots, symbol.records.clone());
                }
                if first {
                    // Shared record definitions and conversions are emitted once per module.
                    let native = symbol.native.as_mut().unwrap();
                    native.definitions = definitions.take().unwrap();
                    native.source = Some(d.source.to_owned());
                    native.adapter.insert_str(0, &self.helpers);
                    first = false;
                }
            }
        }
        Ok(())
    }
}

fn record_roots(symbol: &RustBridgeSymbol) -> Vec<String> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    let mut pending: Vec<_> = symbol
        .input_types
        .iter()
        .chain([&symbol.output_type])
        .collect();
    while let Some(ty) = pending.pop() {
        match ty {
            Scalar::Record(name) if seen.insert(name) => roots.push(name.clone()),
            Scalar::Option(t) | Scalar::Array(t) => pending.push(t),
            Scalar::Result(a, b) => {
                pending.push(a);
                pending.push(b);
            }
            _ => {}
        }
    }
    roots
}

fn check_record_cycles(
    d: &Deriver<'_>,
    records: &HashMap<String, Record>,
) -> Result<(), Diagnostic> {
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, record) in records {
        let mut pending: Vec<_> = record.fields.iter().map(|(_, t)| t).collect();
        let children = edges.entry(name).or_default();
        while let Some(t) = pending.pop() {
            match t {
                Scalar::Record(n) => children.push(n),
                Scalar::Option(t) | Scalar::Array(t) => pending.push(t),
                Scalar::Result(a, b) => {
                    pending.push(a);
                    pending.push(b);
                }
                _ => {}
            }
        }
    }
    let mut done = HashSet::new();
    let mut active = HashSet::new();
    for start in records.keys() {
        let mut stack = vec![(start.as_str(), false)];
        while let Some((n, exit)) = stack.pop() {
            if exit {
                active.remove(n);
                done.insert(n);
                continue;
            }
            if done.contains(n) {
                continue;
            }
            if !active.insert(n) {
                let raw = n.strip_prefix(&format!("{}_", d.module)).unwrap();
                return Err(d.error(
                    d.structs[raw].span(),
                    "recursive value records are unsupported",
                ));
            }
            stack.push((n, true));
            stack.extend(edges[n].iter().map(|child| (*child, false)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod shadow_tests;
