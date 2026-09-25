use super::Substitution;
use crate::ir::{Function, InstructionKind, Operand};

pub(super) fn run(function: &mut Function) -> bool {
    let mut substitution = Substitution::default();
    for block in &mut function.blocks {
        block.instructions.retain_mut(|instruction| {
            let (Some(result), InstructionKind::Phi { incomings }) =
                (instruction.result, &mut instruction.kind)
            else {
                return true;
            };
            for (_, operand) in incomings.iter_mut() {
                substitution.resolve(operand);
            }
            let this = Operand::Value(result);
            let mut unique = None;
            for (_, operand) in incomings.iter() {
                if *operand == this {
                    continue;
                }
                match unique {
                    None => unique = Some(operand),
                    Some(seen) if seen == operand => {}
                    Some(_) => return true,
                }
            }
            // A phi of nothing but itself is only reachable around a loop no
            // path enters; it is left for emission to reject or keep.
            let Some(unique) = unique.cloned() else {
                return true;
            };
            substitution.insert(result, unique);
            false
        });
    }
    let changed = !substitution.is_empty();
    substitution.apply(function);
    changed
}
