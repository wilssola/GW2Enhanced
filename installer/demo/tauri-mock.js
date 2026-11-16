// Demo stand-in for the Tauri bridge, so installer/ui can run in a plain browser (screenshots, UI work).
// Not part of the app: tauri.conf.json serves ui/ only, and this file is never bundled.
// The data mirrors what the commands in src/app.rs return (Overview, ReleasesView, progress events).
//
// Query options (on the page URL):
//   ?hold      install_release stops mid-download, to show the progress bar
//   ?linux     platform "linux" with a Steam (Proton) install that still needs the msimg32 override
//   ?lang=en   language saved in the settings ("pt-BR" or "en"); without it, the browser's language is used
"use strict";

(() => {
  const q = new URLSearchParams(location.search);
  const linux = q.has("linux");
  const hold = q.has("hold");
  // The messages "from Rust" follow the same language as src/i18n.rs: the saved one, else the page's.
  let lang = "pt-BR";
  const tr = (pt, en) => (lang === "en" ? en : pt);

  const STEAM = linux
    ? "/home/player/.local/share/Steam/steamapps/common/Guild Wars 2"
    : "C:\\Program Files (x86)\\Steam\\steamapps\\common\\Guild Wars 2";
  const ANET = linux ? "/home/player/Games/guild-wars-2/drive_c/Program Files/Guild Wars 2" : "C:\\Program Files\\Guild Wars 2";

  const MB = 1024 ** 2;
  const sizes = (scale) => ({
    All: Math.round(412 * MB * scale),
    Translation: Math.round(371 * MB * scale),
    "Translation-CPU": Math.round(96 * MB * scale),
    "Translation-CUDA": Math.round(298 * MB * scale),
    "Translation-DirectML": Math.round(241 * MB * scale),
    Textures: Math.round(14 * MB * scale),
    Graphics: Math.round(9 * MB * scale),
  });
  const release = (version, date, notes, scale, pre) => {
    const package_sizes = sizes(scale);
    return {
      tag: "v" + version,
      version,
      title: "GW2 Enhanced v" + version,
      published_at: date,
      prerelease: !!pre,
      notes_html: notes,
      page: "https://github.com/wilssola/GW2Enhanced/releases/tag/v" + version,
      packages: Object.keys(package_sizes),
      package_sizes,
      signed: true,
    };
  };

  const RELEASES = [
    release("0.2.0", "2026-11-12T18:00:00Z",
      "<h3>Tradução</h3><ul><li>Modelo grande por padrão na CUDA e no DirectML.</li><li>Legendas e diálogos traduzidos na hora.</li></ul>" +
      "<h3>Gráficos</h3><ul><li>HDR, bloom, oclusão de ambiente e reflexos ajustáveis no painel.</li></ul>" +
      "<h3>Instalador</h3><ul><li>Encontra o jogo na Steam, na Epic Games e no cliente da ArenaNet.</li></ul>", 1.0),
    release("0.1.0", "2026-10-20T18:00:00Z",
      "<p>Primeira versão do GW2 Enhanced: tradução, texturas HD e gráficos num só <code>msimg32.dll</code>.</p>" +
      "<ul><li>Painel no jogo com <b>Shift+Home</b>.</li><li>Atualizador assinado.</li></ul>", 0.97),
  ];

  const status = (installed) => installed
    ? { installed: true, version: "0.1.0", package: "All", legacy_remastered: false, legacy_translated: false,
        foreign_dll: false, backup: false, data_dirs: ["GW2Translated", "GW2Remastered", "GW2Graphics"] }
    : { installed: false, version: null, package: null, legacy_remastered: false, legacy_translated: false,
        foreign_dll: false, backup: false, data_dirs: [] };

  const proton = (set) => ({
    prefix: "/home/player/.local/share/Steam/steamapps/compatdata/1284210/pfx",
    launch_options: set ? ['WINEDLLOVERRIDES="msimg32=n,b" %command%'] : [],
    override_set: set,
    line: 'WINEDLLOVERRIDES="msimg32=n,b" %command%',
    steam_running: false,
    accounts: 1,
  });

  const db = {
    selected: STEAM,
    installs: [
      { path: STEAM, channel: "Steam", source: "Steam", status: status(true), running: false,
        proton: linux ? proton(false) : null, wine_prefix: null },
      { path: ANET, channel: "Standalone", source: linux ? "Wine / Lutris" : "ArenaNet", status: status(false),
        running: false, proton: null, wine_prefix: linux ? "/home/player/Games/guild-wars-2" : null },
    ],
    settings: {
      game_dir: STEAM,
      manual_dirs: [],
      check_updates_on_start: true,
      package: "All",
      show_prereleases: false,
      launch: { autologin: true, share_archive: false, map_load_info: true, windowed: false, extra: "" },
      close_on_launch: false,
      language: ["pt-BR", "en"].includes(q.get("lang")) ? q.get("lang") : "",
    },
  };

  const listeners = {};
  const emit = (event, payload) => (listeners[event] || []).forEach((cb) => cb({ event, payload }));
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const clone = (v) => structuredClone(v);
  const sel = () => db.installs.find((i) => i.path === db.selected);

  const commands = {
    async overview({ lang: page }) {
      lang = db.settings.language || page || "pt-BR";
      const source = (s) => (s === "ArenaNet" ? tr("Instalador da ArenaNet", "ArenaNet installer") : s);
      return clone({
        app_version: "0.2.0",
        platform: linux ? "linux" : "windows",
        proton_line: 'WINEDLLOVERRIDES="msimg32=n,b" %command%',
        installs: db.installs.map((i) => ({ ...i, source: source(i.source) })),
        selected: db.selected,
        settings: db.settings,
        game_running: false,
        local_package: null,
      });
    },
    async releases() {
      await wait(150);
      const installed = sel() && sel().status.version;
      return clone({ releases: RELEASES, latest: "0.2.0", installed, update_available: installed === "0.1.0" });
    },
    async select_game({ path }) { db.selected = path; db.settings.game_dir = path; },
    async browse_game() { return null; },
    async save_settings({ settings }) {
      db.settings = { ...settings, game_dir: db.settings.game_dir, manual_dirs: db.settings.manual_dirs };
      if (settings.language) lang = settings.language;
    },
    async install_release({ tag, package: pkg }) {
      const rel = RELEASES.find((r) => r.tag === tag);
      const total = rel.package_sizes[pkg];
      emit("progress", { stage: "verify", done: 0, total: 0 });
      await wait(300);
      const stop = hold ? 0.62 : 1;
      for (let f = 0; f <= stop + 1e-9; f += 0.02) {
        emit("progress", { stage: "download", done: Math.round(total * f), total });
        await wait(25);
      }
      if (hold) return new Promise(() => {});
      emit("progress", { stage: "unpack", done: 0, total: 0 });
      await wait(200);
      const s = sel();
      s.status = { ...status(true), version: rel.version, package: pkg };
      db.settings.package = pkg;
      return [
        tr(`GW2 Enhanced ${rel.version} (${pkg}) baixado e verificado`, `GW2 Enhanced ${rel.version} (${pkg}) downloaded and verified`),
        tr("msimg32.dll instalado", "msimg32.dll installed"),
        tr("GW2Translated\\ atualizado (1204 arquivos)", "GW2Translated\\ updated (1204 files)"),
      ];
    },
    async install_local() { return [tr("msimg32.dll instalado", "msimg32.dll installed")]; },
    async uninstall() { sel().status = status(false); return [tr("msimg32.dll do GW2 Enhanced removido", "GW2 Enhanced msimg32.dll removed")]; },
    async play() {
      return tr("Guild Wars 2 iniciado ", "Guild Wars 2 started ") +
        (linux ? tr("pela Steam (Proton)", "through Steam (Proton)") : tr("pela Steam", "through Steam"));
    },
    async set_proton_override() {
      sel().proton = proton(true);
      return [tr("conta 12345678: opções de inicialização atualizadas", "account 12345678: launch options updated")];
    },
    async open_url() {},
    async open_folder() {},
  };

  const win = { minimize() {}, close() {} };

  window.__TAURI__ = {
    core: {
      invoke(cmd, args) {
        const f = commands[cmd];
        return f ? f(args || {}) : Promise.reject("unknown command: " + cmd);
      },
    },
    event: {
      listen(event, cb) {
        (listeners[event] ||= []).push(cb);
        return Promise.resolve(() => { listeners[event] = listeners[event].filter((x) => x !== cb); });
      },
    },
    window: { getCurrentWindow: () => win },
  };
})();
