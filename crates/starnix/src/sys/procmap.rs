//! Linux: a process's mappings, from `/proc/<pid>/maps`.
//!
//! Linux 6.11 answers `PROCMAP_QUERY` on the open maps file, one binary record
//! per mapping, which spares formatting and parsing every line. Older kernels
//! answer `ENOTTY`, and then the same file is read as text.

use std::fs::File;
use std::io::{self, Read};

use rustix::io::Errno;
use rustix::ioctl::{Updater, ioctl, opcode};

use crate::process::Region;

/// `struct procmap_query` from `<linux/fs.h>`.
#[repr(C)]
#[derive(Default)]
struct ProcmapQuery {
    size: u64,
    query_flags: u64,
    query_addr: u64,
    vma_start: u64,
    vma_end: u64,
    vma_flags: u64,
    vma_page_size: u64,
    vma_offset: u64,
    inode: u64,
    dev_major: u32,
    dev_minor: u32,
    vma_name_size: u32,
    build_id_size: u32,
    vma_name_addr: u64,
    build_id_addr: u64,
}

const PROCMAP_QUERY: rustix::ioctl::Opcode = opcode::read_write::<ProcmapQuery>(b'f', 17);

const VMA_READABLE: u64 = 0x01;
const VMA_WRITABLE: u64 = 0x02;
const VMA_EXECUTABLE: u64 = 0x04;
const VMA_SHARED: u64 = 0x08;
const COVERING_OR_NEXT_VMA: u64 = 0x10;

/// Every mapping `maps` lists, in address order.
pub fn regions(maps: &str) -> io::Result<Vec<Region>> {
    let mut file = File::open(maps)?;
    match query(&file) {
        Ok(regions) => Ok(regions),
        // The ioctl leaves the file offset alone, so the text is still unread.
        Err(_) => {
            let mut text = String::new();
            file.read_to_string(&mut text)?;
            Ok(text.lines().filter_map(parse_line).collect())
        }
    }
}

/// Walks the mappings by asking for the first one at or after the end of the
/// last. Unlike the text, this leaves out the gate area (`[vsyscall]`).
fn query(file: &File) -> Result<Vec<Region>, Errno> {
    let mut regions = Vec::new();
    let mut addr = 0;
    loop {
        let mut q = ProcmapQuery {
            size: size_of::<ProcmapQuery>() as u64,
            query_flags: COVERING_OR_NEXT_VMA,
            query_addr: addr,
            ..Default::default()
        };
        // SAFETY: `q` is a `procmap_query` sized as such, with no name or
        // build-id buffer for the kernel to write through.
        match unsafe { ioctl(file, Updater::<PROCMAP_QUERY, _>::new(&mut q)) } {
            Ok(()) => {}
            // Nothing at or past `addr`: the walk is done.
            Err(Errno::NOENT) => return Ok(regions),
            Err(errno) => return Err(errno),
        }
        regions.push(Region {
            start: q.vma_start,
            end: q.vma_end,
            read: q.vma_flags & VMA_READABLE != 0,
            write: q.vma_flags & VMA_WRITABLE != 0,
            execute: q.vma_flags & VMA_EXECUTABLE != 0,
            shared: q.vma_flags & VMA_SHARED != 0,
            offset: q.vma_offset,
        });
        addr = q.vma_end;
    }
}

/// `lo-hi perms offset dev inode path`.
fn parse_line(line: &str) -> Option<Region> {
    let mut fields = line.split_whitespace();
    let (low, high) = fields.next()?.split_once('-')?;
    let perms = fields.next()?.as_bytes();
    let offset = u64::from_str_radix(fields.next()?, 16).ok()?;
    Some(Region {
        start: u64::from_str_radix(low, 16).ok()?,
        end: u64::from_str_radix(high, 16).ok()?,
        read: perms.first() == Some(&b'r'),
        write: perms.get(1) == Some(&b'w'),
        execute: perms.get(2) == Some(&b'x'),
        shared: perms.get(3) == Some(&b's'),
        offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_maps_line_reads_as_its_region() {
        let region = parse_line("7f00-7f80 r-xs 00001000 08:01 1234   /usr/lib/libc.so.6").unwrap();
        assert_eq!(
            (region.start, region.end, region.offset),
            (0x7f00, 0x7f80, 0x1000)
        );
        assert!(region.read && !region.write && region.execute && region.shared);
    }

    #[test]
    fn the_query_and_the_text_agree_on_this_functions_region() {
        let file = File::open("/proc/self/maps").unwrap();
        let Ok(queried) = query(&file) else {
            eprintln!("skipping: this kernel has no PROCMAP_QUERY");
            return;
        };
        let text = std::fs::read_to_string("/proc/self/maps").unwrap();
        let parsed: Vec<_> = text.lines().filter_map(parse_line).collect();
        let here = the_query_and_the_text_agree_on_this_functions_region as *const () as u64;
        let find = |regions: &[Region]| {
            *regions
                .iter()
                .find(|region| (region.start..region.end).contains(&here))
                .unwrap()
        };
        assert_eq!(find(&queried), find(&parsed));
    }
}
