//! The command line, kept from the console installer:
//!
//! ```text
//! GW2Enhanced-Installer.exe                          opens the window
//! GW2Enhanced-Installer.exe "<folder>"               installs the package next to the installer into the folder
//! GW2Enhanced-Installer.exe --uninstall ["<folder>"] uninstalls and restores the previous msimg32.dll
//! ```
//!
//! Without a folder, `--uninstall` uses the folder chosen in the window, else the only install found.

use std::path::{Path, PathBuf};

use crate::{games, launch, setup};

/// `Some(exit code)` when the arguments asked for a command-line run, `None` to open the window.
pub fn run(args: &[String]) -> Option<i32> {
    let remove = args.iter().any(|a| a == "--uninstall");
    let folder = args.iter().find(|a| !a.starts_with("--")).map(PathBuf::from);
    if !remove && folder.is_none() {
        return None;
    }
    attach_console();
    // The language chosen in the window, else the system's.
    crate::i18n::init(&crate::settings::Settings::load().language);
    println!(
        "{}\n",
        tr!(
            "GW2 Enhanced — tradução, texturas HD e gráficos para o Guild Wars 2",
            "GW2 Enhanced — translation, HD textures and graphics for Guild Wars 2"
        )
    );
    let result = (|| {
        let game = match folder {
            Some(f) => f,
            None => default_game()?,
        };
        if !setup::is_game_dir(&game) {
            return Err(tr!("{} não tem o Gw2-64.exe", "{} has no Gw2-64.exe", game.display()));
        }
        println!("{}", tr!("Pasta do jogo: {}", "Game folder: {}", game.display()));
        if launch::game_running_in(&game) {
            return Err(tr!("feche o Guild Wars 2 antes de continuar", "close Guild Wars 2 before continuing"));
        }
        let log = if remove {
            setup::uninstall(&game)?
        } else {
            let pkg = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .unwrap_or_default();
            setup::install(&pkg, &game, std::env::args().any(|a| a == "--replace-other-mod"))?
        };
        for line in log {
            println!("  {line}");
        }
        Ok(())
    })();
    Some(match result {
        Ok(()) => {
            println!("\n{}", tr!("Pronto! Abra o jogo normalmente.", "Done! Start the game as usual."));
            0
        }
        Err(e) => {
            println!("\n{}", tr!("ERRO: {e}", "ERROR: {e}"));
            1
        }
    })
}

fn default_game() -> Result<PathBuf, String> {
    if let Some(d) = crate::app::saved_game_dir() {
        return Ok(d);
    }
    let found = games::find_installs();
    match found.as_slice() {
        [one] => Ok(one.path.clone()),
        [] => Err(tr!("não encontrei o Guild Wars 2; informe a pasta do jogo", "Guild Wars 2 not found; give the game folder")),
        _ => Err(tr!(
            "encontrei mais de uma instalação do Guild Wars 2; informe a pasta do jogo",
            "found more than one Guild Wars 2 install; give the game folder"
        )),
    }
}

/// The window build has no console of its own: print into the one it was started from, if any.
#[cfg(not(windows))]
fn attach_console() {}

#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}
