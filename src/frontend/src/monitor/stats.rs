// Copyright 2024 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use prometheus::core::{AtomicU64, GenericCounter};
use prometheus::{
    exponential_buckets, histogram_opts, register_histogram_vec_with_registry,
    register_histogram_with_registry, register_int_counter_with_registry,
    register_int_gauge_with_registry, Histogram, HistogramVec, IntGauge, Registry,
};
use risingwave_common::metrics::TrAdderGauge;
use risingwave_common::monitor::GLOBAL_METRICS_REGISTRY;
use tokio::task::JoinHandle;

use crate::session::SessionMapRef;

#[derive(Clone)]
pub struct FrontendMetrics {
    pub query_counter_local_execution: GenericCounter<AtomicU64>,
    pub latency_local_execution: Histogram,
    pub active_sessions: IntGauge,
    pub batch_total_mem: TrAdderGauge,
}

pub static GLOBAL_FRONTEND_METRICS: LazyLock<FrontendMetrics> =
    LazyLock::new(|| FrontendMetrics::new(&GLOBAL_METRICS_REGISTRY));

impl FrontendMetrics {
    fn new(registry: &Registry) -> Self {
        let query_counter_local_execution = register_int_counter_with_registry!(
            "frontend_query_counter_local_execution",
            "Total query number of local execution mode",
            registry
        )
        .unwrap();

        let opts = histogram_opts!(
            "frontend_latency_local_execution",
            "latency of local execution mode",
            exponential_buckets(0.01, 2.0, 23).unwrap()
        );
        let latency_local_execution = register_histogram_with_registry!(opts, registry).unwrap();

        let active_sessions = register_int_gauge_with_registry!(
            "frontend_active_sessions",
            "Total number of active sessions in frontend",
            registry
        )
        .unwrap();

        let batch_total_mem = TrAdderGauge::new(
            "frontend_batch_total_mem",
            "All memory usage of batch executors in bytes",
        )
        .unwrap();

        registry
            .register(Box::new(batch_total_mem.clone()))
            .unwrap();

        Self {
            query_counter_local_execution,
            latency_local_execution,
            active_sessions,
            batch_total_mem,
        }
    }

    /// Create a new `FrontendMetrics` instance used in tests or other places.
    pub fn for_test() -> Self {
        GLOBAL_FRONTEND_METRICS.clone()
    }
}

#[derive(Clone)]
pub struct CursorMetrics {
    pub subscription_cursor_error_count: GenericCounter<AtomicU64>,
    pub subscription_cursor_query_duration: HistogramVec,
    pub subscription_cursor_declare_duration: HistogramVec,
    pub subscription_cursor_fetch_duration: HistogramVec,
    _cursor_metrics_collector: CursorMetricsCollector,
}

impl CursorMetrics {
    pub fn new(registry: &Registry, session_map: SessionMapRef) -> Self {
        let subscription_cursor_error_count = register_int_counter_with_registry!(
            "subscription_cursor_error_count",
            "The subscription error num of cursor",
            registry
        )
        .unwrap();
        let opts = histogram_opts!(
            "subscription_cursor_query_duration",
            "The amount of time a query exists inside the cursor",
            exponential_buckets(1.0, 5.0, 11).unwrap(),
        );
        let subscription_cursor_query_duration =
            register_histogram_vec_with_registry!(opts, &["subscription_name"], registry).unwrap();

        let opts = histogram_opts!(
            "subscription_cursor_declare_duration",
            "Subscription cursor duration of declare",
            exponential_buckets(1.0, 5.0, 11).unwrap(),
        );
        let subscription_cursor_declare_duration =
            register_histogram_vec_with_registry!(opts, &["subscription_name"], registry).unwrap();

        let opts = histogram_opts!(
            "subscription_cursor_fetch_duration",
            "Subscription cursor duration of fetch",
            exponential_buckets(1.0, 5.0, 11).unwrap(),
        );
        let subscription_cursor_fetch_duration =
            register_histogram_vec_with_registry!(opts, &["subscription_name"], registry).unwrap();
        Self {
            _cursor_metrics_collector: CursorMetricsCollector::new(session_map, registry),
            subscription_cursor_error_count,
            subscription_cursor_query_duration,
            subscription_cursor_declare_duration,
            subscription_cursor_fetch_duration,
        }
    }
}

pub struct PeriodicCursorMetrics {
    pub subsription_cursor_nums: i64,
    pub invalid_subsription_cursor_nums: i64,
    pub subscription_cursor_last_fetch_duration: HashMap<String, f64>,
}

#[derive(Clone)]
struct CursorMetricsCollector {
    join_handle: Arc<JoinHandle<()>>,
}
impl CursorMetricsCollector {
    fn new(session_map: SessionMapRef, registry: &Registry) -> Self {
        let subsription_cursor_nums = register_int_gauge_with_registry!(
            "subsription_cursor_nums",
            "The number of subscription cursor",
            registry
        )
        .unwrap();
        let invalid_subsription_cursor_nums = register_int_gauge_with_registry!(
            "invalid_subsription_cursor_nums",
            "The number of invalid subscription cursor",
            registry
        )
        .unwrap();

        let opts = histogram_opts!(
            "subscription_cursor_last_fetch_duration",
            "Since the last fetch, the time up to now",
            exponential_buckets(1.0, 5.0, 11).unwrap(),
        );
        let subscription_cursor_last_fetch_duration =
            register_histogram_vec_with_registry!(opts, &["subscription_name"], registry).unwrap();

        let subsription_cursor_nums = Arc::new(subsription_cursor_nums);
        let invalid_subsription_cursor_nums = Arc::new(invalid_subsription_cursor_nums);
        let subscription_cursor_last_fetch_duration =
            Arc::new(subscription_cursor_last_fetch_duration);

        let subsription_cursor_nums_clone = subsription_cursor_nums.clone();
        let invalid_subsription_cursor_nums_clone = invalid_subsription_cursor_nums.clone();
        let subscription_cursor_last_fetch_duration_clone =
            subscription_cursor_last_fetch_duration.clone();
        let session_map_clone = session_map.clone();

        let join_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                let session_vec = {
                    session_map_clone
                        .read()
                        .values()
                        .cloned()
                        .collect::<Vec<_>>()
                };
                let mut subsription_cursor_nums_value = 0;
                let mut invalid_subsription_cursor_nums_value = 0;
                for session in &session_vec {
                    let periodic_cursor_metrics = session
                        .get_cursor_manager()
                        .get_periodic_cursor_metrics()
                        .await;
                    subsription_cursor_nums_value +=
                        periodic_cursor_metrics.subsription_cursor_nums;
                    invalid_subsription_cursor_nums_value +=
                        periodic_cursor_metrics.invalid_subsription_cursor_nums;
                    for (subscription_name, duration) in
                        &periodic_cursor_metrics.subscription_cursor_last_fetch_duration
                    {
                        println!(
                            "subscription_name: {}, duration: {}",
                            subscription_name, duration
                        );
                        subscription_cursor_last_fetch_duration_clone
                            .with_label_values(&[subscription_name])
                            .observe(*duration);
                    }
                }
                subsription_cursor_nums_clone.set(subsription_cursor_nums_value);
                invalid_subsription_cursor_nums_clone.set(invalid_subsription_cursor_nums_value);
            }
        });
        Self {
            join_handle: Arc::new(join_handle),
        }
    }
}
impl Drop for CursorMetricsCollector {
    fn drop(&mut self) {
        self.join_handle.abort();
    }
}
