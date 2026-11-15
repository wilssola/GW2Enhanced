//! GW2 Enhanced installer and launcher (see lib.rs).
//!
//! Ships at the root of every release zip, next to `msimg32.dll` and the data folders of the mods in that package.

// A window, not a console (the command line attaches to the console it was started from).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = gw2enhanced_installer::cli::run(&args) {
        std::process::exit(code);
    }
    gw2enhanced_installer::app::run();
}
