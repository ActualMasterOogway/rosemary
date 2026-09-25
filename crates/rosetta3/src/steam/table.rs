/// How a value reaches the host's callee. See `BY_VALUE_CLASSES` in the
/// generator, which is where the classification is decided.
pub mod class {
    pub const VOID: u8 = 0;
    /// One general-purpose register, `size` bytes wide.
    pub const INTEGER: u8 = 1;
    /// One vector register: a `float` or a `double`.
    pub const SSE: u8 = 2;
    /// Two general-purpose registers, for a 16-byte aggregate.
    pub const INTEGER2: u8 = 3;
    /// A stack copy going in, a hidden pointer coming back.
    pub const MEMORY: u8 = 4;
}

/// The id of a method that is not one: a builtin entry point's row.
const NOT_A_METHOD: u16 = 0xffff;

const MAGIC: &[u8; 4] = b"R3SC";

/// The format this build reads. A table written by a different generator is
/// refused rather than guessed at - every field in it decides a register.
const VERSION: u32 = 1;

/// One value in a packed params struct: where it is, and how it travels.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub class: u8,
    pub size: u8,
    pub offset: u16,
}

/// One interface method.
#[derive(Clone, Copy, Debug)]
pub struct Method {
    pub vtable_index: u16,
    /// What to add to the interface pointer before reading its vtable. Zero for
    /// every interface today; carried because a multiple-inheritance interface
    /// would not be.
    pub iface_offset: u16,
    /// [`class::VOID`] when the method returns nothing.
    pub ret: Slot,
    params: (u32, u8),
}

/// Every dispatch id the generated dylib can send.
pub struct Table {
    methods: Vec<Method>,
    /// Every method's parameters, flattened: one allocation rather than 6544.
    params: Vec<Slot>,
}

impl Table {
    pub fn parse(bytes: &[u8]) -> Result<Table, String> {
        let mut r = Reader { bytes, at: 0 };
        if r.take(4)? != MAGIC {
            return Err("not a dispatch table".into());
        }
        let version = r.u32()?;
        if version != VERSION {
            return Err(format!(
                "dispatch table version {version}, but this build reads {VERSION}: \
                 regenerate it with tools/steamclient/gen_steamclient.py"
            ));
        }
        let count = r.u32()? as usize;
        let mut methods = Vec::with_capacity(count);
        let mut params = Vec::new();
        for _ in 0..count {
            let vtable_index = r.u16()?;
            let iface_offset = r.u16()?;
            let ret_offset = r.u16()?;
            let ret_class = r.u8()?;
            let ret_size = r.u8()?;
            let param_count = r.u8()?;
            let _reserved = r.u8()?;
            let start = params.len() as u32;
            for _ in 0..param_count {
                let class = r.u8()?;
                let size = r.u8()?;
                let offset = r.u16()?;
                params.push(Slot {
                    class,
                    size,
                    offset,
                });
            }
            methods.push(Method {
                vtable_index,
                iface_offset,
                ret: Slot {
                    class: ret_class,
                    size: ret_size,
                    offset: ret_offset,
                },
                params: (start, param_count),
            });
        }
        Ok(Table { methods, params })
    }

    /// The method `id` names, or `None` for a builtin entry point's id and for
    /// an id this table does not have.
    pub fn method(&self, id: u32) -> Option<(&Method, &[Slot])> {
        let method = self.methods.get(id as usize)?;
        if method.vtable_index == NOT_A_METHOD {
            return None;
        }
        let (start, len) = method.params;
        let params = &self.params[start as usize..start as usize + len as usize];
        Some((method, params))
    }

    pub fn len(&self) -> usize {
        self.methods.len()
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).ok_or("dispatch table overflowed")?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| format!("dispatch table ends mid-record at byte {}", self.at))?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table built by hand, in the shape the generator writes.
    fn encoded(rows: &[(u16, &[(u8, u8, u16)])]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend(VERSION.to_le_bytes());
        out.extend((rows.len() as u32).to_le_bytes());
        for (vtable_index, params) in rows {
            out.extend(vtable_index.to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(8u16.to_le_bytes());
            out.extend([class::INTEGER, 4, params.len() as u8, 0]);
            for (class, size, offset) in *params {
                out.extend([*class, *size]);
                out.extend(offset.to_le_bytes());
            }
        }
        out
    }

    #[test]
    fn reads_back_what_the_generator_writes() {
        let table = Table::parse(&encoded(&[
            (NOT_A_METHOD, &[]),
            (3, &[(class::INTEGER, 8, 12), (class::SSE, 4, 20)]),
        ]))
        .unwrap();
        assert_eq!(table.len(), 2);
        // A builtin entry point's row is not a method, however it is spelled.
        assert!(table.method(0).is_none());
        let (method, params) = table.method(1).unwrap();
        assert_eq!(method.vtable_index, 3);
        assert_eq!(method.ret.class, class::INTEGER);
        assert_eq!(params.len(), 2);
        assert_eq!(params[1].class, class::SSE);
        assert_eq!(params[1].offset, 20);
    }

    /// Past the end is the one answer a lookup must give rather than panic:
    /// the id comes from the guest.
    #[test]
    fn an_id_past_the_end_is_not_a_method() {
        let table = Table::parse(&encoded(&[(0, &[])])).unwrap();
        assert!(table.method(9999).is_none());
    }

    #[test]
    fn a_table_from_another_generator_is_refused() {
        let mut bytes = encoded(&[(0, &[])]);
        bytes[4] = 9;
        let Err(why) = Table::parse(&bytes) else {
            panic!("a table from another generator was accepted");
        };
        assert!(why.contains("version"), "{why}");
        assert!(Table::parse(b"nope").is_err());
    }

    /// A truncated table must be reported, not read past.
    #[test]
    fn a_truncated_table_is_refused() {
        let bytes = encoded(&[(0, &[(class::INTEGER, 8, 12)])]);
        assert!(Table::parse(&bytes[..bytes.len() - 2]).is_err());
    }
}
