//! Control-flow analysis over a Metal IR function.
//!
//! The successor and predecessor graph, dominance over it, and the merge
//! block each divergence reconverges at. Restructuring consults this;
//! emission consults the merges it reports.

use super::*;

pub(super) struct ControlFlow {
    pub(super) blocks: Vec<BlockId>,
    pub(super) successors: HashMap<BlockId, Vec<BlockId>>,
    pub(super) predecessors: HashMap<BlockId, Vec<BlockId>>,
}

impl ControlFlow {
    pub(super) fn new(function: &Function) -> Self {
        let mut graph = Self {
            blocks: function.blocks.iter().map(|block| block.id).collect(),
            successors: HashMap::new(),
            predecessors: HashMap::new(),
        };
        for block in &function.blocks {
            let successors = successors_of(&block.terminator);
            for successor in &successors {
                graph
                    .predecessors
                    .entry(*successor)
                    .or_default()
                    .push(block.id);
            }
            graph.successors.insert(block.id, successors);
        }
        graph
    }

    /// Immediate dominators, by the Cooper-Harvey-Kennedy iteration, used for
    /// both dominators and post-dominators by swapping the edge direction.
    ///
    /// `edges` names each block's incoming edges for the chosen direction, so
    /// the walk itself follows their reverse. A dominator tree rather than a
    /// set per block: a shader whose entry point inlines to a few thousand
    /// blocks makes the set form quadratic in both time and memory, and every
    /// restructuring pass recomputes this.
    pub(super) fn dominance(
        &self,
        roots: &[BlockId],
        edges: &HashMap<BlockId, Vec<BlockId>>,
    ) -> Dominance {
        let mut forward: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
        for (block, incoming) in edges {
            for source in incoming {
                forward.entry(*source).or_default().push(*block);
            }
        }
        // Reverse post-order, so that a block is visited after the blocks
        // whose dominators it depends on wherever the graph is acyclic.
        let mut visited = BTreeSet::new();
        let mut post = Vec::new();
        for root in roots {
            if !visited.insert(*root) {
                continue;
            }
            let mut stack = vec![(*root, 0usize)];
            while let Some((block, index)) = stack.pop() {
                let next = forward.get(&block).map(|s| s.as_slice()).unwrap_or(&[]);
                match next.get(index) {
                    Some(successor) => {
                        stack.push((block, index + 1));
                        if visited.insert(*successor) {
                            stack.push((*successor, 0));
                        }
                    }
                    None => post.push(block),
                }
            }
        }
        post.reverse();
        let order: HashMap<BlockId, usize> = post
            .iter()
            .enumerate()
            .map(|(index, block)| (*block, index))
            .collect();

        let mut idom: HashMap<BlockId, BlockId> = roots.iter().map(|root| (*root, *root)).collect();
        loop {
            let mut changed = false;
            for block in &post {
                if roots.contains(block) {
                    continue;
                }
                let mut candidate = None;
                for source in edges.get(block).into_iter().flatten() {
                    if !idom.contains_key(source) {
                        continue;
                    }
                    candidate = Some(match candidate {
                        None => *source,
                        // Two paths which meet at no common block leave this
                        // one dominated by nothing but itself, which is what
                        // its own identity records.
                        Some(current) => match meet(*source, current, &idom, &order) {
                            Some(meet) => meet,
                            None => *block,
                        },
                    });
                }
                let candidate = candidate.unwrap_or(*block);
                if idom.insert(*block, candidate) != Some(candidate) {
                    changed = true;
                }
            }
            if !changed {
                return Dominance { idom };
            }
        }
    }
}

/// A dominator tree: each block's immediate dominator, with a root naming
/// itself.
pub(super) struct Dominance {
    idom: HashMap<BlockId, BlockId>,
}

impl Dominance {
    /// Reflexive, as a dominator set which contains its own block is.
    pub(super) fn dominates(&self, dominator: BlockId, block: BlockId) -> bool {
        let mut current = block;
        loop {
            if current == dominator {
                return true;
            }
            match self.idom.get(&current) {
                Some(next) if *next != current => current = *next,
                _ => return false,
            }
        }
    }

    /// The closest strict dominator, or `None` for a root and for a block
    /// whose paths in are dominated by nothing they share.
    pub(super) fn immediate(&self, block: BlockId) -> Option<BlockId> {
        self.idom
            .get(&block)
            .copied()
            .filter(|dominator| *dominator != block)
    }
}

/// The closest block both `a` and `b` are dominated by, or `None` when they
/// are reached from separate roots and share none.
fn meet(
    mut a: BlockId,
    mut b: BlockId,
    idom: &HashMap<BlockId, BlockId>,
    order: &HashMap<BlockId, usize>,
) -> Option<BlockId> {
    while a != b {
        let (deeper, shallower) = match order.get(&a)? > order.get(&b)? {
            true => (&mut a, b),
            false => (&mut b, a),
        };
        let next = *idom.get(deeper)?;
        if next == *deeper {
            // A root, and the other block is not below it.
            let _ = shallower;
            return None;
        }
        *deeper = next;
    }
    Some(a)
}

/// Recover the structured constructs of a function's control-flow graph.
pub(super) fn control_flow_merges(
    function: &Function,
) -> Result<HashMap<BlockId, Merge>, TranslateError> {
    let graph = ControlFlow::new(function);
    let entry = function.blocks[0].id;
    let dominators = graph.dominance(&[entry], &graph.predecessors);
    let exits: Vec<_> = graph
        .blocks
        .iter()
        .copied()
        .filter(|block| graph.successors[block].is_empty())
        .collect();
    let post_dominators = graph.dominance(&exits, &graph.successors);
    // Loops first: a selection inside one may branch out of it, and that
    // choice depends on where the loop continues.
    let mut loops = Vec::new();
    for block in &function.blocks {
        let latches: Vec<_> = graph
            .predecessors
            .get(&block.id)
            .into_iter()
            .flatten()
            .copied()
            .filter(|predecessor| dominators.dominates(block.id, *predecessor))
            .collect();
        if latches.is_empty() {
            continue;
        }
        let [continue_target] = latches.as_slice() else {
            return Err(emission(
                "a loop with several back edges needs restructuring",
            ));
        };
        let body = natural_loop(&graph, block.id, *continue_target);
        let mut leaving: Vec<_> = body
            .iter()
            .flat_map(|member| graph.successors[member].iter().copied())
            .filter(|successor| !body.contains(successor))
            .collect();
        leaving.sort_unstable();
        leaving.dedup();
        let [merge] = leaving.as_slice() else {
            return Err(emission("a loop with several exits needs restructuring"));
        };
        loops.push((block.id, body, *merge, *continue_target));
    }
    let mut merges = HashMap::new();
    for (header, _, merge, continue_target) in &loops {
        merges.insert(
            *header,
            Merge::Loop {
                merge: *merge,
                continue_target: *continue_target,
            },
        );
    }
    for block in &function.blocks {
        if !matches!(block.terminator, Terminator::CondBranch { .. })
            || merges.contains_key(&block.id)
        {
            continue;
        }
        let merge = post_dominators.immediate(block.id).ok_or_else(|| {
            emission("a conditional branch's paths never rejoin, which SPIR-V cannot express")
        })?;
        // A conditional which leaves an enclosing loop, whether by `break` or
        // by `continue`, has no rejoining block inside the loop body: its
        // paths meet only at the loop's merge or its continue target, and
        // SPIR-V accepts neither as a selection's merge. Use the arm which
        // keeps running the body instead.
        let enclosing = loops
            .iter()
            .filter(|(_, body, _, _)| body.contains(&block.id))
            .min_by_key(|(_, body, _, _)| body.len());
        let merge = match enclosing {
            Some((header, body, loop_merge, continue_target))
                if !body.contains(&merge) || merge == *continue_target =>
            {
                let successors = successors_of(&block.terminator);
                // A conditional whose two arms are the loop's own merge and
                // its continue target is a `break` against a `continue`, and
                // one in the continue target itself whose arms are the back
                // edge and the merge is a `do`/`while`'s test. Neither
                // declares a selection: SPIR-V lets a branch leave a construct
                // by a merge, a continue target or a back edge without one.
                let structureless = |successor: &BlockId| {
                    successor == loop_merge
                        || successor == continue_target
                        || (block.id == *continue_target && successor == header)
                };
                if successors.iter().all(structureless) {
                    continue;
                }
                let staying: Vec<_> = successors
                    .into_iter()
                    .filter(|successor| successor != continue_target)
                    .filter(|successor| reaches(&graph, *successor, *continue_target, body))
                    .collect();
                match staying.as_slice() {
                    [only] => *only,
                    // Two arms which both run more of the body before
                    // leaving it do not rejoin inside the loop. Expressing
                    // that needs real restructuring, not a merge choice.
                    _ => {
                        return Err(emission(
                            "a conditional which leaves its loop has no single continuing arm",
                        ));
                    }
                }
            }
            _ => merge,
        };
        merges.insert(block.id, Merge::Selection { merge });
    }
    Ok(merges)
}

/// Whether `target` is reachable from `from` without leaving `within`.
pub(super) fn reaches(
    graph: &ControlFlow,
    from: BlockId,
    target: BlockId,
    within: &BTreeSet<BlockId>,
) -> bool {
    let mut seen = BTreeSet::new();
    let mut pending = vec![from];
    while let Some(block) = pending.pop() {
        if block == target {
            return true;
        }
        if !within.contains(&block) || !seen.insert(block) {
            continue;
        }
        pending.extend(graph.successors[&block].iter().copied());
    }
    false
}

/// The blocks of the natural loop formed by the back edge `latch -> header`.
pub(super) fn natural_loop(
    graph: &ControlFlow,
    header: BlockId,
    latch: BlockId,
) -> BTreeSet<BlockId> {
    let mut body = BTreeSet::from([header]);
    let mut pending = vec![latch];
    while let Some(block) = pending.pop() {
        if !body.insert(block) {
            continue;
        }
        pending.extend(
            graph
                .predecessors
                .get(&block)
                .into_iter()
                .flatten()
                .copied(),
        );
    }
    body
}

/// The structured construct a block with two successors introduces.
///
/// SPIR-V has no unstructured control flow: every conditional branch must
/// declare where its two paths rejoin, and a loop must additionally name its
/// back edge. LLVM records neither, so both are recovered from the CFG.
#[derive(Clone, Copy, Debug)]
pub(super) enum Merge {
    Selection {
        merge: BlockId,
    },
    Loop {
        merge: BlockId,
        continue_target: BlockId,
    },
}

pub(super) fn successors_of(terminator: &Terminator) -> Vec<BlockId> {
    match terminator {
        Terminator::Branch(target) => vec![*target],
        Terminator::CondBranch {
            true_block,
            false_block,
            ..
        } => vec![*true_block, *false_block],
        Terminator::Return(_) | Terminator::Unreachable => Vec::new(),
    }
}
