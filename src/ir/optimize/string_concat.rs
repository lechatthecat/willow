//! Fuse string concatenation chains into one sized allocation.
//!
//!
//! `"k" + i.to_string() + ":" + j.to_string()` lowers to a left-nested chain of
//! two-operand `String` additions whose every intermediate result is a fresh
//! temporary read exactly once. Evaluated pairwise, a chain of `n` parts
//! allocates `n - 1` strings and copies `O(n * total_len)` bytes. This pass
//! rewrites the chain into one [`LirRvalue::StringConcat`], which the runtime
//! sizes first and fills once: one allocation and `O(total_len)` copying.
//! `i64.to_string()` and `{i64}` interpolation operands become decimal parts
//! that are formatted straight into the result, never allocated on their own.
//!
//! Only the order of the intermediate allocations changes. Operands are still
//! evaluated in source order by the instructions that compute them; a fused
//! part only defers the READ of its operand local to the consuming
//! instruction, so the read is legal only while nothing writes that local in
//! between. A read crossing a possible alias write is saved in a value local
//! at its original instruction. Heap places are never deferred.
use std::collections::{HashMap, HashSet};

use crate::ir::lowered::async_liveness::{instruction_use_def, terminator_uses};
use crate::ir::lowered::{
    LirLocal, LirLocalId, LirOperand, LirRvalue, SourceBlock, SourceFunction, SourceInst,
};
use crate::parser::ast::BinOp;
use crate::semantic::ids::{FunctionId, SemanticType as Type};
use crate::semantic::intrinsics::{Intrinsic, IntrinsicWrite};

fn reference_parameters(function: &SourceFunction) -> HashSet<LirLocalId> {
    // Lifted captures precede declared parameters. Cleanup bodies inherit IDs.
    function
        .params
        .iter()
        .zip(function.locals.iter().skip(function.captures.len()))
        .filter_map(|(param, local)| param.by_reference.then_some(local.id))
        .collect()
}

/// Plan against the intact graph. Only calls on which an actual deferred
/// read depends need a summary; all bodies see the same answers before edits.
pub(crate) fn fuse_program(
    functions: &mut [SourceFunction],
    lambdas: &mut [crate::ir::lowered::SourceLambda],
    direct: &HashMap<FunctionId, usize>,
) {
    // Pure rewrites preserve storage effects. Apply them immediately and retain
    // only the bodies whose deferred reads require an alias decision.
    let mut pending = Vec::new();
    let mut analysis = CallAnalysis::default();
    for (top, function) in functions
        .iter_mut()
        .chain(lambdas.iter_mut().map(|l| &mut l.function))
        .enumerate()
    {
        let mut plans = HashMap::new();
        visit_fusion_bodies(function, |index, body, references| {
            let plan = FusionPlan::new(body, references);
            if plan.sites.is_empty() {
                for (b, nodes) in plan.blocks {
                    apply_block(&mut body.blocks[b], nodes, Vec::new());
                }
            } else {
                let queries = analysis.query(body, references, &plan.sites, direct);
                plans.insert(index, (plan, queries));
            }
        });
        if !plans.is_empty() {
            pending.push((top, plans));
        }
    }
    analysis.complete(functions, direct);
    for (top, mut plans) in pending {
        let function = if top < functions.len() {
            &mut functions[top]
        } else {
            &mut lambdas[top - functions.len()].function
        };
        visit_fusion_bodies(function, |index, body, references| {
            if let Some((plan, queries)) = plans.remove(&index) {
                plan.apply(body, references, &queries, &analysis.graph);
            }
        });
    }
}

fn visit_fusion_bodies(
    top: &mut SourceFunction,
    mut visit: impl FnMut(usize, &mut SourceFunction, &HashSet<LirLocalId>),
) {
    let references = reference_parameters(top);
    let mut functions = vec![top];
    let mut index = 0;
    while let Some(body) = functions.pop() {
        visit(index, body, &references);
        index += 1;
        for block in &mut body.blocks {
            for instruction in &mut block.instrs {
                match instruction {
                    SourceInst::Defer { body, .. } => functions.push(body.function.as_mut()),
                    ir_variants!(SourceInst; Compute, Let, Assign, EnterDeferScope, LeaveDeferScope, FlushDefers,
                        ClearScopeRoots, FieldAssign, IndexAssign, StaticFieldAssign, SuperInit,
                        SelectInit, SelectProbe, SelectPick, SelectUnregister, SelectCommit,
                        ReleaseLock, MatchTest, MatchBind, Expr) => {}
                }
            }
        }
    }
}

#[cfg(test)]
fn fuse_string_concats(function: &mut SourceFunction) {
    let references = reference_parameters(function);
    let plan = FusionPlan::new(function, &references);
    let mut analysis = CallAnalysis::default();
    let queries = analysis.query(function, &references, &plan.sites, &HashMap::new());
    analysis.graph.solve();
    plan.apply(function, &references, &queries, &analysis.graph);
}

/// How often each local is read and written, counted per instruction.
struct Accesses {
    reads: Vec<u32>,
    writes: Vec<u32>,
}

impl Accesses {
    fn count(function: &SourceFunction) -> Self {
        let names: HashMap<_, _> = function
            .locals
            .iter()
            .map(|local| (local.name.as_str(), local.id))
            .collect();
        let mut reads = vec![0u32; function.locals.len()];
        let mut writes = vec![0u32; function.locals.len()];
        let mut uses = HashSet::new();
        let mut defs = HashSet::new();
        let no_defs = HashSet::new();
        for block in &function.blocks {
            for instruction in &block.instrs {
                instruction_use_def(instruction, &names, &mut uses, &mut defs);
                Self::count_and_clear(&mut uses, &mut reads);
                Self::count_and_clear(&mut defs, &mut writes);
            }
            terminator_uses(&block.terminator, &names, &mut uses, &no_defs);
            Self::count_and_clear(&mut uses, &mut reads);
        }
        Self { reads, writes }
    }

    fn count_and_clear(ids: &mut HashSet<LirLocalId>, counts: &mut [u32]) {
        // HashSet iteration scans buckets. Release a sparse, oversized table
        // after a wide instruction so later narrow instructions stay cheap.
        let oversized = crate::ir::sparse_scratch_table(ids.capacity(), ids.len());
        for id in ids.drain() {
            counts[id.0 as usize] += 1;
        }
        if oversized {
            *ids = HashSet::new();
        }
    }
}

/// One operand of a fused chain: a leaf value, or a whole absorbed node.
enum Part {
    String(LirOperand),
    Integer(LirOperand, Type),
    Node(usize),
}

impl Part {
    /// Concat planning admits plain operands only. Child nodes account for
    /// their own local reads at the instruction where the child was defined.
    fn leaf_local(&self) -> Option<LirLocalId> {
        let operand = match self {
            Self::String(op) | Self::Integer(op, _) => op,
            Self::Node(_) => return None,
        };
        match operand {
            LirOperand::Local(id) => Some(*id),
            LirOperand::Int(_)
            | LirOperand::Integer { .. }
            | LirOperand::Float(_)
            | LirOperand::Bool(_) => None,
            LirOperand::Reference { .. } => unreachable!("heap places cannot be deferred"),
        }
    }
}

struct Node {
    /// Instruction index of the defining `Compute` within its block.
    index: usize,
    local: LirLocalId,
    parts: Vec<Part>,
    absorbed_any: bool,
    /// The node this one was absorbed into, if any.
    parent: Option<usize>,
}

/// A concatenation-shaped right-hand side: its parts, unexpanded. A `String`
/// copy is a one-part chain, so a chain can flow through it.
fn concat_parts(value: &LirRvalue, string_result: bool) -> Option<Vec<Part>> {
    let leaf = |op: &LirOperand, ty: &Type| match ty {
        Type::String => Some(Part::String(op.clone())),
        ty if ty.integer_layout().is_some() && !matches!(ty, Type::U64) => {
            Some(Part::Integer(op.clone(), ty.clone()))
        }
        _ => None,
    };
    let parts = match value {
        LirRvalue::Binary {
            op: BinOp::Add,
            lhs,
            rhs,
            operand_ty: Type::String,
        } => vec![Part::String(lhs.clone()), Part::String(rhs.clone())],
        LirRvalue::StringConcat { parts, part_types } => parts
            .iter()
            .zip(part_types)
            .map(|(op, ty)| leaf(op, ty))
            .collect::<Option<_>>()?,
        LirRvalue::FormatScalar {
            value,
            ty,
            format: None,
        } => vec![leaf(value, ty)?],
        LirRvalue::IntrinsicCall {
            intrinsic: Intrinsic::I64ToString,
            receiver,
            receiver_ty,
            ..
        } => vec![Part::Integer(receiver.clone(), receiver_ty.clone())],
        LirRvalue::Use(op) if string_result => vec![Part::String(op.clone())],
        ir_variants!(
            LirRvalue;
            BeginReferenceCall, Use, AwaitFuture, StartTask, SelectIdleWait, Coerce, ArrayAlloc,
            CaptureArrayOwner, ArrayStore, Index, ObjectAlloc, FieldLoad, FieldStore, StaticField,
            StaticStore, StaticCall, EnumAlloc, EnumPayloadStore, ConstructorCall, Range,
            EnumMethod, BuiltinCall, FormatScalar, Panic, Recover, ReferenceDebug,
            RebindResultError, IntoError, PrepareMethod, MethodCall, StringLiteral, FunctionRef,
            Closure, Print, IntrinsicCall, DirectCall, IndirectCall, Unary, Binary
        ) => return None,
    };
    // Deferring a heap read past intervening instructions is unsound.
    let deferrable = parts.iter().all(|part| match part {
        Part::String(op) | Part::Integer(op, _) => op.is_plain_value(),
        Part::Node(_) => true,
    });
    deferrable.then_some(parts)
}

fn root(nodes: &mut [Node], mut node: usize) -> usize {
    let mut top = node;
    while let Some(parent) = nodes[top].parent {
        top = parent;
    }
    // Path compression keeps repeated leaf invalidation near-linear.
    while let Some(parent) = nodes[node].parent {
        nodes[node].parent = Some(top);
        node = parent;
    }
    top
}

struct FusionPlan {
    blocks: Vec<(usize, Vec<Node>)>,
    sites: HashSet<Site>,
}

impl FusionPlan {
    fn new(body: &SourceFunction, references: &HashSet<LirLocalId>) -> Self {
        let accesses = Accesses::count(body);
        let mut plan = Self {
            blocks: Vec::new(),
            sites: HashSet::new(),
        };
        for block in &body.blocks {
            let nodes = plan_block(block, &accesses, &body.locals, references);
            if nodes.iter().any(|n| n.absorbed_any) {
                crossing_writes(block, &nodes, references, &mut plan.sites);
                plan.blocks.push((block.id.0, nodes));
            }
        }
        plan
    }

    fn apply(
        self,
        body: &mut SourceFunction,
        references: &HashSet<LirLocalId>,
        queries: &HashMap<Site, usize>,
        graph: &EffectGraph,
    ) {
        let changed: HashSet<_> = queries
            .iter()
            .filter_map(|(&(b, _), &node)| (!graph.facts[node].is_empty()).then_some(b))
            .collect();
        for (b, mut nodes) in self.blocks {
            let block = &mut body.blocks[b];
            let snapshots = if changed.contains(&b) {
                snapshot_crossing_reads(
                    block,
                    &mut nodes,
                    &mut body.locals,
                    references,
                    queries,
                    graph,
                )
            } else {
                Vec::new()
            };
            apply_block(block, nodes, snapshots);
        }
    }
}

/// The final instruction at which each node's leaves will be consumed.
fn consumer_indices(nodes: &[Node]) -> Vec<usize> {
    let mut ends = vec![0; nodes.len()];
    for (i, node) in nodes.iter().enumerate().rev() {
        ends[i] = node.parent.map_or(node.index, |p| ends[p]);
    }
    ends
}

/// A string snapshot copies its managed pointer; a decimal part copies its
/// integer. Both are ordinary typed locals visible to the later lifetime pass.
fn snapshot_crossing_reads(
    block: &SourceBlock,
    nodes: &mut [Node],
    locals: &mut Vec<LirLocal>,
    references: &HashSet<LirLocalId>,
    queries: &HashMap<Site, usize>,
    graph: &EffectGraph,
) -> Vec<(usize, SourceInst)> {
    let ends = consumer_indices(nodes);
    let mut leaves = Vec::new();
    let mut next_slot_write = HashMap::new();
    let mut next_external_write = usize::MAX;
    let mut readers = nodes.iter().enumerate().rev().peekable();
    for (index, inst) in block.instrs.iter().enumerate().rev() {
        if let Some((n, node)) = readers.next_if(|(_, node)| node.index == index)
            && node.parent.is_some()
        {
            for (part, leaf) in node.parts.iter().enumerate().rev() {
                #[cfg(test)]
                SNAPSHOT_LEAF_VISITS.set(SNAPSHOT_LEAF_VISITS.get() + 1);
                let Some(id) = leaf.leaf_local() else {
                    continue;
                };
                let write = if references.contains(&id) {
                    next_external_write
                } else {
                    next_slot_write.get(&id).copied().unwrap_or(usize::MAX)
                };
                if write < ends[n] {
                    leaves.push((n, part, id, write));
                }
            }
        }
        if let Some(&query) = queries.get(&(block.id.0, index)) {
            let facts = &graph.facts[query];
            if facts.is_empty() {
                continue;
            }
            let external = facts.contains(&Fact::External);
            if external {
                next_external_write = index;
            }
            if let Some((_, value)) = computation(inst) {
                for id in value.iter_operands().filter_map(LirOperand::borrowed_local) {
                    if external || facts.contains(&Fact::LocalSlot(id)) {
                        next_slot_write.insert(id, index);
                    }
                }
            }
        }
    }

    let mut saved = HashMap::new();
    let mut instructions = Vec::new();
    for (n, part, source, write) in leaves.into_iter().rev() {
        let node = &mut nodes[n];
        let snapshot = *saved.entry((source, write)).or_insert_with(|| {
            let local = LirLocalId(locals.len() as u32);
            let SourceInst::Compute { span, .. } = block.instrs[node.index] else {
                unreachable!("concat nodes are computations");
            };
            locals.push(LirLocal {
                id: local,
                // '$' cannot appear in a source identifier.
                name: format!("$concat{}", local.0),
                ty: locals[source.0 as usize].ty.clone(),
                storage_kind: crate::ir::lowered::LirStorageKind::Value,
                synthetic: true,
                parameter: false,
                source_span: Some(span),
            });
            instructions.push((
                node.index,
                SourceInst::Compute {
                    local,
                    value: LirRvalue::Use(LirOperand::Local(source)),
                    span,
                },
            ));
            local
        });
        match &mut node.parts[part] {
            Part::String(op) | Part::Integer(op, _) => *op = LirOperand::Local(snapshot),
            Part::Node(_) => unreachable!("only leaf reads are saved"),
        }
    }
    instructions
}

/// Record a possible writer only between the definition and final consumer
/// of a leaf that the optimistic plan actually moves.
fn crossing_writes(
    block: &SourceBlock,
    nodes: &[Node],
    references: &HashSet<LirLocalId>,
    sites: &mut HashSet<Site>,
) {
    let ends = consumer_indices(nodes);
    let mut nodes = nodes.iter().zip(ends).peekable();
    let mut borrowed_until = HashMap::<LirLocalId, usize>::new();
    let mut ref_until = 0;
    for (i, inst) in block.instrs.iter().enumerate() {
        if let Some((local, value)) = computation(inst)
            && (references.contains(&local)
                || !matches!(storage_transfer(value).potential_write(), Owner::Fresh))
        {
            let crosses_borrow = i < ref_until
                || (!borrowed_until.is_empty()
                    && value
                        .iter_operands()
                        .filter_map(LirOperand::borrowed_local)
                        .any(|id| borrowed_until.get(&id).is_some_and(|&end| i < end)));
            if crosses_borrow {
                sites.insert((block.id.0, i));
            }
        }
        if nodes.peek().is_some_and(|(n, _)| n.index == i) {
            let (node, end) = nodes.next().unwrap();
            if end == i {
                continue;
            }
            for id in node.parts.iter().filter_map(Part::leaf_local) {
                if references.contains(&id) {
                    ref_until = ref_until.max(end);
                } else {
                    let until = borrowed_until.entry(id).or_default();
                    *until = (*until).max(end);
                }
            }
        }
    }
}

fn computation(inst: &SourceInst) -> Option<(LirLocalId, &LirRvalue)> {
    match inst {
        SourceInst::Compute { local, value, .. } => Some((*local, value)),
        ir_variants!(SourceInst; Defer, Let, Assign, EnterDeferScope, LeaveDeferScope, FlushDefers,
            ClearScopeRoots, FieldAssign, IndexAssign, StaticFieldAssign, SuperInit,
            SelectInit, SelectProbe, SelectPick, SelectUnregister, SelectCommit,
            ReleaseLock, MatchTest, MatchBind, Expr) => None,
    }
}

/// Distinct demands collected during the symbolic scan. A block's longest
/// prefix covers all earlier observations in that block.
struct DependencyQueries {
    callees: HashSet<FunctionId>,
    entries: HashSet<Entry>,
    prefixes: Vec<usize>,
}

impl DependencyQueries {
    fn observe(&mut self, (block, instruction): Site, origin: Origin) {
        match origin {
            Origin::Returned(id) => {
                self.callees.insert(id);
            }
            Origin::Incoming(id) => {
                self.entries.insert((block, id));
            }
            Origin::Fresh | Origin::Allocation(_) => {}
            Origin::Unknown | Origin::Fact(_) => return,
            Origin::Node(_) => unreachable!("symbolic dependency scan"),
        }
        self.prefixes[block] = self.prefixes[block].max(instruction);
    }
}

/// Only returned values reaching a queried store or a potentially written
/// aggregate argument can introduce another result-summary dependency.
fn collect_dependencies(
    f: &SourceFunction,
    references: &HashSet<LirLocalId>,
    sites: &HashSet<Site>,
) -> HashSet<FunctionId> {
    let mut queries = DependencyQueries {
        callees: HashSet::new(),
        entries: HashSet::new(),
        prefixes: vec![0; f.blocks.len()],
    };
    let exits = scan_origins(
        f,
        &mut queries,
        |id| references.contains(&id),
        |queries, callee, operands, params, state| {
            let site = (state.block, state.instruction);
            if sites.contains(&site) {
                queries.callees.insert(callee);
                for (op, ty) in operands.iter().zip(params) {
                    if aggregate(ty) {
                        queries.observe(site, argument_value(op, state));
                    }
                    if let Some(place) = reference_owner(op, state, |_| Origin::Unknown) {
                        queries.observe(site, place);
                    }
                }
            }
            CallTransfer::Handled(Origin::Returned(callee))
        },
        None,
        |queries, origin, site| {
            if sites.contains(&site)
                && let Some((_, value)) = computation(&f.blocks[site.0].instrs[site.1])
                && matches!(storage_transfer(value).potential_write(), Owner::Operand(_))
            {
                queries.observe(site, origin);
            }
        },
        |_, _| {},
    );

    let DependencyQueries {
        mut callees,
        entries,
        mut prefixes,
    } = queries;
    #[cfg(test)]
    DEPENDENCY_QUERIES.set((callees.len(), entries.len(), prefixes.len()));
    if !entries.is_empty() {
        let preds = predecessors(f);
        // An incoming owner may rely on facts surviving calls in predecessor
        // blocks. Share the prefix walk across all demanded locals and joins.
        let mut queue: Vec<_> = entries
            .iter()
            .map(|&(b, _)| b)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        collect_returned_callees(&exits, &preds, entries, &mut callees);
        let mut visited = HashSet::new();
        while let Some(b) = queue.pop() {
            for &pred in &preds[b] {
                if visited.insert(pred) {
                    prefixes[pred] = f.blocks[pred].instrs.len();
                    if !exits[pred].unknown_entry {
                        queue.push(pred);
                    }
                }
            }
        }
    }
    for (block, end) in f.blocks.iter().zip(prefixes) {
        // Direct calls cannot invalidate a fact before it exists. Walking
        // the selected prefixes also avoids retaining individual call sites.
        let mut may_have_owner = block.id.0 != 0;
        for inst in &block.instrs[..end] {
            let Some((local, value)) = computation(inst) else {
                continue;
            };
            match storage_transfer(value) {
                StorageTransfer::Call { callee, .. } => {
                    let factory = aggregate(&f.locals[local.0 as usize].ty);
                    if may_have_owner || factory {
                        callees.insert(callee);
                    }
                    may_have_owner |= factory;
                }
                StorageTransfer::Known(_, Owner::Fresh) => may_have_owner = true,
                // A field can forward a known owner only after its allocation.
                StorageTransfer::Known(_, Owner::Operand(_) | Owner::Unknown)
                | StorageTransfer::LoadField(_)
                | StorageTransfer::StoreField(..) => {}
            }
        }
    }
    callees
}

struct Pending {
    node: usize,
    // The first reading instruction consumes this entry. Only 0, 1 and
    // multiple occurrences need to be distinguished before that happens.
    reads: u8,
}

/// A store to a fresh allocation cannot overwrite storage borrowed at entry.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Origin<I = LirLocalId> {
    Fresh,
    /// Allocation instruction in the current block. At a block boundary only freshness survives.
    Allocation(usize),
    Unknown,
    Incoming(I),
    Returned(FunctionId),
    Fact(Fact),
    Node(usize),
}

/// Symbolic transfer for one block. A copy records its source's origin at
/// the time of the copy, even if the source local is subsequently rebound.
#[derive(Default)]
struct BlockOrigins {
    block: usize,
    instruction: usize,
    values: HashMap<LirLocalId, Origin>,
    unknown_entry: bool,
}

impl BlockOrigins {
    fn get(&self, local: LirLocalId) -> Origin {
        self.values
            .get(&local)
            .copied()
            .unwrap_or(if self.unknown_entry {
                Origin::Unknown
            } else {
                Origin::Incoming(local)
            })
    }

    fn set(&mut self, local: LirLocalId, origin: Origin) {
        if self.unknown_entry && origin == Origin::Unknown {
            self.values.remove(&local);
        } else {
            self.values.insert(local, origin);
        }
    }

    fn forget(&mut self) {
        if self.values.capacity() != 0 {
            self.values = HashMap::new();
        }
        self.unknown_entry = true;
    }
}

fn operand_origin(operand: &LirOperand, origins: &BlockOrigins) -> Origin {
    match operand {
        LirOperand::Local(id) => origins.get(*id),
        LirOperand::Int(_)
        | LirOperand::Integer { .. }
        | LirOperand::Float(_)
        | LirOperand::Bool(_)
        | LirOperand::Reference { .. } => Origin::Unknown,
    }
}

/// A slot is identified by its object allocation and field name. Require the
/// same receiver type when reading so a different field layout cannot reuse it.
#[derive(Clone, Copy)]
struct ObjectField<'a> {
    object: &'a LirOperand,
    view: &'a Type,
    name: &'a str,
}

#[derive(Default)]
struct FieldOrigins<'a> {
    values: HashMap<(usize, &'a str), (&'a Type, Origin)>,
    first_private: usize,
}

impl FieldOrigins<'_> {
    fn private_allocation(&self, owner: Origin) -> Option<usize> {
        if let Origin::Allocation(id) = owner
            && id >= self.first_private
        {
            Some(id)
        } else {
            None
        }
    }

    fn invalidate(&mut self, instruction: usize) {
        // Old handles may have escaped through a heap write or call. A later
        // store through one of their aliases must not reinstate a private field.
        if self.values.capacity() != 0 {
            self.values = HashMap::new();
        }
        self.first_private = instruction + 1;
    }
}

#[derive(Clone, Copy)]
enum Owner<'a> {
    Fresh,
    Unknown,
    Operand(&'a LirOperand),
}

impl Owner<'_> {
    fn resolve(self, state: &BlockOrigins) -> Origin {
        match self {
            Self::Fresh => Origin::Fresh,
            Self::Unknown => Origin::Unknown,
            Self::Operand(op) => operand_origin(op, state),
        }
    }
}

enum StorageTransfer<'a> {
    /// Owner of the potentially written storage and owner of the result.
    /// Fresh in the first position also represents an effect-free operation.
    Known(Owner<'a>, Owner<'a>),
    LoadField(ObjectField<'a>),
    StoreField(ObjectField<'a>, &'a LirOperand),
    Call {
        callee: FunctionId,
        args: &'a [LirOperand],
        params: &'a [Type],
    },
}

impl<'a> StorageTransfer<'a> {
    /// Storage a crossed instruction could write before direct callees are resolved.
    fn potential_write(&self) -> Owner<'a> {
        match self {
            Self::Known(write, _) => *write,
            Self::LoadField(_) => Owner::Fresh,
            Self::StoreField(field, _) => Owner::Operand(field.object),
            Self::Call { .. } => Owner::Unknown,
        }
    }
}

fn storage_transfer(value: &LirRvalue) -> StorageTransfer<'_> {
    use Owner::{Fresh, Operand, Unknown};
    let (write, result) = match value {
        LirRvalue::ArrayAlloc { .. }
        | LirRvalue::EnumAlloc { .. }
        | LirRvalue::ObjectAlloc { .. } => (Fresh, Fresh),
        LirRvalue::CaptureArrayOwner { array: op, .. } | LirRvalue::Use(op) => (Fresh, Operand(op)),
        LirRvalue::ArrayStore { array, .. } => (Operand(array), Unknown),
        LirRvalue::FieldStore {
            object,
            object_ty,
            field,
            value,
        } => {
            return StorageTransfer::StoreField(
                ObjectField {
                    object,
                    view: object_ty,
                    name: field,
                },
                value,
            );
        }
        LirRvalue::FieldLoad {
            object,
            object_ty,
            field,
            ..
        } => {
            return StorageTransfer::LoadField(ObjectField {
                object,
                view: object_ty,
                name: field,
            });
        }
        LirRvalue::EnumPayloadStore { object, .. } => (Operand(object), Operand(object)),
        LirRvalue::DirectCall {
            callee,
            args,
            params,
            ..
        } => {
            return StorageTransfer::Call {
                callee: *callee,
                args,
                params,
            };
        }
        LirRvalue::IntrinsicCall {
            intrinsic,
            receiver,
            ..
        } => {
            let (write, fresh) = intrinsic.storage_effect();
            (
                match write {
                    IntrinsicWrite::None => Fresh,
                    IntrinsicWrite::Receiver => Operand(receiver),
                    IntrinsicWrite::External => Unknown,
                },
                if fresh { Fresh } else { Unknown },
            )
        }
        ir_variants!(
            LirRvalue;
            StartTask, AwaitFuture, SelectIdleWait, ConstructorCall, MethodCall, StaticCall,
            IndirectCall, BuiltinCall, EnumMethod, IntoError, StaticStore, Panic, Recover
        ) => (Unknown, Unknown),
        ir_variants!(
            LirRvalue;
            BeginReferenceCall, ReferenceDebug, PrepareMethod, Coerce,
            Index, StaticField, Range, RebindResultError, StringLiteral, FunctionRef,
            Closure, Print, FormatScalar, Unary, Binary, StringConcat
        ) => (Fresh, Unknown),
    };
    StorageTransfer::Known(write, result)
}

/// Resolve parameter-specific writes against the actual argument at this
/// instruction, before recording the destination's new origin.
fn may_write_aliased_storage<'a>(
    local: LirLocalId,
    value: &'a LirRvalue,
    origins: &mut BlockOrigins,
    fields: &mut FieldOrigins<'a>,
    direct_call: impl FnOnce(FunctionId, &[LirOperand], &[Type], &mut BlockOrigins) -> CallTransfer,
) -> Origin {
    use Origin::{Fresh, Unknown};
    let (write, result) = match storage_transfer(value) {
        StorageTransfer::Known(write, result) => {
            let owner = write.resolve(origins);
            if !matches!(write, Owner::Fresh) && fields.private_allocation(owner).is_none() {
                fields.invalidate(origins.instruction);
            }
            let result = match result {
                Owner::Fresh => Origin::Allocation(origins.instruction),
                Owner::Operand(_) | Owner::Unknown => result.resolve(origins),
            };
            (owner, result)
        }
        StorageTransfer::LoadField(field) => {
            let result = if let Some(id) =
                fields.private_allocation(operand_origin(field.object, origins))
            {
                #[cfg(test)]
                FIELD_READS.set(FIELD_READS.get() + 1);
                fields
                    .values
                    .get(&(id, field.name))
                    .filter(|(view, _)| *view == field.view)
                    .map_or(Unknown, |&(_, origin)| origin)
            } else {
                Unknown
            };
            (Fresh, result)
        }
        StorageTransfer::StoreField(field, value) => {
            let receiver = operand_origin(field.object, origins);
            if let Some(id) = fields.private_allocation(receiver) {
                // Snapshot the value before assigning the instruction destination.
                let key = (id, field.name);
                let source = operand_origin(value, origins);
                if source == Unknown {
                    fields.values.remove(&key);
                } else {
                    fields.values.insert(key, (field.view, source));
                }
                #[cfg(test)]
                {
                    FIELD_STORES.set(FIELD_STORES.get() + 1);
                    PEAK_FIELDS.set(PEAK_FIELDS.get().max(fields.values.len()));
                }
                (Fresh, Unknown)
            } else {
                // A store outside private storage could publish the stored value.
                fields.invalidate(origins.instruction);
                (receiver, Unknown)
            }
        }
        StorageTransfer::Call {
            callee,
            args,
            params,
        } => {
            // A call may overwrite a field through any previously escaped alias.
            fields.invalidate(origins.instruction);
            match direct_call(callee, args, params, origins) {
                CallTransfer::Opaque => (Unknown, Unknown),
                CallTransfer::Handled(result) => (Fresh, result),
            }
        }
    };
    if write == Unknown {
        origins.forget();
    }
    origins.set(local, result);
    if matches!(write, Origin::Allocation(_)) {
        Fresh
    } else {
        write
    }
}

enum CallTransfer {
    Opaque,
    // The callback has accounted for writes and supplies the result origin.
    Handled(Origin),
}

fn aggregate(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Array(_) | Type::FixedArray(_, _) | Type::Named(_) | Type::Generic(_, _)
    )
}

/// Possible sources of a return value or storage write. An empty set denotes
/// fresh storage. Slot and Value are different even for the same formal:
/// replacing an array handle and writing an element affect different places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Fact {
    External,
    Value(usize),
    Slot(usize),
    LocalSlot(LirLocalId),
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Source {
    Fact(Fact),
    Node(usize),
}

#[derive(Default)]
struct EffectGraph {
    facts: Vec<HashSet<Fact>>,
    copies: HashMap<usize, HashSet<usize>>,
    // If a callee can produce this fact, bind its actual to the destination.
    bindings: HashMap<(usize, Fact), HashSet<(Source, usize)>>,
    work: Vec<(usize, Fact)>,
}

impl EffectGraph {
    fn node(&mut self) -> usize {
        self.facts.push(HashSet::new());
        self.facts.len() - 1
    }
    fn insert(&mut self, node: usize, fact: Fact) {
        if self.facts[node].insert(fact) {
            self.work.push((node, fact));
        }
    }
    fn connect(&mut self, source: Source, dest: usize) {
        match source {
            Source::Fact(fact) => self.insert(dest, fact),
            Source::Node(node)
                if node != dest && self.copies.entry(node).or_default().insert(dest) =>
            {
                // Queue newly received facts only. Split the borrows so a join
                // can inherit the existing set without cloning it.
                let [source, destination] = self
                    .facts
                    .get_disjoint_mut([node, dest])
                    .expect("distinct graph nodes");
                for &fact in source.iter() {
                    if destination.insert(fact) {
                        self.work.push((dest, fact));
                    }
                }
            }
            Source::Node(_) => {}
        }
    }
    fn bind(&mut self, from: usize, fact: Fact, source: Option<Source>, dest: usize) {
        if let Some(source) = source {
            self.bindings
                .entry((from, fact))
                .or_default()
                .insert((source, dest));
        }
    }
    fn solve(&mut self) {
        while let Some((node, fact)) = self.work.pop() {
            if let Some(copies) = self.copies.get(&node) {
                for &dest in copies {
                    if self.facts[dest].insert(fact) {
                        self.work.push((dest, fact));
                    }
                }
            }
            if let Some(bindings) = self.bindings.remove(&(node, fact)) {
                #[cfg(test)]
                PROPAGATED_CALL_EDGES.set(PROPAGATED_CALL_EDGES.get() + bindings.len());
                for (source, dest) in bindings {
                    self.connect(source, dest);
                }
            }
        }
    }
}

/// Index CFG entries on demand. Equal reaching definitions still use the
/// shared snapshots, including when they carry a call result or a formal.
#[derive(Default)]
struct EntryNodes {
    nodes: HashMap<Entry, usize>,
    work: Vec<(Entry, usize)>,
}
impl EntryNodes {
    fn source(&mut self, origin: Origin, block: usize, graph: &mut EffectGraph) -> Option<Source> {
        self.qualified(qualify(origin, block), graph)
    }
    fn qualified(&mut self, origin: Origin<Entry>, graph: &mut EffectGraph) -> Option<Source> {
        match origin {
            Origin::Fresh | Origin::Allocation(_) => None,
            Origin::Unknown => Some(Source::Fact(Fact::External)),
            Origin::Fact(fact) => Some(Source::Fact(fact)),
            Origin::Node(id) => Some(Source::Node(id)),
            Origin::Incoming(entry) => Some(Source::Node(*self.nodes.entry(entry).or_insert_with(
                || {
                    let node = graph.node();
                    self.work.push((entry, node));
                    node
                },
            ))),
            Origin::Returned(_) => unreachable!("callee was bound to a result node"),
        }
    }
    fn resolve(&mut self, body: &SourceFunction, exits: &[BlockOrigins], graph: &mut EffectGraph) {
        if self.work.is_empty() {
            return;
        }
        let preds = predecessors(body);
        let mut index = ReachingDefinitions::new(exits, &preds);
        #[cfg(test)]
        let mut entry_edges = 0;
        while let Some((entry, dest)) = self.work.pop() {
            index.visit_inputs(entry, |origin| {
                #[cfg(test)]
                if matches!(origin, Origin::Incoming(_)) {
                    entry_edges += 1;
                }
                if let Some(source) = self.qualified(origin, graph) {
                    graph.connect(source, dest);
                }
            });
        }
        #[cfg(test)]
        ORIGIN_RETAINED.set((
            index.by_block.len(),
            index.snapshots.len(),
            index.snapshots.iter().map(|s| s.state.values.len()).sum(),
            self.nodes.len(),
            entry_edges,
        ));
    }
}

#[derive(Clone, Copy)]
struct Outputs {
    writes: usize,
    result: usize,
}

enum Demand<'a> {
    Function(Outputs),
    Sites(&'a HashMap<Site, usize>),
}
impl Demand<'_> {
    fn writes(&self, site: Site) -> Option<usize> {
        match self {
            Self::Function(out) => Some(out.writes),
            Self::Sites(sites) => sites.get(&site).copied(),
        }
    }
}

#[derive(Default)]
struct CallAnalysis {
    graph: EffectGraph,
    functions: HashMap<FunctionId, Outputs>,
    pending: Vec<usize>,
    #[cfg(test)]
    call_edges: HashSet<(usize, usize)>,
}

impl CallAnalysis {
    fn outputs(&mut self, id: FunctionId, direct: &HashMap<FunctionId, usize>) -> Option<Outputs> {
        if let Some(&out) = self.functions.get(&id) {
            return Some(out);
        }
        #[cfg(test)]
        SUMMARY_INDEX_PROBES.set(SUMMARY_INDEX_PROBES.get() + 1);
        let &index = direct.get(&id)?;
        let out = Outputs {
            writes: self.graph.node(),
            result: self.graph.node(),
        };
        self.functions.insert(id, out);
        self.pending.push(index);
        Some(out)
    }
    fn query(
        &mut self,
        body: &SourceFunction,
        refs: &HashSet<LirLocalId>,
        sites: &HashSet<Site>,
        direct: &HashMap<FunctionId, usize>,
    ) -> HashMap<Site, usize> {
        let queries: HashMap<_, _> = sites
            .iter()
            .map(|&site| (site, self.graph.node()))
            .collect();
        if !sites.is_empty() {
            let roots = collect_dependencies(body, refs, sites);
            self.body(body, refs, Demand::Sites(&queries), Some(&roots), direct);
        }
        queries
    }
    fn complete(&mut self, functions: &[SourceFunction], direct: &HashMap<FunctionId, usize>) {
        #[cfg(test)]
        let prior_edges: usize = self.graph.bindings.values().map(HashSet::len).sum();
        while let Some(index) = self.pending.pop() {
            let body = &functions[index];
            #[cfg(test)]
            SUMMARY_BODIES.set(SUMMARY_BODIES.get() + 1);
            self.body(
                body,
                &reference_parameters(body),
                Demand::Function(self.functions[&body.name]),
                None,
                direct,
            );
        }
        #[cfg(test)]
        {
            SUMMARY_LOOKUP_ENTRIES.set(self.functions.len());
            SUMMARY_EDGES.set(
                self.graph
                    .bindings
                    .values()
                    .map(HashSet::len)
                    .sum::<usize>()
                    - prior_edges,
            );
            STORED_CALL_EDGES.set(self.call_edges.len());
        }
        self.graph.solve();
    }

    fn body(
        &mut self,
        body: &SourceFunction,
        refs: &HashSet<LirLocalId>,
        demand: Demand,
        roots: Option<&HashSet<FunctionId>>,
        direct: &HashMap<FunctionId, usize>,
    ) {
        let symbolic = matches!(demand, Demand::Function(_));
        let mut initial = Vec::new();
        if let Demand::Function(out) = demand {
            let mut alias_guard = None;
            for (i, p) in body
                .params
                .iter()
                .enumerate()
                .filter(|(_, p)| aggregate(&p.ty))
            {
                let value = Fact::Value(i);
                let origin = if p.by_reference {
                    let guard = *alias_guard.get_or_insert_with(|| self.graph.node());
                    self.graph.bind(
                        out.writes,
                        Fact::Slot(i),
                        Some(Source::Fact(Fact::External)),
                        guard,
                    );
                    let node = self.graph.node();
                    self.graph.connect(Source::Fact(value), node);
                    self.graph.connect(Source::Node(guard), node);
                    Origin::Node(node)
                } else {
                    Origin::Fact(value)
                };
                initial.push((LirLocalId((i + body.captures.len()) as u32), origin));
            }
            if let Some(guard) = alias_guard {
                self.graph.bind(
                    out.writes,
                    Fact::External,
                    Some(Source::Fact(Fact::External)),
                    guard,
                );
            }
        }
        let mut entries = EntryNodes::default();
        let mut writes = HashSet::new();
        let mut returns = HashSet::new();
        let exits = scan_origins(
            body,
            &mut writes,
            |id| refs.contains(&id),
            |_, id, args, types, state| {
                if roots.is_some_and(|roots| !roots.contains(&id)) {
                    return CallTransfer::Opaque;
                }
                let Some(out) = self.outputs(id, direct) else {
                    return CallTransfer::Opaque;
                };
                let site = (state.block, state.instruction);
                let write = demand.writes(site);
                #[cfg(test)]
                if symbolic && let Some(dest) = write {
                    self.call_edges.insert((out.writes, dest));
                }
                let result =
                    computation(&body.blocks[site.0].instrs[site.1]).and_then(|(dst, _)| {
                        aggregate(&body.locals[dst.0 as usize].ty).then(|| self.graph.node())
                    });
                let external = Some(Source::Fact(Fact::External));
                if let Some(dest) = write {
                    self.graph.bind(out.writes, Fact::External, external, dest);
                }
                if let Some(dest) = result {
                    self.graph.bind(out.result, Fact::External, external, dest);
                }

                // All actuals read the pre-call state. Each borrowed local has one
                // post-call origin, even if several formals can replace its slot.
                let mut rebound = HashMap::new();
                for (i, (arg, ty)) in args.iter().zip(types).enumerate() {
                    if aggregate(ty) {
                        let owner =
                            entries.source(argument_value(arg, state), site.0, &mut self.graph);
                        if let Some(dest) = write {
                            self.graph.bind(out.writes, Fact::Value(i), owner, dest);
                        }
                        if let Some(dest) = result {
                            self.graph.bind(out.result, Fact::Value(i), owner, dest);
                        }
                        // Incoming reference slots already share the alias guard.
                        if owner != external
                            && let Some(local) = arg.borrowed_local()
                            && !refs.contains(&local)
                        {
                            let after = *rebound.entry(local).or_insert_with(|| {
                                let after = self.graph.node();
                                if let Some(old) = owner {
                                    self.graph.connect(old, after);
                                }
                                self.graph.bind(out.writes, Fact::External, external, after);
                                after
                            });
                            self.graph.bind(out.writes, Fact::Slot(i), external, after);
                        }
                    }
                    if let Some(dest) = write
                        && let Some(place) = reference_owner(arg, state, |local| {
                            if refs.contains(&local) {
                                if symbolic {
                                    Origin::Fact(Fact::Slot(local.0 as usize - body.captures.len()))
                                } else {
                                    Origin::Unknown
                                }
                            } else if symbolic {
                                Origin::Fresh
                            } else {
                                Origin::Fact(Fact::LocalSlot(local))
                            }
                        })
                    {
                        let owner = entries.source(place, site.0, &mut self.graph);
                        self.graph.bind(out.writes, Fact::Slot(i), owner, dest);
                    }
                }
                for (local, after) in rebound {
                    state.set(local, Origin::Node(after));
                }
                CallTransfer::Handled(result.map_or(Origin::Unknown, Origin::Node))
            },
            symbolic.then_some(initial.as_slice()),
            |writes, origin, site| {
                if origin != Origin::Fresh
                    && let Some(dest) = demand.writes(site)
                {
                    writes.insert((qualify(origin, site.0), dest));
                }
            },
            |origin, block| {
                if let Demand::Function(out) = demand
                    && aggregate(&body.return_type)
                {
                    returns.insert((qualify(origin, block), out.result));
                }
            },
        );
        for (origin, dest) in writes.into_iter().chain(returns) {
            if let Some(source) = entries.qualified(origin, &mut self.graph) {
                self.graph.connect(source, dest);
            }
        }
        entries.resolve(body, &exits, &mut self.graph);
    }
}

fn argument_value(operand: &LirOperand, state: &BlockOrigins) -> Origin {
    if let Some(id) = operand.borrowed_local() {
        state.get(id)
    } else {
        operand_origin(operand, state)
    }
}

fn reference_owner(
    operand: &LirOperand,
    state: &BlockOrigins,
    local_slot: impl FnOnce(LirLocalId) -> Origin,
) -> Option<Origin> {
    use crate::ir::lowered::LirPlace;
    match operand {
        LirOperand::Reference { place, .. } => Some(match place {
            LirPlace::Local(local) => local_slot(*local),
            LirPlace::Field { object, .. } => state.get(*object),
            LirPlace::ArrayElement { owner, .. } => state.get(*owner),
        }),
        ir_variants!(LirOperand; Local,Int,Integer,Float,Bool) => None,
    }
}

#[cfg(test)]
fn summarize_callees(functions: &[SourceFunction], roots: HashSet<FunctionId>) -> CallAnalysis {
    let index = crate::ir::lowered::synchronous_function_indices(functions);
    let mut analysis = CallAnalysis::default();
    for root in roots {
        analysis.outputs(root, &index);
    }
    analysis.complete(functions, &index);
    analysis
}

fn qualify(origin: Origin, block: usize) -> Origin<Entry> {
    match origin {
        Origin::Fresh | Origin::Allocation(_) => Origin::Fresh,
        Origin::Unknown => Origin::Unknown,
        Origin::Returned(id) => Origin::Returned(id),
        Origin::Fact(fact) => Origin::Fact(fact),
        Origin::Node(node) => Origin::Node(node),
        Origin::Incoming(id) => Origin::Incoming((block, id)),
    }
}

#[cfg(test)]
fn body_may_write(f: &SourceFunction) -> bool {
    let analysis = summarize_callees(std::slice::from_ref(f), HashSet::from([f.name]));
    !analysis.graph.facts[analysis.functions[&f.name].writes].is_empty()
}
type Site = (usize, usize);
type Entry = (usize, LirLocalId);

fn predecessors(function: &SourceFunction) -> Vec<Vec<usize>> {
    let mut preds = vec![Vec::new(); function.blocks.len()];
    for block in &function.blocks {
        for target in crate::ir::lowered::normal_successors(&block.terminator) {
            preds[target].push(block.id.0);
        }
    }
    preds
}

/// Scan each instruction once, preserving symbolic entry values for CFG resolution.
fn scan_origins<C>(
    function: &SourceFunction,
    context: &mut C,
    writes_ref: impl Fn(LirLocalId) -> bool,
    mut direct_call: impl FnMut(
        &mut C,
        FunctionId,
        &[LirOperand],
        &[Type],
        &mut BlockOrigins,
    ) -> CallTransfer,
    initial: Option<&[(LirLocalId, Origin)]>,
    mut check_origin: impl FnMut(&mut C, Origin, Site),
    mut returned: impl FnMut(Origin, usize),
) -> Vec<BlockOrigins> {
    use crate::ir::lowered::SourceTerminator;
    use Origin::{Fresh, Unknown};

    let names = std::cell::OnceCell::new();
    let local_named = |name: &str| {
        names
            .get_or_init(|| {
                function
                    .locals
                    .iter()
                    .map(|local| (local.name.as_str(), local.id))
                    .collect::<HashMap<_, _>>()
            })
            .get(name)
            .copied()
    };
    let recovery: HashSet<_> = function
        .blocks
        .iter()
        .flat_map(|b| b.recovery.iter().map(|target| target.0))
        .collect();
    let mut exits = Vec::with_capacity(function.blocks.len());
    for block in &function.blocks {
        let b = block.id.0;
        let mut fields = FieldOrigins::default();
        let mut origins = BlockOrigins {
            block: b,
            // An exceptional edge can leave before a definition in its block.
            unknown_entry: b == 0 || recovery.contains(&b),
            ..Default::default()
        };
        if b == 0
            && let Some(bindings) = initial
        {
            for &(local, origin) in bindings {
                origins.set(local, origin);
            }
        }
        for (i, inst) in block.instrs.iter().enumerate() {
            origins.instruction = i;
            let (write, defined) = match inst {
                SourceInst::Compute { local, value, .. } => (
                    may_write_aliased_storage(
                        *local,
                        value,
                        &mut origins,
                        &mut fields,
                        |id, args, types, state| direct_call(context, id, args, types, state),
                    ),
                    Some(*local),
                ),
                SourceInst::Let { local, value, .. } | SourceInst::Assign { local, value, .. } => {
                    if let Some(source) = atom_origin(value, |name| {
                        local_named(name).map_or(Unknown, |id| origins.get(id))
                    }) {
                        origins.set(*local, source);
                        (Fresh, Some(*local))
                    } else {
                        fields.invalidate(i);
                        origins.forget();
                        (Unknown, Some(*local))
                    }
                }
                SourceInst::Expr(expr) if atom_origin(expr, |_| Unknown).is_some() => (Fresh, None),
                SourceInst::ClearScopeRoots { locals } => {
                    for id in locals {
                        origins.set(*id, Unknown);
                    }
                    (Fresh, None)
                }
                ir_variants!(SourceInst; Expr, EnterDeferScope, LeaveDeferScope,
                    FlushDefers, Defer, FieldAssign, IndexAssign, StaticFieldAssign, SuperInit,
                    SelectInit, SelectProbe, SelectPick, SelectUnregister, SelectCommit, ReleaseLock,
                    MatchTest, MatchBind) => {
                    fields.invalidate(i);
                    origins.forget();
                    (Unknown, None)
                }
            };
            check_origin(context, write, (b, i));
            if let Some(local) = defined
                && writes_ref(local)
            {
                // An incoming slot may publish the value while private locals keep their bindings.
                fields.invalidate(i);
                origins.set(local, Unknown);
                let slot = if initial.is_some() {
                    Origin::Fact(Fact::Slot(local.0 as usize - function.captures.len()))
                } else {
                    Unknown
                };
                check_origin(context, slot, (b, i));
            }
        }
        let term = match &block.terminator {
            SourceTerminator::Return(Some(expr)) => {
                let origin = atom_origin(expr, |name| {
                    local_named(name).map_or(Unknown, |id| origins.get(id))
                });
                returned(origin.unwrap_or(Unknown), b);
                if origin.is_some() { Fresh } else { Unknown }
            }
            SourceTerminator::Branch { cond: expr, .. } => {
                if atom_origin(expr, |_| Unknown).is_some() {
                    Fresh
                } else {
                    Unknown
                }
            }
            SourceTerminator::Return(None)
            | SourceTerminator::CleanupReturn
            | SourceTerminator::Jump(_) => Fresh,
            SourceTerminator::Suspend { .. } => Unknown,
        };
        if term == Unknown {
            origins.forget();
        }
        check_origin(context, term, (b, block.instrs.len()));
        exits.push(origins);
    }
    exits
}

/// Unchanged CFG edges share reaching definitions for all locals.
#[derive(Clone, Default)]
struct ReachingOrigins {
    values: im::HashMap<LirLocalId, Origin<Entry>>,
    /// Unmodified locals depend on this join's input. At entry or after a
    /// barrier the default is unknown instead.
    join: Option<usize>,
}

impl ReachingOrigins {
    fn get(&self, local: LirLocalId) -> Origin<Entry> {
        self.values.get(&local).copied().unwrap_or_else(|| {
            self.join
                .map_or(Origin::Unknown, |b| Origin::Incoming((b, local)))
        })
    }
}

/// Postorder DFS frame. Fold predecessor IDs as each edge finishes.
#[derive(Default)]
struct OriginInputs {
    block: usize,
    next: usize,
    common: Option<usize>,
}

/// Keep the defining block so equivalence checks can inspect only the keys
/// changed since two snapshots diverged. Block transfers already own those keys.
struct OriginVersion {
    state: ReachingOrigins,
    parent: Option<usize>,
    block: usize,
}

struct ReachingDefinitions<'a> {
    exits: &'a [BlockOrigins],
    predecessors: &'a [Vec<usize>],
    by_block: Vec<Option<usize>>,
    snapshots: Vec<OriginVersion>,
    equivalent_pairs: HashMap<(usize, usize), bool>,
    inputs: Vec<Option<usize>>,
    path: Vec<OriginInputs>,
    visiting: Vec<bool>,
}

impl<'a> ReachingDefinitions<'a> {
    fn new(exits: &'a [BlockOrigins], predecessors: &'a [Vec<usize>]) -> Self {
        Self {
            exits,
            predecessors,
            by_block: vec![None; exits.len()],
            snapshots: Vec::new(),
            equivalent_pairs: HashMap::new(),
            inputs: vec![None; exits.len()],
            path: Vec::new(),
            visiting: vec![false; exits.len()],
        }
    }

    fn include_input(&mut self, snapshot: Option<usize>) {
        let frame = self.path.last().unwrap();
        let common = if frame.next == 1 || frame.common == snapshot {
            snapshot
        } else if let (Some(a), Some(b)) = (frame.common, snapshot) {
            let key = (a.min(b), a.max(b));
            let agree = if let Some(&agree) = self.equivalent_pairs.get(&key) {
                agree
            } else {
                let agree = self.same_bindings(a, b);
                self.equivalent_pairs.insert(key, agree);
                agree
            };
            agree.then_some(a)
        } else {
            None
        };
        self.path.last_mut().unwrap().common = common;
    }

    fn same_bindings(&self, a: usize, b: usize) -> bool {
        let left = &self.snapshots[a].state;
        let right = &self.snapshots[b].state;
        if left.join != right.join || left.values.len() != right.values.len() {
            return false;
        }
        // Version IDs increase from parent to child. Walk back to their
        // common base, comparing just the modified keys in the final states.
        // Independent roots or long revision histories use one map comparison.
        let (mut x, mut y) = (a, b);
        let mut budget = left.values.len();
        while x != y {
            if x < y {
                std::mem::swap(&mut x, &mut y);
            }
            let version = &self.snapshots[x];
            let changes = &self.exits[version.block].values;
            let Some(parent) = version.parent else { break };
            if changes.len() >= budget {
                break;
            }
            budget -= changes.len();
            if changes.keys().any(|id| left.get(*id) != right.get(*id)) {
                return false;
            }
            x = parent;
        }
        x == y || left.values == right.values
    }

    fn ensure(&mut self, block: usize) {
        if self.by_block[block].is_some() {
            return;
        }
        self.visiting[block] = true;
        self.path.push(OriginInputs {
            block,
            ..Default::default()
        });
        while let Some(frame) = self.path.last_mut() {
            let b = frame.block;
            let preds = &self.predecessors[b];
            if let Some(&pred) = preds.get(frame.next) {
                frame.next += 1;
                #[cfg(test)]
                ORIGIN_EDGE_VISITS.set(ORIGIN_EDGE_VISITS.get() + 1);
                if self.by_block[pred].is_some() || self.visiting[pred] {
                    // An unfinished backedge leaves a symbolic join.
                    self.include_input(self.by_block[pred]);
                } else {
                    self.visiting[pred] = true;
                    self.path.push(OriginInputs {
                        block: pred,
                        ..Default::default()
                    });
                }
                continue;
            }
            let input = frame.common;
            self.path.pop();
            self.visiting[b] = false;
            self.inputs[b] = input;
            let exit = &self.exits[b];
            let parent = input.filter(|_| !exit.unknown_entry);
            let output = if let Some(id) = parent
                && exit.values.is_empty()
            {
                id
            } else {
                // A join (including a cycle with no indexed parent) is resolved
                // only for demanded locals. Definitions on either side of it can
                // still be indexed in a single pass.
                let join = (!exit.unknown_entry && !preds.is_empty()).then_some(b);
                let mut snapshot = parent.map_or_else(
                    || ReachingOrigins {
                        join,
                        ..Default::default()
                    },
                    |idx| self.snapshots[idx].state.clone(),
                );
                let mut changed = false;
                for (&dest, &value) in &exit.values {
                    let source = match value {
                        Origin::Fresh | Origin::Allocation(_) => Origin::Fresh,
                        Origin::Fact(fact) => Origin::Fact(fact),
                        Origin::Node(node) => Origin::Node(node),
                        Origin::Returned(id) => Origin::Returned(id),
                        Origin::Unknown => Origin::Unknown,
                        Origin::Incoming(id) => parent.map_or_else(
                            || join.map_or(Origin::Unknown, |j| Origin::Incoming((j, id))),
                            |idx| self.snapshots[idx].state.get(id),
                        ),
                    };
                    if snapshot.get(dest) == source {
                        continue;
                    }
                    changed = true;
                    let default = snapshot
                        .join
                        .map_or(Origin::Unknown, |j| Origin::Incoming((j, dest)));
                    if source == default {
                        snapshot.values.remove(&dest);
                    } else {
                        snapshot.values.insert(dest, source);
                    }
                }
                if let Some(parent) = parent
                    && !changed
                {
                    parent
                } else {
                    let id = self.snapshots.len();
                    self.snapshots.push(OriginVersion {
                        state: snapshot,
                        parent,
                        block: b,
                    });
                    id
                }
            };
            self.by_block[b] = Some(output);
            if !self.path.is_empty() {
                self.include_input(Some(output));
            }
        }
    }
    fn visit_inputs(&mut self, (block, local): Entry, mut observe: impl FnMut(Origin<Entry>)) {
        let preds = &self.predecessors[block];
        if preds.is_empty() {
            observe(Origin::Unknown);
            return;
        }
        if preds.len() > 1 {
            self.ensure(block);
        }
        if let Some(id) = self.inputs[block] {
            observe(self.snapshots[id].state.get(local));
        } else {
            for &pred in preds {
                #[cfg(test)]
                ORIGIN_EDGE_VISITS.set(ORIGIN_EDGE_VISITS.get() + 1);
                self.ensure(pred);
                observe(
                    self.snapshots[self.by_block[pred].unwrap()]
                        .state
                        .get(local),
                );
            }
        }
    }
}

/// Discover the callees that can supply a demanded value. Each incoming
/// entry is scheduled once, even at a loop header or a shared predecessor.
fn collect_returned_callees(
    exits: &[BlockOrigins],
    preds: &[Vec<usize>],
    mut visited: HashSet<Entry>,
    callees: &mut HashSet<FunctionId>,
) {
    let mut index = ReachingDefinitions::new(exits, preds);
    let mut work: Vec<_> = visited.iter().copied().collect();
    #[cfg(test)]
    let mut incoming = 0;
    while let Some(entry) = work.pop() {
        index.visit_inputs(entry, |origin| match origin {
            Origin::Incoming(input) => {
                #[cfg(test)]
                {
                    incoming += 1;
                }
                if visited.insert(input) {
                    work.push(input);
                }
            }
            Origin::Returned(id) => {
                callees.insert(id);
            }
            Origin::Node(_) => unreachable!("unresolved graph node in dependency scan"),
            Origin::Unknown | Origin::Fact(_) | Origin::Fresh | Origin::Allocation(_) => {}
        });
    }
    #[cfg(test)]
    CALLEE_SCAN.set((visited.len(), incoming));
}

fn atom_origin(
    expr: &crate::ir::typed_ast::HirExpr,
    variable: impl FnOnce(&str) -> Origin,
) -> Option<Origin> {
    use crate::ir::typed_ast::HirExprKind as E;
    match &expr.kind {
        E::Var(name) => Some(variable(name)),
        ir_variants!(E; Int, Float, Bool, Str, FnRef) => Some(Origin::Unknown),
        // A residual compound expression has not been lowered to flat effects.
        ir_variants!(E; Binary, Unary, Call, Invoke, Print, Array, Index, Ternary, New,
            FieldAccess, MethodCall, ObjectLiteral, StaticField, StaticCall, ReferenceArg,
            Range, Await, TryPropagate, Lambda, Match, Select) => None,
    }
}

/// Retire the current root of each affected tree. An absorbed child may
/// have passed its deferred reads on to several successive parents.
fn invalidate_readers(
    readers: impl IntoIterator<Item = usize>,
    nodes: &mut [Node],
    pending: &mut HashMap<LirLocalId, Pending>,
) {
    for reader in readers {
        let top = root(nodes, reader);
        let local = nodes[top].local;
        if pending.get(&local).is_some_and(|p| p.node == top) {
            pending.remove(&local);
        }
    }
}

#[cfg(test)]
thread_local! {
    static FIELD_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FIELD_STORES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PEAK_FIELDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PENDING_OPERAND_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    // Direct callees, distinct incoming owners and prefix slots at the end of collection.
    static DEPENDENCY_QUERIES: std::cell::Cell<(usize, usize, usize)> = const { std::cell::Cell::new((0, 0, 0)) };
    // Visited entry keys and incoming dependencies examined by callee discovery.
    static CALLEE_SCAN: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    // Block index slots, snapshots, logical bindings, query nodes, reverse edges.
    static ORIGIN_RETAINED: std::cell::Cell<(usize, usize, usize, usize, usize)> = const { std::cell::Cell::new((0,0,0,0,0)) };
    static ORIGIN_EDGE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SUMMARY_INDEX_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SUMMARY_LOOKUP_ENTRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SUMMARY_EDGES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SUMMARY_BODIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static STORED_CALL_EDGES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PROPAGATED_CALL_EDGES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SNAPSHOT_LEAF_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn plan_block(
    block: &SourceBlock,
    accesses: &Accesses,
    locals: &[LirLocal],
    reference_params: &HashSet<LirLocalId>,
) -> Vec<Node> {
    let mut nodes: Vec<Node> = Vec::new();
    // The not-yet-consumed concatenation node each local currently holds.
    let mut pending: HashMap<LirLocalId, Pending> = HashMap::new();
    // Leaf locals whose reads some node defers, for invalidation on write.
    let mut leaf_readers: HashMap<LirLocalId, Vec<usize>> = HashMap::new();
    let mut first_node_after_barrier = 0;
    let mut read_pending = Vec::new();
    for (index, instruction) in block.instrs.iter().enumerate() {
        let Some((local, value)) = computation(instruction) else {
            // Non-compute instructions may read or write locals through expressions.
            first_node_after_barrier = nodes.len();
            continue;
        };
        let destination = &locals[local.0 as usize];
        let string_result = matches!(destination.ty, Type::String);
        let mut node_parts = concat_parts(value, string_result);
        if !pending.is_empty() {
            for operand in value.iter_operands() {
                for id in operand.iter_locals() {
                    #[cfg(test)]
                    PENDING_OPERAND_VISITS.with(|visits| visits.set(visits.get() + 1));
                    if let Some(child) = pending.get_mut(&id) {
                        if child.reads == 0 {
                            read_pending.push(id);
                        }
                        child.reads = child.reads.saturating_add(1);
                    }
                }
            }
            if let Some(parts) = &mut node_parts {
                for part in parts.iter_mut() {
                    let Part::String(LirOperand::Local(id)) = part else {
                        continue;
                    };
                    let id = *id;
                    #[cfg(test)]
                    PENDING_OPERAND_VISITS.with(|visits| visits.set(visits.get() + 1));
                    let Some(child) = pending.get(&id) else {
                        continue;
                    };
                    // The absorbed value must have no other reader: either this
                    // instruction overwrites it, or it is a single-def, single-use
                    // temporary.
                    let sole_reader = local == id
                        || (accesses.reads[id.0 as usize] == 1
                            && accesses.writes[id.0 as usize] == 1);
                    if child.node >= first_node_after_barrier && child.reads == 1 && sole_reader {
                        *part = Part::Node(child.node);
                    }
                }
            }
        }
        // Retire every read child, including values that escaped absorption.
        for id in read_pending.drain(..) {
            #[cfg(test)]
            PENDING_OPERAND_VISITS.with(|visits| visits.set(visits.get() + 1));
            pending.remove(&id);
        }
        // Writing a local invalidates every deferred read of it.
        if let Some(readers) = leaf_readers.remove(&local) {
            invalidate_readers(readers, &mut nodes, &mut pending);
        }
        pending.remove(&local);
        let Some(parts) = node_parts else {
            continue;
        };
        let id = nodes.len();
        let mut absorbed_any = false;
        for part in &parts {
            if let Part::Node(child) = part {
                nodes[*child].parent = Some(id);
                absorbed_any = true;
            } else if let Some(leaf) = part.leaf_local()
                && !reference_params.contains(&leaf)
            {
                leaf_readers.entry(leaf).or_default().push(id);
            }
        }
        nodes.push(Node {
            index,
            local,
            parts,
            absorbed_any,
            parent: None,
        });
        // Only a compiler temporary may vanish into its consumer.
        if string_result && destination.synthetic && !destination.parameter {
            pending.insert(local, Pending { node: id, reads: 0 });
        }
    }
    nodes
}

fn apply_block(block: &mut SourceBlock, mut nodes: Vec<Node>, snapshots: Vec<(usize, SourceInst)>) {
    if !nodes.iter().any(|node| node.absorbed_any) {
        return;
    }
    let mut removed = vec![false; block.instrs.len()];
    let mut stack = Vec::new();
    for top in 0..nodes.len() {
        if nodes[top].parent.is_some() {
            removed[nodes[top].index] = true;
            continue;
        }
        if !nodes[top].absorbed_any {
            continue;
        }
        // Flatten in source order; every absorbed node is expanded once.
        let mut parts = Vec::new();
        let mut part_types = Vec::new();
        stack.clear();
        stack.extend(std::mem::take(&mut nodes[top].parts).into_iter().rev());
        while let Some(part) = stack.pop() {
            match part {
                Part::Node(child) => {
                    stack.extend(std::mem::take(&mut nodes[child].parts).into_iter().rev());
                }
                Part::String(op) => {
                    parts.push(op);
                    part_types.push(Type::String);
                }
                Part::Integer(op, ty) => {
                    parts.push(op);
                    part_types.push(ty);
                }
            }
        }
        let fused = match (parts.len(), part_types.as_slice()) {
            (1, [Type::String]) => LirRvalue::Use(parts.pop().expect("one part")),
            (2, [Type::String, Type::String]) => {
                let rhs = parts.pop().expect("two parts");
                LirRvalue::Binary {
                    op: BinOp::Add,
                    lhs: parts.pop().expect("two parts"),
                    rhs,
                    operand_ty: Type::String,
                }
            }
            _ => LirRvalue::StringConcat { parts, part_types },
        };
        let SourceInst::Compute { value, .. } = &mut block.instrs[nodes[top].index] else {
            unreachable!("concatenation nodes are Compute instructions");
        };
        *value = fused;
    }
    if snapshots.is_empty() {
        let mut indices = removed.into_iter();
        block.instrs.retain(|_| !indices.next().unwrap());
    } else {
        let mut snapshots = snapshots.into_iter().peekable();
        let old = std::mem::take(&mut block.instrs);
        block.instrs.reserve(old.len() + snapshots.len());
        for (index, inst) in old.into_iter().enumerate() {
            while let Some((_, save)) = snapshots.next_if(|(at, _)| *at == index) {
                block.instrs.push(save);
            }
            if !removed[index] {
                block.instrs.push(inst);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Span;
    use crate::ir::lowered::{
        BlockId, LirLocal, LirPlace, LirStorageKind, SourceProgram, SourceTerminator,
    };

    fn lowered(source: &str) -> SourceProgram {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (ast, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        checker.check_program(&ast);
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        let tables = crate::ir::lower::SemanticFacts::from_checker(&checker);
        let (hir, errors) = crate::ir::lower::lower_program_with(&ast, &tables);
        assert!(errors.is_empty(), "{errors:?}");
        crate::ir::lowered::lower_source_program(&hir)
    }

    fn function<'a>(program: &'a SourceProgram, name: &str) -> &'a SourceFunction {
        program
            .functions
            .iter()
            .find(|f| f.name.is_free_named(name))
            .unwrap()
    }

    /// Every Compute right-hand side of `function` and its defer bodies.
    fn values(function: &SourceFunction) -> Vec<&LirRvalue> {
        let mut values = Vec::new();
        let mut functions = vec![function];
        while let Some(function) = functions.pop() {
            for instruction in function.blocks.iter().flat_map(|block| &block.instrs) {
                match instruction {
                    SourceInst::Compute { value, .. } => values.push(value),
                    SourceInst::Defer { body, .. } => functions.push(&body.function),
                    _ => {}
                }
            }
        }
        values
    }

    /// Part types of each fused concatenation.
    fn concats(function: &SourceFunction) -> Vec<Vec<Type>> {
        values(function)
            .into_iter()
            .filter_map(|value| match value {
                LirRvalue::StringConcat { part_types, .. } => Some(part_types.clone()),
                _ => None,
            })
            .collect()
    }

    fn string_adds(function: &SourceFunction) -> usize {
        values(function)
            .into_iter()
            .filter(|value| {
                matches!(
                    value,
                    LirRvalue::Binary {
                        op: BinOp::Add,
                        operand_ty: Type::String,
                        ..
                    }
                )
            })
            .count()
    }

    /// The deferred operand must read a typed snapshot defined before the
    /// writer. Merely producing one concat would also accept a late alias read.
    fn assert_saved_string(f: &SourceFunction) {
        assert_eq!(string_adds(f), 0);
        assert_eq!(concats(f), vec![vec![S, S, S]]);
        let instrs: Vec<_> = f.blocks.iter().flat_map(|b| &b.instrs).collect();
        let (end, snapshot) = instrs
            .iter()
            .enumerate()
            .find_map(|(i, inst)| {
                let (_, LirRvalue::StringConcat { parts, .. }) = computation(inst)? else {
                    return None;
                };
                let LirOperand::Local(id) = parts[0] else {
                    return None;
                };
                Some((i, id))
            })
            .unwrap();
        let start = instrs[..end]
            .iter()
            .position(|inst| {
                matches!(computation(inst),
            Some((dest, LirRvalue::Use(LirOperand::Local(_)))) if dest == snapshot)
            })
            .unwrap();
        assert!(f.locals[snapshot.0 as usize].synthetic);
        assert!(instrs[start + 1..end].iter().any(|inst| matches!(
            computation(inst),
            Some((
                _,
                LirRvalue::DirectCall { .. }
                    | LirRvalue::IndirectCall { .. }
                    | LirRvalue::IntrinsicCall { .. }
            ))
        )));
    }

    fn i64_to_strings(function: &SourceFunction) -> usize {
        values(function)
            .into_iter()
            .filter(|value| {
                matches!(
                    value,
                    LirRvalue::IntrinsicCall {
                        intrinsic: Intrinsic::I64ToString,
                        ..
                    }
                )
            })
            .count()
    }

    use crate::parser::ast::Type::{I64, String as S};

    #[test]
    fn left_nested_chain_becomes_one_concatenation() {
        let program = lowered(
            r#"fn f(a: String, b: String) -> String { return a + "-" + b + "!"; }
               fn main() { println(f("x", "y")); }"#,
        );
        let f = function(&program, "f");
        assert_eq!(concats(f), vec![vec![S, S, S, S]]);
        assert_eq!(string_adds(f), 0);
    }

    #[test]
    fn two_string_operands_keep_the_pairwise_entry() {
        let program = lowered(
            r#"fn f(a: String, b: String) -> String { return a + b; }
               fn main() { println(f("x", "y")); }"#,
        );
        let f = function(&program, "f");
        assert!(concats(f).is_empty());
        assert_eq!(string_adds(f), 1);
    }

    #[test]
    fn i64_to_string_operands_become_decimal_parts() {
        let program = lowered(
            r#"fn f(a: String, n: i64) -> String { return a + n.to_string(); }
               fn g(n: i64) -> String { return "k" + n.to_string() + ":" + (n * 7).to_string(); }
               fn main() { println(f("x", 1)); println(g(2)); }"#,
        );
        assert_eq!(concats(function(&program, "f")), vec![vec![S, I64]]);
        assert_eq!(concats(function(&program, "g")), vec![vec![S, I64, S, I64]]);
        assert_eq!(i64_to_strings(function(&program, "g")), 0);
    }

    #[test]
    fn a_lone_to_string_is_left_alone() {
        let program = lowered(
            r#"fn f(n: i64) -> String { return n.to_string(); }
               fn main() { println(f(1)); }"#,
        );
        let f = function(&program, "f");
        assert!(concats(f).is_empty());
        assert_eq!(i64_to_strings(f), 1);
    }

    #[test]
    fn right_and_balanced_nesting_flatten_in_source_order() {
        let program = lowered(
            r#"fn r(a: String, b: String, c: String) -> String { return a + (b + c); }
               fn q(a: String, b: String, c: String, n: i64) -> String {
                   return (a + n.to_string()) + (b + c);
               }
               fn main() { println(r("a", "b", "c")); println(q("a", "b", "c", 1)); }"#,
        );
        assert_eq!(concats(function(&program, "r")), vec![vec![S, S, S]]);
        assert_eq!(concats(function(&program, "q")), vec![vec![S, I64, S, S]]);
        let q = function(&program, "q");
        let fused = values(q)
            .into_iter()
            .find_map(|value| match value {
                LirRvalue::StringConcat { parts, .. } => Some(parts.clone()),
                _ => None,
            })
            .unwrap();
        // Parameters are locals 0..4 in declaration order: `a`, decimal `n`,
        // `b`, `c`.
        assert_eq!(fused[0], LirOperand::Local(LirLocalId(0)));
        assert_eq!(fused[2], LirOperand::Local(LirLocalId(1)));
        assert_eq!(fused[3], LirOperand::Local(LirLocalId(2)));
    }

    #[test]
    fn format_fuses_integer_and_string_arguments() {
        let program = lowered(
            r#"fn f(name: String, n: i64) -> String { return format("{}:{}:{}!", name, n, 1.5); }
               fn main() { println(f("x", 1)); }"#,
        );
        let f = function(&program, "f");
        assert_eq!(concats(f), vec![vec![S, S, I64, S, S, S]]);
        // The f64 argument keeps its own formatting call.
        assert!(
            values(f)
                .iter()
                .any(|value| matches!(value, LirRvalue::FormatScalar { ty: Type::F64, .. }))
        );
    }

    #[test]
    fn calls_between_operands_still_fuse() {
        let program = lowered(
            r#"fn p(s: String) -> String { println(s); return s; }
               fn f() -> String { return p("a") + p("b") + p("c"); }
               fn main() { println(f()); }"#,
        );
        assert_eq!(concats(function(&program, "f")), vec![vec![S, S, S]]);
    }

    #[test]
    fn named_intermediates_are_not_absorbed() {
        let program = lowered(
            r#"fn f(a: String, b: String) -> String { let t = a + b; return t + a; }
               fn main() { println(f("x", "y")); }"#,
        );
        let f = function(&program, "f");
        assert!(concats(f).is_empty());
        assert_eq!(string_adds(f), 2);
    }

    #[test]
    fn defer_and_lambda_bodies_fuse() {
        let program = lowered(
            r#"fn f(a: String, n: i64) {
                   defer println(a + "-" + n.to_string());
                   println("body");
               }
               fn main() {
                   let g = |s: String| s + "!" + "?";
                   println(g("x"));
                   f("y", 2);
               }"#,
        );
        assert_eq!(concats(function(&program, "f")), vec![vec![S, S, I64]]);
        let lambda = program
            .lambdas
            .iter()
            .find(|lambda| !concats(&lambda.function).is_empty())
            .expect("fused lambda body");
        assert_eq!(concats(&lambda.function), vec![vec![S, S, S]]);
    }

    // -- hand-built blocks ---------------------------------------------------

    /// `sources` named String locals, then `temps` synthetic String temporaries.
    fn hand_built(sources: u32, temps: u32, instrs: Vec<SourceInst>) -> SourceFunction {
        let mut function = lowered("fn main() {}")
            .functions
            .into_iter()
            .find(|f| f.name.is_free_named("main"))
            .unwrap();
        function.locals = (0..sources + temps)
            .map(|id| LirLocal {
                storage_kind: LirStorageKind::GcOwner,
                id: LirLocalId(id),
                name: format!("l{id}"),
                ty: Type::String,
                source_span: None,
                synthetic: id >= sources,
                parameter: false,
            })
            .collect();
        function.blocks = vec![SourceBlock {
            id: BlockId(0),
            instrs,
            terminator: SourceTerminator::Return(None),
            recovery: Vec::new(),
        }];
        function
    }

    fn local(id: u32) -> LirOperand {
        LirOperand::Local(LirLocalId(id))
    }

    fn compute(target: u32, value: LirRvalue) -> SourceInst {
        SourceInst::Compute {
            local: LirLocalId(target),
            value,
            span: Span::dummy(),
        }
    }

    fn add(target: u32, lhs: LirOperand, rhs: LirOperand) -> SourceInst {
        compute(
            target,
            LirRvalue::Binary {
                op: BinOp::Add,
                lhs,
                rhs,
                operand_ty: Type::String,
            },
        )
    }

    fn fused(instrs: Vec<SourceInst>, sources: u32, temps: u32) -> SourceFunction {
        let mut function = hand_built(sources, temps, instrs);
        fuse_string_concats(&mut function);
        function
    }

    fn return_local(id: u32) -> SourceTerminator {
        use crate::ir::typed_ast::{HirExpr, HirExprKind};
        SourceTerminator::Return(Some(HirExpr {
            kind: HirExprKind::Var(format!("l{id}")),
            ty: Type::String,
            span: Span::dummy(),
        }))
    }

    #[test]
    fn return_immediately_after_definition_counts_as_a_read() {
        let mut function = hand_built(2, 1, vec![add(2, local(0), local(1))]);
        function.blocks[0].terminator = return_local(2);
        let accesses = Accesses::count(&function);
        assert_eq!(accesses.reads, [1, 1, 1]);
        assert_eq!(accesses.writes, [0, 0, 1]);
    }

    #[test]
    fn return_from_an_empty_block_counts_its_own_read() {
        let mut function = hand_built(2, 1, vec![add(2, local(0), local(1))]);
        function.blocks[0].terminator = SourceTerminator::Jump(BlockId(1));
        function.blocks.push(SourceBlock {
            id: BlockId(1),
            instrs: Vec::new(),
            terminator: return_local(2),
            recovery: Vec::new(),
        });
        let accesses = Accesses::count(&function);
        assert_eq!(accesses.reads, [1, 1, 1]);
        assert_eq!(accesses.writes, [0, 0, 1]);
    }

    #[test]
    fn a_temporary_read_twice_is_not_absorbed() {
        let f = fused(
            vec![
                add(3, local(0), local(1)),
                add(4, local(3), local(2)),
                add(5, local(3), local(0)),
            ],
            3,
            3,
        );
        assert!(concats(&f).is_empty());
        assert_eq!(f.blocks[0].instrs.len(), 3);
    }

    #[test]
    fn pending_operand_work_for_repeated_call_arguments() {
        let callee = FunctionId::free_from_source_name("consume");
        for arity in [1, 8, 64, 512, 4096] {
            for live in [false, true] {
                let mut instructions = Vec::new();
                let argument = if live {
                    instructions.push(add(2, local(0), local(1)));
                    local(2)
                } else {
                    local(0)
                };
                instructions.push(compute(
                    3,
                    LirRvalue::DirectCall {
                        callee,
                        args: vec![argument; arity],
                        params: vec![S; arity],
                        result: S,
                    },
                ));
                let f = hand_built(2, 2, instructions);
                let accesses = Accesses::count(&f);
                PENDING_OPERAND_VISITS.set(0);
                let nodes = plan_block(&f.blocks[0], &accesses, &f.locals, &HashSet::new());
                assert_eq!(nodes.len(), usize::from(live));
                // A live child is read at every position and retired just once.
                assert_eq!(
                    PENDING_OPERAND_VISITS.get(),
                    if live { arity + 1 } else { 0 }
                );
            }
        }
    }

    #[test]
    fn wide_concat_counts_pending_children_in_linear_work() {
        for width in [4, 32, 256, 2048] {
            let sources = width + 1;
            let children = sources..sources + width;
            let mut instructions: Vec<_> = children
                .clone()
                .enumerate()
                .map(|(i, child)| add(child, local(i as u32), local(i as u32 + 1)))
                .collect();
            instructions.push(compute(
                sources + width,
                LirRvalue::StringConcat {
                    parts: children.map(local).collect(),
                    part_types: vec![S; width as usize],
                },
            ));
            let mut function = hand_built(sources, width + 1, instructions);
            PENDING_OPERAND_VISITS.with(|visits| visits.set(0));
            fuse_string_concats(&mut function);
            // The first two leaves have no pending children. Each subsequent
            // leaf is counted and checked, and each child is retired once.
            assert_eq!(
                PENDING_OPERAND_VISITS.with(|visits| visits.get()),
                7 * width as usize - 4
            );
            let [
                SourceInst::Compute {
                    value: LirRvalue::StringConcat { parts, .. },
                    ..
                },
            ] = function.blocks[0].instrs.as_slice()
            else {
                panic!("expected one fused concat")
            };
            let expected: Vec<_> = (0..width).flat_map(|i| [local(i), local(i + 1)]).collect();
            assert_eq!(parts, &expected);
        }
    }

    #[test]
    fn a_double_read_in_one_instruction_is_not_absorbed() {
        let f = fused(
            vec![add(3, local(0), local(1)), add(4, local(3), local(3))],
            3,
            2,
        );
        assert!(concats(&f).is_empty());
    }

    #[test]
    fn repeated_child_stays_materialized_when_its_sibling_fuses() {
        for repeats in [2, 257] {
            let mut parts = vec![local(2); repeats];
            parts.push(local(3));
            let f = fused(
                vec![
                    add(2, local(0), local(1)),
                    add(3, local(1), local(0)),
                    compute(
                        4,
                        LirRvalue::StringConcat {
                            part_types: vec![S; parts.len()],
                            parts,
                        },
                    ),
                ],
                2,
                3,
            );
            let [
                SourceInst::Compute { local: kept, .. },
                SourceInst::Compute {
                    value: LirRvalue::StringConcat { parts, .. },
                    ..
                },
            ] = f.blocks[0].instrs.as_slice()
            else {
                panic!("expected a retained child and a fused parent")
            };
            assert_eq!(*kept, LirLocalId(2));
            let mut expected = vec![local(2); repeats];
            expected.extend([local(1), local(0)]);
            assert_eq!(parts, &expected);
        }
    }

    #[test]
    fn reference_writes_share_one_snapshot_of_the_same_value() {
        for width in [4, 32, 256] {
            // Half the children read l0 before the same write. Its saved
            // pointer is shared. l1 and l2 can be read at the consumer.
            let children = 3..3 + 2 * width;
            let last = children.end;
            let mut instrs: Vec<_> = children
                .clone()
                .map(|id| add(id, local((id - 3) % 2), local(2)))
                .collect();
            let call = LirRvalue::DirectCall {
                callee: crate::semantic::ids::FunctionId::free_from_source_name("change"),
                args: vec![LirOperand::Reference {
                    place: LirPlace::Local(LirLocalId(0)),
                    span: Span::dummy(),
                    display: "l0".into(),
                }],
                params: vec![S],
                result: S,
            };
            instrs.extend((0..width).map(|_| compute(last, call.clone())));
            instrs.push(compute(
                last + 1,
                LirRvalue::StringConcat {
                    parts: children.map(local).collect(),
                    part_types: vec![S; (2 * width) as usize],
                },
            ));
            let mut function = hand_built(3, 2 * width + 2, instrs);
            SNAPSHOT_LEAF_VISITS.set(0);
            fuse_string_concats(&mut function);
            assert_eq!(SNAPSHOT_LEAF_VISITS.get(), 4 * width as usize);
            assert_eq!(string_adds(&function), 0);
            let snapshot_id = last + 2;
            assert_eq!(function.locals.len(), snapshot_id as usize + 1);
            assert!(matches!(computation(&function.blocks[0].instrs[0]),
                Some((id, LirRvalue::Use(op))) if id.0 == snapshot_id && op == &local(0)));
            let Some(SourceInst::Compute {
                value: LirRvalue::StringConcat { parts, .. },
                ..
            }) = function.blocks[0].instrs.last()
            else {
                panic!("expected the fused parent")
            };
            let expected: Vec<_> = (0..width)
                .flat_map(|_| [local(snapshot_id), local(2), local(1), local(2)])
                .collect();
            assert_eq!(parts, &expected);
        }
    }

    #[test]
    fn repeated_barriers_copy_each_part_once() {
        for groups in [1, 2, 4, 8, 16, 32] {
            let expression = vec![r#"x + ":" + helper(n)"#; groups].join(" + ");
            let source = format!(
                r#"
                import std::collections::Array;
                class Holder {{ pub items: Array<String>; }}
                fn grow(items:Array<String>, n:String) {{ items.push(n); }}
                fn helper(n:String)->String {{
                    let box = new Holder([n,n]);
                    if n.len() == 8 {{return n;}}
                    let items = box.items;
                    grow(items,n);
                    if items.len() == 0 {{return "empty";}}
                    return n;
                }}
                fn render(x:&String,n:String)->String {{return {expression};}}
            "#
            );
            let program = lowered(&source);
            let render = function(&program, "render");
            assert_eq!(string_adds(render), 0);
            assert_eq!(concats(render), vec![vec![S; 3 * groups]]);
            let mut saved = Vec::new();
            let mut calls = 0;
            for inst in render.blocks.iter().flat_map(|b| &b.instrs) {
                match computation(inst) {
                    Some((id, LirRvalue::Use(op))) if op == &local(0) => {
                        assert_eq!(saved.len(), calls);
                        saved.push(id);
                    }
                    Some((_, LirRvalue::DirectCall { callee, .. }))
                        if callee.is_free_named("helper") =>
                    {
                        calls += 1;
                        assert_eq!(saved.len(), calls);
                    }
                    Some((_, LirRvalue::StringConcat { parts, .. })) => {
                        assert_eq!(calls, groups);
                        for (part, &id) in parts.chunks_exact(3).zip(&saved) {
                            assert_eq!(part[0], LirOperand::Local(id));
                        }
                    }
                    _ => {}
                }
            }
            assert_eq!(calls, groups);
        }
    }

    #[test]
    fn borrowed_string_stays_fused_across_read_only_direct_calls() {
        let program = lowered(
            r#"
            fn helper(n: i64) -> String {
                if n == 0 { return "zero"; }
                return n.to_string();
            }
            fn wrapper(n: i64) -> String { return helper(n); }
            fn read(x: &String) -> String { return x; }
            fn render(x: &String, n: i64) -> String { return x + ":" + wrapper(n); }
            fn local() -> String { let x = "old"; return x + ":" + read(&x); }
            fn change(x: &mut String) -> String { x = "new"; return "!"; }
            fn forward(x: &mut String) -> String { return change(&x); }
            fn write(x: &mut String) -> String { return x + ":" + forward(&x); }
        "#,
        );
        let render = function(&program, "render");
        assert!(values(render).iter().any(
            |v| matches!(v, LirRvalue::DirectCall { callee, .. } if callee.is_free_named("wrapper"))
        ));
        assert_eq!(concats(render), vec![vec![S, S, S]]);
        assert_eq!(concats(function(&program, "local")), vec![vec![S, S, S]]);
        assert_saved_string(function(&program, "write"));
    }

    #[test]
    fn borrowed_string_stays_fused_across_string_length_call() {
        let program = lowered(
            r#"
            fn helper(n: String) -> String {
                if n.len() == 0 { return "empty"; }
                return n;
            }
            fn render(x: &String, n: String) -> String { return x + ":" + helper(n); }
        "#,
        );
        assert!(
            values(function(&program, "helper"))
                .iter()
                .any(|v| matches!(
                    v,
                    LirRvalue::IntrinsicCall {
                        intrinsic: Intrinsic::StringLen,
                        ..
                    }
                ))
        );
        let render = function(&program, "render");
        assert!(values(render).iter().any(
            |v| matches!(v, LirRvalue::DirectCall { callee, .. } if callee.is_free_named("helper"))
        ));
        assert_eq!(concats(render), vec![vec![S, S, S]]);
        assert_eq!(string_adds(render), 0);
    }

    #[test]
    fn fresh_aggregate_stores_allow_fusion_but_stores_to_inputs_do_not() {
        let program = lowered(
            r#"
            import std::collections::Array;
            class Cell { pub text: String; }
            enum Pair { Values(String, String) }
            fn helper(n: String) -> String {
                let items = [n, n];
                if items.len() == 0 { return "empty"; }
                return n;
            }
            fn render(x: &String, n: String) -> String { return x + ":" + helper(n); }
            fn fresh_object(n: String) -> String {
                let cell = new Cell(n);
                return cell.text;
            }
            fn fresh_enum(n: String) -> Pair { return Pair::Values(n, n); }
            fn replace(items: Array<String>) -> String { items[0] = "new"; return "!"; }
            fn write(x: &String, items: Array<String>) -> String {
                return x + ":" + replace(items);
            }
        "#,
        );
        let helper = function(&program, "helper");
        assert_eq!(
            values(helper)
                .iter()
                .filter(|v| matches!(v, LirRvalue::ArrayStore { .. }))
                .count(),
            2
        );
        let render = function(&program, "render");
        assert!(
            values(render).iter().any(
                |v| matches!(v, LirRvalue::DirectCall { callee, .. } if *callee == helper.name)
            )
        );
        assert_eq!(concats(render), vec![vec![S, S, S]]);
        assert_eq!(string_adds(render), 0);
        for name in ["fresh_object", "fresh_enum"] {
            assert!(!body_may_write(function(&program, name)), "{name}");
        }
        assert_saved_string(function(&program, "write"));
    }

    #[test]
    fn stores_follow_named_allocations_across_branches_joins_and_loops() {
        let program = lowered(
            r#"
            import std::collections::Array;
            fn helper(n: String) -> String {
                let items = [n, n];
                if n.len() == 8 { return n; }
                items[0] = n;
                if items.len() == 0 { return "empty"; }
                return n;
            }
            fn render(x: &String, n: String) -> String { return x + ":" + helper(n); }
            fn join(flag: bool, n: String, input: Array<String>) -> String {
                let mut items = input;
                if flag { items = [n]; } else { items = [n,n]; }
                items[0] = n;
                return n;
            }
            fn mixed(flag: bool, n: String, input: Array<String>) -> String {
                let mut items = [n];
                if flag { items = input; }
                items[0] = n;
                return n;
            }
            fn loop_fresh(limit: i64, n: String) -> String {
                let mut items = [n];
                let mut i = 0;
                while i < limit { items[0] = n; i = i + 1; }
                return n;
            }
            fn loop_alias(limit: i64, n: String, input: Array<String>) -> String {
                let mut items = [n];
                let mut i = 0;
                while i < limit { items[0] = n; items = input; i = i + 1; }
                return n;
            }
        "#,
        );
        let render = function(&program, "render");
        assert!(values(render).iter().any(
            |v| matches!(v, LirRvalue::DirectCall { callee, .. } if callee.is_free_named("helper"))
        ));
        assert_eq!(concats(render), vec![vec![S, S, S]]);
        for name in ["helper", "join", "loop_fresh"] {
            assert!(!body_may_write(function(&program, name)), "{name}");
        }
        for name in ["mixed", "loop_alias"] {
            assert!(body_may_write(function(&program, name)), "{name}");
        }
    }

    #[test]
    fn direct_call_results_prove_freshness_on_every_return_path() {
        let program = lowered(
            r#"
            import std::collections::Array;
            fn make(n: String) -> Array<String> {
                if n.len() == 0 { return [n]; }
                return [n,n];
            }
            fn wrap(n: String) -> Array<String> { return make(n); }
            fn mixed(n: String, input: Array<String>) -> Array<String> {
                if n.len() == 8 { return input; }
                return make(n);
            }
            fn wrap_mixed(n: String, input: Array<String>) -> Array<String> {
                return mixed(n, input);
            }
            fn push_new(n: String) -> String {
                let items = wrap(n);
                if n.len() == 8 { return n; }
                items.push(n);
                return n;
            }
            fn push_mixed(n: String, input: Array<String>) -> String {
                let items = wrap_mixed(n,input);
                items.push(n);
                return n;
            }
            fn fresh(x: &String,n: String) -> String {return x+":"+push_new(n);}
            fn borrowed(x: &String,n: String,a: Array<String>) -> String {return x+":"+push_mixed(n,a);}
        "#,
        );
        let direct = program.functions.iter().map(|f| f.name).collect();
        let callees = summarize_callees(&program.functions, direct);
        for name in ["make", "wrap", "mixed", "wrap_mixed"] {
            let out = callees.functions[&function(&program, name).name];
            assert!(callees.graph.facts[out.writes].is_empty());
            let expected = if name.contains("mixed") {
                HashSet::from([Fact::Value(1)])
            } else {
                HashSet::new()
            };
            assert_eq!(callees.graph.facts[out.result], expected, "{name}");
        }
        assert_eq!(concats(function(&program, "fresh")), vec![vec![S, S, S]]);
        assert_saved_string(function(&program, "borrowed"));
    }

    #[test]
    fn receiver_mutations_use_the_array_origin_across_a_branch() {
        let program = lowered(
            r#"
            import std::collections::Array;
            fn helper(n: String) -> String {
                let items = [n, n];
                if n.len() == 8 { return n; }
                items.push(n);
                if items.len() == 0 { return "empty"; }
                return n;
            }
            fn render(x: &String, n: String) -> String { return x + ":" + helper(n); }
            fn grow(items: Array<String>, n: String) -> String { items.push(n); return n; }
            fn write(x: &String, items: Array<String>, n: String) -> String {
                return x + ":" + grow(items, n);
            }
        "#,
        );
        for (name, writes) in [("helper", false), ("grow", true)] {
            let f = function(&program, name);
            let (resize, receiver) = values(f)
                .into_iter()
                .find_map(|v| {
                    if let LirRvalue::IntrinsicCall {
                        intrinsic: Intrinsic::ArrayResize,
                        receiver: LirOperand::Local(id),
                        ..
                    } = v
                    {
                        Some((v, *id))
                    } else {
                        None
                    }
                })
                .expect("push must resize the array receiver");
            // Check resize independently of the element store also emitted by push.
            for origin in [Origin::Fresh, Origin::Unknown, Origin::Incoming(receiver)] {
                let mut state = BlockOrigins::default();
                state.set(receiver, origin);
                assert!(
                    may_write_aliased_storage(
                        LirLocalId(receiver.0 + 1),
                        resize,
                        &mut state,
                        &mut FieldOrigins::default(),
                        |_, _, _, _| CallTransfer::Opaque,
                    ) == origin
                );
            }
            assert_eq!(body_may_write(f), writes, "{name}");
        }
        assert_eq!(concats(function(&program, "render")), vec![vec![S, S, S]]);
        assert_saved_string(function(&program, "write"));
    }

    /// A graph with an allocation per local at entry and a store per local
    /// in the exit block. Only the intervening control flow varies.
    fn array_stores_after(locals: usize, terms: Vec<SourceTerminator>) -> SourceFunction {
        let ids = 0..locals as u32;
        let allocations = ids
            .clone()
            .map(|id| {
                compute(
                    id,
                    LirRvalue::ArrayAlloc {
                        length: 1,
                        element: Type::I64,
                    },
                )
            })
            .collect();
        let mut f = hand_built(0, locals as u32 + 1, allocations);
        for array in &mut f.locals[..locals] {
            array.ty = Type::Array(Box::new(Type::I64));
        }
        f.locals[locals].ty = Type::Void;
        f.locals[locals].storage_kind = LirStorageKind::Value;
        for (b, term) in terms.into_iter().enumerate() {
            f.blocks[b].terminator = term;
            f.blocks.push(SourceBlock {
                id: BlockId(b + 1),
                instrs: vec![],
                recovery: vec![],
                terminator: SourceTerminator::Return(None),
            });
        }
        f.blocks.last_mut().unwrap().instrs.extend(ids.map(|id| {
            compute(
                locals as u32,
                LirRvalue::ArrayStore {
                    array: local(id),
                    index: LirOperand::Int(0),
                    value: LirOperand::Int(1),
                    element: Type::I64,
                },
            )
        }));
        f
    }

    #[test]
    fn independent_origins_share_unchanged_blocks() {
        for (blocks, locals) in [(8, 8), (64, 8), (8, 64), (64, 64), (512, 512), (2048, 2048)] {
            let f = array_stores_after(
                locals,
                (1..=blocks + 1)
                    .map(|id| SourceTerminator::Jump(BlockId(id)))
                    .collect(),
            );
            ORIGIN_EDGE_VISITS.set(0);
            assert!(!body_may_write(&f));
            assert_eq!(ORIGIN_EDGE_VISITS.get(), blocks + locals);
            assert_eq!(ORIGIN_RETAINED.get(), (blocks + 2, 1, locals, locals, 0));
        }
    }

    #[test]
    fn diamonds_share_agreeing_origins_for_independent_stores() {
        use crate::ir::typed_ast::{HirExpr, HirExprKind};
        for (diamonds, locals) in [(8, 8), (64, 8), (8, 64), (64, 64), (512, 512), (2048, 2048)] {
            let terms = (0..diamonds)
                .flat_map(|d| {
                    let b = 3 * d;
                    [
                        SourceTerminator::Branch {
                            cond: HirExpr {
                                kind: HirExprKind::Bool(true),
                                ty: Type::Bool,
                                span: Span::dummy(),
                            },
                            then_block: BlockId(b + 1),
                            else_block: BlockId(b + 2),
                        },
                        SourceTerminator::Jump(BlockId(b + 3)),
                        SourceTerminator::Jump(BlockId(b + 3)),
                    ]
                })
                .collect();
            let f = array_stores_after(locals, terms);
            ORIGIN_EDGE_VISITS.set(0);
            assert!(!body_may_write(&f));
            assert_eq!(ORIGIN_EDGE_VISITS.get(), 4 * diamonds);
            assert_eq!(
                ORIGIN_RETAINED.get(),
                (3 * diamonds + 1, 1, locals, locals, 0)
            );
        }
    }

    #[test]
    fn dependency_discovery_deduplicates_repeated_arguments_and_stores() {
        let factory = FunctionId::free_from_source_name("factory");
        let consumer = FunctionId::free_from_source_name("consumer");
        let prefix_call = FunctionId::free_from_source_name("prefix_call");
        let array = Type::Array(Box::new(I64));
        for (arity, stores) in [
            (1, 0),
            (0, 1),
            (1, 1),
            (8, 1),
            (64, 1),
            (512, 1),
            (1, 512),
            (512, 512),
        ] {
            let mut f = array_stores_after(1, vec![SourceTerminator::Jump(BlockId(1))]);
            f.locals[0].storage_kind = LirStorageKind::Value;
            f.blocks[0].instrs[0] = compute(
                0,
                LirRvalue::DirectCall {
                    callee: factory,
                    args: vec![],
                    params: vec![],
                    result: array.clone(),
                },
            );
            let instrs = &mut f.blocks[1].instrs;
            instrs.resize(stores, instrs[0].clone());
            // This unqueried call lies in the demanded prefix only if a later
            // argument supplies an owner. A call with no arguments adds no prefix.
            instrs.push(compute(
                1,
                LirRvalue::DirectCall {
                    callee: prefix_call,
                    args: vec![],
                    params: vec![],
                    result: Type::Void,
                },
            ));
            instrs.push(compute(
                1,
                LirRvalue::DirectCall {
                    callee: consumer,
                    args: vec![local(0); arity],
                    params: vec![array.clone(); arity],
                    result: Type::Void,
                },
            ));
            let mut sites: HashSet<_> = (0..stores).map(|i| (1, i)).collect();
            sites.insert((1, stores + 1));
            let roots = collect_dependencies(&f, &HashSet::new(), &sites);
            assert_eq!(DEPENDENCY_QUERIES.get(), (1, 1, 2));
            let mut expected = HashSet::from([factory, consumer]);
            if arity > 0 {
                expected.insert(prefix_call);
            }
            assert_eq!(roots, expected);
        }
    }

    #[test]
    fn returned_callees_are_discovered_through_shared_join_inputs() {
        let left = FunctionId::free_from_source_name("left");
        let right = FunctionId::free_from_source_name("right");
        let unused = FunctionId::free_from_source_name("unqueried");
        for (width, arms) in [(1, 2), (8, 2), (64, 2), (512, 2), (64, 8), (64, 32)] {
            // A diamond followed by a binary branch tree. All its leaves
            // inherit the diamond's join but disagree on an unrelated local.
            let mut preds = vec![vec![], vec![0], vec![0], vec![1, 2]];
            for parent in 3..3 + arms - 1 {
                preds.extend([vec![parent], vec![parent]]);
            }
            let join = preds.len();
            let leaves = join - arms..join;
            preds.push(leaves.clone().collect());
            let mut exits: Vec<_> = preds.iter().map(|_| BlockOrigins::default()).collect();
            exits[0].unknown_entry = true;
            for i in 0..width {
                let id = LirLocalId(i);
                exits[1].set(id, Origin::Returned(left));
                exits[2].set(
                    id,
                    if i % 2 == 0 {
                        Origin::Returned(right)
                    } else {
                        Origin::Unknown
                    },
                );
            }
            let other = LirLocalId(width);
            for (i, block) in leaves.enumerate() {
                exits[block].set(
                    other,
                    if i % 2 == 0 {
                        Origin::Fresh
                    } else {
                        Origin::Returned(unused)
                    },
                );
            }
            let entries = (0..width).map(|i| (join, LirLocalId(i)));
            let mut callees = HashSet::new();
            collect_returned_callees(&exits, &preds, entries.collect(), &mut callees);
            assert_eq!(callees, HashSet::from([left, right]));
            // All final arms route to the same input per local. The scan
            // retains a key for each distinct entry, with no per-edge payload.
            assert_eq!(
                CALLEE_SCAN.get(),
                (2 * width as usize, arms * width as usize)
            );
        }
    }

    #[test]
    fn equivalent_definitions_on_distinct_diamond_arms_share_the_join() {
        for (diamonds, locals) in [(8, 64), (64, 8), (512, 512)] {
            let size = 3 * diamonds + 1;
            let mut exits: Vec<_> = (0..size).map(|_| BlockOrigins::default()).collect();
            exits[0].unknown_entry = true;
            for id in 0..locals {
                exits[0].set(LirLocalId(id as u32), Origin::Fresh);
            }
            let mut preds = vec![vec![]; size];
            for d in 0..diamonds {
                let b = 3 * d;
                let value = if d % 2 == 0 {
                    Origin::Fresh
                } else {
                    Origin::Unknown
                };
                for arm in [b + 1, b + 2] {
                    preds[arm].push(b);
                    exits[arm].set(LirLocalId(locals as u32), value);
                }
                preds[b + 3].extend([b + 1, b + 2]);
            }
            ORIGIN_EDGE_VISITS.set(0);
            let stores = (0..locals).map(|i| (size - 1, LirLocalId(i as u32)));
            let mut callees = HashSet::new();
            collect_returned_callees(&exits, &preds, stores.clone().collect(), &mut callees);
            assert!(callees.is_empty());
            assert_eq!(ORIGIN_EDGE_VISITS.get(), 4 * diamonds);
            assert_eq!(CALLEE_SCAN.get(), (locals, 0));
            let mut index = ReachingDefinitions::new(&exits, &preds);
            for entry in stores {
                index.visit_inputs(entry, |origin| assert!(origin == Origin::Fresh));
            }
        }
    }

    #[test]
    fn repeated_origin_queries_share_the_predecessor_walk() {
        for n in [8, 64, 512, 2048] {
            let id = LirLocalId(0);
            let mut exits: Vec<_> = (0..n).map(|_| BlockOrigins::default()).collect();
            exits[0].set(id, Origin::Fresh);
            let mut preds = vec![Vec::new(); n];
            for (i, row) in preds.iter_mut().enumerate().skip(1) {
                row.push(i - 1);
            }
            ORIGIN_EDGE_VISITS.set(0);
            let entry = (n - 1, id);
            let mut callees = HashSet::new();
            collect_returned_callees(
                &exits,
                &preds,
                std::iter::repeat_n(entry, n).collect(),
                &mut callees,
            );
            assert!(callees.is_empty());
            assert_eq!(CALLEE_SCAN.get(), (1, 0));
            assert_eq!(ORIGIN_EDGE_VISITS.get(), n - 1);
            ReachingDefinitions::new(&exits, &preds)
                .visit_inputs(entry, |origin| assert!(origin == Origin::Fresh));
        }
    }

    #[test]
    fn allocation_proof_is_lost_on_rebinding_or_an_unknown_writer() {
        let mut fresh = BlockOrigins {
            unknown_entry: true,
            ..Default::default()
        };
        let alloc = LirRvalue::ArrayAlloc {
            length: 1,
            element: S,
        };
        let store = LirRvalue::ArrayStore {
            array: local(1),
            index: LirOperand::Int(0),
            value: local(3),
            element: S,
        };
        let call = LirRvalue::DirectCall {
            callee: FunctionId::free_from_source_name("unknown"),
            args: vec![],
            params: vec![],
            result: S,
        };
        let mut writes = |dest, value: &LirRvalue| {
            may_write_aliased_storage(
                LirLocalId(dest),
                value,
                &mut fresh,
                &mut FieldOrigins::default(),
                |_, _, _, _| CallTransfer::Opaque,
            ) != Origin::Fresh
        };
        assert!(!writes(0, &alloc));
        assert!(!writes(1, &LirRvalue::Use(local(0))));
        assert!(!writes(2, &store));
        assert!(!writes(1, &LirRvalue::Use(local(3))));
        assert!(writes(2, &store));
        assert!(!writes(1, &alloc));
        assert!(writes(0, &call));
        assert!(writes(2, &store));
    }

    #[test]
    fn mutating_intrinsics_and_unresolved_calls_block_borrowed_reads() {
        let program = lowered(
            r#"
            import std::collections::Map;
            fn store(m: Map<String, i64>) -> String {
                m.insert("k", 1);
                return "!";
            }
            fn via_map(x: &String, m: Map<String, i64>) -> String {
                return x + ":" + store(m);
            }
            fn via_callback(x: &String, f: fn() -> String) -> String {
                return x + ":" + f();
            }
        "#,
        );
        assert!(values(function(&program, "store")).iter().any(|v| matches!(
            v,
            LirRvalue::IntrinsicCall {
                intrinsic: Intrinsic::MapInsert,
                ..
            }
        )));
        for name in ["via_map", "via_callback"] {
            assert_saved_string(function(&program, name));
        }
    }

    #[test]
    fn fresh_intrinsic_results_and_written_arguments_preserve_concat_fusion() {
        let program = lowered(
            r#"
            import std::collections::Array;
            fn grow(other:Array<String>, items:Array<String>, n:String) {
                items.push(n);
                items[0] = n;
            }
            fn forward(a:Array<String>, b:Array<String>, n:String) { grow(b,a,n); }
            fn split(n:String) -> String {
                let items=n.split(",");
                if n.len() == 8 {return n;}
                items.push(n);
                return n;
            }
            fn private(other:Array<String>, n:String) -> String {
                let items=[n,n]; forward(items,other,n); return n;
            }
            fn shared(other:Array<String>, n:String) -> String {
                let items=[n,n]; forward(other,items,n); return n;
            }
            fn mixed(other:Array<String>,n:String) -> String {
                let mut items=n.split(",");
                if n.len() == 8 { items=other; }
                forward(items,other,n); return n;
            }
            fn via_split(x:&String,n:String)->String { return x+":"+split(n); }
            fn via_private(x:&String,n:String,a:Array<String>)->String { return x+":"+private(a,n); }
            fn via_shared(x:&String,n:String,a:Array<String>)->String { return x+":"+shared(a,n); }
            fn via_mixed(x:&String,n:String,a:Array<String>)->String { return x+":"+mixed(a,n); }
        "#,
        );
        for name in ["via_split", "via_private"] {
            assert_eq!(
                concats(function(&program, name)),
                vec![vec![S, S, S]],
                "{name}"
            );
        }
        for name in ["via_shared", "via_mixed"] {
            assert_saved_string(function(&program, name));
        }
    }

    #[test]
    fn a_mutating_intrinsic_can_return_a_new_container() {
        let program = lowered(
            r#"
            import std::collections::Map;
            fn take(m:Map<String,String>)->Option<String> { return m.remove("k"); }
        "#,
        );
        let take = function(&program, "take");
        let summaries = summarize_callees(&program.functions, HashSet::from([take.name]));
        let out = summaries.functions[&take.name];
        assert_eq!(
            summaries.graph.facts[out.writes],
            HashSet::from([Fact::Value(0)])
        );
        assert!(summaries.graph.facts[out.result].is_empty());
    }

    #[test]
    fn fresh_arguments_survive_calls_before_the_deferred_read() {
        let program = lowered(
            r#"
            import std::collections::Array;
            fn label(n:String)->String {if n.len()==0 {return "ok";} return n;}
            fn grow(a:Array<String>,n:String)->String {a.push(n); return n;}
            fn local(x:&String,n:String)->String {
                let a=n.split(","); let s=label(n); return x+":"+grow(a,s);
            }
            fn branched(x:&String,n:String)->String {
                let a=n.split(","); let s=label(n); if s.len()==8 {return n;}
                return x+":"+grow(a,s);
            }
        "#,
        );
        for name in ["local", "branched"] {
            assert_eq!(
                concats(function(&program, name)),
                vec![vec![S, S, S]],
                "{name}"
            );
        }
    }

    #[test]
    fn field_reads_use_the_last_stored_owner_on_the_same_object() {
        let program = lowered(
            r#"
            import std::collections::Array;
            class Holder { pub items: Array<String>; }
            class Pair { pub left: Array<String>; pub right: Array<String>; }
            fn grow(items:Array<String>,n:String)->String { items.push(n); return n; }
            fn replace(h:Holder,a:Array<String>) { h.items=a; }
            fn private(n:String, other:Array<String>)->String {
                let a=new Holder([n,n]); let b=new Holder(other);
                return grow(a.items,n);
            }
            fn shared(n:String, other:Array<String>)->String {
                let a=new Holder([n,n]); let b=new Holder(other);
                return grow(b.items,n);
            }
            fn overwritten(n:String, other:Array<String>)->String {
                let a=new Holder([n,n]); let alias=a; alias.items=other;
                return grow(a.items,n);
            }
            fn replaced(n:String, other:Array<String>)->String {
                let a=new Holder([n,n]); replace(a,other);
                return grow(a.items,n);
            }
            fn restored(n:String, other:Array<String>)->String {
                let a=new Holder(other); let alias=a; alias.items=[n,n];
                return grow(a.items,n);
            }
            fn snapshot(n:String, other:Array<String>)->String {
                let mut items=[n,n]; let a=new Holder(items); items=other;
                return grow(a.items,n);
            }
            fn left(n:String, other:Array<String>)->String {
                let h=new Pair([n,n],other); return grow(h.left,n);
            }
            fn right(n:String, other:Array<String>)->String {
                let h=new Pair([n,n],other); return grow(h.right,n);
            }
            fn via_left(x:&String,n:String,a:Array<String>)->String { return x+":"+left(n,a); }
            fn via_right(x:&String,n:String,a:Array<String>)->String { return x+":"+right(n,a); }
            fn via_private(x:&String,n:String,a:Array<String>)->String { return x+":"+private(n,a); }
            fn via_shared(x:&String,n:String,a:Array<String>)->String { return x+":"+shared(n,a); }
            fn via_overwrite(x:&String,n:String,a:Array<String>)->String { return x+":"+overwritten(n,a); }
            fn via_replace(x:&String,n:String,a:Array<String>)->String { return x+":"+replaced(n,a); }
            fn via_restore(x:&String,n:String,a:Array<String>)->String { return x+":"+restored(n,a); }
            fn via_snapshot(x:&String,n:String,a:Array<String>)->String { return x+":"+snapshot(n,a); }
        "#,
        );
        for name in ["via_private", "via_restore", "via_snapshot", "via_left"] {
            assert_eq!(
                concats(function(&program, name)),
                vec![vec![S, S, S]],
                "{name}"
            );
        }
        for name in ["via_shared", "via_overwrite", "via_replace", "via_right"] {
            assert_saved_string(function(&program, name));
        }
    }

    #[test]
    fn a_field_read_before_a_private_allocation_adds_no_callee_dependency() {
        SUMMARY_BODIES.set(0);
        let program = lowered(
            r#"
            import std::collections::Array;
            class Holder {pub items:Array<String>;}
            fn label(a:Array<String>,n:String)->String {if a.len()==0{return "empty";} return n;}
            fn grow(a:Array<String>,n:String)->String {a.push(n);return n;}
            fn render(x:&String,n:String,h:Holder)->String {
                let value=label(h.items,n);
                let a=[n,n]; return x+":"+grow(a,value);
            }
        "#,
        );
        let render = function(&program, "render");
        let label = function(&program, "label").name;
        assert!(
            values(render)
                .iter()
                .any(|v| matches!(v, LirRvalue::DirectCall{callee,..} if *callee == label))
        );
        assert_eq!(concats(render), vec![vec![S, S, S]]);
        assert_eq!(SUMMARY_BODIES.get(), 1);
    }

    #[test]
    fn distinct_objects_and_repeated_field_reads_use_one_lookup_per_access() {
        for (objects, reads) in [(1, 1), (8, 64), (64, 8), (128, 256)] {
            let mut source = String::from(
                r#"
                import std::collections::Array;
                class Holder {pub items:Array<String>;}
                fn helper(n:String)->String { let items=[n,n];
            "#,
            );
            for i in 0..objects {
                source.push_str(&format!("let h{i}=new Holder(items);\n"));
            }
            for i in 0..reads {
                source.push_str(&format!("h{}.items.push(n);\n", i % objects));
            }
            source.push_str(
                r#"return n;}
                fn render(x:&String,n:String)->String {return x+":"+helper(n);}"#,
            );
            FIELD_STORES.set(0);
            FIELD_READS.set(0);
            PEAK_FIELDS.set(0);
            let program = lowered(&source);
            assert_eq!(concats(function(&program, "render")), vec![vec![S, S, S]]);
            assert_eq!(
                (FIELD_STORES.get(), FIELD_READS.get(), PEAK_FIELDS.get()),
                (objects, reads, objects)
            );
        }
    }

    #[test]
    fn returned_parameters_bind_to_each_calls_actual_owners() {
        let program = lowered(
            r#"
            import std::collections::Array;
            fn choose(a:Array<String>, b:Array<String>, n:String)->Array<String> {
                if n.len()==8 {return a;} return b;
            }
            fn identity(a:Array<String>, n:String)->Array<String> {
                if n.len()==8 {return a;} return a;
            }
            fn swap(a:Array<String>, b:Array<String>, n:String)->Array<String> {
                return choose(b,a,n);
            }
            fn grow(items:Array<String>, n:String)->String {items.push(n); return n;}
            fn private(x:&String,n:String)->String {
                let a=identity([n,n],n); return x+":"+grow(a,n);
            }
            fn shared(x:&String,input:Array<String>,n:String)->String {
                let a=identity(input,n); return x+":"+grow(a,n);
            }
            fn mixed(x:&String,input:Array<String>,n:String)->String {
                let a=swap([n,n],input,n); return x+":"+grow(a,n);
            }
            fn both_private(x:&String,n:String)->String {
                let a=swap(n.split(","),[n,n],n); return x+":"+grow(a,n);
            }
        "#,
        );
        for name in ["private", "both_private"] {
            assert_eq!(
                concats(function(&program, name)),
                vec![vec![S, S, S]],
                "{name}"
            );
        }
        for name in ["shared", "mixed"] {
            assert_saved_string(function(&program, name));
        }
    }

    #[test]
    fn repeated_reference_arguments_share_a_post_call_origin() {
        for arity in [1, 8, 64, 256] {
            let params = (0..arity)
                .map(|i| format!("a{i}:&Array<String>"))
                .collect::<Vec<_>>()
                .join(",");
            let args = vec!["&items"; arity].join(",");
            let program = lowered(&format!(
                r#"
                import std::collections::Array;
                fn inspect({params})->i64 {{
                    if a0.len()==0 {{return 0;}} return 1;
                }}
                fn caller()->Array<String> {{
                    let items=["private"]; inspect({args}); return items;
                }}
            "#
            ));
            let caller = function(&program, "caller");
            let direct = crate::ir::lowered::synchronous_function_indices(&program.functions);
            let mut analysis = CallAnalysis::default();
            let out = analysis.outputs(caller.name, &direct).unwrap();
            analysis.body(
                caller,
                &HashSet::new(),
                Demand::Function(out),
                None,
                &direct,
            );
            let callee = function(&program, "inspect");
            let inspect = analysis.functions[&callee.name];
            let mut destinations = HashSet::new();
            for i in 0..arity {
                // Every actual supplies the original fresh value, including the last.
                assert!(
                    !analysis
                        .graph
                        .bindings
                        .contains_key(&(inspect.writes, Fact::Value(i)))
                );
                let bindings = &analysis.graph.bindings[&(inspect.writes, Fact::Slot(i))];
                assert_eq!(bindings.len(), 1);
                for &(source, dest) in bindings {
                    assert!(source == Source::Fact(Fact::External));
                    destinations.insert(dest);
                }
            }
            assert_eq!(destinations.len(), 1);
            let after = *destinations.iter().next().unwrap();
            assert_eq!(analysis.graph.copies[&after], HashSet::from([out.result]));
            // Two outputs each for caller and callee, plus the single post-call value.
            assert_eq!(analysis.graph.facts.len(), 5);
            analysis.body(
                callee,
                &reference_parameters(callee),
                Demand::Function(inspect),
                None,
                &direct,
            );
            analysis.graph.solve();
            assert!(analysis.graph.facts[out.result].is_empty());
        }
    }

    #[test]
    fn mutable_reference_rebinds_only_the_written_array_slot() {
        let program = lowered(
            r#"
            import std::collections::Array;
            fn rebind(keep:&Array<String>, target:&mut Array<String>, input:Array<String>) {
                target=input;
            }
            fn kept(input:Array<String>)->Array<String> {
                let keep=["private"]; let mut target=["initial"];
                rebind(&keep,&target,input); return keep;
            }
            fn changed(prior:Array<String>, input:Array<String>)->Array<String> {
                let keep=["private"]; let mut target=prior;
                rebind(&keep,&target,input); return target;
            }
        "#,
        );
        let kept = function(&program, "kept").name;
        let changed = function(&program, "changed").name;
        let analysis = summarize_callees(&program.functions, HashSet::from([kept, changed]));
        assert!(analysis.graph.facts[analysis.functions[&kept].result].is_empty());
        assert_eq!(
            analysis.graph.facts[analysis.functions[&changed].result],
            HashSet::from([Fact::Value(0), Fact::External])
        );
    }

    #[test]
    fn reference_slot_writes_bind_to_the_actual_place() {
        let program = lowered(
            r#"
            import std::collections::Array;
            fn change(value:&mut String)->String {value="new"; return "!";}
            fn forward(value:&mut String)->String {return change(&value);}
            fn helper(n:String)->String {let mut tmp=n; forward(&tmp); return tmp;}
            fn private(x:&String,n:String)->String {return x+":"+helper(n);}
            fn distinct(x:&String,n:String)->String {
                let mut tmp=n; return x+":"+forward(&tmp);
            }
            fn same(n:String)->String {
                let mut tmp=n; return tmp+":"+forward(&tmp);
            }
            fn element(x:&String,items:Array<String>)->String {
                return x+":"+forward(&items[0]);
            }
            fn rebind(dst:&mut Array<String>,input:Array<String>) {dst=input;}
            fn replace(input:Array<String>)->String {
                let mut tmp=["local"]; rebind(&tmp,input); tmp[0]="new"; return "!";
            }
            fn rebound(x:&String,input:Array<String>)->String {
                return x+":"+replace(input);
            }
        "#,
        );
        for name in ["private", "distinct"] {
            assert_eq!(
                concats(function(&program, name)),
                vec![vec![S, S, S]],
                "{name}"
            );
        }
        for name in ["same", "element", "rebound"] {
            assert_saved_string(function(&program, name));
        }
    }

    #[test]
    fn forwarded_return_origins_propagate_once_per_wrapper() {
        for n in [1, 8, 64, 256] {
            let mut source = String::from(
                "import std::collections::Array; fn wrap_0(a:Array<String>)->Array<String>{return a;}\n",
            );
            for i in 1..=n {
                source.push_str(&format!(
                    "fn wrap_{i}(a:Array<String>)->Array<String>{{return wrap_{}(a);}}\n",
                    i - 1
                ));
            }
            let program = lowered(&source);
            let root = function(&program, &format!("wrap_{n}")).name;
            PROPAGATED_CALL_EDGES.set(0);
            let analysis = summarize_callees(&program.functions, HashSet::from([root]));
            assert_eq!(analysis.functions.len(), n + 1);
            assert_eq!(SUMMARY_EDGES.get(), 4 * n);
            assert_eq!(PROPAGATED_CALL_EDGES.get(), n);
            for out in analysis.functions.values() {
                assert!(analysis.graph.facts[out.writes].is_empty());
                assert_eq!(
                    analysis.graph.facts[out.result],
                    HashSet::from([Fact::Value(0)])
                );
            }
        }
    }

    #[test]
    fn write_summary_visits_only_callees_of_crossed_reads() {
        for n in [0, 8, 64, 256] {
            let mut source = String::from(
                r#"
                import std::collections::Array;
                fn grow(a:Array<String>,n:String) {a.push(n);}
                fn helper(n:String)->String {let a=n.split(","); grow(a,n); return n;}
                fn unrelated(x:&String,n:String)->String {return helper(n);}
            "#,
            );
            for i in 0..n {
                source.push_str(&format!(
                    "fn unused_{i}(x:&String,n:String)->String {{return unrelated(&x,n);}}\n"
                ));
            }
            for active in [false, true] {
                crate::ir::lowered::DIRECT_INDEX_WORK.set((0, 0));
                SUMMARY_INDEX_PROBES.set(0);
                SUMMARY_BODIES.set(0);
                SUMMARY_EDGES.set(0);
                let program = lowered(&source);
                assert_eq!(
                    crate::ir::lowered::DIRECT_INDEX_WORK.get(),
                    (1, n + 3 + usize::from(active))
                );
                let retained = if active { 2 } else { 0 };
                assert_eq!(
                    (SUMMARY_INDEX_PROBES.get(), SUMMARY_LOOKUP_ENTRIES.get()),
                    (retained, retained)
                );
                if active {
                    assert_eq!(concats(function(&program, "render")), vec![vec![S, S, S]]);
                    assert_eq!((SUMMARY_BODIES.get(), SUMMARY_EDGES.get()), (2, 1), "N={n}");
                } else {
                    assert_eq!((SUMMARY_BODIES.get(), SUMMARY_EDGES.get()), (0, 0), "N={n}");
                    source.push_str(
                        r#"
                       fn render(x:&String,n:String)->String {
                           let unused=unrelated(&x,n);
                           return x+":"+helper(n);
                       }"#,
                    );
                }
            }
        }
    }

    #[test]
    fn repeated_calls_store_and_propagate_one_write_edge() {
        let program = lowered(
            r#"
            fn change(x: &mut String) -> String { x = "new"; return "!"; }
            fn caller(x: &mut String) -> String { return change(&x); }
        "#,
        );
        let caller = function(&program, "caller");
        let writer = function(&program, "change");
        let is_writer_call = |inst: &SourceInst| {
            matches!(
                inst,
                SourceInst::Compute { value: LirRvalue::DirectCall { callee, .. }, .. }
                    if *callee == writer.name
            )
        };
        for n in [1, 8, 64, 512, 2048] {
            let mut caller = caller.clone();
            let instrs = &mut caller.blocks[0].instrs;
            let index = instrs.iter().position(is_writer_call).unwrap();
            let call = instrs[index].clone();
            instrs.splice(index..=index, std::iter::repeat_n(call, n));
            let targets = HashSet::from([caller.name, writer.name]);
            PROPAGATED_CALL_EDGES.set(0);
            let read_only = summarize_callees(&[caller, writer.clone()], targets);
            for out in read_only.functions.values() {
                assert_eq!(
                    read_only.graph.facts[out.writes],
                    HashSet::from([Fact::Slot(0)])
                );
            }
            assert_eq!(STORED_CALL_EDGES.get(), 1);
            assert_eq!(SUMMARY_EDGES.get(), 2);
            assert_eq!(PROPAGATED_CALL_EDGES.get(), 1);
        }
    }

    #[test]
    fn suspend_access_counts_the_read_after_a_local_definition() {
        let mut f = hand_built(0, 1, vec![compute(0, LirRvalue::Use(LirOperand::Int(1)))]);
        f.locals[0].ty = Type::I64;
        f.blocks[0].terminator = SourceTerminator::Suspend {
            operation: crate::ir::lowered::SuspendOp::Sleep {
                millis: LirLocalId(0),
            },
            resume: BlockId(0),
        };
        let accesses = Accesses::count(&f);
        assert_eq!(accesses.reads, vec![1]);
        assert_eq!(accesses.writes, vec![1]);
    }

    #[test]
    fn a_write_to_a_deferred_operand_blocks_absorption() {
        let f = fused(
            vec![
                add(3, local(0), local(1)),
                compute(0, LirRvalue::Use(local(2))),
                add(4, local(3), local(2)),
            ],
            3,
            2,
        );
        assert!(concats(&f).is_empty());
        assert_eq!(f.blocks[0].instrs.len(), 3);
        // A write to an unrelated local does not.
        let f = fused(
            vec![
                add(3, local(0), local(1)),
                compute(2, LirRvalue::Use(local(0))),
                add(4, local(3), local(2)),
            ],
            3,
            2,
        );
        assert_eq!(concats(&f), vec![vec![S, S, S]]);
        assert_eq!(f.blocks[0].instrs.len(), 2);
    }

    #[test]
    fn a_write_invalidates_the_whole_absorbed_tree() {
        // `l0` is a leaf of `t3`, which `t4` absorbed; overwriting `l0` before
        // `t5` must stop `t5` from deferring `t3`'s read of it.
        let f = fused(
            vec![
                add(3, local(0), local(1)),
                add(4, local(3), local(2)),
                compute(0, LirRvalue::Use(local(2))),
                add(5, local(4), local(1)),
            ],
            3,
            3,
        );
        assert_eq!(concats(&f), vec![vec![S, S, S]]);
        assert_eq!(string_adds(&f), 1);
    }

    #[test]
    fn non_compute_instructions_are_barriers() {
        let f = fused(
            vec![
                add(3, local(0), local(1)),
                SourceInst::ClearScopeRoots { locals: Vec::new() },
                add(4, local(3), local(2)),
            ],
            3,
            2,
        );
        assert!(concats(&f).is_empty());
    }

    #[test]
    fn heap_places_are_never_deferred() {
        let place = LirOperand::Reference {
            place: LirPlace::Local(LirLocalId(1)),
            span: Span::dummy(),
            display: "l1".into(),
        };
        let f = fused(
            vec![add(3, local(0), place), add(4, local(3), local(2))],
            3,
            2,
        );
        assert!(concats(&f).is_empty());
    }

    #[test]
    fn a_self_updating_temporary_chain_fuses() {
        let f = fused(
            vec![
                compute(3, LirRvalue::Use(local(0))),
                add(3, local(3), local(1)),
                add(3, local(3), local(2)),
                compute(4, LirRvalue::Use(local(3))),
            ],
            3,
            2,
        );
        assert_eq!(concats(&f), vec![vec![S, S, S]]);
        assert_eq!(f.blocks[0].instrs.len(), 2);
    }

    #[test]
    fn a_long_chain_is_flattened_without_recursion() {
        const PARTS: u32 = 100_000;
        let mut instrs = vec![add(2, local(0), local(1))];
        for temp in 3..PARTS + 1 {
            instrs.push(add(temp, local(temp - 1), local(0)));
        }
        let f = fused(instrs, 2, PARTS - 1);
        assert_eq!(f.blocks[0].instrs.len(), 1);
        let parts = concats(&f);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].len(), PARTS as usize);
    }
}
