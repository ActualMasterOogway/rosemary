use super::*;

pub(crate) enum Element {
    Record(u64, Vec<u64>),
    Block(u32, Vec<Element>),
}

pub(crate) fn record(code: u64, fields: &[u64]) -> Element {
    Element::Record(code, fields.to_vec())
}

pub(crate) fn block(id: u32, elements: Vec<Element>) -> Element {
    Element::Block(id, elements)
}

fn fixed(bits: &mut Vec<bool>, value: u64, width: usize) {
    bits.extend((0..width).map(|bit| value & (1 << bit) != 0));
}

fn vbr(bits: &mut Vec<bool>, mut value: u64, width: usize) {
    loop {
        let mut chunk = value & ((1 << (width - 1)) - 1);
        value >>= width - 1;
        if value != 0 {
            chunk |= 1 << (width - 1);
        }
        fixed(bits, chunk, width);
        if value == 0 {
            break;
        }
    }
}

fn pad(bits: &mut Vec<bool>) {
    while !bits.len().is_multiple_of(32) {
        bits.push(false);
    }
}

/// `width` is the enclosing block's abbreviation-ID width: two at the top
/// level, and three inside every block this builder writes.
fn write(bits: &mut Vec<bool>, element: &Element, width: usize) {
    match element {
        Element::Record(code, fields) => {
            fixed(bits, 3, width);
            vbr(bits, *code, 6);
            vbr(bits, fields.len() as u64, 6);
            for field in fields {
                vbr(bits, *field, 6);
            }
        }
        Element::Block(id, elements) => {
            fixed(bits, 1, width);
            vbr(bits, u64::from(*id), 8);
            vbr(bits, 3, 4);
            pad(bits);
            // The length in words is only known after the body is written.
            let length_at = bits.len();
            fixed(bits, 0, 32);
            let body_at = bits.len();
            for element in elements {
                write(bits, element, 3);
            }
            fixed(bits, 0, 3);
            pad(bits);
            let words = ((bits.len() - body_at) / 32) as u64;
            for bit in 0..32 {
                bits[length_at + bit] = words & (1 << bit) != 0;
            }
        }
    }
}

/// Assemble one top-level block and parse it back.
pub(crate) fn bitcode_block(root: Element) -> BitcodeBlock {
    let mut bits = Vec::new();
    write(&mut bits, &root, 2);
    let mut bytes = b"BC\xc0\xde".to_vec();
    bytes.extend(bits.chunks(8).map(|chunk| {
        chunk
            .iter()
            .enumerate()
            .fold(0u8, |byte, (bit, set)| byte | (u8::from(*set) << bit))
    }));
    Bitcode::new(&bytes).unwrap().elements[0]
        .as_block()
        .unwrap()
        .clone()
}
