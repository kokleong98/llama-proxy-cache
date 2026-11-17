//! Process resource sampling for the request log's per-request CPU and
//! disk-I/O fields.
//!
//! The request-log middleware takes one [`ProcSample`] when a request
//! arrives and computes the [`ProcDelta`] against a second sample when
//! the response body completes (or the client disconnects):
//!
//! - **CPU** — the proxy process's own user + system CPU time
//!   (`CLOCK_PROCESS_CPUTIME_ID` on Unix; `GetProcessTimes` on Windows).
//!   The llama.cpp backend is a separate process, so its CPU time is
//!   never counted here.
//! - **Disk** — the process's actual storage I/O:
//!   - Linux: `read_bytes` / `write_bytes` of `/proc/self/io` (bytes
//!     transferred to/from storage; page-cache hits excluded).
//!   - macOS: `ri_diskio_bytesread` / `ri_diskio_byteswritten` from
//!     `proc_pid_rusage` (same semantics).
//!   - Windows: `ReadTransferCount` / `WriteTransferCount` from
//!     `GetProcessIoCounters` — *transfer* counts, i.e. page-cache
//!     inclusive, the closest thing Windows exposes (not strictly
//!     comparable to the Linux/macOS actual-disk numbers).
//! - **Memory** — the process's resident memory, reported as
//!   point-in-time values at request completion (memory is a gauge, not
//!   a monotonic counter, so nothing is subtracted):
//!   - current RSS — Linux: `VmRSS` of `/proc/self/status`; macOS:
//!     `ri_phys_footprint` from `proc_pid_rusage` (Apple's "physical
//!     footprint", the closest equivalent of RSS); Windows:
//!     `WorkingSetSize` from `GetProcessMemoryInfo`.
//!   - peak RSS since process start — Linux: `VmHWM`; macOS:
//!     `ri_phys_footprint_max`; Windows: `PeakWorkingSetSize`.
//!
//! All counters are **process-wide and monotonic**, so the per-request
//! delta is exact only when a single request is in flight; with
//! concurrent requests each record's delta covers the whole window and
//! the values are shared/approximate. Every field is optional: when a
//! sample is unavailable (unsupported platform, I/O failure) the field
//! renders as `-` in the XLF record.
//!
//! Sampling is deliberately cheap (one `clock_gettime` plus, on Linux,
//! three tiny `/proc` reads) — it runs once per request start and once
//! per request finish. The module uses only `std`: the system calls are
//! declared in small hand-rolled `extern` blocks (the same approach as
//! the macOS `proc_pid_rusage` call), so no third-party crates are
//! needed.

/// One snapshot of the process's monotonic resource counters and
/// memory gauges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcSample {
    /// Total CPU time (user + system) consumed by the process so far,
    /// in nanoseconds (`None` when unavailable).
    pub cpu_ns: Option<u128>,
    /// Storage bytes read by the process so far (`None` when
    /// unavailable).
    pub disk_read_bytes: Option<u64>,
    /// Storage bytes written by the process so far (`None` when
    /// unavailable).
    pub disk_write_bytes: Option<u64>,
    /// Resident memory (RSS) of the process, in bytes (`None` when
    /// unavailable). A gauge — not a monotonic counter.
    pub rss_bytes: Option<u64>,
    /// High-water mark of the process's resident memory since process
    /// start, in bytes (`None` when unavailable).
    pub peak_rss_bytes: Option<u64>,
}

/// Resource consumption of one request: for the monotonic counters,
/// `end` minus `start` (see the module docs for the process-wide /
/// concurrency caveat); for the memory gauges, the values at request
/// completion (nothing is subtracted — memory freed during the request
/// was not "consumed").
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProcDelta {
    /// CPU time consumed during the request, in milliseconds.
    pub cpu_ms: Option<f64>,
    /// Storage bytes read during the request.
    pub disk_read_bytes: Option<u64>,
    /// Storage bytes written during the request.
    pub disk_write_bytes: Option<u64>,
    /// The process's resident memory (bytes) when the request
    /// completed.
    pub rss_bytes: Option<u64>,
    /// The process's peak resident memory (bytes, high-water mark since
    /// process start) when the request completed.
    pub peak_rss_bytes: Option<u64>,
}

impl ProcSample {
    /// Snapshot the process counters and memory gauges now.
    pub fn now() -> Self {
        let (disk_read_bytes, disk_write_bytes) = sample_disk_io();
        let (rss_bytes, peak_rss_bytes) = sample_memory();
        Self {
            cpu_ns: sample_cpu_ns(),
            disk_read_bytes,
            disk_write_bytes,
            rss_bytes,
            peak_rss_bytes,
        }
    }

    /// The consumption of one request that started at `self` and ended
    /// at `end`: saturating per-field subtraction for the monotonic
    /// counters (a field is `None` when either endpoint is missing it);
    /// the memory gauges are taken from `end` as-is.
    pub fn delta(&self, end: &ProcSample) -> ProcDelta {
        ProcDelta {
            cpu_ms: match (self.cpu_ns, end.cpu_ns) {
                (Some(start), Some(end)) if end >= start => {
                    Some((end - start) as f64 / 1_000_000.0)
                }
                _ => None,
            },
            disk_read_bytes: sub64(self.disk_read_bytes, end.disk_read_bytes),
            disk_write_bytes: sub64(self.disk_write_bytes, end.disk_write_bytes),
            rss_bytes: end.rss_bytes,
            peak_rss_bytes: end.peak_rss_bytes,
        }
    }
}

/// Saturating `end - start` for one counter field (`None` when either
/// endpoint is missing it or the counter went backwards).
fn sub64(start: Option<u64>, end: Option<u64>) -> Option<u64> {
    match (start, end) {
        (Some(s), Some(e)) if e >= s => Some(e - s),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// CPU time
// ---------------------------------------------------------------------------

/// Hand-rolled FFI for `clock_gettime` — declared here so the crate needs
/// no `libc` dependency (the symbol comes from the default C library:
/// `libSystem` on macOS, `libc` on Linux).
#[cfg(unix)]
mod unix_ffi {
    /// `struct timespec` (`time.h`); `i64` is `time_t` on all 64-bit
    /// targets.
    #[repr(C)]
    #[derive(Default)]
    pub(super) struct Timespec {
        pub(super) tv_sec: i64,
        pub(super) tv_nsec: i64,
    }

    /// `CLOCK_PROCESS_CPUTIME_ID` (monotonic; counts only this process's
    /// own user + system time). The Unix families number it differently
    /// (`sys/time.h`): glibc/musl = 2, Darwin = 3.
    #[cfg(target_os = "macos")]
    pub(super) const CLOCK_PROCESS_CPUTIME_ID: i32 = 3;
    #[cfg(not(target_os = "macos"))]
    pub(super) const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;

    unsafe extern "C" {
        pub(super) fn clock_gettime(clockid: i32, ts: *mut Timespec) -> i32;
    }
}

/// The process's total CPU time (user + system) in nanoseconds.
#[cfg(unix)]
fn sample_cpu_ns() -> Option<u128> {
    use std::mem::MaybeUninit;
    use std::ptr;
    use unix_ffi::{CLOCK_PROCESS_CPUTIME_ID, clock_gettime};

    let mut ts: unix_ffi::Timespec = unsafe { MaybeUninit::zeroed().assume_init() };
    let ok = unsafe { clock_gettime(CLOCK_PROCESS_CPUTIME_ID, ptr::addr_of_mut!(ts)) };
    if ok != 0 || ts.tv_sec < 0 {
        return None;
    }
    Some(ts.tv_sec as u128 * 1_000_000_000 + ts.tv_nsec as u128)
}

/// Hand-rolled FFI for the `kernel32.dll` queries this module needs on
/// Windows — declared here so the crate needs no `windows-sys`
/// dependency. `extern "system"` is the WINAPI calling convention
/// (stdcall on x86; identical to C on the 64-bit targets this crate
/// builds).
#[cfg(windows)]
mod win_ffi {
    pub(super) type Handle = *mut core::ffi::c_void;

    /// `FILETIME` (`winbase.h`): a 64-bit count of 100-nanosecond
    /// intervals.
    #[repr(C)]
    #[derive(Default)]
    pub(super) struct FileTime {
        pub(super) dwLowDateTime: u32,
        pub(super) dwHighDateTime: u32,
    }

    /// `IO_COUNTERS` (`processthreadsapi.h`): per-process I/O counters.
    #[repr(C)]
    #[derive(Default)]
    pub(super) struct IoCounters {
        pub(super) read_ops: u64,
        pub(super) write_ops: u64,
        pub(super) other_ops: u64,
        pub(super) read_transfer: u64,
        pub(super) write_transfer: u64,
        pub(super) other_transfer: u64,
    }

    unsafe extern "system" {
        /// Pseudo-handle for the current process (the calls that take it
        /// never fail because of it).
        pub(super) fn GetCurrentProcess() -> Handle;
        pub(super) fn GetProcessTimes(
            hProcess: Handle,
            lpCreationTime: *mut FileTime,
            lpExitTime: *mut FileTime,
            lpKernelTime: *mut FileTime,
            lpUserTime: *mut FileTime,
        ) -> i32;
        pub(super) fn GetProcessIoCounters(hProcess: Handle, lpIoCounters: *mut IoCounters) -> i32;
    }

    /// `PROCESS_MEMORY_COUNTERS` (`psapi.h`): per-process memory counters.
    #[repr(C)]
    #[derive(Default)]
    pub(super) struct ProcessMemoryCounters {
        pub(super) cb: u32,
        pub(super) page_fault_count: u32,
        pub(super) peak_working_set_size: usize,
        pub(super) working_set_size: usize,
        pub(super) quota_peak_paged_pool_usage: usize,
        pub(super) quota_paged_pool_usage: usize,
        pub(super) quota_non_paged_pool_usage: usize,
        pub(super) pagefile_usage: usize,
        pub(super) peak_pagefile_usage: usize,
    }

    /// `GetProcessMemoryInfo` lives in `psapi.dll` (the other calls above
    /// are in `kernel32.dll`), hence the separate block.
    #[link(name = "psapi")]
    unsafe extern "system" {
        pub(super) fn GetProcessMemoryInfo(
            hProcess: Handle,
            pmc: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }
}

/// The process's total CPU time (user + system) in nanoseconds
/// (`GetProcessTimes`: 100-nanosecond `FILETIME` units).
#[cfg(windows)]
fn sample_cpu_ns() -> Option<u128> {
    use win_ffi::{FileTime, GetCurrentProcess, GetProcessTimes};

    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    if unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return None;
    }
    Some(filetime_ns(&kernel) + filetime_ns(&user))
}

/// A `FILETIME` (100-nanosecond units) as nanoseconds.
#[cfg(windows)]
fn filetime_ns(ft: &win_ffi::FileTime) -> u128 {
    let ticks = ((ft.dwHighDateTime as u128) << 32) | (ft.dwLowDateTime as u128);
    ticks * 100
}

// ---------------------------------------------------------------------------
// Disk I/O
// ---------------------------------------------------------------------------

/// The process's actual storage I/O so far: `(read bytes, write bytes)`
/// (each `None` when unavailable).
#[cfg(target_os = "linux")]
fn sample_disk_io() -> (Option<u64>, Option<u64>) {
    let Ok(content) = std::fs::read_to_string("/proc/self/io") else {
        return (None, None);
    };
    parse_proc_io(&content)
}

/// Parse the `read_bytes` / `write_bytes` lines of `/proc/self/io`
/// (actual storage transfers; `rchar`/`wchar` are not used because they
/// count page-cache hits too and would not be "disk" I/O). Compiled only
/// where it is used: the Linux sampler and the unit tests.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_io(content: &str) -> (Option<u64>, Option<u64>) {
    let mut read = None;
    let mut write = None;
    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Ok(v) = value.trim().parse::<u64>() else {
            continue;
        };
        match key {
            "read_bytes" => read = Some(v),
            "write_bytes" => write = Some(v),
            _ => {}
        }
    }
    (read, write)
}

/// The process's resident memory: `(current RSS bytes, peak RSS bytes
/// since process start)` (each `None` when unavailable).
#[cfg(target_os = "linux")]
fn sample_memory() -> (Option<u64>, Option<u64>) {
    let Ok(content) = std::fs::read_to_string("/proc/self/status") else {
        return (None, None);
    };
    parse_proc_status(&content)
}

/// Parse the `VmRSS` / `VmHWM` lines of `/proc/self/status` (kilobytes
/// → bytes; `VmPeak` is virtual, not resident, so it is not used).
/// Compiled only where it is used: the Linux sampler and the unit tests.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_status(content: &str) -> (Option<u64>, Option<u64>) {
    let mut rss = None;
    let mut peak = None;
    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Ok(v) = value.trim().trim_end_matches(" kB").parse::<u64>() else {
            continue;
        };
        match key {
            "VmRSS" => rss = Some(v * 1024),
            "VmHWM" => peak = Some(v * 1024),
            _ => {}
        }
    }
    (rss, peak)
}

/// The process's I/O transfer counts so far via `GetProcessIoCounters`.
/// Caveat: Windows reports *transfer* counts (page-cache inclusive), not
/// actual disk transfers — the closest thing the API exposes.
#[cfg(windows)]
fn sample_disk_io() -> (Option<u64>, Option<u64>) {
    use win_ffi::{GetCurrentProcess, GetProcessIoCounters, IoCounters};

    let mut counters = IoCounters::default();
    if unsafe { GetProcessIoCounters(GetCurrentProcess(), &mut counters) } == 0 {
        return (None, None);
    }
    (Some(counters.read_transfer), Some(counters.write_transfer))
}

/// The process's resident memory so far via `GetProcessMemoryInfo`:
/// the working set (current RSS) and its peak (high-water mark since
/// process start).
#[cfg(windows)]
fn sample_memory() -> (Option<u64>, Option<u64>) {
    use win_ffi::{GetCurrentProcess, GetProcessMemoryInfo, ProcessMemoryCounters};

    let mut counters = ProcessMemoryCounters::default();
    counters.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
    if unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) } == 0 {
        return (None, None);
    }
    (
        Some(counters.working_set_size as u64),
        Some(counters.peak_working_set_size as u64),
    )
}

/// Hand-rolled FFI for libproc's `proc_pid_rusage` (`pid = 0` = self).
/// `libproc` is not re-exported by `libc`, so the call and the
/// `rusage_info_v4` layout are declared here (all 32 fields must be
/// present for the buffer to be the right size; only the disk counters
/// and the footprint gauges are read).
#[cfg(target_os = "macos")]
mod macos_ffi {
    /// `rusage_info_v4` (`sys/proc_info.h`).
    #[repr(C)]
    #[derive(Default)]
    pub(super) struct RusageInfoV4 {
        pub(super) ri_user_time: u64,
        pub(super) ri_system_time: u64,
        pub(super) ri_pkg_idle_wkups: u64,
        pub(super) ri_interrupt_wkups: u64,
        pub(super) ri_diskio_bytesread: u64,
        pub(super) ri_diskio_byteswritten: u64,
        pub(super) ri_cpu_clks: u64,
        pub(super) ri_io_read_count: u64,
        pub(super) ri_io_write_count: u64,
        pub(super) ri_cpu_ticks: u64,
        pub(super) ri_boottime: u64,
        pub(super) ri_run_time: u64,
        pub(super) ri_vnode_reads: u64,
        pub(super) ri_vnode_writes: u64,
        pub(super) ri_vnode_fs_bytesread: u64,
        pub(super) ri_vnode_fs_byteswritten: u64,
        pub(super) ri_vnode_fs_blocksread: u64,
        pub(super) ri_vnode_fs_blockswritten: u64,
        pub(super) ri_vnode_vfs_bytesread: u64,
        pub(super) ri_vnode_vfs_byteswritten: u64,
        pub(super) ri_vnode_vfs_blocksread: u64,
        pub(super) ri_vnode_vfs_blockswritten: u64,
        pub(super) ri_pageins: u64,
        pub(super) ri_faults: u64,
        pub(super) ri_cow_faults: u64,
        pub(super) ri_compressions: u64,
        pub(super) ri_decompressions: u64,
        pub(super) ri_dirt_max: u64,
        pub(super) ri_idle_wkups: u64,
        pub(super) ri_iokit_idle_wkups: u64,
        pub(super) ri_phys_footprint: u64,
        pub(super) ri_phys_footprint_max: u64,
    }

    unsafe extern "C" {
        pub(super) fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut RusageInfoV4) -> i32;
    }
}

/// One `rusage_info_v4` snapshot of the process (`None` when the call
/// fails).
#[cfg(target_os = "macos")]
fn rusage_info_v4() -> Option<macos_ffi::RusageInfoV4> {
    use macos_ffi::{RusageInfoV4, proc_pid_rusage};
    use std::mem::MaybeUninit;
    use std::ptr;

    // RUSAGE_INFO_V4 (sys/proc_info.h).
    const RUSAGE_INFO_V4: i32 = 4;

    let mut info: RusageInfoV4 = unsafe { MaybeUninit::zeroed().assume_init() };
    let ok = unsafe { proc_pid_rusage(0, RUSAGE_INFO_V4, ptr::addr_of_mut!(info)) };
    if ok != 0 {
        return None;
    }
    Some(info)
}

/// The process's actual storage I/O so far via libproc's
/// `proc_pid_rusage` (the disk counters of the `rusage_info_v4` buffer).
#[cfg(target_os = "macos")]
fn sample_disk_io() -> (Option<u64>, Option<u64>) {
    match rusage_info_v4() {
        Some(info) => (
            Some(info.ri_diskio_bytesread),
            Some(info.ri_diskio_byteswritten),
        ),
        None => (None, None),
    }
}

/// The process's resident memory via `proc_pid_rusage`: the physical
/// footprint (Apple's closest equivalent of RSS) and its high-water
/// mark, from the same `rusage_info_v4` buffer.
#[cfg(target_os = "macos")]
fn sample_memory() -> (Option<u64>, Option<u64>) {
    match rusage_info_v4() {
        Some(info) => (
            Some(info.ri_phys_footprint),
            Some(info.ri_phys_footprint_max),
        ),
        None => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_proc_io_reads_the_disk_counters() {
        let content = "\
rchar: 4092
wchar: 13
syscr: 8
syscw: 1
read_bytes: 2048
write_bytes: 512
cancelled_write_bytes: 0
";
        assert_eq!(parse_proc_io(content), (Some(2048), Some(512)));
    }

    #[test]
    fn parse_proc_io_ignores_other_and_malformed_lines() {
        let content = "rchar: 4092\nread_bytes: not-a-number\nwrite_bytes: 7\n";
        assert_eq!(parse_proc_io(content), (None, Some(7)));
        assert_eq!(parse_proc_io(""), (None, None));
    }

    #[test]
    fn parse_proc_status_reads_the_memory_gauges() {
        let content = "\
Name:\tlpcache
VmPeak:\t   100000 kB
VmSize:\t    90000 kB
VmHWM:\t    51200 kB
VmRSS:\t    31457 kB
VmSwap:\t       0 kB
";
        assert_eq!(
            parse_proc_status(content),
            (Some(31_457 * 1024), Some(51_200 * 1024))
        );
    }

    #[test]
    fn parse_proc_status_ignores_other_and_malformed_lines() {
        let content = "VmPeak: 100000 kB\nVmRSS: not-a-number kB\nVmHWM: 7 kB\n";
        assert_eq!(parse_proc_status(content), (None, Some(7 * 1024)));
        assert_eq!(parse_proc_status(""), (None, None));
    }

    #[cfg(windows)]
    #[test]
    fn filetime_conversion_is_in_100ns_units() {
        use win_ffi::FileTime;
        let ft = FileTime {
            dwLowDateTime: 0xFFFF_FFFF,
            dwHighDateTime: 0,
        };
        assert_eq!(filetime_ns(&ft), 4_294_967_295 * 100);
        let ft = FileTime {
            dwLowDateTime: 0,
            dwHighDateTime: 1,
        };
        assert_eq!(filetime_ns(&ft), (1u128 << 32) * 100);
    }

    #[test]
    fn delta_subtracts_per_field_with_saturating_semantics() {
        let start = ProcSample {
            cpu_ns: Some(0),
            disk_read_bytes: Some(100),
            disk_write_bytes: Some(50),
            rss_bytes: Some(1_000),
            peak_rss_bytes: Some(2_000),
        };
        let end = ProcSample {
            cpu_ns: Some(2_500_000_000), // +2.5 s of CPU
            disk_read_bytes: Some(131_072),
            disk_write_bytes: Some(50),
            rss_bytes: Some(900),
            peak_rss_bytes: Some(3_000),
        };
        let d = start.delta(&end);
        assert_eq!(d.cpu_ms, Some(2500.0));
        assert_eq!(d.disk_read_bytes, Some(130_972));
        assert_eq!(d.disk_write_bytes, Some(0));
        // Memory is a gauge: the values at completion, not a delta
        // (the RSS went *down* between the samples, yet it is reported).
        assert_eq!(d.rss_bytes, Some(900));
        assert_eq!(d.peak_rss_bytes, Some(3_000));

        // A counter going backwards (or a missing endpoint) is `None`.
        let later = ProcSample {
            cpu_ns: Some(1_000_000),
            disk_read_bytes: Some(99),
            disk_write_bytes: Some(50),
            rss_bytes: None,
            peak_rss_bytes: Some(3_000),
        };
        let earlier = ProcSample {
            cpu_ns: Some(500_000),
            disk_read_bytes: Some(150),
            disk_write_bytes: Some(49),
            rss_bytes: Some(1),
            peak_rss_bytes: None,
        };
        let d = later.delta(&earlier);
        assert_eq!(d.cpu_ms, None); // went backwards
        assert_eq!(d.disk_read_bytes, Some(51)); // fine
        assert_eq!(d.disk_write_bytes, None); // went backwards
        assert_eq!(d.rss_bytes, Some(1));
        assert_eq!(d.peak_rss_bytes, None);
        assert_eq!(start.delta(&ProcSample::default()).cpu_ms, None);
        assert_eq!(start.delta(&ProcSample::default()).rss_bytes, None);
    }

    #[test]
    fn live_sampling_reports_monotonic_counters() {
        let start = ProcSample::now();
        // Burn a little CPU (the assertions only need monotonicity, but
        // some work keeps the delta from being a rounding artifact).
        let mut acc: u64 = 0;
        for i in 0..2_000_000 {
            acc = acc.wrapping_add(i);
        }
        let _ = acc;
        let end = ProcSample::now();
        assert!(start.cpu_ns.is_some(), "CPU sampling should work");
        assert!(end.cpu_ns >= start.cpu_ns);
        // Disk fields may legitimately be `None` on restricted systems,
        // but when present they must be monotonic.
        if let (Some(r0), Some(r1)) = (start.disk_read_bytes, end.disk_read_bytes) {
            assert!(r1 >= r0);
        }
        if let (Some(w0), Some(w1)) = (start.disk_write_bytes, end.disk_write_bytes) {
            assert!(w1 >= w0);
        }
        // Memory gauges: may legitimately be `None` on restricted
        // systems; when present they are plain byte counts. (No
        // cross-sample invariant is asserted: the kernel's
        // high-water-mark update is not atomic, so even `VmHWM` can
        // transiently regress under concurrent load.)
        if let Some(rss) = end.rss_bytes {
            assert!(rss > 0);
        }
        if let Some(peak) = end.peak_rss_bytes {
            assert!(peak > 0);
        }
        assert!(start.delta(&end).cpu_ms.is_some());
    }
}
