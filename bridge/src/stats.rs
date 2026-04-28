use serde_json::{json, Value};
use std::sync::Mutex;
use sysinfo::{Disks, System};

static SYS: once_cell::sync::Lazy<Mutex<System>> = once_cell::sync::Lazy::new(|| {
    let mut sys = System::new_all();
    sys.refresh_cpu_all();
    Mutex::new(sys)
});

pub fn collect() -> Value {
    let mut sys = SYS.lock().unwrap();
    sys.refresh_memory();
    sys.refresh_cpu_all();

    let cpu_pct = sys.global_cpu_usage();
    let ram_free = sys.available_memory();

    let disks = Disks::new_with_refreshed_list();
    let disk_free: u64 = disks.iter().map(|d| d.available_space()).sum();

    json!({
        "type": "system_stats",
        "cpu_pct": cpu_pct,
        "ram_free": ram_free,
        "disk_free": disk_free
    })
}

pub fn collect_with_agents(agent_ids: &[String]) -> Value {
    let mut stats = collect();
    if let Some(obj) = stats.as_object_mut() {
        obj.insert("agent_ids".to_string(), json!(agent_ids));
    }
    stats
}
