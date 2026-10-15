use std::time::Duration;

use serde_json::{Value, json};

#[cfg(target_os = "ios")]
mod ios {
    use std::ffi::CString;

    use libc::{c_char, c_int, c_void};

    pub const PAGE_SIZE: u64 = 16_384;

    const HOST_VM_INFO64: c_int = 4;
    const HOST_CPU_LOAD_INFO: c_int = 3;
    const CPU_STATE_COUNT: usize = 4;
    const CPU_STATE_USER: usize = 0;
    const CPU_STATE_SYSTEM: usize = 1;
    const CPU_STATE_IDLE: usize = 2;
    const CPU_STATE_NICE: usize = 3;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
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

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct HostCpuLoadInfo {
        ticks: [u32; CPU_STATE_COUNT],
    }

    unsafe extern "C" {
        fn mach_host_self() -> u32;
        fn host_statistics64(host: u32, flavor: c_int, info: *mut c_void, count: *mut u32)
        -> c_int;
        fn host_statistics(host: u32, flavor: c_int, info: *mut c_void, count: *mut u32) -> c_int;
        fn sysctlbyname(
            name: *const c_char,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> c_int;
    }

    fn sysctl_u64(name: &str) -> Option<u64> {
        let name = CString::new(name).ok()?;
        let mut value: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        // SAFETY: the name is a valid C string, `value` is a live u64 buffer of
        // the declared size, and sysctlbyname only writes within it on success.
        let result = unsafe {
            sysctlbyname(
                name.as_ptr(),
                &mut value as *mut u64 as *mut c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (result == 0).then_some(value)
    }

    pub fn logical_cpus() -> usize {
        sysctl_u64("hw.ncpu").unwrap_or(1) as usize
    }

    pub fn memory_bytes() -> (u64, u64) {
        let total = sysctl_u64("hw.memsize").unwrap_or(0);
        let mut statistics = VmStatistics64::default();
        let mut count =
            (std::mem::size_of::<VmStatistics64>() / std::mem::size_of::<c_int>()) as u32;
        // SAFETY: mach_host_self returns the current host port; `statistics` is
        // a live buffer sized by `count`, which host_statistics64 fills.
        let result = unsafe {
            host_statistics64(
                mach_host_self(),
                HOST_VM_INFO64,
                &mut statistics as *mut _ as *mut c_void,
                &mut count,
            )
        };
        if result != 0 {
            return (0, total);
        }
        let used_pages = u64::from(statistics.active_count)
            + u64::from(statistics.inactive_count)
            + u64::from(statistics.wire_count)
            + u64::from(statistics.compressor_page_count);
        (used_pages.saturating_mul(PAGE_SIZE), total)
    }

    /// Cumulative CPU ticks as (idle, total). The caller diffs two samples to
    /// derive a usage percentage, exactly like the Android sampler reads
    /// /proc/stat twice.
    pub fn cpu_ticks() -> (u64, u64) {
        let mut load = HostCpuLoadInfo::default();
        let mut count =
            (std::mem::size_of::<HostCpuLoadInfo>() / std::mem::size_of::<c_int>()) as u32;
        // SAFETY: mach_host_self returns the current host port; `load` is a
        // live buffer sized by `count`, which host_statistics fills.
        let result = unsafe {
            host_statistics(
                mach_host_self(),
                HOST_CPU_LOAD_INFO,
                &mut load as *mut _ as *mut c_void,
                &mut count,
            )
        };
        if result != 0 {
            return (0, 0);
        }
        let total: u64 = load.ticks.iter().map(|tick| u64::from(*tick)).sum();
        let idle = u64::from(load.ticks[CPU_STATE_IDLE]);
        let _ = (CPU_STATE_USER, CPU_STATE_SYSTEM, CPU_STATE_NICE);
        (idle, total)
    }
}

#[cfg(not(target_os = "ios"))]
mod ios {
    pub fn logical_cpus() -> usize {
        std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
    }

    pub fn memory_bytes() -> (u64, u64) {
        (0, 0)
    }

    pub fn cpu_ticks() -> (u64, u64) {
        (0, 0)
    }
}

#[derive(Default)]
pub struct ResourceSampler {
    last: Option<(u64, u64)>,
}

impl ResourceSampler {
    /// Reports the same shape the desktop Worker sends. The first call needs a
    /// second CPU sample, taken 100 ms later, to derive a usage percentage.
    pub async fn sample(&mut self) -> Value {
        let previous = match self.last.take() {
            Some(previous) => previous,
            None => {
                let first = ios::cpu_ticks();
                tokio::time::sleep(Duration::from_millis(100)).await;
                first
            }
        };
        let current = ios::cpu_ticks();
        self.last = Some(current);
        let (used, total) = ios::memory_bytes();
        json!({
            "logical_cpus": ios::logical_cpus(),
            "cpu_usage_percent": cpu_percent(previous, current),
            "memory_used_bytes": used,
            "memory_total_bytes": total,
        })
    }
}

fn cpu_percent(previous: (u64, u64), current: (u64, u64)) -> f64 {
    let idle_delta = current.0.saturating_sub(previous.0);
    let total_delta = current.1.saturating_sub(previous.1);
    if total_delta == 0 {
        return 0.0;
    }
    let busy = total_delta.saturating_sub(idle_delta);
    (busy as f64) * 100.0 / (total_delta as f64)
}

#[cfg(test)]
mod tests {
    use super::cpu_percent;

    #[test]
    fn cpu_percentage_is_bounded() {
        assert_eq!(cpu_percent((0, 0), (0, 0)), 0.0);
        let fifty = cpu_percent((100, 200), (150, 300));
        assert!((fifty - 50.0).abs() < f64::EPSILON);
    }
}
