//! GW2 Translated backend (Cloudflare Worker).
//!
//! Pure logic lives in plain modules that build and test on the host
//! (`cargo test`); the Worker glue (`app`, D1/R2/fetch I/O) only builds for
//! `wasm32-unknown-unknown`.

pub mod api;
pub mod chunk;
pub mod hash;
pub mod headers;
pub mod manifest;
pub mod queries;
pub mod ratelimit;
pub mod release;
pub mod review;
pub mod score;
pub mod time;
pub mod trust;
pub mod validate;

#[cfg(target_arch = "wasm32")]
mod app;
