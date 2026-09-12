//! Block splitting: terminators, block references, and `switch` expansion.
//!
//! Metal IR has no n-way branch, so a `switch` is recorded while its block
//! is lowered and expanded into a chain of conditional branches once every
//! block exists to branch to.

use super::*;

/// A decoded `switch`, before it has been expanded into comparisons.
pub(super) struct PendingSwitch {
    pub(super) block: usize,
    pub(super) condition: Operand,
    pub(super) ty: Type,
    pub(super) default: BlockId,
    pub(super) cases: Vec<(u64, BlockId)>,
}

/// Replace one `switch` with a chain of equality tests.
///
/// SPIR-V has `OpSwitch`, but structured control flow would then have to
/// recover a merge for an n-way branch. A chain reduces the switch to the
/// two-way form the rest of the translator already restructures, at the cost
/// of one comparison per case.
pub(super) fn expand_switch(
    function: &mut Function,
    blocks: &mut Vec<Block>,
    switch: PendingSwitch,
) {
    let PendingSwitch {
        block,
        condition,
        ty,
        default,
        cases,
    } = switch;
    let origin = BlockId(block as u32);
    if cases.is_empty() {
        blocks[block].terminator = Terminator::Branch(default);
        return;
    }
    // The first test stays in the switch's own block; each later one needs a
    // block of its own to be reached from the test before it.
    let mut chain = vec![origin];
    for position in 1..cases.len() {
        let id = BlockId(blocks.len() as u32);
        blocks.push(Block {
            id,
            name: format!("switch{block}_case{position}"),
            instructions: Vec::new(),
            terminator: Terminator::Unreachable,
        });
        chain.push(id);
    }
    let mut arrivals: Vec<(BlockId, Vec<BlockId>)> = Vec::new();
    let mut record_arrival = |target: BlockId, from: BlockId| match arrivals
        .iter_mut()
        .find(|(block, _)| *block == target)
    {
        Some((_, sources)) => sources.push(from),
        None => arrivals.push((target, vec![from])),
    };
    for (position, (value, target)) in cases.iter().enumerate() {
        let result = ValueId(function.values.len() as u32);
        function.values.push(Type::Bool);
        let here = chain[position];
        let next = chain.get(position + 1).copied().unwrap_or(default);
        let owner = &mut blocks[here.0 as usize];
        owner.instructions.push(Instruction {
            result: Some(result),
            kind: InstructionKind::IntegerCompare {
                predicate: IntegerPredicate::Equal,
                lhs: condition.clone(),
                rhs: Operand::Integer {
                    ty: ty.clone(),
                    value: *value,
                },
            },
        });
        owner.terminator = Terminator::CondBranch {
            condition: Operand::Value(result),
            true_block: *target,
            false_block: next,
        };
        record_arrival(*target, here);
    }
    record_arrival(default, *chain.last().expect("at least one case"));

    // Every edge which left the switch block now leaves the chain block which
    // tests that case, so the phis in each target follow their edge.
    for (target, sources) in arrivals {
        for instruction in &mut blocks[target.0 as usize].instructions {
            let InstructionKind::Phi { incomings } = &mut instruction.kind else {
                continue;
            };
            let Some(position) = incomings
                .iter()
                .position(|(predecessor, _)| *predecessor == origin)
            else {
                continue;
            };
            let (_, operand) = incomings.remove(position);
            for source in &sources {
                incomings.push((*source, operand.clone()));
            }
        }
    }
}

/// A phi whose incoming values are resolved after every block is lowered.
pub(super) struct PhiFixup {
    pub(super) block: usize,
    pub(super) instruction: usize,
    pub(super) sources: Vec<(BlockId, usize)>,
}

/// LLVM record codes which end a basic block.
pub(super) fn is_terminator(code: u64) -> bool {
    matches!(code, 10 | 11 | 12 | 13 | 15 | 31 | 39 | 55 | 56)
}

pub(super) fn block_id(index: u64, count: usize) -> Result<BlockId, TranslateError> {
    let id = u32::try_from(index).map_err(|_| semantic("LLVM block index is too large"))?;
    if id as usize >= count {
        return Err(semantic("LLVM branch names a block which does not exist"));
    }
    Ok(BlockId(id))
}
