use crate::{
    diagnostics::Diagnostic,
    ir::lower::CheckerTables,
    parser::ast::*,
    semantic::{TypeChecker, symbols::SymbolTable, type_checker::LambdaCapture},
};
use std::collections::HashMap;

/// The immutable output of checking one unit. No evaluator or mutable checker
/// state crosses the frontend/backend boundary.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct CheckedUnit {
    pub(crate) bodies: Vec<BodyId>,
    pub diagnostics: Vec<Diagnostic>,
    pub symbols: SymbolTable,
    pub expr_types: HashMap<ExprId, Type>,
    pub reference_arg_modes: HashMap<ExprId, ParamMode>,
    pub enum_variant_resolutions: HashMap<ExprId, String>,
    pub pattern_resolutions: HashMap<PatternId, Pattern>,
    #[serde(with = "super::map_entries")]
    pub normalized_types: HashMap<Type, Type>,
    pub static_call_classes: HashMap<ExprId, String>,
    pub lambda_captures: HashMap<ExprId, Vec<LambdaCapture>>,
}

impl From<TypeChecker> for CheckedUnit {
    fn from(checker: TypeChecker) -> Self {
        Self {
            bodies: checker.checked_bodies,
            diagnostics: checker.errors,
            symbols: checker.symbols,
            expr_types: checker.expr_types,
            reference_arg_modes: checker.reference_arg_modes,
            enum_variant_resolutions: checker.enum_variant_resolutions,
            pattern_resolutions: checker.pattern_resolutions,
            normalized_types: checker.normalized_types,
            static_call_classes: checker.static_call_classes,
            lambda_captures: checker.lambda_captures,
        }
    }
}

impl CheckedUnit {
    pub fn tables(&self) -> CheckerTables<'_> {
        CheckerTables {
            symbols: Some(&self.symbols),
            expr_types: Some(&self.expr_types),
            enums: Some(&self.symbols.enums),
            enum_variant_resolutions: Some(&self.enum_variant_resolutions),
            pattern_resolutions: Some(&self.pattern_resolutions),
            normalized_types: Some(&self.normalized_types),
            static_call_classes: Some(&self.static_call_classes),
            lambda_captures: Some(&self.lambda_captures),
        }
    }
}

/// Declarations are written once per unit; executable maps live exclusively in
/// body artifacts. Rehydrating a unit is a source-order aggregation of those
/// artifacts, rather than another copy of their serialized contents.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct CheckedDeclarations {
    pub symbols: SymbolTable,
    #[serde(with = "super::map_entries")]
    pub normalized_types: HashMap<Type, Type>,
}

impl CheckedDeclarations {
    pub fn into_unit(self) -> CheckedUnit {
        CheckedUnit {
            bodies: Vec::new(),
            diagnostics: Vec::new(),
            symbols: self.symbols,
            normalized_types: self.normalized_types,
            expr_types: HashMap::new(),
            reference_arg_modes: HashMap::new(),
            enum_variant_resolutions: HashMap::new(),
            pattern_resolutions: HashMap::new(),
            static_call_classes: HashMap::new(),
            lambda_captures: HashMap::new(),
        }
    }
}

/// Body-normalized annotation cache; declaration symbols have their own query.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct CheckedTypes {
    #[serde(with = "super::map_entries")]
    pub normalized_types: HashMap<Type, Type>,
}
