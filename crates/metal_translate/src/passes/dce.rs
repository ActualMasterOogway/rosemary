//! Dead-code elimination.
//!
//! An instruction is kept if it has an effect beyond its result - a store, an
//! atomic, a barrier, a texture write, a discard - or if something kept uses
//! its result, or a terminator does. Everything else goes, including phis
//! which only feed one another around a loop.
//!
//! A store is an effect only if something can read what it wrote. A `thread`
//! local whose address never escapes into anything but more addresses and the
//! pointer of a store is never read, so it goes with every store into it.

use std::collections::{HashMap, HashSet};

use super::operand_values;
use crate::ir::{
    Function, InstructionKind, Operand, ValueId, instruction_operands, terminator_operands,
};

/// A block index and an instruction index within it.
type Site = (usize, usize);

pub(super) fn run(function: &mut Function) -> bool {
    let mut definitions: HashMap<ValueId, Site> = HashMap::new();
    for (b, block) in function.blocks.iter().enumerate() {
        for (i, instruction) in block.instructions.iter().enumerate() {
            if let Some(result) = instruction.result {
                definitions.insert(result, (b, i));
            }
        }
    }
    let unread = unread_local_stores(function);

    let mut live: HashSet<Site> = HashSet::new();
    let mut worklist: Vec<Site> = Vec::new();
    let mark = |operand: &Operand, live: &mut HashSet<Site>, worklist: &mut Vec<Site>| {
        operand_values(operand, &mut |value| {
            if let Some(&site) = definitions.get(&value)
                && live.insert(site)
            {
                worklist.push(site);
            }
        });
    };
    for (b, block) in function.blocks.iter().enumerate() {
        for (i, instruction) in block.instructions.iter().enumerate() {
            if has_effect(&instruction.kind) && !unread.contains(&(b, i)) && live.insert((b, i)) {
                worklist.push((b, i));
            }
        }
        for operand in terminator_operands(&block.terminator) {
            mark(operand, &mut live, &mut worklist);
        }
    }
    while let Some((b, i)) = worklist.pop() {
        for operand in instruction_operands(&function.blocks[b].instructions[i].kind) {
            mark(operand, &mut live, &mut worklist);
        }
    }

    let mut changed = false;
    for (b, block) in function.blocks.iter_mut().enumerate() {
        let mut i = 0;
        block.instructions.retain(|_| {
            let keep = live.contains(&(b, i));
            i += 1;
            changed |= !keep;
            keep
        });
    }
    changed
}

fn has_effect(kind: &InstructionKind) -> bool {
    matches!(
        kind,
        InstructionKind::Store { .. }
            | InstructionKind::Atomic { .. }
            | InstructionKind::Barrier { .. }
            | InstructionKind::TextureWrite { .. }
            | InstructionKind::DiscardFragment
    )
}

/// The stores into `thread` locals nothing ever reads.
///
/// A local is read unless every use of it, and of every address computed
/// from it, is either another address computation from it or the pointer a
/// store writes through. Passing the address anywhere else - a load, an
/// atomic, a phi, a select, a store of the pointer itself - is a read.
fn unread_local_stores(function: &Function) -> HashSet<Site> {
    let mut stores = HashSet::new();
    let locals: Vec<ValueId> = function
        .blocks
        .iter()
        .flat_map(|block| &block.instructions)
        .filter(|instruction| matches!(instruction.kind, InstructionKind::LocalVariable { .. }))
        .filter_map(|instruction| instruction.result)
        .collect();
    if locals.is_empty() {
        return stores;
    }

    let mut uses: HashMap<ValueId, Vec<Site>> = HashMap::new();
    let mut escaping: HashSet<ValueId> = HashSet::new();
    for (b, block) in function.blocks.iter().enumerate() {
        for (i, instruction) in block.instructions.iter().enumerate() {
            for operand in instruction_operands(&instruction.kind) {
                operand_values(operand, &mut |value| {
                    let sites = uses.entry(value).or_default();
                    if sites.last() != Some(&(b, i)) {
                        sites.push((b, i));
                    }
                });
            }
        }
        for operand in terminator_operands(&block.terminator) {
            operand_values(operand, &mut |value| {
                escaping.insert(value);
            });
        }
    }

    'locals: for local in locals {
        let mut derived = vec![local];
        let mut local_stores = Vec::new();
        let mut index = 0;
        while let Some(&pointer) = derived.get(index) {
            index += 1;
            if escaping.contains(&pointer) {
                continue 'locals;
            }
            let this = Operand::Value(pointer);
            for &(b, i) in uses.get(&pointer).into_iter().flatten() {
                let instruction = &function.blocks[b].instructions[i];
                let mentions = |operand: &Operand| {
                    let mut found = false;
                    operand_values(operand, &mut |value| found |= value == pointer);
                    found
                };
                match &instruction.kind {
                    InstructionKind::Store { pointer: to, value }
                        if *to == this && !mentions(value) =>
                    {
                        local_stores.push((b, i));
                    }
                    InstructionKind::ElementAddress {
                        pointer: from,
                        index,
                    } if *from == this && !mentions(index) => {
                        derived.extend(instruction.result);
                    }
                    InstructionKind::FieldAddress { pointer: from, .. } if *from == this => {
                        derived.extend(instruction.result);
                    }
                    InstructionKind::BufferAddress {
                        pointer: from,
                        indices,
                    } if *from == this && !indices.iter().any(mentions) => {
                        derived.extend(instruction.result);
                    }
                    _ => continue 'locals,
                }
            }
        }
        stores.extend(local_stores);
    }
    stores
}
