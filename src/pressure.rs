//! Host CPU and memory sampling for admission control and `/pressure`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const SCALE: f32 = 100.0;

/// Latest host load readings, updated by a background sampler.
#[derive(Default)]
pub struct PressureGauge {
    cpu_centi: AtomicU32,
    memory_centi: AtomicU32,
}

impl PressureGauge {
    /// Current CPU and memory usage as percentages.
    #[must_use]
    pub fn snapshot(&self) -> (f64, f64) {
        (
            f64::from(self.cpu_centi.load(Ordering::Relaxed)) / f64::from(SCALE),
            f64::from(self.memory_centi.load(Ordering::Relaxed)) / f64::from(SCALE),
        )
    }
}

/// Starts the sampler task and returns its shared gauge.
#[must_use]
pub fn spawn_sampler() -> Arc<PressureGauge> {
    let gauge = Arc::new(PressureGauge::default());
    let shared = Arc::clone(&gauge);
    tokio::spawn(async move {
        let mut system = sysinfo::System::new();
        loop {
            system.refresh_cpu_usage();
            system.refresh_memory();
            let cpu = system.global_cpu_usage();
            let total = system.total_memory();
            let memory = if total == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                let ratio = system.used_memory() as f32 / total as f32;
                ratio * 100.0
            };
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                shared
                    .cpu_centi
                    .store((cpu.clamp(0.0, 100.0) * SCALE) as u32, Ordering::Relaxed);
                shared
                    .memory_centi
                    .store((memory.clamp(0.0, 100.0) * SCALE) as u32, Ordering::Relaxed);
            }
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }
    });
    gauge
}
