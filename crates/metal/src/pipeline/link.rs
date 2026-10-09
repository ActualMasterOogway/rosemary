//! Link a fragment function's varyings to its vertex function's by name.
//!
//! Metal matches a vertex output to a fragment input by its `user(NAME)`
//! attribute; Vulkan matches them by location. A library compiled from source
//! numbers its varyings in declaration order, and a vertex and a fragment
//! function from two libraries need not declare them in the same order, so a
//! fragment module's input locations are renumbered to the vertex module's
//! before the pipeline is made.

use metal_translate::StageVariable;

const OP_DECORATE: u32 = 71;
const OP_VARIABLE: u32 = 59;
const DECORATION_LOCATION: u32 = 30;
const STORAGE_CLASS_INPUT: u32 = 1;
const HEADER_WORDS: usize = 5;

/// The fragment module's words with its inputs at the vertex module's
/// locations, or `None` where nothing moves.
///
/// A fragment input no vertex output names is given a location of its own,
/// past every vertex output, rather than one it would share.
pub(super) fn link_varyings(
    vertex: &[StageVariable],
    fragment: &[StageVariable],
    words: &[u32],
) -> Option<Vec<u32>> {
    if fragment.is_empty() {
        return None;
    }
    let mut spare = vertex
        .iter()
        .map(|varying| varying.location + 1)
        .max()
        .unwrap_or(0);
    let mut moves = Vec::new();
    for input in fragment {
        let location = match vertex.iter().find(|output| output.name == input.name) {
            Some(output) => output.location,
            None => {
                spare += 1;
                spare - 1
            }
        };
        moves.push((input.location, location));
    }
    if moves.iter().all(|(from, to)| from == to) {
        return None;
    }
    Some(renumber_inputs(words, &moves))
}

/// Rewrite the `Location` decoration of every `Input` variable by `moves`.
fn renumber_inputs(words: &[u32], moves: &[(u32, u32)]) -> Vec<u32> {
    let mut words = words.to_vec();
    let instructions = || {
        let mut at = HEADER_WORDS;
        let words = words.clone();
        std::iter::from_fn(move || {
            let header = *words.get(at)?;
            let count = (header >> 16) as usize;
            if count == 0 || at + count > words.len() {
                return None;
            }
            let start = at;
            at += count;
            Some((start, header & 0xffff, count))
        })
    };
    let inputs = instructions()
        .filter(|&(start, opcode, count)| {
            opcode == OP_VARIABLE && count >= 4 && words[start + 3] == STORAGE_CLASS_INPUT
        })
        .map(|(start, _, _)| words[start + 2])
        .collect::<Vec<_>>();
    let decorations = instructions()
        .filter(|&(start, opcode, count)| {
            opcode == OP_DECORATE
                && count == 4
                && words[start + 2] == DECORATION_LOCATION
                && inputs.contains(&words[start + 1])
        })
        .map(|(start, _, _)| start + 3)
        .collect::<Vec<_>>();
    for at in decorations {
        if let Some((_, to)) = moves.iter().find(|(from, _)| *from == words[at]) {
            words[at] = *to;
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varying(name: &str, location: u32) -> StageVariable {
        StageVariable {
            name: name.to_owned(),
            location,
            data_type: 0,
        }
    }

    /// A module header, two input variables decorated at 0 and 1, and an
    /// output variable decorated at 0.
    fn module() -> Vec<u32> {
        let mut words = vec![0x0723_0203, 0x0001_0000, 0, 20, 0];
        for (id, location) in [(10, 0), (11, 1), (12, 0)] {
            words.extend([4 << 16 | OP_DECORATE, id, DECORATION_LOCATION, location]);
        }
        for (id, class) in [
            (10, STORAGE_CLASS_INPUT),
            (11, STORAGE_CLASS_INPUT),
            (12, 3),
        ] {
            words.extend([4 << 16 | OP_VARIABLE, 99, id, class]);
        }
        words
    }

    #[test]
    fn fragment_inputs_take_the_vertex_outputs_locations_by_name() {
        let vertex = [varying("user(A)", 0), varying("user(B)", 1)];
        let fragment = [varying("user(B)", 0), varying("user(A)", 1)];
        let linked = link_varyings(&vertex, &fragment, &module()).unwrap();
        // The inputs swap; the output keeps its render target.
        assert_eq!(linked[5 + 3], 1);
        assert_eq!(linked[9 + 3], 0);
        assert_eq!(linked[13 + 3], 0);
    }

    #[test]
    fn matching_orders_leave_the_module_alone() {
        let vertex = [varying("user(A)", 0), varying("user(B)", 1)];
        assert!(link_varyings(&vertex, &vertex, &module()).is_none());
    }

    #[test]
    fn an_input_no_output_names_gets_a_location_of_its_own() {
        let vertex = [varying("user(A)", 1)];
        let fragment = [varying("user(A)", 0), varying("user(Z)", 1)];
        let linked = link_varyings(&vertex, &fragment, &module()).unwrap();
        assert_eq!(linked[5 + 3], 1);
        assert_eq!(linked[9 + 3], 2);
    }
}
