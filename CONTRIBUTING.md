# Contributing to GW2 Enhanced

Thanks for wanting to help! GW2 Enhanced is a fan-made, open-source mod for Guild Wars 2 with three features
(Translation, HD Textures, Graphics) built into one mod. Every kind of help counts: playing and reporting, testing on
your hardware, translating, writing docs, or code.

Please read the [Code of Conduct](CODE_OF_CONDUCT.md) first. For questions, see [SUPPORT.md](SUPPORT.md). Security
problems never go in a public issue: see [SECURITY.md](SECURITY.md).

## Where help is needed most

### Graphics: testers and calibration (help wanted)

The Graphics feature works inside the game's own renderer, and the only way to know whether an effect looks right is
to see it in the game, on many GPUs, monitors and maps. We need players who can:

- **Test on their hardware:** NVIDIA, AMD and Intel GPUs; SDR and HDR monitors; different resolutions and aspect
  ratios (ultrawide included); Windows and Linux (Proton).
- **Calibrate:** compare effects on and off in the same spot and tell us what looks wrong: too bright or too dark,
  halos, lines or dots, flicker, colours shifted from the original art, effects showing through walls, etc.
- **Find good defaults:** each effect has sliders in the overlay (<kbd>Shift</kbd>+<kbd>Home</kbd> → Graphics).
  Settings that look great across many places are worth sharing.

How to send a useful report: open a **Graphics calibration report** issue (the template asks for everything below).

1. GPU, driver version, monitor (SDR/HDR, resolution, refresh rate), Windows or Linux/Proton.
2. **Two screenshots of the same spot**, effects on and off (the overlay has a toggle for each effect and one for the
   whole Graphics feature). Say where it is (map and place) and the time of day if it matters.
3. Your `GW2Graphics\config.ini` and the `GW2Graphics\gw2graphics_log.txt` from that session (attach the files). The
   log is off by default: turn on **Logs** > Graphics in the overlay's Health tab before that session.
4. What you expected and what looks wrong.

The overlay's debug mode can capture paired screenshots and live-reload the config while you play; how to use it is in
the Graphics docs ([graphics/docs/debug-mode.md](graphics/docs/debug-mode.md) and [graphics/docs/](graphics/docs/)).

### Translation (pt-BR)

- Vote and suggest better translations on the website: https://app.gw2enhanced.workers.dev
- Turn on the optional text sending in the overlay (Translation tab): the texts you see help the community dictionary.
- Report wrong or awkward translations of the interface and systems with the **Bug report** template.

### HD Textures

- Report a texture that looks wrong (blurry, wrong colours, broken alpha, visible seams) with a screenshot and where
  it was. Never attach extracted game files (see "Legal" below).

### Code and docs

Look for issues labelled `good first issue` or `help wanted`, or open an issue first to discuss a larger change.

## Project layout

| Folder | What it is |
|---|---|
| `enhanced/` | The mod itself: hosts the overlay and the three features |
| `core/` | Shared code: locating game code across patches, health registry, signed updates |
| `translated/` | Translation feature, local translator, tools |
| `remastered/` | HD Textures feature and the texture pipeline tools |
| `graphics/` | Graphics feature (Direct3D 11 hooks, shaders, effects) |
| `installer/` | Installer and launcher |
| `backend/` | Website and API (Cloudflare Worker) |
| `docs/` | Design notes and images |

## Building and testing

Windows with the MSVC toolchain and the Windows SDK. See "Building" in the [README](README.md) for details.

```powershell
cargo build --release              # the mod with all three features
cargo mod-translation              # one feature only (also mod-textures, mod-graphics)
cargo test --release -p gw2enhanced-core -p gw2translated -p gw2remastered
cargo test --release -p gw2graphics  # slow (about 30 minutes)
cd backend; cargo test             # website/API logic
```

Use `--release` for tests: some debug test builds do not link.

Install a build by copying `target\release\msimg32.dll` next to `Gw2-64.exe` (keep a backup of what was there).

## Pull requests

1. Fork, create a branch from `main`, keep the change focused (one topic per PR).
2. Add or update tests for what you change; run the tests of the crates you touched.
3. Do not reformat files you did not otherwise change (the codebase is not `rustfmt`-clean as a whole; running
   `cargo fmt` on the workspace creates huge unrelated diffs). Format only your own lines.
4. Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/): `feat(graphics): ...`,
   `fix(translated): ...`, `docs: ...`.
5. Describe what you changed, why, and how you tested it (in the game, if it changes what the player sees; include
   before/after screenshots).
6. CI must pass. A maintainer reviews every PR.

By contributing you agree that your contribution is licensed under the [MIT License](LICENSE) of the project.

## Legal

- GW2 Enhanced is not affiliated with ArenaNet or NCSOFT. Do not add game files, extracted textures, models, sounds
  or text dumps to the repository, issues or PRs. Screenshots of the game for bug reports are fine.
- Do not add code that automates gameplay, reads other players' data, or gives an advantage in play. The mod changes
  how the game looks and reads, nothing else.
