use std::collections::HashSet;

use crate::ir::{Function, InstructionKind, successors};

pub(super) fn run(function: &mut Function) -> bool {
    let Some(entry) = function.blocks.first().map(|block| block.id) else {
        return false;
    };
    let terminators: std::collections::HashMap<_, _> = function
        .blocks
        .iter()
        .map(|block| (block.id, &block.terminator))
        .collect();
    let mut reachable = HashSet::from([entry]);
    let mut worklist = vec![entry];
    while let Some(block) = worklist.pop() {
        let Some(terminator) = terminators.get(&block) else {
            continue;
        };
        for successor in successors(terminator) {
            if reachable.insert(successor) {
                worklist.push(successor);
            }
        }
    }
    if reachable.len() == function.blocks.len() {
        return false;
    }

    function
        .blocks
        .retain(|block| reachable.contains(&block.id));
    for block in &mut function.blocks {
        for instruction in &mut block.instructions {
            if let InstructionKind::Phi { incomings } = &mut instruction.kind {
                incomings.retain(|(from, _)| reachable.contains(from));
            }
        }
    }
    true
}
