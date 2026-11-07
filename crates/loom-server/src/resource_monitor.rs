use super::*;

impl ResourceMonitor {
    pub(crate) fn sample(
        &mut self,
        disk_total_bytes: Option<u64>,
        disk_available_bytes: Option<u64>,
    ) -> WorkerNodeResources {
        let mut cpu_usage_percent = None;
        let mut memory_total_bytes = None;
        let mut memory_available_bytes = None;
        if let Some(system) = self.system.as_mut() {
            system.refresh_cpu_all();
            system.refresh_memory();
            if self.has_cpu_baseline {
                cpu_usage_percent = super::cpu_usage_percent(system.global_cpu_usage());
            }
            self.has_cpu_baseline = true;
            let memory_total = system.total_memory();
            memory_total_bytes = (memory_total > 0).then_some(memory_total);
            memory_available_bytes =
                memory_total_bytes.map(|_| system.available_memory().min(memory_total));
        }

        WorkerNodeResources {
            cpu_count: std::thread::available_parallelism()
                .map(|count| count.get())
                .unwrap_or(1),
            cpu_usage_percent,
            memory_usage_percent: memory_usage_percent(memory_total_bytes, memory_available_bytes),
            memory_total_bytes,
            memory_available_bytes,
            disk_total_bytes,
            disk_available_bytes,
        }
    }
}
