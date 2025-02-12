//! This crate is intended to contain code that is required to provide or
//! improve the observability of a system. That includes initialization logic
//! for metrics and logging as well as logging helper functions.
pub mod metrics;
pub mod panic_hook;
pub mod request_id;
pub mod tracing;
