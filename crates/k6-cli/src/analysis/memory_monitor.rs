/// Runtime memory growth detection for VUs.
///
/// Samples QuickJS heap usage per VU every N iterations.
/// Uses linear regression on recent samples to detect monotonic growth.
/// Warns before the VU hits its memory limit.

const SAMPLE_INTERVAL: u32 = 50; // Sample every 50 iterations
const MIN_SAMPLES: usize = 5; // Need at least 5 samples for regression
const GROWTH_THRESHOLD_KB: f64 = 1.0; // Warn if growing > 1 KB/iteration

#[derive(Debug)]
pub struct MemoryMonitor {
    samples: Vec<(u32, usize)>, // (iteration, heap_bytes)
    memory_limit: usize,
    warned: bool,
}

#[derive(Debug, Clone)]
pub struct MemoryWarning {
    pub vu_id: u32,
    pub current_heap_mb: f64,
    pub growth_kb_per_iter: f64,
    pub iters_until_limit: u64,
    pub estimated_minutes: f64,
}

impl MemoryMonitor {
    pub fn new(memory_limit: usize) -> Self {
        Self {
            samples: Vec::with_capacity(64),
            memory_limit,
            warned: false,
        }
    }

    /// Call after each iteration with the current heap size.
    /// Returns a warning if growth is detected.
    pub fn check(&mut self, iteration: u32, heap_bytes: usize, vu_id: u32, iters_per_sec: f64) -> Option<MemoryWarning> {
        // Only sample every N iterations
        if iteration % SAMPLE_INTERVAL != 0 {
            return None;
        }

        self.samples.push((iteration, heap_bytes));

        // Keep last 20 samples to avoid unbounded growth of the monitor itself
        if self.samples.len() > 20 {
            self.samples.drain(..self.samples.len() - 20);
        }

        if self.samples.len() < MIN_SAMPLES {
            return None;
        }

        // Only warn once
        if self.warned {
            return None;
        }

        // Linear regression: growth_per_iteration
        let growth = self.estimate_growth();

        if growth > GROWTH_THRESHOLD_KB * 1024.0 {
            // growth is in bytes per iteration
            let growth_kb = growth / 1024.0;
            let remaining_bytes = self.memory_limit.saturating_sub(heap_bytes);
            let iters_until_limit = if growth > 0.0 {
                (remaining_bytes as f64 / growth) as u64
            } else {
                u64::MAX
            };

            let estimated_minutes = if iters_per_sec > 0.0 {
                iters_until_limit as f64 / iters_per_sec / 60.0
            } else {
                f64::INFINITY
            };

            self.warned = true;

            Some(MemoryWarning {
                vu_id,
                current_heap_mb: heap_bytes as f64 / (1024.0 * 1024.0),
                growth_kb_per_iter: growth_kb,
                iters_until_limit,
                estimated_minutes,
            })
        } else {
            None
        }
    }

    /// Estimate bytes growth per iteration via linear regression on samples.
    fn estimate_growth(&self) -> f64 {
        let n = self.samples.len() as f64;
        if n < 2.0 {
            return 0.0;
        }

        let mut sum_x = 0.0;
        let mut sum_y = 0.0;
        let mut sum_xy = 0.0;
        let mut sum_xx = 0.0;

        for &(iter, heap) in &self.samples {
            let x = iter as f64;
            let y = heap as f64;
            sum_x += x;
            sum_y += y;
            sum_xy += x * y;
            sum_xx += x * x;
        }

        let denominator = n * sum_xx - sum_x * sum_x;
        if denominator.abs() < 1e-10 {
            return 0.0;
        }

        // Slope = bytes per iteration
        (n * sum_xy - sum_x * sum_y) / denominator
    }
}

impl MemoryWarning {
    pub fn format(&self) -> String {
        format!(
            "\u{26a0}  VU {} heap growing ~{:.1} KB/iteration ({:.1} MB → {:.1} MB limit)\n\
             Will hit memory limit in ~{} iterations (~{:.0} minutes at current rate)\n\
             This usually means an unbounded cache, array, or map in your script.\n\
             Tip: Use SharedCache (bounded), or clear data periodically.",
            self.vu_id,
            self.growth_kb_per_iter,
            self.current_heap_mb,
            64.0, // our default limit
            self.iters_until_limit,
            self.estimated_minutes,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_warning_for_stable_memory() {
        let mut monitor = MemoryMonitor::new(64 * 1024 * 1024);

        for i in 1..=500 {
            // Stable at ~2MB with small noise
            let heap = 2_000_000 + (i % 100) * 10;
            let warning = monitor.check(i * SAMPLE_INTERVAL, heap as usize, 0, 10.0);
            assert!(warning.is_none(), "unexpected warning at iteration {i}");
        }
    }

    #[test]
    fn warns_on_monotonic_growth() {
        let mut monitor = MemoryMonitor::new(64 * 1024 * 1024);
        let mut found_warning = false;

        for i in 1..=20 {
            let iter = i * SAMPLE_INTERVAL;
            // Growing 20KB per sample interval (= 20KB/50 = 400 bytes per iteration)
            // But we sample at SAMPLE_INTERVAL intervals, so growth per iteration
            // needs to be > 1KB. Let's grow 100KB per sample = 2KB/iter
            let heap = 2_000_000 + i as usize * 100_000;
            if let Some(warning) = monitor.check(iter, heap, 42, 10.0) {
                assert_eq!(warning.vu_id, 42);
                assert!(warning.growth_kb_per_iter > 0.5);
                assert!(warning.iters_until_limit > 0);
                found_warning = true;
                break;
            }
        }

        assert!(found_warning, "should have detected memory growth");
    }

    #[test]
    fn warns_only_once() {
        let mut monitor = MemoryMonitor::new(64 * 1024 * 1024);
        let mut warning_count = 0;

        for i in 1..=100 {
            let iter = i * SAMPLE_INTERVAL;
            let heap = 2_000_000 + i as usize * 100_000;
            if monitor.check(iter, heap, 0, 10.0).is_some() {
                warning_count += 1;
            }
        }

        assert_eq!(warning_count, 1);
    }

    #[test]
    fn estimate_growth_accuracy() {
        let mut monitor = MemoryMonitor::new(64 * 1024 * 1024);

        // Add samples with exactly 2000 bytes growth per iteration
        for i in 0..10 {
            monitor.samples.push((i * 50, 1_000_000 + i as usize * 100_000));
        }

        let growth = monitor.estimate_growth();
        // 100_000 bytes per 50 iterations = 2000 bytes per iteration
        assert!(
            (growth - 2000.0).abs() < 10.0,
            "expected ~2000 bytes/iter, got {growth}"
        );
    }

    #[test]
    fn format_warning_readable() {
        let warning = MemoryWarning {
            vu_id: 5,
            current_heap_mb: 8.5,
            growth_kb_per_iter: 19.5,
            iters_until_limit: 2900,
            estimated_minutes: 48.3,
        };

        let output = warning.format();
        assert!(output.contains("VU 5"));
        assert!(output.contains("19.5 KB/iteration"));
        assert!(output.contains("2900 iterations"));
        assert!(output.contains("48 minutes"));
    }
}
