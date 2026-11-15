//! GW2 Enhanced installer and launcher.
//!
//! - [`setup`]: installing / removing the mod in a game folder (backups of other mods, the old separate mods).
//! - [`games`]: finding the Guild Wars 2 installs (Steam, Epic Games, ArenaNet's own client).
//! - [`launch`]: starting the game through the right store.
//! - [`github`]: the releases, and downloading a package with the same checks as the in-game updater.
//! - [`settings`]: the installer's settings.
//! - [`app`]: the window (Tauri) and its commands.
//! - [`cli`]: the command line (`GW2Enhanced-Installer.exe "<folder>"`, `--uninstall`).

pub mod app;
pub mod cli;
pub mod dialog;
pub mod games;
pub mod github;
pub mod launch;
pub mod settings;
pub mod setup;
