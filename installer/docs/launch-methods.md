# Guild Wars 2: where it is installed and how to start it

Notes behind `src/games.rs` (finding installs) and `src/launch.rs` (starting the game). Checked in November 2026.

## Distribution channels

| Channel | Since | Account | How the game must be started |
|---|---|---|---|
| ArenaNet (guildwars2.com) | 2012 | ArenaNet account | `Gw2-64.exe` directly (it is its own launcher and updater) |
| Steam, app **1284210** | 23 Aug 2022 | Steam account (or an ArenaNet account with `-provider Portal`) | through Steam |
| Epic Games Store | 13 Nov 2024 | Epic account | through the Epic Games Launcher |

- Accounts do not move between channels: an account created on one store logs in through that store only, and its
  purchases stay there. All three play on the same worlds.
- There is no Microsoft Store / Xbox version. The macOS client is no longer supported (since 18 Feb 2021) and the
  game is 64-bit only.
- The game folder looks the same on every channel (`Gw2-64.exe`, `Gw2.dat`), so the mod (`msimg32.dll` next to
  `Gw2-64.exe`) installs the same way everywhere.

Sources: [Steam store page](https://store.steampowered.com/app/1284210/Guild_Wars_2/),
[SteamDB 1284210](https://steamdb.info/app/1284210/),
[Guild Wars 2 Is Now on the Epic Games Store (ArenaNet)](https://guildwars2.com/news/guild-wars-2-is-now-on-the-epic-games-store),
[Epic Games Store (GW2 wiki)](https://wiki.guildwars2.com/wiki/Epic_Games_Store),
[Running the game (GW2 wiki)](https://wiki.guildwars2.com/wiki/Running_the_game).

## Finding installs

### ArenaNet client

- Default folder: `C:\Program Files\Guild Wars 2` (ArenaNet's
  [Download the Guild Wars 2 Client](https://help.guildwars2.com/hc/en-us/articles/201862968-Download-the-Guild-Wars-2-Client)).
  The installer lets the player pick another folder, and `Gw2-64.exe` copied anywhere installs the game into its own
  folder ([Running the game](https://wiki.guildwars2.com/wiki/Running_the_game)).
- Registry: `HKLM\SOFTWARE\ArenaNet\Guild Wars 2`, value `Path` = full path of `Gw2-64.exe`
  ([Gw2.dat, GW2 wiki](https://wiki.guildwars2.com/wiki/Gw2.dat)). We also read the `WOW6432Node` and `HKCU` copies
  and accept a folder as well as an exe there.
- Uninstall entry: ArenaNet's uninstall command is `Gw2-64.exe -uninstall` (see the arguments below), so any
  `...\CurrentVersion\Uninstall\*` entry whose `DisplayName` starts with "Guild Wars 2" points at the folder through
  `InstallLocation`, `DisplayIcon` or `UninstallString`.
- Fallback: `<drive>:\Guild Wars 2`, `<drive>:\Games\Guild Wars 2`, `<drive>:\Jogos\Guild Wars 2`, the Program Files
  folders, and the installer's own folder (when the zip was extracted into the game folder).

### Steam

- Steam's folder: `HKCU\Software\Valve\Steam` `SteamPath` (forward slashes) and `SteamExe`; else
  `HKLM\SOFTWARE\WOW6432Node\Valve\Steam` `InstallPath`.
- Libraries: `<Steam>\steamapps\libraryfolders.vdf` lists every library folder (`"path"` keys).
- The game is installed in a library when it has `steamapps\appmanifest_1284210.acf`; its `"installdir"` names the
  folder under `steamapps\common\` (normally `Guild Wars 2`).
- Steam also writes an uninstall entry `HKLM\...\CurrentVersion\Uninstall\Steam App 1284210` with `InstallLocation`
  (seen on a real install; its `UninstallString` is `steam.exe steam://uninstall/1284210`).

### Epic Games Store

- The launcher keeps one JSON manifest per installed game in
  `%ProgramData%\Epic\EpicGamesLauncher\Data\Manifests\*.item`, with `DisplayName`, `InstallLocation`,
  `LaunchExecutable`, `AppName`, `CatalogNamespace`, `CatalogItemId`, the `MainGame*` variants of the last three, and
  `bIsIncompleteInstall`. We take the item whose `LaunchExecutable` is `Gw2-64.exe` (or whose name starts with
  "Guild Wars 2") and skip incomplete installs. The ids are read from the manifest, never hard-coded.
- Sources: the format as read by open-source launchers,
  [game-scanner (epicgames/item.rs)](https://docs.rs/crate/game-scanner/1.1.0/source/src/epicgames/item.rs),
  [game-detector (epic_games.rs)](https://docs.rs/crate/game-detector/0.2.1/source/src/epic_games.rs), and a
  manifest on a real machine.

## Starting the game

| Channel | What we run | Why |
|---|---|---|
| ArenaNet | `Gw2-64.exe <args>` with the game folder as working directory | it patches itself, then shows the login |
| Steam, no arguments | `steam://rungameid/1284210` | Steam starts the game with a character's own Steam launch options and logs the Steam account in |
| Steam, with arguments | `steam.exe -applaunch 1284210 <args>` (fallback `steam://run/1284210//<args>/`) | passes the arguments; Steam may ask the player to confirm a launch with arguments |
| Epic | `com.epicgames.launcher://apps/<namespace>%3A<catalogItemId>%3A<appName>?action=launch&silent=true` (or `.../apps/<appName>?...` when the ids are missing) | Epic logs the Epic account in; `silent=true` keeps the launcher window hidden |

- A Steam or Epic install must go through its store: per the wiki, `-provider Steam` / `-provider Epic` is the
  default there and "can't bypass" the Steam / Epic client. Running their `Gw2-64.exe` directly does not log in.
- The Epic launcher takes no arguments from outside; extra arguments for an Epic install go in Epic's own
  *Manage > Launch Options*.
- Links are opened with `ShellExecuteW("open")`, which hands them to the program registered for the scheme.

Sources: [Steam browser protocol (Valve)](https://developer.valvesoftware.com/wiki/Steam_browser_protocol),
[Command line options (Steam, Valve)](https://developer.valvesoftware.com/wiki/Command_line_options),
[Command line arguments (GW2 wiki)](https://wiki.guildwars2.com/wiki/Command_line_arguments),
[Epic Games Launcher protocol notes (OriginSteamOverlayLauncher wiki)](https://github.com/WombatFromHell/OriginSteamOverlayLauncher/wiki/Epic-Games-Launcher),
[How do I create a shortcut for games (Epic Games support)](https://www.epicgames.com/help/c-202300000001639/c-202300000001735/how-do-i-create-a-shortcut-for-games-a202300000015684?lang=en-US).

## Command-line arguments that matter

From [Command line arguments (GW2 wiki)](https://wiki.guildwars2.com/wiki/Command_line_arguments):

| Argument | Effect | In the installer |
|---|---|---|
| `-autologin` | logs in at once, only if the client remembers the account e-mail and password | toggle |
| `-shareArchive` | opens `Gw2.dat` in shared mode, so other programs (or a second client) can read it while the game runs | toggle |
| `-mapLoadinfo` | shows load percentage and elapsed time on loading screens | toggle |
| `-windowed` | starts in a window (Alt+Enter switches in game) | toggle |
| `-image` | runs only the patcher: downloads the update and closes without the login screen | "Só atualizar o jogo" (ArenaNet client only) |
| `-provider <Portal\|Steam\|Epic>` | login provider, case-sensitive. `Steam`/`Epic` are the defaults on those stores; `Portal` logs an ArenaNet account in when launching through Steam or Epic. Does not link accounts | free text |
| `-clientport 443` / `-clientport 80` | connects on port 443/80 instead of 6112 (networks that block it) | free text |
| `-repair` | checks and fixes the game files (can take 30-60 min) | free text |
| `-verify` | verifies `Gw2.dat` | free text |
| `-diag` | writes `NetworkDiag.log` instead of starting the game | free text |
| `-log` | writes a log for Support (`%APPDATA%\Guild Wars 2`) | free text |
| `-dat <file>` | uses another `Gw2.dat` | free text |
| `-mumble <name>` | name of the MumbleLink shared memory (`0` turns it off) | free text |
| `-uninstall` | shows the game's uninstall dialog | not offered |

Arguments the wiki does not list (`-email`, `-password`, `-nopatchui`, `-dx9`) are not offered: some were removed by
ArenaNet over the years (the wiki keeps them on a "deprecated" page).

## Linux (Steam with Proton, Wine, Lutris)

There is no native Linux client. Players run the Windows game through Steam's Proton (the Steam version is rated
for Steam Deck), or the ArenaNet client under Wine, usually installed with Lutris. Epic's version runs through the
Heroic Games Launcher.

### Finding installs

- Steam folders, each checked for `steamapps/` and listed once (`~/.steam/steam` and `~/.steam/root` are usually
  symlinks to `~/.local/share/Steam`):
  - native: `~/.steam/steam`, `~/.steam/root`, `~/.local/share/Steam`;
  - Flatpak (`com.valvesoftware.Steam` on Flathub): `~/.var/app/com.valvesoftware.Steam/.local/share/Steam`
    (older builds: `.../data/Steam`);
  - Snap: `~/snap/steam/common/.local/share/Steam`.
- Libraries and the game: as on Windows, `steamapps/libraryfolders.vdf` (every `"path"`) and
  `steamapps/appmanifest_1284210.acf` (`"installdir"`), so the game is `<library>/steamapps/common/Guild Wars 2`.
- The Proton prefix (the game's own Windows "C:"): `<library>/steamapps/compatdata/1284210/pfx`. Steam creates it on
  the first launch.
- ArenaNet client: `<prefix>/drive_c/Program Files/Guild Wars 2` (and `Program Files (x86)`) in `~/.wine` and in each
  Lutris prefix under `~/Games/<name>`. Other places: "Escolher pasta…" (zenity or kdialog); the prefix is then the
  folder above `drive_c`.

Sources: [Steam for Linux (Valve)](https://github.com/ValveSoftware/steam-for-linux),
[Protontricks](https://github.com/Matoking/protontricks) (works on the `steamapps/compatdata/<appid>/pfx` prefixes of
every library, native and Flatpak Steam), [Steam on Flathub](https://flathub.org/apps/com.valvesoftware.Steam),
[Steam snap](https://snapcraft.io/steam), [Lutris](https://lutris.net/games/guild-wars-2/).

### What the mod needs under Wine: `msimg32=n,b`

`msimg32.dll` is one of the DLLs Wine implements itself ("builtin"). For those, Wine's default is to load its own
copy even when the game folder has one, so our proxy is ignored and the mod does not load. A DLL override changes the
order: `n,b` = native (a real Windows DLL, ours) first, then builtin. It is set for one program with the environment
variable

```
WINEDLLOVERRIDES="msimg32=n,b"
```

or for a whole prefix in `winecfg` > Libraries (stored in the prefix's `user.reg` under
`[Software\\Wine\\DllOverrides]` as `"msimg32"="native,builtin"`). Proton is Wine and reads the same variable.

Sources: [Wine User's Guide: DLL overrides (WineHQ)](https://gitlab.winehq.org/wine/wine/-/wikis/Wine-User%27s-Guide#dll-overrides),
[wine(1): WINEDLLOVERRIDES (WineHQ)](https://gitlab.winehq.org/wine/wine/-/wikis/Man-Pages/wine),
[Proton README, "Runtime Config Options" (Valve)](https://github.com/ValveSoftware/Proton#runtime-config-options)
(variables are set in the launch options "followed by `%command%`"); the same pattern is what
other proxy-DLL mods ask for on [ProtonDB](https://www.protondb.com/) (e.g. `WINEDLLOVERRIDES="winmm=n,b" %command%`,
`WINEDLLOVERRIDES="dinput8=n,b" %command%`).

### Steam: the launch options

On Steam the variable goes in the game's launch options (right-click > Properties > General > Launch Options):

```
WINEDLLOVERRIDES="msimg32=n,b" %command%
```

`%command%` is replaced by the command Steam runs (Proton and the game), so what comes before it is the environment
and what comes after it is arguments for the game. Without `%command%` the whole line is passed to the game as
arguments, so the line is put in front.

- Steam stores the launch options per account in `<Steam>/userdata/<account id>/config/localconfig.vdf` (Valve's
  KeyValues text format), at `UserLocalConfigStore/Software/Valve/Steam/apps/1284210/LaunchOptions`. `userdata/0` is
  not an account.
- Steam reads that file at start and writes it back when it exits, so an edit made while it runs is lost. The
  launcher edits it only when no `steam` / `steamwebhelper` process is running, copies the original once to
  `localconfig.vdf.antes-do-gw2enhanced`, keeps the player's other options and overrides (the override is merged into
  an existing `WINEDLLOVERRIDES`), and writes through a temporary file then a rename. Otherwise it shows the exact line
  to paste, with the explanation.
- The override is considered set when any account's launch options have it, or when the Proton prefix's `user.reg`
  has it.

Sources: the format, [KeyValues (Valve Developer Community)](https://developer.valvesoftware.com/wiki/KeyValues); the
file and the key path, a real Steam install (Steam keeps the same `userdata` layout on Windows and Linux).

### Starting the game on Linux

| Install | What we run | Why |
|---|---|---|
| Steam, no arguments | `steam steam://rungameid/1284210` (Flatpak: `flatpak run com.valvesoftware.Steam steam://rungameid/1284210`; no `steam` command: `xdg-open steam://rungameid/1284210`) | Steam runs it with Proton and the launch options above |
| Steam, with arguments | `steam -applaunch 1284210 <args>` (Flatpak likewise) | as on Windows |
| ArenaNet client in Wine | `wine Gw2-64.exe <args>` in the game folder, with `WINEPREFIX=<prefix>` and `WINEDLLOVERRIDES=msimg32=n,b` | the override travels with the launch |
| Lutris | not started by us; the player sets `WINEDLLOVERRIDES` = `msimg32=n,b` in the game's *System options > Environment variables* (or `msimg32` = `n,b` in *Runner options > DLL overrides*) | Lutris owns the runner and prefix |
| Epic | not started by us: run it from Heroic, with the same override in Heroic's *Environment variables* | Epic's launcher is Windows-only |

The game counts as running (installs are refused) when a process's first argument ends in `Gw2-64.exe`; under Wine
it is a Windows path such as `Z:\home\<user>\...\Gw2-64.exe`, mapped back to the Unix path.

Not verified on a Linux machine with the game: whether the textures and graphics hooks work on top of DXVK (Proton
turns Direct3D 11 into Vulkan; the hooks see the D3D11 interfaces DXVK exposes).
