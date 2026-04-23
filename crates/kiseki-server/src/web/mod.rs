//! Admin web UI — cluster-wide dashboard.
//!
//! Serves a vanilla HTML + HTMX + Chart.js dashboard on the metrics
//! HTTP port. Every node shows the full cluster view by scraping
//! peer metrics and caching them locally.

pub mod aggregator;
pub mod api;
pub mod events;
