//! Executable low-level IR. Every runtime input is an immediate or local operand.
//! Source expression trees are confined to the private construction graph.
use super::{
    BlockId, LirAsyncFrameLayout, LirCapture, LirDeferId, LirLocal, LirLocalId, LirLockSlots,
    LirOperand, LirRvalue, LirSelectOp, SuspendOp,
};
use crate::diagnostics::Span;
use crate::parser::ast::ExprId;
use crate::semantic::ids::{FunctionId, SemanticType as Type, TypeId};

#[derive(Debug, Clone, PartialEq)]
pub struct LirProgram {
    pub functions: Vec<LirFunction>,
    pub lambdas: Vec<LirLambda>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct LirLambda {
    pub id: ExprId,
    pub span: Span,
    pub function: LirFunction,
}
#[derive(Debug, Clone, PartialEq)]
pub struct LirParam {
    pub name: String,
    pub ty: Type,
    pub by_reference: bool,
    pub span: Span,
}
#[derive(Debug, PartialEq)]
pub struct LirFunction {
    pub name: FunctionId,
    pub is_async: bool,
    pub params: Vec<LirParam>,
    pub return_type: Type,
    pub blocks: Vec<LirBlock>,
    pub locals: Vec<LirLocal>,
    pub async_frame: LirAsyncFrameLayout,
    pub captures: Vec<LirCapture>,
}
// Cleanup regions form an ownership tree. Keep its depth off the native stack.
impl Drop for LirFunction {
    fn drop(&mut self) {
        let mut pending = std::mem::take(&mut self.blocks);
        while let Some(mut block) = pending.pop() {
            for instruction in &mut block.instrs {
                if let LirInst::Defer { body, .. } = instruction {
                    pending.append(&mut body.function.blocks);
                }
            }
        }
    }
}
impl Clone for LirFunction {
    fn clone(&self) -> Self {
        // Inline work items avoid another allocation for each cleanup frame.
        #[allow(clippy::large_enum_variant)]
        enum Work<'a> {
            Enter(usize, &'a LirFunction),
            Finish(usize, LirFunction, Vec<(usize, usize, usize)>),
        }
        let mut pending = vec![Work::Enter(0, self)];
        let mut finished = std::collections::HashMap::new();
        let mut next_id = 1;
        while let Some(work) = pending.pop() {
            match work {
                Work::Finish(id, mut function, children) => {
                    for (block, instruction, child) in children {
                        let LirInst::Defer { body, .. } =
                            &mut function.blocks[block].instrs[instruction]
                        else {
                            unreachable!()
                        };
                        *body.function = finished.remove(&child).expect("cloned cleanup");
                    }
                    finished.insert(id, function);
                }
                Work::Enter(id, source) => {
                    let mut blocks = Vec::with_capacity(source.blocks.len());
                    let mut children = Vec::new();
                    let mut child_work = Vec::new();
                    for block in &source.blocks {
                        let mut instrs = Vec::with_capacity(block.instrs.len());
                        for instruction in &block.instrs {
                            let cloned = if let LirInst::Defer { id, body, span } = instruction {
                                let child = next_id;
                                next_id += 1;
                                children.push((blocks.len(), instrs.len(), child));
                                child_work.push(Work::Enter(child, &body.function));
                                LirInst::Defer {
                                    id: *id,
                                    body: LirDeferBody {
                                        function: Box::new(empty_function()),
                                        captures: body.captures.clone(),
                                        recovery_capable: body.recovery_capable,
                                    },
                                    span: *span,
                                }
                            } else {
                                instruction.clone()
                            };
                            instrs.push(cloned);
                        }
                        blocks.push(LirBlock {
                            id: block.id,
                            instrs,
                            terminator: block.terminator.clone(),
                            recovery: block.recovery.clone(),
                        });
                    }
                    let function = LirFunction {
                        name: source.name,
                        is_async: source.is_async,
                        params: source.params.clone(),
                        return_type: source.return_type.clone(),
                        blocks,
                        locals: source.locals.clone(),
                        async_frame: source.async_frame.clone(),
                        captures: source.captures.clone(),
                    };
                    pending.push(Work::Finish(id, function, children));
                    pending.extend(child_work);
                }
            }
        }
        finished.remove(&0).expect("cloned function")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LirBlock {
    pub id: BlockId,
    pub instrs: Vec<LirInst>,
    pub terminator: Terminator,
    pub recovery: Vec<BlockId>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct LirDeferBody {
    pub function: Box<LirFunction>,
    pub captures: Vec<LirLocalId>,
    pub recovery_capable: bool,
}
impl LirDeferBody {
    pub fn contains_recover(&self) -> bool {
        self.recovery_capable
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LirPattern {
    Wildcard,
    Binding {
        name: String,
        ty: Type,
    },
    LiteralBool(bool),
    LiteralInt(i64),
    EnumVariant {
        enum_name: TypeId,
        variant: String,
    },
    EnumVariantTuple {
        enum_name: TypeId,
        variant: String,
        bindings: Vec<(String, Type)>,
    },
    ClassDowncast {
        class_name: TypeId,
        binding: String,
        binding_ty: Type,
    },
}

#[derive(Debug, Clone, PartialEq)]
// Keep the hot instruction stream contiguous; boxing changes every emitter.
#[allow(clippy::large_enum_variant)]
pub enum LirInst {
    Compute {
        local: LirLocalId,
        value: LirRvalue,
        span: Span,
    },
    Let {
        local: LirLocalId,
        name: String,
        mutable: bool,
        span: Span,
        ty: Type,
        value: LirOperand,
    },
    Assign {
        local: LirLocalId,
        name: String,
        value: LirOperand,
    },
    EnterDeferScope {
        sites: Vec<(LirDeferId, Span)>,
        resume: Option<BlockId>,
        lock: Option<LirLockSlots>,
    },
    LeaveDeferScope {
        sites: Vec<LirDeferId>,
    },
    FlushDefers {
        sites: Vec<LirDeferId>,
    },
    ClearScopeRoots {
        locals: Vec<LirLocalId>,
    },
    Defer {
        id: LirDeferId,
        body: LirDeferBody,
        span: Span,
    },
    SelectInit {
        operations: Vec<LirSelectOp>,
    },
    SelectProbe {
        operations: Vec<LirSelectOp>,
        ready: Vec<Option<LirLocalId>>,
    },
    SelectPick {
        ready: Vec<Option<LirLocalId>>,
        chosen: LirLocalId,
    },
    SelectUnregister {
        operations: Vec<LirSelectOp>,
        winner: LirLocalId,
    },
    SelectCommit {
        operation: LirSelectOp,
        success: LirLocalId,
    },
    ReleaseLock(LirLockSlots),
    MatchTest {
        scrutinee: LirLocalId,
        pattern: LirPattern,
        result: LirLocalId,
        span: Span,
    },
    MatchBind {
        scrutinee: LirLocalId,
        pattern: LirPattern,
        bindings: Vec<LirLocalId>,
        span: Span,
    },
    /// A rejected source construct, retained only as diagnostic metadata.
    Unsupported {
        span: Span,
        reason: String,
    },
}
#[derive(Debug, Clone, PartialEq)]
pub enum Terminator {
    Jump(BlockId),
    Branch {
        cond: LirOperand,
        then_block: BlockId,
        else_block: BlockId,
    },
    Suspend {
        operation: SuspendOp,
        resume: BlockId,
    },
    Return(Option<LirOperand>),
    CleanupReturn,
}

impl From<crate::ir::typed_ast::HirParam> for LirParam {
    fn from(value: crate::ir::typed_ast::HirParam) -> Self {
        Self {
            name: value.name,
            ty: value.ty,
            by_reference: value.by_reference,
            span: value.span,
        }
    }
}
impl From<crate::ir::typed_ast::HirPattern> for LirPattern {
    fn from(value: crate::ir::typed_ast::HirPattern) -> Self {
        use crate::ir::typed_ast::HirPattern as P;
        match value {
            P::Wildcard => Self::Wildcard,
            P::Binding { name, ty } => Self::Binding { name, ty },
            P::LiteralBool(value) => Self::LiteralBool(value),
            P::LiteralInt(value) => Self::LiteralInt(value),
            P::EnumVariant { enum_name, variant } => Self::EnumVariant { enum_name, variant },
            P::EnumVariantTuple {
                enum_name,
                variant,
                bindings,
            } => Self::EnumVariantTuple {
                enum_name,
                variant,
                bindings,
            },
            P::ClassDowncast {
                class_name,
                binding,
                binding_ty,
            } => Self::ClassDowncast {
                class_name,
                binding,
                binding_ty,
            },
        }
    }
}

fn empty_function() -> LirFunction {
    LirFunction {
        name: FunctionId::free(""),
        is_async: false,
        params: Vec::new(),
        return_type: Type::Void,
        blocks: Vec::new(),
        locals: Vec::new(),
        async_frame: Default::default(),
        captures: Vec::new(),
    }
}

fn operand(
    expression: &crate::ir::typed_ast::HirExpr,
    names: &std::collections::HashMap<String, LirLocalId>,
    instructions: &mut Vec<LirInst>,
) -> Option<LirOperand> {
    use crate::ir::typed_ast::HirExprKind as E;
    let value = match &expression.kind {
        E::Var(name) => names.get(name).copied().map(LirOperand::Local),
        E::Int(value) => Some(LirOperand::Int(*value)),
        E::Float(value) => Some(LirOperand::Float(*value)),
        E::Bool(value) => Some(LirOperand::Bool(*value)),
        _ => None,
    };
    if value.is_none() {
        if let Some(node) = expression
            .walk_postorder(false)
            .find(|node| node.ty == Type::Never)
        {
            let name = match &node.kind {
                E::Call { callee, .. } => format!("`{callee}`"),
                _ => "a value operand".into(),
            };
            instructions.push(LirInst::Unsupported {
                span: node.span,
                reason: format!("{name} has type `!` and cannot produce an operand"),
            });
            return None;
        }
        let description = match &expression.kind {
            E::Var(name) => format!("unbound local `{name}`"),
            E::Call { callee, .. } => format!("call `{callee}`"),
            E::MethodCall { method, .. } => format!("method `{method}`"),
            E::StaticCall { class, method, .. } => format!("static call `{class}::{method}`"),
            E::New { class, .. } => format!("constructor `{class}`"),
            E::StaticField { class, field } => format!("static field `{class}::{field}`"),
            E::FieldAccess { field, .. } => format!("field `{field}`"),
            E::Await { .. } => "await".into(),
            E::TryPropagate { .. } => "? propagation".into(),
            E::Select { .. } => "select".into(),
            E::Match { .. } => "match".into(),
            E::Lambda { .. } => "lambda construction".into(),
            E::ReferenceArg { .. } => "reference argument".into(),
            _ => "expression".into(),
        };
        instructions.push(LirInst::Unsupported {
            span: expression.span,
            reason: format!("{description} was not lowered to operands"),
        });
    }
    value
}

pub(super) fn finish_program(source: super::SourceProgram) -> LirProgram {
    LirProgram {
        functions: source.functions.into_iter().map(finish_function).collect(),
        lambdas: source
            .lambdas
            .into_iter()
            .map(|lambda| LirLambda {
                id: lambda.id,
                span: lambda.span,
                function: finish_function(lambda.function),
            })
            .collect(),
    }
}

/// Convert cleanup regions in postorder without consuming native stack depth.
fn finish_function(source: super::SourceFunction) -> LirFunction {
    enum Work {
        Enter(usize, super::SourceFunction),
        Finish(usize, LirFunction, Vec<(usize, usize, usize)>),
    }
    let mut pending = vec![Work::Enter(0, source)];
    let mut finished = std::collections::HashMap::new();
    let mut next_id = 1;
    while let Some(work) = pending.pop() {
        match work {
            Work::Finish(id, mut function, children) => {
                for (block, instruction, child) in children {
                    let LirInst::Defer { body, .. } =
                        &mut function.blocks[block].instrs[instruction]
                    else {
                        unreachable!()
                    };
                    *body.function = finished.remove(&child).expect("finished cleanup region");
                }
                finished.insert(id, function);
            }
            Work::Enter(id, source) => {
                use super::{SourceInst as I, SourceTerminator as T};
                let names = source
                    .locals
                    .iter()
                    .map(|local| (local.name.clone(), local.id))
                    .collect();
                let mut blocks = Vec::new();
                let mut children = Vec::new();
                let mut child_work = Vec::new();
                for block in source.blocks {
                    let mut instrs = Vec::new();
                    for instruction in block.instrs {
                        let instruction = match instruction {
                            I::Compute { local, value, span } => {
                                LirInst::Compute { local, value, span }
                            }
                            I::Let {
                                local,
                                name,
                                mutable,
                                span,
                                ty,
                                value,
                            } => {
                                let Some(value) = operand(&value, &names, &mut instrs) else {
                                    continue;
                                };
                                LirInst::Let {
                                    local,
                                    name,
                                    mutable,
                                    span,
                                    ty,
                                    value,
                                }
                            }
                            I::Assign { local, name, value } => {
                                let Some(value) = operand(&value, &names, &mut instrs) else {
                                    continue;
                                };
                                LirInst::Assign { local, name, value }
                            }
                            I::Expr(value) => {
                                operand(&value, &names, &mut instrs);
                                continue;
                            }
                            I::EnterDeferScope {
                                sites,
                                resume,
                                lock,
                            } => LirInst::EnterDeferScope {
                                sites,
                                resume,
                                lock,
                            },
                            I::LeaveDeferScope { sites } => LirInst::LeaveDeferScope { sites },
                            I::FlushDefers { sites } => LirInst::FlushDefers { sites },
                            I::ClearScopeRoots { locals } => LirInst::ClearScopeRoots { locals },
                            I::Defer { id, body, span } => {
                                let child = next_id;
                                next_id += 1;
                                children.push((blocks.len(), instrs.len(), child));
                                child_work.push(Work::Enter(child, *body.function));
                                LirInst::Defer {
                                    id,
                                    body: LirDeferBody {
                                        function: Box::new(empty_function()),
                                        captures: body.captures,
                                        recovery_capable: body.recovery_capable,
                                    },
                                    span,
                                }
                            }
                            I::SelectInit { operations } => LirInst::SelectInit { operations },
                            I::SelectProbe { operations, ready } => {
                                LirInst::SelectProbe { operations, ready }
                            }
                            I::SelectPick { ready, chosen } => {
                                LirInst::SelectPick { ready, chosen }
                            }
                            I::SelectUnregister { operations, winner } => {
                                LirInst::SelectUnregister { operations, winner }
                            }
                            I::SelectCommit { operation, success } => {
                                LirInst::SelectCommit { operation, success }
                            }
                            I::ReleaseLock(slots) => LirInst::ReleaseLock(slots),
                            I::MatchTest {
                                scrutinee,
                                pattern,
                                result,
                                span,
                            } => LirInst::MatchTest {
                                scrutinee,
                                pattern: pattern.into(),
                                result,
                                span,
                            },
                            I::MatchBind {
                                scrutinee,
                                pattern,
                                bindings,
                                span,
                            } => LirInst::MatchBind {
                                scrutinee,
                                pattern: pattern.into(),
                                bindings,
                                span,
                            },
                            I::FieldAssign { object, .. } => LirInst::Unsupported {
                                span: object.span,
                                reason: "field assignment was not lowered to operands".into(),
                            },
                            I::IndexAssign { array, .. } => LirInst::Unsupported {
                                span: array.span,
                                reason: "index assignment was not lowered to operands".into(),
                            },
                            I::StaticFieldAssign { value, .. } => LirInst::Unsupported {
                                span: value.span,
                                reason: "static assignment was not lowered to operands".into(),
                            },
                            I::SuperInit { span, .. } => LirInst::Unsupported {
                                span,
                                reason: "super constructor was not lowered to operands".into(),
                            },
                        };
                        instrs.push(instruction);
                    }
                    let terminator = match block.terminator {
                        T::Jump(target) => Terminator::Jump(target),
                        T::Branch {
                            cond,
                            then_block,
                            else_block,
                        } => Terminator::Branch {
                            cond: operand(&cond, &names, &mut instrs)
                                .unwrap_or(LirOperand::Bool(false)),
                            then_block,
                            else_block,
                        },
                        T::Suspend { operation, resume } => {
                            Terminator::Suspend { operation, resume }
                        }
                        T::Return(value) => Terminator::Return(
                            value
                                .as_ref()
                                .and_then(|value| operand(value, &names, &mut instrs)),
                        ),
                        T::CleanupReturn => Terminator::CleanupReturn,
                    };
                    blocks.push(LirBlock {
                        id: block.id,
                        instrs,
                        terminator,
                        recovery: block.recovery,
                    });
                }
                let function = LirFunction {
                    name: source.name,
                    is_async: source.is_async,
                    params: source.params.into_iter().map(Into::into).collect(),
                    return_type: source.return_type,
                    blocks,
                    locals: source.locals,
                    async_frame: source.async_frame,
                    captures: source.captures,
                };
                pending.push(Work::Finish(id, function, children));
                pending.extend(child_work);
            }
        }
    }
    finished.remove(&0).expect("finished function")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deeply_nested_cleanup_clone_dump_and_drop_use_bounded_stack() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let mut function = empty_function();
                for _ in 0..50_000 {
                    let mut parent = empty_function();
                    parent.blocks.push(LirBlock {
                        id: BlockId(0),
                        instrs: vec![LirInst::Defer {
                            id: LirDeferId(0),
                            body: LirDeferBody {
                                function: Box::new(function),
                                captures: Vec::new(),
                                recovery_capable: false,
                            },
                            span: Span::dummy(),
                        }],
                        terminator: Terminator::CleanupReturn,
                        recovery: Vec::new(),
                    });
                    function = parent;
                }
                let cloned = function.clone();
                let mut depth = 0;
                let mut cursor = &cloned;
                while let Some(block) = cursor.blocks.first() {
                    let LirInst::Defer { body, .. } = &block.instrs[0] else {
                        panic!("cleanup")
                    };
                    depth += 1;
                    cursor = &body.function;
                }
                assert_eq!(depth, 50_000);
                let program = LirProgram {
                    functions: vec![cloned],
                    lambdas: Vec::new(),
                };
                let dump = super::super::format_program(&program);
                assert_eq!(dump.matches("cleanup region ").count(), 50_000);
                drop(program);
                drop(function);
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
