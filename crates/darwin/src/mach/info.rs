//! The informational half of Mach: what a process asks about itself, and what
//! it asks about the machine.
//!
//! Not a port question like the rest of this directory - nothing is sent and no
//! right changes hands - but these are spelled as Mach calls, so they are
//! denied by the `task_` and `host_` prefixes along with the families and have
//! to be answered here.
//!
//! They have to be answered rather than refused because a game engine reports
//! its own memory use and budgets against the machine's, and this is where both
//! figures come from. An import nothing defines is a call to address zero on
//! the native backend, which is where the client was dying twice over:
//! `task_info(0x103, 22, ...)` and then `host_page_size(0x203, ...)`, each with
//! `rip` at zero.
//!
//! Each flavour is a different structure and the caller says how many 32-bit
//! words it has room for, on the way in *and* on the way out. The structures
//! are packed to four bytes: Mach lays them out as arrays of `natural_t`, so a
//! 64-bit size follows a leading `integer_t` with no padding, and letting Rust
//! align them naturally moves every field after the first.
//!
//! The figures come from `/proc/self/statm` and `getrusage`. What this host has
//! no counterpart for stays zero, which is what Mach itself reports for a
//! figure it has none of - and the flavours not listed here are refused, not
//! zero-filled, because a caller cannot tell an invented measurement from a
//! real one.

#![cfg(not(target_os = "macos"))]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::kern::{KERN_INVALID_ARGUMENT, KERN_SUCCESS};
use super::space::{HOST_SELF, Name, TASK_SELF};

const TASK_THREAD_TIMES_INFO: i32 = 3;
const TASK_BASIC_INFO_64: i32 = 5;
const MACH_TASK_BASIC_INFO: i32 = 20;
const TASK_VM_INFO: i32 = 22;

/// `time_value_t`, which is a `timeval` of two `integer_t`.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct TimeValue {
    seconds: i32,
    microseconds: i32,
}

/// `task_basic_info_64`, the flavour a 64-bit process gets from
/// `TASK_BASIC_INFO`.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct TaskBasicInfo64 {
    suspend_count: i32,
    virtual_size: u64,
    resident_size: u64,
    user_time: TimeValue,
    system_time: TimeValue,
    policy: i32,
}

/// `mach_task_basic_info`, which leads with the sizes and adds a peak.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct MachTaskBasicInfo {
    virtual_size: u64,
    resident_size: u64,
    resident_size_max: u64,
    user_time: TimeValue,
    system_time: TimeValue,
    policy: i32,
    suspend_count: i32,
}

/// `task_thread_times_info`: the CPU time of the task's *live* threads.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct TaskThreadTimesInfo {
    user_time: TimeValue,
    system_time: TimeValue,
}

/// `task_vm_info`, as far as `phys_footprint`.
///
/// The footprint is the field callers actually want and it is at offset 144,
/// so every member before it has to be present and in the right place even
/// though most of them stay zero. The structure continues past this point in
/// the real header; a caller with room for more gets zeroes, which is the same
/// answer as "this host does not count that".
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct TaskVmInfo {
    virtual_size: u64,
    region_count: i32,
    page_size: i32,
    resident_size: u64,
    resident_size_peak: u64,
    device: u64,
    device_peak: u64,
    internal: u64,
    internal_peak: u64,
    external: u64,
    external_peak: u64,
    reusable: u64,
    reusable_peak: u64,
    purgeable_volatile_pmap: u64,
    purgeable_volatile_resident: u64,
    purgeable_volatile_virtual: u64,
    compressed: u64,
    compressed_peak: u64,
    compressed_lifetime: u64,
    phys_footprint: u64,
}

const _: () = {
    assert!(size_of::<TaskBasicInfo64>() == 40);
    assert!(size_of::<MachTaskBasicInfo>() == 48);
    assert!(size_of::<TaskThreadTimesInfo>() == 16);
    assert!(std::mem::offset_of!(TaskVmInfo, phys_footprint) == 144);
};

fn page_size() -> u64 {
    // SAFETY: `sysconf` reads a static system parameter and takes no pointer.
    (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).max(4096) as u64
}

/// `vm_size_t vm_page_size`, `vm_size_t vm_page_mask` and `int vm_page_shift` -
/// the three data symbols `<mach/vm_param.h>` publishes beside
/// [`host_page_size`].
///
/// Written once, before the loader binds anything, by [`publish_page_size`].
pub static VM_PAGE_SIZE: AtomicU64 = AtomicU64::new(0);
pub static VM_PAGE_MASK: AtomicU64 = AtomicU64::new(0);
pub static VM_PAGE_SHIFT: AtomicU32 = AtomicU32::new(0);

/// Fill the three globals above. Idempotent.
pub fn publish_page_size() {
    let size = page_size();
    VM_PAGE_SIZE.store(size, Ordering::Relaxed);
    VM_PAGE_MASK.store(size - 1, Ordering::Relaxed);
    VM_PAGE_SHIFT.store(size.trailing_zeros(), Ordering::Relaxed);
}

/// The page counts `/proc/self/statm` reports: total, resident, shared.
fn statm() -> (u64, u64, u64) {
    let Ok(text) = std::fs::read_to_string("/proc/self/statm") else {
        return (0, 0, 0);
    };
    let mut fields = text
        .split_whitespace()
        .map(|field| field.parse::<u64>().unwrap_or(0));
    (
        fields.next().unwrap_or(0),
        fields.next().unwrap_or(0),
        fields.next().unwrap_or(0),
    )
}

/// This process's virtual and resident size, in bytes.
fn footprint() -> (u64, u64) {
    let (virtual_pages, resident_pages, _) = statm();
    let page = page_size();
    (
        virtual_pages.saturating_mul(page),
        resident_pages.saturating_mul(page),
    )
}

/// Resident memory this process does not share with another, which is the
/// closest thing here to Mach's `phys_footprint`.
///
/// macOS charges a process for its private pages and not for shared or
/// file-backed ones, and an engine budgets against that figure: reporting the
/// whole resident size instead tells it that it is out of memory.
fn private_memory() -> u64 {
    let (_, resident, shared) = statm();
    resident.saturating_sub(shared).saturating_mul(page_size())
}

/// The high-water mark of resident memory, which `mach_task_basic_info`
/// reports separately.
fn resident_peak() -> u64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|kilobytes| kilobytes.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_mul(1024)
}

/// How many mappings this process has, for `task_vm_info`'s region count.
fn region_count() -> i32 {
    std::fs::read_to_string("/proc/self/maps").map_or(0, |maps| maps.lines().count() as i32)
}

/// The CPU time this process has used, as user and system.
fn cpu_time() -> (TimeValue, TimeValue) {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `getrusage` fills the `rusage` it is given and nothing else.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return (TimeValue::default(), TimeValue::default());
    }
    // SAFETY: the call above succeeded, so it is initialised.
    let usage = unsafe { usage.assume_init() };
    let value = |t: libc::timeval| TimeValue {
        seconds: t.tv_sec as i32,
        microseconds: t.tv_usec as i32,
    };
    (value(usage.ru_utime), value(usage.ru_stime))
}

/// Copy `value` into the caller's buffer and say how many words it holds.
///
/// The count is in 32-bit words both ways, and a buffer too small for the
/// flavour is `KERN_INVALID_ARGUMENT` rather than a partial structure: the
/// caller would read the missing fields as measurements.
///
/// # Safety
///
/// `out` is writable for `*count` words, and `count` is writable.
unsafe fn publish<T: Copy>(value: T, out: *mut c_void, count: *mut u32) -> i32 {
    // SAFETY: the caller states `count` is readable.
    let available = unsafe { *count } as usize * size_of::<u32>();
    if available < size_of::<T>() {
        return KERN_INVALID_ARGUMENT;
    }
    // SAFETY: the caller's buffer has room, and is not assumed to be aligned
    // for `T` - Mach's own structures are packed to four bytes.
    unsafe {
        out.cast::<T>().write_unaligned(value);
        *count = (size_of::<T>() / size_of::<u32>()) as u32;
    }
    KERN_SUCCESS
}

/// `kern_return_t task_info(task_name_t, task_flavor_t, task_info_t,
/// mach_msg_type_number_t *)`
///
/// # Safety
///
/// `out` is writable for `*count` 32-bit words, and `count` is writable.
pub unsafe extern "C" fn task_info(
    task: Name,
    flavor: i32,
    out: *mut c_void,
    count: *mut u32,
) -> i32 {
    if out.is_null() || count.is_null() {
        return KERN_INVALID_ARGUMENT;
    }
    // Only this process. Everything below reads `/proc/self`, and answering
    // about the wrong task with a straight face is the failure this crate
    // refuses everywhere else.
    if task != TASK_SELF {
        return KERN_INVALID_ARGUMENT;
    }
    let (virtual_size, resident_size) = footprint();
    let (user_time, system_time) = cpu_time();
    match flavor {
        // SAFETY: the caller's obligation, checked against the flavour's size.
        TASK_BASIC_INFO_64 => unsafe {
            publish(
                TaskBasicInfo64 {
                    virtual_size,
                    resident_size,
                    user_time,
                    system_time,
                    ..TaskBasicInfo64::default()
                },
                out,
                count,
            )
        },
        // SAFETY: as above.
        MACH_TASK_BASIC_INFO => unsafe {
            publish(
                MachTaskBasicInfo {
                    virtual_size,
                    resident_size,
                    resident_size_max: resident_peak().max(resident_size),
                    user_time,
                    system_time,
                    ..MachTaskBasicInfo::default()
                },
                out,
                count,
            )
        },
        // SAFETY: as above.
        TASK_THREAD_TIMES_INFO => unsafe {
            publish(
                TaskThreadTimesInfo {
                    user_time,
                    system_time,
                },
                out,
                count,
            )
        },
        TASK_VM_INFO => {
            // SAFETY: the caller's obligation.
            let available = unsafe { *count } as usize * size_of::<u32>();
            if available < size_of::<TaskVmInfo>() {
                return KERN_INVALID_ARGUMENT;
            }
            // Anything past this structure is zeroed rather than left as the
            // caller found it. A buffer the caller never cleared is the
            // `machdep.cpu.brand_string` trap: the guest reads its own stack
            // back as a measurement and the failure surfaces elsewhere.
            // SAFETY: the caller states the buffer holds `available` bytes.
            unsafe { std::ptr::write_bytes(out.cast::<u8>(), 0, available) };
            let info = TaskVmInfo {
                virtual_size,
                region_count: region_count(),
                page_size: page_size() as i32,
                resident_size,
                resident_size_peak: resident_peak().max(resident_size),
                phys_footprint: private_memory(),
                ..TaskVmInfo::default()
            };
            // SAFETY: as above, and the buffer is at least this long.
            unsafe { out.cast::<TaskVmInfo>().write_unaligned(info) };
            KERN_SUCCESS
        }
        _ => KERN_INVALID_ARGUMENT,
    }
}

/// `kern_return_t host_page_size(host_t, vm_size_t *)`
///
/// # Safety
///
/// `out` is a writable `vm_size_t`.
pub unsafe extern "C" fn host_page_size(host: Name, out: *mut u64) -> i32 {
    if out.is_null() {
        return KERN_INVALID_ARGUMENT;
    }
    if host != HOST_SELF {
        return KERN_INVALID_ARGUMENT;
    }
    // SAFETY: the caller's `vm_size_t`.
    unsafe { out.write(page_size()) };
    KERN_SUCCESS
}

const HOST_VM_INFO: i32 = 2;
const HOST_CPU_LOAD_INFO: i32 = 3;
const HOST_VM_INFO64: i32 = 4;

/// `vm_statistics_data_t`, the 32-bit counters `HOST_VM_INFO` answers with.
///
/// Every count is in *pages*, which is why the caller asked for the page size
/// first.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct VmStatistics {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u32,
    reactivations: u32,
    pageins: u32,
    pageouts: u32,
    faults: u32,
    cow_faults: u32,
    lookups: u32,
    hits: u32,
    purgeable_count: u32,
    purges: u32,
    speculative_count: u32,
}

/// `vm_statistics64_data_t`, the same idea with 64-bit event counters and four
/// more page counts on the end.
///
/// The offsets matter and are asserted below: a caller works out how much
/// memory is available as `external + purgeable + (free - speculative)`, which
/// reads four separate fields, three of them past the point where this
/// structure stops looking like its 32-bit ancestor.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct VmStatistics64 {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: u32,
    throttled_count: u32,
    external_page_count: u32,
    internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64,
}

/// `host_cpu_load_info_data_t`: ticks in each of the four CPU states.
///
/// Darwin orders them `USER`, `SYSTEM`, `IDLE`, `NICE`; `/proc/stat` reports
/// user, **nice**, system, idle. Copying the four in the order they are read is
/// the collision this structure exists to avoid.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct HostCpuLoadInfo {
    user: u32,
    system: u32,
    idle: u32,
    nice: u32,
}

const _: () = {
    assert!(size_of::<VmStatistics>() == 60);
    assert!(std::mem::offset_of!(VmStatistics64, purgeable_count) == 88);
    assert!(std::mem::offset_of!(VmStatistics64, speculative_count) == 92);
    assert!(std::mem::offset_of!(VmStatistics64, external_page_count) == 136);
    assert!(size_of::<VmStatistics64>() == 152);
};

/// The value of `key` in `/proc/meminfo`, in pages.
///
/// The file reports kilobytes and Mach counts pages, so a machine whose page
/// size is not 4 KiB converts rather than assuming.
fn meminfo(text: &str, key: &str) -> u64 {
    let page = page_size().max(1);
    text.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_suffix(" kB"))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_mul(1024)
        / page
}

/// The machine's memory, as the page counts Mach reports.
///
/// What this host does not keep a counter for stays zero rather than being
/// approximated from a neighbour: a caller reading `speculative_count` wants
/// pages the kernel read ahead and has not charged to anyone, and no number
/// here means that. The four that *are* answered are the four a caller needs
/// to work out how much memory is available.
fn vm_pages() -> VmStatistics64 {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    VmStatistics64 {
        free_count: meminfo(&text, "MemFree:") as u32,
        active_count: meminfo(&text, "Active:") as u32,
        inactive_count: meminfo(&text, "Inactive:") as u32,
        // Darwin's "wired" is memory the kernel may not page out, which is
        // what this host calls unevictable.
        wire_count: meminfo(&text, "Unevictable:") as u32,
        // File-backed pages, which are the ones macOS calls external and a
        // caller counts as available because they can be evicted.
        external_page_count: meminfo(&text, "Cached:") as u32,
        internal_page_count: meminfo(&text, "AnonPages:") as u32,
        ..VmStatistics64::default()
    }
}

/// The machine's CPU ticks, in Darwin's order.
fn cpu_ticks() -> HostCpuLoadInfo {
    let Ok(text) = std::fs::read_to_string("/proc/stat") else {
        return HostCpuLoadInfo::default();
    };
    let Some(totals) = text.lines().find_map(|line| line.strip_prefix("cpu ")) else {
        return HostCpuLoadInfo::default();
    };
    let mut fields = totals
        .split_whitespace()
        .map(|field| field.parse::<u32>().unwrap_or(0));
    let (user, nice, system, idle) = (
        fields.next().unwrap_or(0),
        fields.next().unwrap_or(0),
        fields.next().unwrap_or(0),
        fields.next().unwrap_or(0),
    );
    HostCpuLoadInfo {
        user,
        system,
        idle,
        nice,
    }
}

/// `kern_return_t host_statistics(host_t, host_flavor_t, host_info_t,
/// mach_msg_type_number_t *)`
///
/// # Safety
///
/// As [`task_info`]: `out` is writable for `*count` words.
pub unsafe extern "C" fn host_statistics(
    host: Name,
    flavor: i32,
    out: *mut c_void,
    count: *mut u32,
) -> i32 {
    if out.is_null() || count.is_null() || host != HOST_SELF {
        return KERN_INVALID_ARGUMENT;
    }
    match flavor {
        HOST_VM_INFO => {
            let wide = vm_pages();
            // The 32-bit flavour is the same counts in narrower fields, so it
            // is the wide answer with the event counters this host does not
            // keep left where they are: zero.
            let value = VmStatistics {
                free_count: wide.free_count,
                active_count: wide.active_count,
                inactive_count: wide.inactive_count,
                wire_count: wide.wire_count,
                ..VmStatistics::default()
            };
            // SAFETY: the caller's obligation, checked against the size.
            unsafe { publish(value, out, count) }
        }
        // SAFETY: as above.
        HOST_CPU_LOAD_INFO => unsafe { publish(cpu_ticks(), out, count) },
        _ => KERN_INVALID_ARGUMENT,
    }
}

/// `kern_return_t host_statistics64(host_t, host_flavor_t, host_info64_t,
/// mach_msg_type_number_t *)`
///
/// # Safety
///
/// As [`host_statistics`].
pub unsafe extern "C" fn host_statistics64(
    host: Name,
    flavor: i32,
    out: *mut c_void,
    count: *mut u32,
) -> i32 {
    if out.is_null() || count.is_null() || host != HOST_SELF {
        return KERN_INVALID_ARGUMENT;
    }
    match flavor {
        // SAFETY: the caller's obligation, checked against the size.
        HOST_VM_INFO64 => unsafe { publish(vm_pages(), out, count) },
        // The narrow flavours are not answered through the wide call: the
        // caller sized its buffer for the wide structure and would read the
        // fields at the other set of offsets.
        _ => KERN_INVALID_ARGUMENT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read `n` bytes at `offset` out of a buffer `task_info` filled.
    fn word(buffer: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(buffer[offset..offset + 8].try_into().expect("eight bytes"))
    }

    #[test]
    fn the_basic_flavours_report_this_processs_own_memory() {
        let mut info = [0u8; size_of::<MachTaskBasicInfo>()];
        let mut count = (info.len() / 4) as u32;
        // SAFETY: a buffer this test sized, and a live count.
        let rc = unsafe {
            task_info(
                TASK_SELF,
                MACH_TASK_BASIC_INFO,
                info.as_mut_ptr().cast(),
                &raw mut count,
            )
        };
        assert_eq!(rc, KERN_SUCCESS);
        assert_eq!(count, 12);
        let resident = word(&info, 8);
        assert!(resident > 0, "this process is resident in memory");
        assert!(word(&info, 0) >= resident, "virtual covers resident");
        assert!(word(&info, 16) >= resident, "the peak covers the present");
    }

    #[test]
    fn the_vm_flavour_answers_the_footprint_at_the_offset_callers_read() {
        let mut info = [0u8; 512];
        let mut count = (info.len() / 4) as u32;
        // SAFETY: as above.
        let rc = unsafe {
            task_info(
                TASK_SELF,
                TASK_VM_INFO,
                info.as_mut_ptr().cast(),
                &raw mut count,
            )
        };
        assert_eq!(rc, KERN_SUCCESS);
        let footprint = word(&info, 144);
        assert!(footprint > 0, "some memory is this process's own");
        assert!(
            footprint <= word(&info, 16),
            "and no more than the resident"
        );
        assert_eq!(
            u32::from_le_bytes(info[12..16].try_into().expect("four bytes")) as u64,
            page_size()
        );
    }

    #[test]
    fn the_page_size_is_a_page_size() {
        let mut size = 0u64;
        // SAFETY: a live `vm_size_t`.
        assert_eq!(
            unsafe { host_page_size(HOST_SELF, &raw mut size) },
            KERN_SUCCESS
        );
        assert!(size >= 4096 && size.is_power_of_two());
        // SAFETY: a null out-parameter is what this checks.
        assert_eq!(
            unsafe { host_page_size(HOST_SELF, std::ptr::null_mut()) },
            KERN_INVALID_ARGUMENT
        );
    }

    /// The four fields a caller works "available memory" out of, read at the
    /// offsets it reads them at rather than through this file's own structure.
    #[test]
    fn the_wide_vm_statistics_answer_what_available_memory_is_computed_from() {
        let mut info = [0u8; size_of::<VmStatistics64>()];
        let mut count = (info.len() / 4) as u32;
        // SAFETY: a buffer this test sized, and a live count.
        let rc = unsafe {
            host_statistics64(
                HOST_SELF,
                HOST_VM_INFO64,
                info.as_mut_ptr().cast(),
                &raw mut count,
            )
        };
        assert_eq!(rc, KERN_SUCCESS);
        assert_eq!(count, 38);
        let at = |offset: usize| {
            u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four bytes")) as u64
        };
        let (free, purgeable, speculative, external) = (at(0), at(88), at(92), at(136));
        assert!(free > 0, "the machine has some free memory");
        assert!(external > 0, "and some file-backed memory");
        let available = (external + purgeable + (free - speculative)) * page_size();
        assert!(available > 0);
    }

    /// The narrow flavour is a different structure, and the CPU ticks are in
    /// Darwin's order and not this host's.
    #[test]
    fn the_narrow_flavours_are_their_own_structures() {
        let mut info = [0u8; 128];
        let mut count = (size_of::<VmStatistics>() / 4) as u32;
        // SAFETY: a buffer larger than either flavour, and a live count.
        unsafe {
            assert_eq!(
                host_statistics(
                    HOST_SELF,
                    HOST_VM_INFO,
                    info.as_mut_ptr().cast(),
                    &raw mut count
                ),
                KERN_SUCCESS
            );
            assert_eq!(count, 15);

            let mut count = 4;
            assert_eq!(
                host_statistics(
                    HOST_SELF,
                    HOST_CPU_LOAD_INFO,
                    info.as_mut_ptr().cast(),
                    &raw mut count
                ),
                KERN_SUCCESS
            );
        }
        let ticks: Vec<u32> = (0..4)
            .map(|i| u32::from_le_bytes(info[i * 4..i * 4 + 4].try_into().expect("four bytes")))
            .collect();
        assert!(ticks[0] > 0, "user time has passed");
        assert!(
            ticks[2] > ticks[3],
            "idle is the third field and nice the fourth, which is not this host's order"
        );
        // And the wide flavour is not reachable through the narrow call, or a
        // caller would read the wide fields at the narrow offsets.
        let mut count = (info.len() / 4) as u32;
        // SAFETY: as above.
        assert_eq!(
            unsafe {
                host_statistics(
                    HOST_SELF,
                    HOST_VM_INFO64,
                    info.as_mut_ptr().cast(),
                    &raw mut count,
                )
            },
            KERN_INVALID_ARGUMENT
        );
    }

    /// A flavour nothing here measures, a buffer too small for the one asked
    /// for, and another task are all refused rather than answered with zeroes.
    #[test]
    fn what_it_cannot_answer_is_refused() {
        let info = [0u8; 512];
        let mut count = (info.len() / 4) as u32;
        let ask = |flavor, task, count: &mut u32| {
            let mut info = info;
            // SAFETY: a buffer this test sized, and a live count.
            unsafe { task_info(task, flavor, info.as_mut_ptr().cast(), count) }
        };
        assert_eq!(ask(999, TASK_SELF, &mut count), KERN_INVALID_ARGUMENT);
        assert_eq!(
            ask(TASK_VM_INFO, TASK_SELF + 1, &mut count),
            KERN_INVALID_ARGUMENT,
            "another task is not this one"
        );
        let mut small = 4;
        assert_eq!(
            ask(TASK_VM_INFO, TASK_SELF, &mut small),
            KERN_INVALID_ARGUMENT
        );
        // SAFETY: a null buffer is what this checks.
        assert_eq!(
            unsafe {
                task_info(
                    TASK_SELF,
                    TASK_VM_INFO,
                    std::ptr::null_mut(),
                    &raw mut count,
                )
            },
            KERN_INVALID_ARGUMENT
        );
    }
}
