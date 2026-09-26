//! Flat ownership encoding for executable LIR cleanup trees. Function records
//! contain only empty cleanup placeholders; links own the separate child records.
use crate::ir::lowered::{LirFunction, LirInst};
use anyhow::{Context, Result, ensure};
use std::collections::HashSet;

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct FlatLir {
    functions: Vec<LirFunction>,
    children: Vec<Vec<(usize, usize, usize)>>,
}

impl FlatLir {
    pub(crate) fn async_frame(&self) -> &crate::ir::lowered::async_liveness::LirAsyncFrameLayout {
        &self.functions[0].async_frame
    }
    pub(crate) fn set_async_frame(
        &mut self,
        frame: crate::ir::lowered::async_liveness::LirAsyncFrameLayout,
    ) {
        self.functions[0].async_frame = frame;
    }

    /// Move each region once into a flat array. No subtree is cloned or walked
    /// recursively, and each child index is greater than its parent's index.
    pub(crate) fn from_function(function: LirFunction) -> Self {
        let mut functions = vec![function];
        let mut children = Vec::new();
        let mut index = 0;
        while index < functions.len() {
            let first_child = functions.len();
            let mut detached = Vec::new();
            let mut links = Vec::new();
            for (block_index, block) in functions[index].blocks.iter_mut().enumerate() {
                for (instruction_index, instruction) in block.instrs.iter_mut().enumerate() {
                    if let LirInst::Defer { body, .. } = instruction {
                        let child_index = first_child + detached.len();
                        detached.push(std::mem::replace(
                            body.function.as_mut(),
                            LirFunction::empty_artifact_region(),
                        ));
                        links.push((block_index, instruction_index, child_index));
                    }
                }
            }
            functions.extend(detached);
            children.push(links);
            index += 1;
        }
        Self {
            functions,
            children,
        }
    }

    pub(crate) fn into_function(self) -> Result<LirFunction> {
        ensure!(!self.functions.is_empty(), "empty LIR artifact");
        ensure!(
            self.functions.len() == self.children.len(),
            "LIR region/link count mismatch"
        );
        let mut owned = vec![false; self.functions.len()];
        owned[0] = true;
        // Validate everything before attaching any region, including all defer
        // slots: a missing link must not silently become an empty cleanup body.
        for (parent, (function, links)) in self.functions.iter().zip(&self.children).enumerate() {
            let mut slots = HashSet::new();
            for (block_index, block) in function.blocks.iter().enumerate() {
                for (instruction_index, instruction) in block.instrs.iter().enumerate() {
                    if let LirInst::Defer { body, .. } = instruction {
                        let placeholder = &body.function;
                        ensure!(
                            placeholder.blocks.is_empty()
                                && placeholder.params.is_empty()
                                && placeholder.locals.is_empty()
                                && placeholder.captures.is_empty()
                                && placeholder.async_frame.locals.is_empty()
                                && placeholder.async_frame.slots.is_empty()
                                && !placeholder.is_async
                                && placeholder.name == crate::semantic::ids::FunctionId::free("")
                                && placeholder.return_type
                                    == crate::semantic::ids::SemanticType::Void,
                            "nonempty LIR cleanup placeholder"
                        );
                        slots.insert((block_index, instruction_index));
                    }
                }
            }
            for &(block, instruction, child) in links {
                ensure!(
                    child > parent && child < owned.len(),
                    "invalid LIR child index"
                );
                ensure!(
                    !std::mem::replace(&mut owned[child], true),
                    "reused LIR child region"
                );
                ensure!(
                    slots.remove(&(block, instruction)),
                    "invalid or duplicate LIR cleanup slot"
                );
            }
            ensure!(slots.is_empty(), "unlinked LIR cleanup slot");
        }
        ensure!(
            owned.into_iter().all(|owned| owned),
            "unreachable LIR region"
        );
        let mut functions: Vec<Option<LirFunction>> =
            self.functions.into_iter().map(Some).collect();
        for (parent, links) in self.children.into_iter().enumerate().rev() {
            for (block, instruction, child) in links {
                let child = functions[child]
                    .take()
                    .context("missing LIR child region")?;
                let function = functions[parent]
                    .as_mut()
                    .context("missing LIR parent region")?;
                let LirInst::Defer { body, .. } = &mut function.blocks[block].instrs[instruction]
                else {
                    anyhow::bail!("invalid LIR cleanup slot");
                };
                *body.function = child;
            }
        }
        functions[0].take().context("missing LIR root region")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        diagnostics::Span,
        ir::lowered::{
            BlockId, LirBlock, LirDeferBody, LirDeferId, LirLocalId, LirOperand, LirRvalue,
            SuspendOp, Terminator,
        },
    };

    fn parent(child: LirFunction) -> LirFunction {
        let mut function = LirFunction::empty_artifact_region();
        function.blocks.push(LirBlock {
            id: BlockId(0),
            instrs: vec![LirInst::Defer {
                id: LirDeferId(0),
                body: LirDeferBody {
                    function: Box::new(child),
                    captures: vec![LirLocalId(7)],
                    recovery_capable: true,
                },
                span: Span::dummy(),
            }],
            terminator: Terminator::CleanupReturn,
            recovery: vec![],
        });
        function
    }

    #[test]
    fn float_suspend_and_cleanup_roundtrip_preserves_payloads() {
        for bits in [
            (-0.0_f64).to_bits(),
            f64::INFINITY.to_bits(),
            0x7ff8_1234_5678_9abc,
        ] {
            let mut child = LirFunction::empty_artifact_region();
            child.blocks.push(LirBlock {
                id: BlockId(0),
                instrs: vec![LirInst::Compute {
                    local: LirLocalId(0),
                    value: LirRvalue::Use(LirOperand::Float(f64::from_bits(bits))),
                    span: Span::dummy(),
                }],
                terminator: Terminator::Suspend {
                    operation: SuspendOp::Yield,
                    resume: BlockId(1),
                },
                recovery: vec![BlockId(2)],
            });
            let artifact = FlatLir::from_function(parent(child));
            let wire = serde_json::to_vec(&artifact).unwrap();
            let restored: FlatLir = serde_json::from_slice(&wire).unwrap();
            let function = restored.into_function().unwrap();
            let LirInst::Defer { body, .. } = &function.blocks[0].instrs[0] else {
                panic!("defer");
            };
            assert_eq!(body.captures, [LirLocalId(7)]);
            assert!(body.recovery_capable);
            let LirInst::Compute {
                value: LirRvalue::Use(LirOperand::Float(value)),
                ..
            } = &body.function.blocks[0].instrs[0]
            else {
                panic!("float");
            };
            assert_eq!(value.to_bits(), bits);
            assert_eq!(
                body.function.blocks[0].terminator,
                Terminator::Suspend {
                    operation: SuspendOp::Yield,
                    resume: BlockId(1)
                }
            );
            assert_eq!(body.function.blocks[0].recovery, [BlockId(2)]);
        }
    }

    #[test]
    fn compiler_generated_lir_roundtrip() {
        let source = "async fn f() { defer { println(1); } await sleep(1); }";
        let (program, errors) =
            crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
                .parse();
        assert!(errors.is_empty());
        let (hir, _) = crate::ir::lower::lower_program(&program);
        let mut lir = crate::ir::lowered::lower_program(&hir);
        let function = lir.functions.remove(0);
        let expected = function.clone();
        let wire = serde_json::to_vec(&FlatLir::from_function(function)).unwrap();
        let artifact: FlatLir = serde_json::from_slice(&wire).unwrap();
        assert_eq!(artifact.into_function().unwrap(), expected);
    }

    #[test]
    fn fifty_thousand_cleanup_regions_roundtrip_on_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let mut function = LirFunction::empty_artifact_region();
                for _ in 0..50_000 {
                    function = parent(function);
                }
                let artifact = FlatLir::from_function(function);
                assert_eq!(artifact.functions.len(), 50_001);
                let wire = serde_json::to_vec(&artifact).unwrap();
                drop(artifact);
                let artifact: FlatLir = serde_json::from_slice(&wire).unwrap();
                let function = artifact.into_function().unwrap();
                let mut cursor = &function;
                let mut count = 0;
                while let Some(block) = cursor.blocks.first() {
                    let LirInst::Defer { body, .. } = &block.instrs[0] else {
                        panic!("defer");
                    };
                    cursor = &body.function;
                    count += 1;
                }
                assert_eq!(count, 50_000);
                drop(function);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn malformed_links_and_hidden_regions_are_rejected() {
        let make = || FlatLir::from_function(parent(LirFunction::empty_artifact_region()));
        let mut invalid = make();
        invalid.children[0][0].2 = 99;
        assert!(invalid.into_function().is_err());
        let mut backward = make();
        backward.children[0][0].2 = 0;
        assert!(backward.into_function().is_err());
        let mut duplicate = make();
        duplicate.children[0].push((0, 0, 1));
        assert!(duplicate.into_function().is_err());
        let mut wrong_slot = make();
        wrong_slot.children[0][0].0 = 99;
        assert!(wrong_slot.into_function().is_err());
        let mut unlinked = make();
        unlinked.children[0].clear();
        assert!(unlinked.into_function().is_err());
        let mut unreachable = make();
        unreachable
            .functions
            .push(LirFunction::empty_artifact_region());
        unreachable.children.push(vec![]);
        assert!(unreachable.into_function().is_err());
        let mut hidden = make();
        let LirInst::Defer { body, .. } = &mut hidden.functions[0].blocks[0].instrs[0] else {
            panic!("defer");
        };
        *body.function = parent(LirFunction::empty_artifact_region());
        assert!(hidden.into_function().is_err());
        assert!(
            FlatLir {
                functions: vec![],
                children: vec![]
            }
            .into_function()
            .is_err()
        );
    }
}
