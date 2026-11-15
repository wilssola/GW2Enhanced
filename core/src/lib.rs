//! Shared plumbing of GW2 Enhanced.
//!
//! The point of this crate is to survive game updates: a feature never hard-codes where something is in
//! `Gw2-64.exe`, it describes **how to find it** ([`site::Site`]: several strategies, tried in order), the
//! result is **validated** before anyone hooks it, remembered per build ([`cache`]), and what happens to each
//! feature and hook is recorded in the [`registry`] so the overlay can show it.
//!
//! Everything here also works on a plain file (see [`module::Module::from_file_bytes`]), which is what the
//! patch-day tool uses.

pub mod cache;
pub mod module;
pub mod pattern;
pub mod registry;
pub mod resolve;
pub mod signed;
pub mod site;

pub use cache::{Cache, resolve_cached};
pub use module::Module;
pub use pattern::Pattern;
pub use resolve::{Located, ResolveError, resolve};
pub use site::{Entry, Risk, Site, Strategy};
