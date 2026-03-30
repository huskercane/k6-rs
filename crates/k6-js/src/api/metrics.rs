use std::sync::Arc;

use anyhow::Result;
use rquickjs::{Ctx, Function};

use k6_core::metrics::MetricsRegistry;

/// Register custom metrics constructors: Trend, Counter, Rate, Gauge.
///
/// Usage in JS:
/// ```js
/// import { Trend, Counter, Rate, Gauge } from 'k6/metrics';
/// const myTrend = new Trend('my_trend');
/// myTrend.add(42);
/// ```
///
/// Since we rewrite imports, these are available as globals.
pub fn register(ctx: &Ctx<'_>, registry: Arc<MetricsRegistry>) -> Result<()> {
    // Register low-level Rust functions for each metric type
    {
        let reg = Arc::clone(&registry);
        ctx.globals().set(
            "__metrics_trend_add",
            Function::new(ctx.clone(), move |name: String, value: f64| {
                reg.trend_add(&name, value);
            })?,
        )?;
    }

    {
        let reg = Arc::clone(&registry);
        ctx.globals().set(
            "__metrics_counter_add",
            Function::new(ctx.clone(), move |name: String, value: f64| {
                reg.counter_add(&name, value as u64);
            })?,
        )?;
    }

    {
        let reg = Arc::clone(&registry);
        ctx.globals().set(
            "__metrics_rate_add",
            Function::new(ctx.clone(), move |name: String, value: bool| {
                reg.rate_add(&name, value);
            })?,
        )?;
    }

    {
        let reg = Arc::clone(&registry);
        ctx.globals().set(
            "__metrics_gauge_set",
            Function::new(ctx.clone(), move |name: String, value: f64| {
                reg.gauge_set(&name, value);
            })?,
        )?;
    }

    // JS constructors that wrap the Rust functions
    ctx.eval::<(), _>(r#"
        function Trend(name, isTime) {
            this.name = name;
            this.isTime = isTime !== false;
        }
        Trend.prototype.add = function(value, tags) {
            __metrics_trend_add(this.name, Number(value));
        };

        function Counter(name) {
            this.name = name;
        }
        Counter.prototype.add = function(value, tags) {
            __metrics_counter_add(this.name, Number(value || 1));
        };

        function Rate(name) {
            this.name = name;
        }
        Rate.prototype.add = function(value, tags) {
            __metrics_rate_add(this.name, !!value);
        };

        function Gauge(name) {
            this.name = name;
        }
        Gauge.prototype.add = function(value, tags) {
            __metrics_gauge_set(this.name, Number(value));
        };

        globalThis.Trend = Trend;
        globalThis.Counter = Counter;
        globalThis.Rate = Rate;
        globalThis.Gauge = Gauge;
    "#)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime;

    #[test]
    fn trend_add() {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();
        let registry = Arc::new(MetricsRegistry::new());

        ctx.with(|ctx| {
            register(&ctx, Arc::clone(&registry)).unwrap();

            ctx.eval::<(), _>(r#"
                const myTrend = new Trend('my_trend');
                myTrend.add(100);
                myTrend.add(200);
                myTrend.add(300);
            "#)
            .unwrap();
        });

        let stats = registry.trend_stats("my_trend").unwrap();
        assert_eq!(stats.count, 3);
        assert!((stats.avg - 200.0).abs() < 0.01);
    }

    #[test]
    fn counter_add() {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();
        let registry = Arc::new(MetricsRegistry::new());

        ctx.with(|ctx| {
            register(&ctx, Arc::clone(&registry)).unwrap();

            ctx.eval::<(), _>(r#"
                const myCounter = new Counter('my_counter');
                myCounter.add(1);
                myCounter.add(5);
                myCounter.add(10);
            "#)
            .unwrap();
        });

        assert_eq!(registry.counter_get("my_counter"), 16);
    }

    #[test]
    fn rate_add() {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();
        let registry = Arc::new(MetricsRegistry::new());

        ctx.with(|ctx| {
            register(&ctx, Arc::clone(&registry)).unwrap();

            ctx.eval::<(), _>(r#"
                const myRate = new Rate('my_rate');
                myRate.add(true);
                myRate.add(true);
                myRate.add(false);
            "#)
            .unwrap();
        });

        let (rate, passes, total) = registry.rate_get("my_rate");
        assert_eq!(passes, 2);
        assert_eq!(total, 3);
        assert!((rate - 0.6667).abs() < 0.01);
    }

    #[test]
    fn gauge_set() {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();
        let registry = Arc::new(MetricsRegistry::new());

        ctx.with(|ctx| {
            register(&ctx, Arc::clone(&registry)).unwrap();

            ctx.eval::<(), _>(r#"
                const myGauge = new Gauge('my_gauge');
                myGauge.add(42);
                myGauge.add(99);
            "#)
            .unwrap();
        });

        assert!((registry.gauge_get("my_gauge") - 99.0).abs() < 0.01);
    }

    #[test]
    fn metrics_in_check_pattern() {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();
        let registry = Arc::new(MetricsRegistry::new());

        ctx.with(|ctx| {
            register(&ctx, Arc::clone(&registry)).unwrap();

            // Common pattern: create metric, use in iteration
            ctx.eval::<(), _>(r#"
                const apiDuration = new Trend('api_duration');
                const apiErrors = new Rate('api_errors');

                // Simulate an iteration
                apiDuration.add(150);
                apiErrors.add(false); // no error
                apiDuration.add(200);
                apiErrors.add(true); // error!
            "#)
            .unwrap();
        });

        let stats = registry.trend_stats("api_duration").unwrap();
        assert_eq!(stats.count, 2);

        let (rate, _, total) = registry.rate_get("api_errors");
        assert_eq!(total, 2);
        assert!((rate - 0.5).abs() < 0.01);
    }
}
