//! The memory a Metal buffer is cut out of.
//!
//! Metal buffers are made and thrown away by the thousand: a game which streams
//! its world in creates gigabytes of them in the second the world arrives. One
//! `vkAllocateMemory` and one `vkMapMemory` each is what a driver charges most
//! for, and on a discrete adapter the natural-looking heap - device-local *and*
//! host-visible - is the small PCI aperture, which such a burst overruns.
//!
//! So a buffer is a range of one large block. A block is allocated and mapped
//! once, and a buffer is a `VkBuffer` bound into it at an offset; freeing a
//! buffer returns the range to the block. Blocks are never given back to the
//! driver, because a run which needed the peak once will need it again and the
//! whole point is not to call the allocator on a hot path.
//!
//! An allocation larger than a block gets a block of its own, which is freed
//! with it: those are render-target-sized and rare, and holding one for the
//! life of the process would be the leak this module exists to end.

use std::sync::Mutex;

use ash::{Device, vk};

/// The block size. Large enough that a streaming burst is a handful of
/// allocations, small enough that a program which uses one buffer does not
/// reserve a hundred megabytes for it.
const BLOCK: u64 = 64 << 20;

/// Where one buffer lives.
#[derive(Clone, Copy)]
pub struct Allocation {
    pub memory: vk::DeviceMemory,
    pub contents: *mut u8,
    /// The block this came out of, or `None` for a block of its own.
    block: Option<usize>,
    offset: u64,
    size: u64,
    /// Whether this range has never been handed out before. A driver zeroes
    /// the memory it allocates - it must, or one process would read another's -
    /// so a caller which needs zeros need only write them over a range some
    /// earlier buffer has already used.
    pub pristine: bool,
}

// SAFETY: `contents` is a permanent mapping of device memory, which every
// thread that holds the Metal object may write through - exactly as Metal's
// own `-contents` pointer is.
unsafe impl Send for Allocation {}
unsafe impl Sync for Allocation {}

struct Block {
    memory: vk::DeviceMemory,
    contents: *mut u8,
    size: u64,
    /// Free ranges, kept sorted by offset and merged with their neighbours, so
    /// a block which fills and empties does not fragment into nothing.
    free: Vec<(u64, u64)>,
    /// One past the highest offset ever carved out. Everything above it is
    /// memory as the driver handed it over, which is zeroed.
    reached: u64,
}

// SAFETY: as for `Allocation`.
unsafe impl Send for Block {}

#[derive(Default)]
struct State {
    blocks: Vec<Block>,
    /// Blocks a large allocation owns outright, by the index it was given.
    dedicated: Vec<usize>,
}

/// One memory type's blocks.
pub struct Heap {
    memory_type: u32,
    state: Mutex<State>,
}

impl Heap {
    pub fn new(memory_type: u32) -> Self {
        Self {
            memory_type,
            state: Mutex::new(State::default()),
        }
    }

    /// Carve `size` bytes out, allocating another block where none has room.
    pub fn allocate(
        &self,
        device: &Device,
        size: u64,
        alignment: u64,
    ) -> Result<Allocation, String> {
        let alignment = alignment.max(1);
        let size = size.max(1).next_multiple_of(alignment);
        let mut state = self.state.lock().expect("buffer heap");
        if size <= BLOCK
            && let Some(allocation) = state.carve(size, alignment)
        {
            return Ok(allocation);
        }
        let capacity = size.max(BLOCK);
        let block = crate::stats::timed(crate::stats::Counter::BufferBlockMicros, || {
            self.block(device, capacity)
        })?;
        state.blocks.push(block);
        if size > BLOCK {
            // Its own block, freed when it is: nothing else is cut from it.
            let index = state.blocks.len() - 1;
            state.dedicated.push(index);
            let block = &mut state.blocks[index];
            block.free.clear();
            block.reached = capacity;
            return Ok(Allocation {
                memory: block.memory,
                contents: block.contents,
                block: Some(index),
                offset: 0,
                size: capacity,
                pristine: true,
            });
        }
        state
            .carve(size, alignment)
            .ok_or_else(|| "a fresh block had no room for the allocation".to_owned())
    }

    /// Give a range back. The memory stays mapped and the block stays alive.
    pub fn free(&self, device: &Device, allocation: Allocation) {
        let Some(index) = allocation.block else {
            return;
        };
        let mut state = self.state.lock().expect("buffer heap");
        if let Some(at) = state.dedicated.iter().position(|owned| *owned == index) {
            state.dedicated.swap_remove(at);
            let block = &mut state.blocks[index];
            let (memory, size) = (block.memory, block.size);
            // The block stays in place so that no later index shifts; it is
            // marked spent by having neither memory nor room.
            block.memory = vk::DeviceMemory::null();
            block.contents = std::ptr::null_mut();
            block.size = 0;
            block.free.clear();
            drop(state);
            let _ = size;
            // SAFETY: nothing else was cut from this block, and the caller
            // guarantees the buffer bound into it is destroyed and idle.
            unsafe {
                device.unmap_memory(memory);
                device.free_memory(memory, None);
            }
            return;
        }
        state.blocks[index].release(allocation.offset, allocation.size);
        // A burst which needed gigabytes for a second must not hold them for
        // the rest of the run. One empty block is kept, so a program which
        // fills and empties one block does not allocate on every cycle.
        let Some(spare) = state.spare_block() else {
            return;
        };
        let block = &mut state.blocks[spare];
        let memory = std::mem::replace(&mut block.memory, vk::DeviceMemory::null());
        block.contents = std::ptr::null_mut();
        block.size = 0;
        block.free.clear();
        drop(state);
        // SAFETY: the block is entirely free, so no live buffer is bound into
        // it, and every buffer that was is destroyed and idle.
        unsafe {
            device.unmap_memory(memory);
            device.free_memory(memory, None);
        }
    }

    fn block(&self, device: &Device, size: u64) -> Result<Block, String> {
        let info = vk::MemoryAllocateInfo::default()
            .allocation_size(size)
            .memory_type_index(self.memory_type);
        // SAFETY: a well-formed allocation on this live device.
        let memory = unsafe { device.allocate_memory(&info, None) }
            .map_err(|error| format!("vkAllocateMemory of {size} bytes failed with {error:?}"))?;
        // SAFETY: host-visible memory nothing else has mapped.
        let contents =
            unsafe { device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) }
                .map_err(|error| {
                    // SAFETY: nothing is bound to it yet.
                    unsafe { device.free_memory(memory, None) };
                    format!("vkMapMemory failed with {error:?}")
                })? as *mut u8;
        Ok(Block {
            memory,
            contents,
            size,
            free: vec![(0, size)],
            reached: 0,
        })
    }
}

impl State {
    /// A wholly-free block to give back, once a second one is also free.
    fn spare_block(&self) -> Option<usize> {
        let mut empty = self
            .blocks
            .iter()
            .enumerate()
            .filter(|(index, block)| {
                block.size > 0
                    && block.free.len() == 1
                    && block.free[0] == (0, block.size)
                    && !self.dedicated.contains(index)
            })
            .map(|(index, _)| index);
        let _keep = empty.next()?;
        empty.next()
    }

    /// First fit across the blocks, honouring `alignment` inside the range.
    fn carve(&mut self, size: u64, alignment: u64) -> Option<Allocation> {
        for (index, block) in self.blocks.iter_mut().enumerate() {
            if block.size == 0 {
                continue;
            }
            for (at, (start, length)) in block.free.iter().copied().enumerate() {
                let aligned = start.next_multiple_of(alignment);
                let head = aligned - start;
                if head + size > length {
                    continue;
                }
                block.free.remove(at);
                if head > 0 {
                    block.free.insert(at, (start, head));
                }
                let tail = start + length - (aligned + size);
                if tail > 0 {
                    let after = at + usize::from(head > 0);
                    block.free.insert(after, (aligned + size, tail));
                }
                let pristine = aligned >= block.reached;
                block.reached = block.reached.max(aligned + size);
                return Some(Allocation {
                    memory: block.memory,
                    // SAFETY: `aligned` is inside a mapping of `block.size`.
                    contents: unsafe { block.contents.add(aligned as usize) },
                    block: Some(index),
                    offset: aligned,
                    size,
                    pristine,
                });
            }
        }
        None
    }
}

impl Block {
    /// Return a range, merging it with whichever neighbours are also free.
    fn release(&mut self, offset: u64, size: u64) {
        let at = self.free.partition_point(|(start, _)| *start < offset);
        self.free.insert(at, (offset, size));
        // Merge forward first, so the backward merge sees the whole run.
        if at + 1 < self.free.len() {
            let (next_start, next_size) = self.free[at + 1];
            if offset + size == next_start {
                self.free[at].1 += next_size;
                self.free.remove(at + 1);
            }
        }
        if at > 0 {
            let (previous_start, previous_size) = self.free[at - 1];
            if previous_start + previous_size == offset {
                self.free[at - 1].1 += self.free[at].1;
                self.free.remove(at);
            }
        }
    }
}

impl Allocation {
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// The allocation of a buffer which was never given one. Freeing it is a
    /// no-op, because it holds no range of any block.
    pub fn none() -> Self {
        Self {
            memory: vk::DeviceMemory::null(),
            contents: std::ptr::null_mut(),
            block: None,
            offset: 0,
            size: 0,
            pristine: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(size: u64) -> Block {
        Block {
            memory: vk::DeviceMemory::null(),
            contents: std::ptr::null_mut(),
            size,
            free: vec![(0, size)],
            reached: 0,
        }
    }

    #[test]
    fn a_freed_range_merges_with_both_of_its_neighbours() {
        let mut block = block(300);
        block.free.clear();
        block.release(0, 100);
        block.release(200, 100);
        assert_eq!(block.free, [(0, 100), (200, 100)]);
        block.release(100, 100);
        assert_eq!(block.free, [(0, 300)]);
    }

    #[test]
    fn carving_honours_alignment_and_keeps_the_skipped_head_free() {
        let mut state = State {
            blocks: vec![block(1024)],
            dedicated: Vec::new(),
        };
        let first = state.carve(8, 1).expect("room");
        assert_eq!(first.offset, 0);
        let aligned = state.carve(16, 256).expect("room");
        assert_eq!(aligned.offset, 256);
        // The bytes between the two are still available.
        let head = state.carve(8, 1).expect("room");
        assert_eq!(head.offset, 8);
    }

    /// A range some earlier buffer used is not pristine, so its new owner
    /// zeroes it; one above everything ever carved is memory as the driver
    /// handed it over, and needs no writing at all.
    #[test]
    fn only_a_reused_range_is_reported_as_needing_zeroes() {
        let mut state = State {
            blocks: vec![block(1024)],
            dedicated: Vec::new(),
        };
        let first = state.carve(128, 1).expect("room");
        assert!(first.pristine);
        let second = state.carve(128, 1).expect("room");
        assert!(second.pristine);
        state.blocks[0].release(first.offset, first.size);
        let again = state.carve(128, 1).expect("room");
        assert_eq!(again.offset, first.offset);
        assert!(!again.pristine);
    }

    #[test]
    fn a_block_which_fills_and_empties_is_whole_again() {
        let mut state = State {
            blocks: vec![block(1024)],
            dedicated: Vec::new(),
        };
        let mut taken = Vec::new();
        while let Some(allocation) = state.carve(128, 1) {
            taken.push(allocation);
        }
        assert_eq!(taken.len(), 8);
        for allocation in taken {
            state.blocks[0].release(allocation.offset, allocation.size);
        }
        assert_eq!(state.blocks[0].free, [(0, 1024)]);
    }
}
