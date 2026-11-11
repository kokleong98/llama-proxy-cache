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
//!
//! All counters are **process-wide and monotonic**, so the per-request
//! delta is exact only when a single request is in flight; with
//! concurrent requests each record's delta covers the whole window and
//! the values are shared/approximate. Every field is optional: when a
//! sample is unavailable (unsupported platform, I/O failure) the field
//! renders as `-` in the XLF record.
//!
//! Sampling is deliberately cheap (one `clock_gettime` plus, on Linux,
//! two tiny `/proc` reads) — it runs once per request start and once per
//! request finish. The module uses only `std`: the system calls are
//! declared in small hand-rolled `extern` blocks (the same approach as
//! the macOS `proc_pid_rusage` call), so no third-party crates are
//! needed.

/// One snapshot of the process's monotonic resource counters.
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
}

/// Resource consumption of one request: `end` minus `start` (see the
/// module docs for the process-wide / concurrency caveat).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProcDelta {
    /// CPU time consumed during the request, in milliseconds.
    pub cpu_ms: Option<f64>,
    /// Storage bytes read during the request.
    pub disk_read_bytes: Option<u64>,
    /// Storage bytes written during the request.
    pub disk_write_bytes: Option<u64>,
}

impl ProcSample {
    /// Snapshot the process counters now.
    pub fn now() -> Self {
        let (disk_read_bytes, disk_write_bytes) = sample_disk_io();
        Self {
            cpu_ns: sample_cpu_ns(),
            disk_read_bytes,
            disk_write_bytes,
        }
    }

    /// The consumption of one request that started at `self` and ended
    /// at `end`: saturating per-field subtraction; a field is `None`
    /// when either endpoint is missing it.
    pub fn delta(&self, end: &ProcSample) -> ProcDelta {
        ProcDelta {
            cpu_ms: match (self.cpu_ns, end.cpu_ns) {
                (Some(start), Some(end)) if end >= start => Some((end - start) as f64 / 1_000_000.0),
                _ => None,
            },
            disk_read_bytes: sub64(self.disk_read_bytes, end.disk_read_bytes),
            disk_write_bytes: sub64(self.disk_write_bytes, end.disk_write_bytes),
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
    use unix_ffi::{clock_gettime, CLOCK_PROCESS_CPUTIME_ID};

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
        pub(super) fn GetProcessIoCounters(
            hProcess: Handle,
            lpIoCounters: *mut IoCounters,
        ) -> i32;
    }
}

/// The process's total CPU time (user + system) in nanoseconds
/// (`GetProcessTimes`: 100-nanosecond `FILETIME` units).
#[cfg(windows)]
fn sample_cpu_ns() -> Option<u128> {
    use win_ffi::{GetCurrentProcess, GetProcessTimes, FileTime};

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
    (
        Some(counters.read_transfer),
        Some(counters.write_transfer),
    )
}

/// The process's actual storage I/O so far via libproc's
/// `proc_pid_rusage` (`pid = 0` = self). `libproc` is not re-exported by
/// `libc`, so the call and the `rusage_info_v2` layout are declared
/// here (all 13 fields must be present for the buffer to be the right
/// size; only the two disk counters are read).
#[cfg(target_os = "macos")]
fn sample_disk_io() -> (Option<u64>, Option<u64>) {
    use std::mem::MaybeUninit;
    use std::ptr;

    #[repr(C)]
    #[derive(Default)]
    struct RusageInfoV2 {
        ri_user_time: u64,
        ri_system_time: u64,
        ri_pkg_idle_wkups: u64,
        ri_interrupt_wkups: u64,
        ri_diskio_bytesread: u64,
        ri_diskio_byteswritten: u64,
        ri_cpu_clks: u64,
        ri_io_read_count: u64,
        ri_io_write_count: u64,
        ri_cpu_ticks: u64,
        ri_boottime: u64,
        ri_run_time: u64,
        ri_vnode_reads: u64,
        ri_vnode_writes: u64,
    }

    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut RusageInfoV2) -> i32;
    }

    // RUSAGE_INFO_V2 (sys/proc_info.h).
    const RUSAGE_INFO_V2: i32 = 4;

    let mut info: RusageInfoV2 = unsafe { MaybeUninit::zeroed().assume_init() };
    let ok = unsafe { proc_pid_rusage(0, RUSAGE_INFO_V2, ptr::addr_of_mut!(info)) };
    if ok != 0 {
        return (None, None);
    }
    (Some(info.ri_diskio_bytesread), Some(info.ri_diskio_byteswritten))
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
        };
        let end = ProcSample {
            cpu_ns: Some(2_500_000_000), // +2.5 s of CPU
            disk_read_bytes: Some(131_072),
            disk_write_bytes: Some(50),
        };
        let d = start.delta(&end);
        assert_eq!(d.cpu_ms, Some(2500.0));
        assert_eq!(d.disk_read_bytes, Some(130_972));
        assert_eq!(d.disk_write_bytes, Some(0));

        // A counter going backwards (or a missing endpoint) is `None`.
        let later = ProcSample {
            cpu_ns: Some(1_000_000),
            disk_read_bytes: Some(99),
            disk_write_bytes: Some(50),
        };
        let earlier = ProcSample {
            cpu_ns: Some(500_000),
            disk_read_bytes: Some(150),
            disk_write_bytes: Some(49),
        };
        let d = later.delta(&earlier);
        assert_eq!(d.cpu_ms, None); // went backwards
        assert_eq!(d.disk_read_bytes, Some(51)); // fine
        assert_eq!(d.disk_write_bytes, None); // went backwards
        assert_eq!(start.delta(&ProcSample::default()).cpu_ms, None);
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
        assert!(start.delta(&end).cpu_ms.is_some());
    }
}