//! GW2 Enhanced backend (Cloudflare Worker): translation contributions, votes,
//! dictionary releases and the project site.
//!
//! Pure logic lives in plain modules that build and test on the host
//! (`cargo test`); the Worker glue (`app`, D1/R2/fetch I/O) only builds for
//! `wasm32-unknown-unknown`.

pub mod api;
pub mod budget;
pub mod chunk;
pub mod ghrelease;
pub mod hash;
pub mod headers;
pub mod images;
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
