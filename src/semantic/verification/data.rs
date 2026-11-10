//! Shared immutable data model for proof and contract comparison.
use super::*;
use crate::semantic::field_writes::FieldWrites;
use std::collections::BTreeMap;

#[derive(Clone)]
pub(super) struct Constructor {
    pub name: String,
    pub fields: Vec<(String, Type)>,
}
#[derive(Default)]
pub(super) struct DataTypes {
    pub constructors: HashMap<Type, Vec<Constructor>>,
    pub definitions: String,
    pub digest: String,
    pub fields: HashMap<Type, HashMap<String, (usize, Type)>>,
    pub variants: HashMap<Type, HashMap<String, usize>>,
    pub errors: HashMap<Type, String>,
    pub visits: usize,
}
pub(super) fn type_key(ty: &Type) -> String {
    digest(&serde_json::to_string(ty).expect("type serialization"))[7..].to_owned()
}
pub(super) fn frozen_element(ty: &Type) -> Option<&Type> {
    if let Type::Generic(name, args) = ty
        && name == "FrozenArray"
        && args.len() == 1
    {
        Some(&args[0])
    } else {
        None
    }
}
pub(super) fn sort(ty: &Type) -> Result<String, String> {
    match ty {
        Type::I64 => Ok("Int".into()),
        Type::Bool => Ok("Bool".into()),
        Type::Named(_) => Ok(format!("D{}", type_key(ty))),
        _ if frozen_element(ty).is_some() => Ok(format!("D{}", type_key(ty))),
        _ => Err("mutable or unsupported verification type".into()),
    }
}
pub(super) fn domain(ty: &Type, value: &str) -> String {
    match ty {
        Type::I64 => bounds(value),
        Type::Bool => "true".into(),
        _ => format!("(valid_{} {value})", type_key(ty)),
    }
}
pub(super) fn default_value(ty: &Type) -> String {
    match ty {
        Type::I64 => "0".into(),
        Type::Bool => "false".into(),
        _ => format!("default_{}", type_key(ty)),
    }
}
pub(super) fn constructor(ty: &Type, index: usize) -> String {
    format!("make_{}_{index}", type_key(ty))
}
pub(super) fn selector(ty: &Type, variant: usize, field: usize) -> String {
    format!("field_{}_{variant}_{field}", type_key(ty))
}
impl DataTypes {
    pub fn new(program: &Program, writes: &FieldWrites) -> Self {
        let declarations: HashMap<_, _> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(c) => Some((c.name.as_str(), item)),
                Item::Enum(e) => Some((e.name.as_str(), item)),
                _ => None,
            })
            .collect();
        let mut model = Self::default();
        let mut roots = BTreeMap::new();
        for item in &program.items {
            if let Item::Function(f) = item {
                for ty in f
                    .params
                    .iter()
                    .map(|p| &p.ty)
                    .chain(std::iter::once(&f.return_type))
                {
                    roots.insert(type_key(ty), ty);
                }
            }
        }
        for ty in roots.values() {
            let _ = model.register(ty, &declarations, writes, 0);
        }
        model.digest = digest(&model.definitions);
        model
    }
    fn register(
        &mut self,
        ty: &Type,
        declarations: &HashMap<&str, &Item>,
        writes: &FieldWrites,
        depth: usize,
    ) -> Result<(), String> {
        if matches!(ty, Type::I64 | Type::Bool) {
            return Ok(());
        }
        if let Some(error) = self.errors.get(ty) {
            return Err(error.clone());
        }
        if self.constructors.contains_key(ty) {
            return Ok(());
        }
        self.visits += 1;
        self.errors
            .insert(ty.clone(), "recursive or unsupported data type".into());
        let result: Result<(), String> = (|| {
            if depth > 128 {
                return Err("data type depth budget exceeded".into());
            }
            let name = sort(ty)?;
            let key = type_key(ty);
            if let Some(elem) = frozen_element(ty) {
                self.register(elem, declarations, writes, depth + 1)?;
                let elem_sort = sort(elem)?;
                self.definitions += &format!(
                    "(declare-datatypes () (({name} (make_{key} (len_{key} Int) (values_{key} (Array Int {elem_sort}))))))\n"
                );
                self.definitions += &format!(
                    "(define-fun valid_{key} ((x {name})) Bool (and (<= 0 (len_{key} x)) {} (forall ((i Int)) (=> (and (<= 0 i) (< i (len_{key} x))) {}))))\n",
                    bounds(&format!("(len_{key} x)")),
                    domain(elem, &format!("(select (values_{key} x) i)"))
                );
                self.constructors.insert(ty.clone(), Vec::new());
            } else {
                let Type::Named(n) = ty else {
                    return Err("unsupported data type".into());
                };
                let ctors = match declarations.get(n.as_str()) {
                    Some(Item::Class(c))
                        if !c.is_open
                            && c.base_class.is_none()
                            && c.type_params.is_empty()
                            && c.abi_storage.is_none() =>
                    {
                        if c.fields
                            .iter()
                            .any(|f| !f.is_static && writes.is_reassigned(&f.name, f.span))
                        {
                            return Err("record contains reassigned fields".into());
                        }
                        vec![Constructor {
                            name: c.name.clone(),
                            fields: c
                                .fields
                                .iter()
                                .filter(|f| !f.is_static)
                                .map(|f| (f.name.clone(), f.ty.clone()))
                                .collect(),
                        }]
                    }
                    Some(Item::Enum(e)) if e.type_params.is_empty() => e
                        .variants
                        .iter()
                        .map(|v| Constructor {
                            name: v.name.clone(),
                            fields: v
                                .payload
                                .iter()
                                .enumerate()
                                .map(|(i, t)| (i.to_string(), t.clone()))
                                .collect(),
                        })
                        .collect(),
                    _ => return Err("mutable, open or unavailable data declaration".into()),
                };
                if ctors.is_empty() {
                    return Err("empty data type".into());
                }
                for c in &ctors {
                    for (_, t) in &c.fields {
                        self.register(t, declarations, writes, depth + 1)?;
                    }
                }
                let mut forms = Vec::new();
                let mut valid = Vec::new();
                for (i, c) in ctors.iter().enumerate() {
                    let make = constructor(ty, i);
                    let mut fields = Vec::new();
                    let mut checks = Vec::new();
                    for (j, (_, t)) in c.fields.iter().enumerate() {
                        let field = selector(ty, i, j);
                        fields.push(format!("({field} {})", sort(t)?));
                        checks.push(domain(t, &format!("({field} x)")));
                    }
                    forms.push(format!("({make} {})", fields.join(" ")));
                    valid.push(format!(
                        "(=> ((_ is {make}) x) (and true {}))",
                        checks.join(" ")
                    ));
                }
                self.definitions += &format!(
                    "(declare-datatypes () (({name} {})))\n(define-fun valid_{key} ((x {name})) Bool (and {}))\n",
                    forms.join(" "),
                    valid.join(" ")
                );
                self.variants.insert(
                    ty.clone(),
                    ctors
                        .iter()
                        .enumerate()
                        .map(|(i, c)| (c.name.clone(), i))
                        .collect(),
                );
                if let Some(Item::Class(_)) = declarations.get(n.as_str()) {
                    self.fields.insert(
                        ty.clone(),
                        ctors[0]
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(i, (name, ty))| (name.clone(), (i, ty.clone())))
                            .collect(),
                    );
                }
                self.constructors.insert(ty.clone(), ctors);
            }
            self.definitions += &format!("(declare-const default_{key} {name})\n");
            Ok(())
        })();
        match &result {
            Ok(()) => {
                self.errors.remove(ty);
            }
            Err(e) => {
                self.errors.insert(ty.clone(), e.clone());
            }
        }
        result
    }
    pub fn check(&self, ty: &Type) -> Result<(), String> {
        if matches!(ty, Type::I64 | Type::Bool) || self.constructors.contains_key(ty) {
            Ok(())
        } else {
            Err(self
                .errors
                .get(ty)
                .cloned()
                .unwrap_or_else(|| "unsupported verification type".into()))
        }
    }
}
