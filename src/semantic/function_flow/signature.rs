use crate::{parser::ast::Type, semantic::symbols::SymbolTable};

/// Full invariant signature. Capturing and noncapturing values share a key
/// because the language permits widening fn to closure. Async declarations
/// cannot be taken as values; a wrapper's Task/Future result remains in its key.
pub(crate) fn type_key(symbols: &SymbolTable, ty: &Type) -> Option<String> {
    fn append(symbols: &SymbolTable, ty: &Type, depth: usize, out: &mut String) -> Option<()> {
        if depth == 64 {
            return None;
        }
        match ty {
            Type::Fn(params, ret) | Type::Closure(params, ret) => {
                out.push('(');
                for p in params {
                    append(symbols, p, depth + 1, out)?;
                    out.push(',');
                }
                out.push_str(")->");
                append(symbols, ret, depth + 1, out)?;
            }
            Type::Generic(name, args) => {
                if name.contains("$mono$") {
                    return None;
                }
                let nominal = Type::Named(name.clone());
                if let Some(key) = symbols.type_key(&nominal) {
                    out.push_str(&key);
                } else if crate::semantic::builtin_types::BuiltinTypeId::from_name(name).is_some() {
                    out.push_str(name);
                } else {
                    return None;
                }
                out.push('<');
                for arg in args {
                    append(symbols, arg, depth + 1, out)?;
                    out.push(',');
                }
                out.push('>');
            }
            Type::Array(element) => {
                out.push('[');
                append(symbols, element, depth + 1, out)?;
                out.push(']');
            }
            Type::FixedArray(element, n) => {
                out.push_str(&format!("[{n};"));
                append(symbols, element, depth + 1, out)?;
                out.push(']');
            }
            Type::Void => out.push('v'),
            Type::Never => out.push('!'),
            Type::Named(name) if name.contains("$mono$") => return None,
            _ => out.push_str(&symbols.type_key(ty)?),
        }
        Some(())
    }
    let mut key = String::new();
    append(symbols, ty, 0, &mut key)?;
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn function_flow_signature_preserves_all_dimensions() {
        let symbols = SymbolTable::default();
        let scalar = Type::I64;
        let function = |params, result| Type::Fn(params, Box::new(result));
        let key = |ty| type_key(&symbols, &ty).unwrap();
        let base = key(function(vec![scalar.clone()], scalar.clone()));
        let mut seen = std::collections::HashSet::from([base.clone()]);
        for ty in [
            function(vec![Type::Bool], scalar.clone()),
            function(vec![scalar.clone()], Type::Bool),
            function(vec![], scalar.clone()),
            function(
                vec![scalar.clone()],
                Type::Generic("Task".into(), vec![scalar.clone()]),
            ),
            function(
                vec![scalar.clone()],
                Type::Generic("Future".into(), vec![scalar.clone()]),
            ),
            function(
                vec![Type::Generic("Channel".into(), vec![scalar.clone()])],
                scalar.clone(),
            ),
            function(
                vec![Type::Generic("Channel".into(), vec![Type::Bool])],
                scalar.clone(),
            ),
            function(
                vec![Type::Generic("BlockingCell".into(), vec![scalar.clone()])],
                scalar.clone(),
            ),
            function(
                vec![Type::Generic("BlockingCell".into(), vec![Type::Bool])],
                scalar.clone(),
            ),
        ] {
            assert!(seen.insert(key(ty)));
        }
        assert_eq!(
            base,
            key(Type::Closure(vec![scalar.clone()], Box::new(scalar)))
        );
    }
}
