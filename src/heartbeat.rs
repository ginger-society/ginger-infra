use sysinfo::{Components, Disks, Networks, System};
use serde_json::{json, Value};

pub fn collect_stats() -> Value {
    let mut sys = System::new_all();
    sys.refresh_all();

    // ── CPU ──────────────────────────────────────────────────────────────────
    let global_cpu = sys.global_cpu_info().cpu_usage();
    let per_core: Vec<Value> = sys
        .cpus()
        .iter()
        .enumerate()
        .map(|(i, cpu)| {
            json!({
                "core": i,
                "usage_pct": (cpu.cpu_usage() * 10.0).round() / 10.0,
                "frequency_mhz": cpu.frequency(),
            })
        })
        .collect();

    // ── Memory ───────────────────────────────────────────────────────────────
    let total_mem  = sys.total_memory();   // bytes
    let used_mem   = sys.used_memory();
    let total_swap = sys.total_swap();
    let used_swap  = sys.used_swap();

    // ── Disk ─────────────────────────────────────────────────────────────────
    let disks = Disks::new_with_refreshed_list();
    let storage: Vec<Value> = disks
        .iter()
        .map(|d| {
            json!({
                "mount":        d.mount_point().to_string_lossy(),
                "total_mb":     mb(d.total_space()),
                "available_mb": mb(d.available_space()),
                "used_pct":     usage_pct(d.total_space(), d.available_space()),
                "fs":           d.file_system().to_string_lossy(),
            })
        })
        .collect();

    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "cpu": {
            "global_usage_pct": (global_cpu * 10.0).round() / 10.0,
            "cores": per_core,
        },
        "memory": {
            "total_mb":     mb(total_mem),
            "used_mb":      mb(used_mem),
            "free_mb":      mb(total_mem.saturating_sub(used_mem)),
            "used_pct":     usage_pct(total_mem, total_mem.saturating_sub(used_mem)),
            "swap_total_mb": mb(total_swap),
            "swap_used_mb":  mb(used_swap),
        },
        "storage": storage,
    })
}

fn mb(bytes: u64) -> f64 {
    (bytes as f64 / 1_048_576.0 * 10.0).round() / 10.0
}

fn usage_pct(total: u64, available: u64) -> f64 {
    if total == 0 { return 0.0; }
    let used = total.saturating_sub(available);
    ((used as f64 / total as f64) * 1000.0).round() / 10.0
}