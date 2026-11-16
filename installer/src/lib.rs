//! GW2 Enhanced installer and launcher (Windows and Linux).
//!
//! - [`setup`]: installing / removing the mod in a game folder (backups of other mods, the old separate mods).
//! - [`games`]: finding the Guild Wars 2 installs (Steam, Epic Games, ArenaNet's own client; Proton and Wine).
//! - [`launch`]: starting the game through the right store.
//! - [`proton`]: the DLL override Wine and Proton need to load the mod (Linux).
//! - [`github`]: the releases, and downloading a package with the same checks as the in-game updater.
//! - [`settings`]: the installer's settings.
//! - [`vdf`]: Steam's KeyValues files.
//! - [`app`]: the window (Tauri) and its commands.
//! - [`cli`]: the command line (`GW2Enhanced-Installer "<folder>"`, `--uninstall`).
//! - [`i18n`]: the messages in Brazilian Portuguese and English ([`tr!`]).

#[macro_use]
pub mod i18n;

pub mod app;
pub mod cli;
pub mod dialog;
pub mod games;
pub mod github;
pub mod launch;
pub mod proton;
pub mod settings;
pub mod setup;
pub mod vdf;
