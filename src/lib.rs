//! Jetway: an LLM gateway in front of Ollama. See `docs/plan/README.md` for the design.

pub mod app;
pub mod cache;
pub mod config;
pub mod dev;
pub mod metrics;
pub mod protocol;
pub mod proxy;
pub mod server;
pub mod store;
pub mod telemetry;
pub mod web;
