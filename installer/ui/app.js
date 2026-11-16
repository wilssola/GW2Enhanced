// GW2 Enhanced installer: the page. Talks to the Rust side through Tauri commands (src/app.rs).
"use strict";

const T = window.__TAURI__;
const invoke = (cmd, args) => T.core.invoke(cmd, args);
const $ = (id) => document.getElementById(id);

// ---------------------------------------------------------------- languages
//
// Same scheme as the website (backend/site/app.js). index.html is written in Portuguese; its translatable parts carry
// data-i18n="key" (the element's content) or data-i18n-attr="attr:key ..." (attributes). Their Portuguese is the
// markup itself; STR.en holds the English. In a page template, {0}, {1}… stand for the element's child elements, in
// order (<code>, <span>…), which stay as they are. Strings built here have both languages in STR, and {0}, {1}… in
// them are the arguments of t(). The language is the `language` of the installer's settings; until the player picks
// one, the browser's (the system's). The messages from the Rust side follow the same choice (src/i18n.rs).
const LANGS = ["pt-BR", "en"];
const STR = {
  "pt-BR": {
    "pkg.All": "Completo",
    "pkg.All.text": "Os três mods: tradução para português, texturas HD e gráficos. O tradutor local vem com todos os modos (processador, CUDA e DirectML).",
    "pkg.Translation": "Tradução",
    "pkg.Translation.text": "Só a tradução, com o tradutor local em todos os modos. Ele usa a placa de vídeo quando der e o processador nos outros casos.",
    "pkg.Translation-CPU": "Tradução · CPU",
    "pkg.Translation-CPU.text": "Só a tradução, com o tradutor só no processador. Funciona em qualquer PC; o modelo é o pequeno.",
    "pkg.Translation-CUDA": "Tradução · CUDA",
    "pkg.Translation-CUDA.text": "Só a tradução, com processador e CUDA: para placas NVIDIA. Usa o modelo grande, mais preciso, rodando na placa.",
    "pkg.Translation-DirectML": "Tradução · DirectML",
    "pkg.Translation-DirectML.text": "Só a tradução, com processador e DirectML: qualquer placa com DirectX 12 no Windows (AMD, Intel ou NVIDIA). Modelo grande.",
    "pkg.Textures": "Texturas HD",
    "pkg.Textures.text": "Só o motor das texturas HD. O pacote de texturas (cerca de 95 GB) é baixado à parte, no Nexus Mods.",
    "pkg.Graphics": "Gráficos",
    "pkg.Graphics.text": "Só os gráficos: HDR de verdade, bloom, oclusão de ambiente, reflexos, raios de luz, efeitos de lente e correção de cor.",
    "tag.recommended": "Recomendado",
    "tag.translation": "Tradução",
    "tag.textures": "Texturas HD",
    "tag.graphics": "Gráficos",
    "tag.smallest": "Menor download",
    "tag.hdPack": "Pacote HD à parte",

    "err.prefix": "Erro: {0}",
    "dlg.confirm": "Confirmar",
    "size.decimal": ",",

    "via.steamProton": "pela Steam (Proton)",
    "via.steam": "pela Steam",
    "via.heroic": "pelo Heroic (Epic Games)",
    "via.epic": "pela Epic Games",
    "via.wine": "pelo Wine",
    "via.exe": "pelo Gw2-64.exe",

    "st.installed": "Instalado · {0}",
    "st.unknownVersion": "versão desconhecida",
    "st.legacyRemastered": "GW2 Remastered antigo",
    "st.legacyTranslated": "GW2 Translated antigo",
    "st.notInstalled": "Não instalado",

    "home.loading": "Carregando…",
    "home.notFound": "Guild Wars 2 não encontrado",
    "home.notFoundLinux": "Não achei o jogo na Steam (nativa, Flatpak ou Snap) nem em prefixos do Wine ou do Lutris. Escolha a pasta onde fica o Gw2-64.exe.",
    "home.notFoundWin": "Não achei o jogo na Steam, na Epic Games nem nas pastas padrão. Escolha a pasta onde fica o Gw2-64.exe.",
    "home.installedTitle": "GW2 Enhanced {0}",
    "home.installedWord": "instalado",
    "home.packageIn": "Pacote {0} em {1}",
    "home.installedIn": "Instalado em {0}",
    "chip.installed": "Instalado",
    "chip.unknownVersion": "Versão desconhecida",
    "home.notInstalledTitle": "Mod não instalado",
    "home.chooseAndInstall": "Escolha um pacote e instale em {0}.",
    "chip.notInstalled": "Não instalado",
    "chip.legacyTranslated": "GW2 Translated antigo: será trocado",
    "chip.legacyRemastered": "GW2 Remastered antigo: será trocado",
    "chip.foreignDll": "msimg32.dll de outro mod",
    "chip.backup": "Backup de outro mod guardado",
    "chip.gameOpen": "Jogo aberto",
    "chip.inUse": "Em uso",
    "chip.protonSet": "Proton configurado",
    "chip.protonMissing": "Proton: falta o ajuste",
    "upd.newTitle": "Nova versão: v{0}",
    "upd.newText": "Você tem a v{0}. Baixe e instale com um clique.",
    "upd.availTitle": "Versão v{0} disponível",
    "upd.availText": "Instale o pacote {0} direto do GitHub.",
    "news.searching": "Procurando…",
    "news.failed": "Não foi possível consultar",
    "notes.none": "Sem notas.",
    "news.noneYet": "Nenhuma versão publicada ainda",
    "news.autoOff": "Verificação automática desligada",
    "news.autoOffText": "Abra “Atualizações” para consultar as versões.",
    "installs.none": "Nenhuma instalação encontrada. Use “Escolher pasta…” e selecione o Gw2-64.exe.",
    "installs.foundBy": "Encontrada por: {0} · abre {1}",
    "installs.use": "Usar esta",
    "installs.folder": "Pasta",
    "installs.play": "Jogar",

    "wine.why": "No Linux o jogo roda pelo Wine, que usa a própria cópia do msimg32.dll e ignora a do mod. O ajuste msimg32=n,b manda usar primeiro a do mod (n = nativa) e depois a do Wine (b = embutida).",
    "wine.protonSet": "Proton configurado",
    "wine.protonMissing": "Falta um ajuste na Steam (Proton)",
    "wine.protonOk": "As opções de inicialização da Steam já têm o ajuste msimg32=n,b: o mod vai carregar pelo Proton.",
    "wine.protonHow": " Na Steam: botão direito no Guild Wars 2 › Propriedades › Geral › Opções de inicialização, e cole a linha abaixo.",
    "wine.closeSteamFirst": " Para eu configurar sozinho, feche a Steam antes.",
    "wine.wineTitle": "Wine ou Lutris: ajuste do msimg32",
    "wine.playPrefix": " O botão Jogar já abre com o ajuste (prefixo {0}).",
    "wine.play": " O botão Jogar já abre com o ajuste.",
    "wine.lutris": " No Lutris, coloque WINEDLLOVERRIDES = msimg32=n,b nas variáveis de ambiente do jogo, ou msimg32 = n,b em Opções do Wine › Substituições de DLL.",
    "wine.closeSteamTitle": "Feche a Steam primeiro",
    "proton.dlgTitle": "Configurar a Steam",
    "proton.dlgBody": "Vou adicionar WINEDLLOVERRIDES=\"msimg32=n,b\" às opções de inicialização do Guild Wars 2 em cada conta da Steam deste computador. Uma cópia do arquivo original fica ao lado (localconfig.vdf.antes-do-gw2enhanced). A Steam precisa estar fechada.",
    "proton.dlgOk": "Configurar",
    "proton.done": "Steam configurada ({0}). Abra a Steam e jogue.",
    "copy.done": "Linha copiada",
    "copy.selected": "Selecionei a linha: copie com Ctrl+C",

    "pkg.installedTag": "Instalado",
    "pkg.notInRelease": "Fora desta versão",
    "inst.noFolder": "Nenhuma pasta escolhida",
    "inst.installedNote": "Instalado: {0}.",
    "inst.notInstalledNote": "O mod não está instalado nesta pasta.",
    "inst.foreignNote": "Já existe um msimg32.dll de outro mod nesta pasta: ao instalar, você escolhe se quer substituí-lo.",
    "inst.closeGame": "Feche o jogo antes de instalar.",
    "inst.querying": "Consultando o GitHub…",
    "inst.downloadVersion": "Baixar e instalar v{0}",
    "inst.download": "Baixar e instalar",
    "inst.local": "Instalar do pacote local",
    "inst.preparing": "Preparando…",

    "rel.querying": "Consultando as versões no GitHub…",
    "rel.failed": "Não consegui consultar as versões",
    "rel.none": "Nenhuma versão publicada ainda.",
    "rel.latest": "Mais recente",
    "rel.installed": "Instalada",
    "rel.prerelease": "Pré-lançamento",
    "rel.unsigned": "Sem assinatura",
    "rel.github": "Ver no GitHub",
    "rel.noPackage": "Esta versão não tem o pacote {0} assinado",
    "rel.install": "Instalar {0}",

    "set.noInstalls": "Nenhuma instalação encontrada",
    "pb.none": "Nenhuma instalação escolhida",
    "pb.choose": "Escolha a pasta do jogo",
    "pb.opens": "Abre {0}",
    "pb.gameOpen": "Jogo aberto · ",
    "pb.mod": "mod {0}",
    "pb.modInstalled": "mod instalado",
    "pb.noMod": "sem o mod",

    "toast.folderChosen": "Pasta escolhida: {0}",
    "toast.scanDone": "Busca concluída",
    "stage.verify": "Verificando a assinatura",
    "stage.download": "Baixando",
    "stage.unpack": "Descompactando",
    "stage.install": "Instalando",
    "stage.done": "Concluído",
    "progress.of": "{0} de {1}",
    "foreign.title": "Já existe um msimg32.dll de outro mod",
    "foreign.body": "Esta pasta já tem um msimg32.dll que não é do GW2 Enhanced (outro mod usa o mesmo nome). Substituir? O arquivo dele fica guardado como msimg32.dll.antes-do-gw2enhanced e volta ao desinstalar o GW2 Enhanced; enquanto isso, aquele mod não carrega.",
    "foreign.ok": "Substituir",
    "done.release": "GW2 Enhanced v{0} instalado",
    "done.local": "Pacote local instalado",
    "uninstall.title": "Desinstalar o GW2 Enhanced?",
    "uninstall.body": "O msimg32.dll do mod é removido e o de outro mod, se havia, volta para o lugar. Suas configurações, traduções e logs ficam na pasta do jogo.",
    "uninstall.ok": "Desinstalar",
    "done.uninstall": "GW2 Enhanced desinstalado",
  },
  en: {
    // index.html (data-i18n / data-i18n-attr)
    "tb.min": "Minimize",
    "tb.close": "Close",
    "brand.kicker": "Translation · HD textures · Graphics",
    "nav.aria": "Sections",
    "nav.home": "Home",
    "nav.install": "Install",
    "nav.updates": "Updates{0}",
    "nav.settings": "Settings",
    "home.kicker": "Mod status",
    "home.updateNow": "Update",
    "wine.copy": "Copy",
    "wine.apply": "Set up in Steam",
    "home.toInstall": "Packages and install",
    "home.toNotes": "Release notes",
    "news.kicker": "Latest version",
    "news.more": "Read the full notes",
    "installs.kicker": "Where the game is",
    "installs.title": "Guild Wars 2 installs",
    "installs.rescan": "Search again",
    "installs.browse": "Choose folder…",
    "inst.kicker": "Choose what to install",
    "inst.title": "Packages",
    "inst.lead": "Every package is the same {0} with different parts inside. Pick one; to switch later, just install another one over it. Your settings and translations are never deleted.",
    "inst.pkgAria": "Package",
    "inst.target": "Install to",
    "inst.uninstall": "Uninstall",
    "inst.note": "Downloads only come from the official releases on GitHub, and are only installed if the checksum is signed by GW2 Enhanced and the file matches it.",
    "upd.kicker": "Release notes",
    "upd.title": "Versions",
    "upd.check": "Check now",
    "set.kicker": "Preferences",
    "set.title": "Settings",
    "set.game": "Game folder",
    "set.gameLabel": "Install used to install the mod and play",
    "set.browse": "Choose another folder…",
    "set.open": "Open the folder",
    "set.patch": "Only update the game",
    "set.patchNote": "“Only update the game” opens Gw2-64.exe with {0}: it downloads the game update and closes. On Steam and Epic, the store's launcher updates the game.",
    "set.updates": "Updates",
    "set.check": "Check for mod updates on start",
    "set.pre": "Show test versions (pre-releases)",
    "set.close": "Close this program after starting the game",
    "set.lang": "Language",
    "set.langNote": "The language of this program: screens, messages and notices.",
    "set.launch": "Launch options",
    "set.launchNote": "They apply to Gw2-64.exe and to Steam. On Epic Games, use the Epic launcher's own “Additional Command Line Arguments”.",
    "opt.autologin": "{0} Logs straight in, if the game remembers your email and password",
    "opt.share": "{0} Opens Gw2.dat in shared mode (multiple clients, tools)",
    "opt.maplo": "{0} Shows progress and time on loading screens",
    "opt.windowed": "{0} Starts in windowed mode",
    "opt.extra": "Other arguments",
    "opt.extraPh": "e.g. -clientport 443",
    "about.title": "About",
    "about.text": "GW2 Enhanced {0}. Fan project, not affiliated with ArenaNet or NCSOFT. Guild Wars 2 is a registered trademark of ArenaNet/NCSOFT.",
    "about.fonts": "Cinzel, EB Garamond and Barlow Condensed fonts under the SIL Open Font License.",
    "about.repo": "Source code on GitHub",
    "play": "Play",
    "dlg.cancel": "Cancel",

    // Built by this script
    "pkg.All": "Full",
    "pkg.All.text": "All three mods: Portuguese translation, HD textures and graphics. The local translator comes with every mode (processor, CUDA and DirectML).",
    "pkg.Translation": "Translation",
    "pkg.Translation.text": "Translation only, with the local translator in every mode. It uses the graphics card when it can and the processor otherwise.",
    "pkg.Translation-CPU": "Translation · CPU",
    "pkg.Translation-CPU.text": "Translation only, with the translator on the processor only. Works on any PC; uses the small model.",
    "pkg.Translation-CUDA": "Translation · CUDA",
    "pkg.Translation-CUDA.text": "Translation only, with processor and CUDA: for NVIDIA cards. Uses the large, more accurate model, running on the card.",
    "pkg.Translation-DirectML": "Translation · DirectML",
    "pkg.Translation-DirectML.text": "Translation only, with processor and DirectML: any card with DirectX 12 on Windows (AMD, Intel or NVIDIA). Large model.",
    "pkg.Textures": "HD textures",
    "pkg.Textures.text": "Only the HD texture engine. The texture pack (about 95 GB) is downloaded separately, from Nexus Mods.",
    "pkg.Graphics": "Graphics",
    "pkg.Graphics.text": "Graphics only: real HDR, bloom, ambient occlusion, reflections, light rays, lens effects and color grading.",
    "tag.recommended": "Recommended",
    "tag.translation": "Translation",
    "tag.textures": "HD textures",
    "tag.graphics": "Graphics",
    "tag.smallest": "Smallest download",
    "tag.hdPack": "Separate HD pack",

    "err.prefix": "Error: {0}",
    "dlg.confirm": "Confirm",
    "size.decimal": ".",

    "via.steamProton": "through Steam (Proton)",
    "via.steam": "through Steam",
    "via.heroic": "through Heroic (Epic Games)",
    "via.epic": "through Epic Games",
    "via.wine": "with Wine",
    "via.exe": "with Gw2-64.exe",

    "st.installed": "Installed · {0}",
    "st.unknownVersion": "unknown version",
    "st.legacyRemastered": "Old GW2 Remastered",
    "st.legacyTranslated": "Old GW2 Translated",
    "st.notInstalled": "Not installed",

    "home.loading": "Loading…",
    "home.notFound": "Guild Wars 2 not found",
    "home.notFoundLinux": "Could not find the game in Steam (native, Flatpak or Snap) or in Wine or Lutris prefixes. Choose the folder where Gw2-64.exe is.",
    "home.notFoundWin": "Could not find the game in Steam, Epic Games or the default folders. Choose the folder where Gw2-64.exe is.",
    "home.installedTitle": "GW2 Enhanced {0}",
    "home.installedWord": "installed",
    "home.packageIn": "{0} package in {1}",
    "home.installedIn": "Installed in {0}",
    "chip.installed": "Installed",
    "chip.unknownVersion": "Unknown version",
    "home.notInstalledTitle": "Mod not installed",
    "home.chooseAndInstall": "Pick a package and install it in {0}.",
    "chip.notInstalled": "Not installed",
    "chip.legacyTranslated": "Old GW2 Translated: will be replaced",
    "chip.legacyRemastered": "Old GW2 Remastered: will be replaced",
    "chip.foreignDll": "Another mod's msimg32.dll",
    "chip.backup": "Another mod's backup kept",
    "chip.gameOpen": "Game running",
    "chip.inUse": "In use",
    "chip.protonSet": "Proton set up",
    "chip.protonMissing": "Proton: setting missing",
    "upd.newTitle": "New version: v{0}",
    "upd.newText": "You have v{0}. Download and install it in one click.",
    "upd.availTitle": "Version v{0} available",
    "upd.availText": "Install the {0} package straight from GitHub.",
    "news.searching": "Searching…",
    "news.failed": "Could not check",
    "notes.none": "No notes.",
    "news.noneYet": "No version published yet",
    "news.autoOff": "Automatic check turned off",
    "news.autoOffText": "Open “Updates” to check the versions.",
    "installs.none": "No install found. Use “Choose folder…” and select Gw2-64.exe.",
    "installs.foundBy": "Found by: {0} · opens {1}",
    "installs.use": "Use this one",
    "installs.folder": "Folder",
    "installs.play": "Play",

    "wine.why": "On Linux the game runs through Wine, which uses its own copy of msimg32.dll and ignores the mod's. The msimg32=n,b setting tells it to use the mod's first (n = native) and then Wine's (b = builtin).",
    "wine.protonSet": "Proton set up",
    "wine.protonMissing": "Steam (Proton) needs one setting",
    "wine.protonOk": "Steam's launch options already have the msimg32=n,b setting: the mod will load through Proton.",
    "wine.protonHow": " In Steam: right-click Guild Wars 2 › Properties › General › Launch Options, and paste the line below.",
    "wine.closeSteamFirst": " To let me set it up for you, close Steam first.",
    "wine.wineTitle": "Wine or Lutris: the msimg32 setting",
    "wine.playPrefix": " The Play button already starts it with the setting (prefix {0}).",
    "wine.play": " The Play button already starts it with the setting.",
    "wine.lutris": " In Lutris, add WINEDLLOVERRIDES = msimg32=n,b to the game's environment variables, or msimg32 = n,b under Wine options › DLL overrides.",
    "wine.closeSteamTitle": "Close Steam first",
    "proton.dlgTitle": "Set up Steam",
    "proton.dlgBody": "I will add WINEDLLOVERRIDES=\"msimg32=n,b\" to the Guild Wars 2 launch options of every Steam account on this computer. A copy of the original file is kept next to it (localconfig.vdf.antes-do-gw2enhanced). Steam must be closed.",
    "proton.dlgOk": "Set up",
    "proton.done": "Steam set up ({0}). Open Steam and play.",
    "copy.done": "Line copied",
    "copy.selected": "Line selected: copy it with Ctrl+C",

    "pkg.installedTag": "Installed",
    "pkg.notInRelease": "Not in this version",
    "inst.noFolder": "No folder chosen",
    "inst.installedNote": "Installed: {0}.",
    "inst.notInstalledNote": "The mod is not installed in this folder.",
    "inst.foreignNote": "This folder already has another mod's msimg32.dll: when installing, you choose whether to replace it.",
    "inst.closeGame": "Close the game before installing.",
    "inst.querying": "Checking GitHub…",
    "inst.downloadVersion": "Download and install v{0}",
    "inst.download": "Download and install",
    "inst.local": "Install from the local package",
    "inst.preparing": "Preparing…",

    "rel.querying": "Checking the versions on GitHub…",
    "rel.failed": "Could not check the versions",
    "rel.none": "No version published yet.",
    "rel.latest": "Latest",
    "rel.installed": "Installed",
    "rel.prerelease": "Pre-release",
    "rel.unsigned": "Unsigned",
    "rel.github": "View on GitHub",
    "rel.noPackage": "This version has no signed {0} package",
    "rel.install": "Install {0}",

    "set.noInstalls": "No install found",
    "pb.none": "No install chosen",
    "pb.choose": "Choose the game folder",
    "pb.opens": "Opens {0}",
    "pb.gameOpen": "Game running · ",
    "pb.mod": "mod {0}",
    "pb.modInstalled": "mod installed",
    "pb.noMod": "without the mod",

    "toast.folderChosen": "Folder chosen: {0}",
    "toast.scanDone": "Search finished",
    "stage.verify": "Checking the signature",
    "stage.download": "Downloading",
    "stage.unpack": "Unpacking",
    "stage.install": "Installing",
    "stage.done": "Done",
    "progress.of": "{0} of {1}",
    "foreign.title": "Another mod's msimg32.dll is already there",
    "foreign.body": "This folder already has an msimg32.dll that is not GW2 Enhanced's (another mod uses the same name). Replace it? Its file is kept as msimg32.dll.antes-do-gw2enhanced and comes back when you uninstall GW2 Enhanced; meanwhile, that mod does not load.",
    "foreign.ok": "Replace",
    "done.release": "GW2 Enhanced v{0} installed",
    "done.local": "Local package installed",
    "uninstall.title": "Uninstall GW2 Enhanced?",
    "uninstall.body": "The mod's msimg32.dll is removed and another mod's, if there was one, is put back. Your settings, translations and logs stay in the game folder.",
    "uninstall.ok": "Uninstall",
    "done.uninstall": "GW2 Enhanced uninstalled",
  },
};

/** The first of the browser's languages that is Portuguese or English; English otherwise. */
function browserLang() {
  const prefs = navigator.languages && navigator.languages.length ? navigator.languages : [navigator.language || ""];
  for (const l of prefs) {
    const b = String(l).toLowerCase();
    if (b === "pt" || b.startsWith("pt-")) return "pt-BR";
    if (b === "en" || b.startsWith("en-")) return "en";
  }
  return "en";
}
let ui = browserLang();

/** The string `key` in the page's language, with {0}, {1}… replaced by `args`. */
function t(key, ...args) {
  const d = STR[ui], v = Object.prototype.hasOwnProperty.call(d, key) ? d[key] : STR["pt-BR"][key];
  if (typeof v !== "string") return key;
  return args.length ? v.replace(/\{(\d+)\}/g, (m, i) => (i < args.length ? String(args[i]) : m)) : v;
}

// The Portuguese of the static page, kept the first time it is replaced (element -> its original child nodes, and
// element -> { attribute: original value }).
const origKids = new WeakMap(), origAttrs = new WeakMap();

/** Nodes for a page template: text, with {n} taken from the original child elements `kids`. */
function fill(tpl, kids) {
  const out = [];
  let last = 0;
  tpl.replace(/\{(\d+)\}/g, (m, i, at) => {
    if (at > last) out.push(tpl.slice(last, at));
    out.push(kids[Number(i)] || m);
    last = at + m.length;
    return m;
  });
  if (last < tpl.length) out.push(tpl.slice(last));
  return out;
}

/** Writes the static page in the current language. */
function translatePage() {
  const en = ui === "en";
  for (const n of document.querySelectorAll("[data-i18n]")) {
    if (!origKids.has(n)) { if (!en) continue; origKids.set(n, [...n.childNodes]); }
    const orig = origKids.get(n), tpl = STR.en[n.dataset.i18n];
    if (!en || typeof tpl !== "string") n.replaceChildren(...orig);
    else n.replaceChildren(...fill(tpl, orig.filter((c) => c.nodeType === 1)));
  }
  for (const n of document.querySelectorAll("[data-i18n-attr]")) {
    if (!origAttrs.has(n)) { if (!en) continue; origAttrs.set(n, {}); }
    const orig = origAttrs.get(n);
    for (const pair of n.dataset.i18nAttr.split(/\s+/)) {
      const [attr, key] = pair.split(":");
      if (!attr || !key) continue;
      if (!(attr in orig)) orig[attr] = n.getAttribute(attr) || "";
      const v = en ? STR.en[key] : undefined;
      n.setAttribute(attr, typeof v === "string" ? v : orig[attr]);
    }
  }
}

/** Shows the page in `l` (no saving). */
function applyLang(l) {
  if (!LANGS.includes(l)) return;
  ui = l;
  document.documentElement.lang = l;
  translatePage();
  for (const b of document.querySelectorAll("[data-lang]")) b.setAttribute("aria-pressed", String(b.dataset.lang === l));
}

/** The player picked a language in the settings: shown at once, saved, and the Rust side follows. */
function chooseLang(l) {
  if (!LANGS.includes(l) || !state.overview || l === ui && state.overview.settings.language === l) return;
  applyLang(l);
  state.overview.settings.language = l;
  renderAll();
  saveSettings(() => refresh(false));
}

// ---------------------------------------------------------------- data

// Tags: a STR key, or text that reads the same in both languages.
const PACKAGES = [
  { id: "All", tags: [["tag.recommended", "red"], ["tag.translation"], ["tag.textures"], ["tag.graphics"]] },
  { id: "Translation", tags: [["CPU · CUDA · DirectML", "soft"]] },
  { id: "Translation-CPU", tags: [["tag.smallest", "gold"]] },
  { id: "Translation-CUDA", tags: [["NVIDIA", "soft"]] },
  { id: "Translation-DirectML", tags: [["AMD · Intel · NVIDIA", "soft"]] },
  { id: "Textures", tags: [["tag.hdPack", "soft"]] },
  { id: "Graphics", tags: [["HDR", "soft"]] },
];
const CHANNEL = { Steam: "Steam", Epic: "Epic Games", Standalone: "ArenaNet" };
const CHANNEL_SHORT = { Steam: "Steam", Epic: "Epic", Standalone: "ANet" };

const state = {
  overview: null,
  releases: null, // { releases, latest, installed, update_available }
  releasesError: null,
  loadingReleases: false,
  package: "All",
  busy: false,
};

// ---------------------------------------------------------------- helpers

function el(tag, attrs = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === "class") e.className = v;
    else if (k === "text") e.textContent = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) if (c != null) e.append(c);
  return e;
}

/** Error text from the Rust side, as a sentence. */
function errText(e) {
  const s = String(e);
  return s.charAt(0).toUpperCase() + s.slice(1);
}

/** A tag's text: a STR key, else the text itself. */
function tagText(s) { return Object.prototype.hasOwnProperty.call(STR["pt-BR"], s) ? t(s) : s; }

function chip(text, kind) { return el("span", { class: "chip" + (kind ? " " + kind : ""), text }); }

let toastTimer;
function toast(msg, isErr) {
  const t = $("toast");
  t.textContent = msg;
  t.className = "toast" + (isErr ? " err" : "");
  t.hidden = false;
  t.style.animation = "none"; void t.offsetWidth; t.style.animation = "";
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), isErr ? 7000 : 3500);
}

function confirmDialog(title, body, okText) {
  return new Promise((resolve) => {
    const d = $("dlg");
    $("dlg-title").textContent = title;
    $("dlg-body").replaceChildren(el("p", { text: body }));
    $("dlg-ok").textContent = okText || t("dlg.confirm");
    const done = (v) => { d.close(); resolve(v); };
    $("dlg-ok").onclick = () => done(true);
    $("dlg-cancel").onclick = () => done(false);
    d.oncancel = () => resolve(false);
    d.showModal();
  });
}

function fmtDate(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  return isNaN(d) ? "" : d.toLocaleDateString(ui, { day: "numeric", month: "long", year: "numeric" });
}
function fmtSize(n) {
  if (!n) return "";
  if (n >= 1024 ** 3) return (n / 1024 ** 3).toFixed(1).replace(".", t("size.decimal")) + " GB";
  return Math.max(1, Math.round(n / 1024 ** 2)) + " MB";
}
function pkgName(id) { return PACKAGES.some((p) => p.id === id) ? t("pkg." + id) : id; }

function selectedInstall() {
  const o = state.overview;
  return o && o.installs.find((i) => i.path === o.selected);
}

/** The release to install from the Install screen: the latest stable one (or the newest listed). */
function targetRelease() {
  const r = state.releases;
  if (!r) return null;
  return r.releases.find((x) => x.version === r.latest) || r.releases[0] || null;
}

function visibleReleases() {
  const r = state.releases;
  if (!r) return [];
  const pre = state.overview && state.overview.settings.show_prereleases;
  return r.releases.filter((x) => pre || !x.prerelease);
}

// ---------------------------------------------------------------- navigation

function go(view) {
  if (!document.getElementById("view-" + view)) view = "inicio";
  for (const s of document.querySelectorAll(".view")) s.hidden = s.id !== "view-" + view;
  for (const a of document.querySelectorAll(".mainnav a")) {
    if (a.dataset.view === view) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  }
  $("main").scrollTop = 0;
  if (view === "atualizacoes" && !state.releases && !state.loadingReleases) loadReleases();
}

// ---------------------------------------------------------------- rendering

/** "pela Steam (Proton)", "pelo Wine"... */
function opensWith(i) {
  const linux = state.overview.platform === "linux";
  if (i.channel === "Steam") return t(linux ? "via.steamProton" : "via.steam");
  if (i.channel === "Epic") return t(linux ? "via.heroic" : "via.epic");
  return t(linux ? "via.wine" : "via.exe");
}

function statusText(st) {
  if (st.installed) {
    const v = st.version ? "v" + st.version : t("st.unknownVersion");
    return { title: t("st.installed", v), kind: "ok" };
  }
  if (st.legacy_remastered) return { title: t("st.legacyRemastered"), kind: "warn" };
  if (st.legacy_translated) return { title: t("st.legacyTranslated"), kind: "warn" };
  return { title: t("st.notInstalled"), kind: "bad" };
}

function renderHome() {
  const o = state.overview;
  const sel = selectedInstall();
  const title = document.querySelector(".hero-title");
  const sub = $("hero-sub");
  const chips = $("hero-chips");
  chips.replaceChildren();
  if (!sel) {
    title.textContent = t("home.notFound");
    sub.textContent = t(o.platform === "linux" ? "home.notFoundLinux" : "home.notFoundWin");
  } else {
    const st = sel.status;
    if (st.installed) {
      title.textContent = t("home.installedTitle", st.version ? "v" + st.version : t("home.installedWord"));
      sub.textContent = st.package ? t("home.packageIn", pkgName(st.package), sel.path) : t("home.installedIn", sel.path);
      chips.append(chip(t("chip.installed"), "ok"));
      if (!st.version) chips.append(chip(t("chip.unknownVersion"), "warn"));
    } else {
      title.textContent = t("home.notInstalledTitle");
      sub.textContent = t("home.chooseAndInstall", sel.path);
      chips.append(chip(t("chip.notInstalled"), "bad"));
    }
    for (const d of st.data_dirs) chips.append(chip(d.replace("GW2", "")));
    if (st.legacy_translated) chips.append(chip(t("chip.legacyTranslated"), "warn"));
    if (st.legacy_remastered) chips.append(chip(t("chip.legacyRemastered"), "warn"));
    if (st.foreign_dll) chips.append(chip(t("chip.foreignDll"), "warn"));
    if (st.backup) chips.append(chip(t("chip.backup")));
  }
  if (o.game_running) chips.append(chip(t("chip.gameOpen"), "dark"));
  renderWine(sel);

  // Update banner.
  const r = state.releases;
  const banner = $("update-banner");
  const latest = targetRelease();
  const installed = sel && sel.status.installed;
  if (r && latest && installed && r.update_available) {
    $("update-title").textContent = t("upd.newTitle", latest.version);
    $("update-text").textContent = t("upd.newText", r.installed);
    banner.hidden = false;
  } else if (r && latest && sel && !installed) {
    $("update-title").textContent = t("upd.availTitle", latest.version);
    $("update-text").textContent = t("upd.availText", pkgName(state.package));
    banner.hidden = false;
  } else {
    banner.hidden = true;
  }
  $("nav-dot").hidden = !(r && r.update_available);

  // News.
  $("news-more").hidden = !latest || !!state.releasesError;
  if (state.releasesError) {
    $("news-title").textContent = t("news.failed");
    $("news-date").textContent = "";
    $("news-notes").replaceChildren(el("p", { text: state.releasesError }));
  } else if (latest) {
    $("news-title").textContent = latest.title;
    $("news-date").textContent = fmtDate(latest.published_at);
    if (latest.notes_html) $("news-notes").innerHTML = latest.notes_html;
    else $("news-notes").replaceChildren(el("p", {}, el("em", { text: t("notes.none") })));
  } else if (r) {
    $("news-title").textContent = t("news.noneYet");
    $("news-date").textContent = "";
    $("news-notes").replaceChildren();
  } else if (!o.settings.check_updates_on_start && !state.loadingReleases) {
    $("news-title").textContent = t("news.autoOff");
    $("news-date").textContent = "";
    $("news-notes").replaceChildren(el("p", { text: t("news.autoOffText") }));
  } else {
    $("news-title").textContent = t("news.searching");
  }

  // Installs.
  const list = $("installs");
  list.replaceChildren();
  if (!o.installs.length) {
    list.append(el("div", { class: "paper empty-card" },
      el("p", { text: t("installs.none") })));
  }
  o.installs.forEach((i, n) => {
    const s = statusText(i.status);
    const isSel = i.path === o.selected;
    const card = el("div", { class: "paper install" + (isSel ? " selected" : ""), style: `animation-delay:${n * 60}ms` },
      el("div", { class: "channel-icon " + i.channel, text: CHANNEL_SHORT[i.channel], title: CHANNEL[i.channel] }),
      el("div", { class: "meta" },
        el("div", { class: "path", text: i.path }),
        el("div", { class: "sub", text: t("installs.foundBy", i.source, opensWith(i)) }),
        el("div", { class: "chips" }, chip(s.title, s.kind), i.running ? chip(t("chip.gameOpen"), "dark") : null, isSel ? chip(t("chip.inUse"), "dark") : null,
          i.proton ? chip(t(i.proton.override_set ? "chip.protonSet" : "chip.protonMissing"), i.proton.override_set ? "ok" : "warn") : null)),
      el("div", { class: "actions" },
        isSel ? null : el("button", { class: "gbtn small", type: "button", text: t("installs.use"), onclick: () => selectGame(i.path) }),
        el("button", { class: "gbtn small", type: "button", text: t("installs.folder"), onclick: () => invoke("open_folder", { path: i.path }).catch((e) => toast(errText(e), true)) }),
        el("button", { class: "gbtn small primary", type: "button", text: t("installs.play"), onclick: () => play(i.path, false) })));
    list.append(card);
  });
}

/** Linux: what Wine / Proton needs to load the mod (the msimg32 override), for the chosen install. */
function renderWine(sel) {
  const o = state.overview;
  const box = $("wine");
  const linux = o.platform === "linux";
  if (!linux || !sel || sel.channel === "Epic") { box.hidden = true; return; }
  box.hidden = false;
  const p = sel.proton;
  const why = t("wine.why");
  let ok = false, line = o.proton_line, canApply = false;
  if (sel.channel === "Steam" && p) {
    ok = p.override_set;
    line = p.line;
    canApply = !ok && p.accounts > 0;
    $("wine-title").textContent = t(ok ? "wine.protonSet" : "wine.protonMissing");
    $("wine-text").textContent = ok
      ? t("wine.protonOk")
      : why + t("wine.protonHow") + (p.steam_running ? t("wine.closeSteamFirst") : "");
  } else {
    $("wine-title").textContent = t("wine.wineTitle");
    $("wine-text").textContent = why + (sel.wine_prefix ? t("wine.playPrefix", sel.wine_prefix) : t("wine.play")) + t("wine.lutris");
    line = "WINEDLLOVERRIDES=\"msimg32=n,b\" wine Gw2-64.exe";
  }
  box.classList.toggle("ok", ok);
  $("wine-line").textContent = line;
  $("wine-line-row").hidden = ok;
  $("wine-actions").hidden = !canApply;
  $("wine-apply").disabled = state.busy || (p && p.steam_running);
  $("wine-apply").title = p && p.steam_running ? t("wine.closeSteamTitle") : "";
}

async function applyProton() {
  const sel = selectedInstall();
  if (!sel) return;
  const yes = await confirmDialog(t("proton.dlgTitle"), t("proton.dlgBody"), t("proton.dlgOk"));
  if (!yes) return;
  try {
    const log = await invoke("set_proton_override", { path: sel.path });
    toast(t("proton.done", log.join("; ")));
  } catch (e) { toast(errText(e), true); }
  refresh(false);
}

async function copyLine() {
  const text = $("wine-line").textContent;
  try { await navigator.clipboard.writeText(text); toast(t("copy.done")); }
  catch {
    const r = document.createRange(); r.selectNodeContents($("wine-line"));
    const s = getSelection(); s.removeAllRanges(); s.addRange(r);
    toast(t("copy.selected"));
  }
}

function renderPackages() {
  const box = $("packages");
  box.replaceChildren();
  const rel = targetRelease();
  const st = selectedInstall() && selectedInstall().status;
  PACKAGES.forEach((p) => {
    const on = state.package === p.id;
    const size = rel ? fmtSize(rel.package_sizes[p.id]) : "";
    const missing = rel && !rel.packages.includes(p.id);
    const foot = el("div", { class: "pkg-foot" },
      p.tags.map(([s, k]) => el("span", { class: "tag " + (k || ""), text: tagText(s) })),
      st && st.installed && st.package === p.id ? el("span", { class: "tag gold", text: t("pkg.installedTag") }) : null,
      size ? el("span", { class: "tag soft", text: size }) : null,
      missing ? el("span", { class: "tag", text: t("pkg.notInRelease") }) : null);
    const b = el("button", { class: "paper pkg", type: "button", role: "radio", "aria-checked": on ? "true" : "false",
      onclick: () => choosePackage(p.id) },
      el("div", { class: "pkg-head" }, el("b", { text: t("pkg." + p.id) }), el("span", { class: "radio", "aria-hidden": "true" })),
      el("p", { text: t("pkg." + p.id + ".text") }), foot);
    box.append(b);
  });
}

function renderInstallPanel() {
  const sel = selectedInstall();
  const o = state.overview;
  const rel = targetRelease();
  $("install-path").textContent = sel ? sel.path : t("inst.noFolder");
  const notes = [];
  if (sel) {
    const s = sel.status;
    notes.push(s.installed
      ? t("inst.installedNote", (s.version ? "v" + s.version : t("st.unknownVersion")) + (s.package ? " · " + pkgName(s.package) : ""))
      : t("inst.notInstalledNote"));
    if (s.foreign_dll) notes.push(t("inst.foreignNote"));
  }
  if (o.game_running) notes.push(t("inst.closeGame"));
  $("install-state").textContent = notes.join(" ");
  const gh = $("install-gh");
  if (state.loadingReleases) gh.textContent = t("inst.querying");
  else if (rel) gh.textContent = t("inst.downloadVersion", rel.version);
  else gh.textContent = t("inst.download");
  gh.disabled = state.busy || !sel || !rel || !rel.packages.includes(state.package);
  const local = o.local_package;
  const lb = $("install-local");
  lb.hidden = !local;
  if (local) lb.textContent = t("inst.local") + (local.version ? " (v" + local.version + (local.package ? " · " + pkgName(local.package) : "") + ")" : "");
  lb.disabled = state.busy || !sel;
  $("uninstall").disabled = state.busy || !sel || !(sel.status.installed || sel.status.legacy_translated || sel.status.legacy_remastered);
}

function renderReleases() {
  const box = $("releases");
  box.replaceChildren();
  if (state.loadingReleases && !state.releases) {
    box.append(el("p", { class: "empty", text: t("rel.querying") }));
    return;
  }
  if (state.releasesError) {
    box.append(el("div", { class: "paper error-card" },
      el("h3", { text: t("rel.failed") }),
      el("p", { text: state.releasesError })));
    return;
  }
  const list = visibleReleases();
  if (!list.length) {
    box.append(el("p", { class: "empty", text: t("rel.none") }));
    return;
  }
  const r = state.releases;
  list.forEach((rel, n) => {
    const tags = [];
    if (rel.version === r.latest) tags.push(el("span", { class: "tag red", text: t("rel.latest") }));
    if (r.installed && rel.version === r.installed) tags.push(el("span", { class: "tag gold", text: t("rel.installed") }));
    if (rel.prerelease) tags.push(el("span", { class: "tag", text: t("rel.prerelease") }));
    if (!rel.signed) tags.push(el("span", { class: "tag", text: t("rel.unsigned") }));
    tags.push(el("span", { class: "tag soft", text: fmtDate(rel.published_at) }));
    const canInstall = rel.signed && rel.packages.includes(state.package);
    const notes = el("div", { class: "notes" });
    if (rel.notes_html) notes.innerHTML = rel.notes_html;
    else notes.append(el("p", {}, el("em", { text: t("notes.none") })));
    box.append(el("article", { class: "paper release", style: `animation-delay:${Math.min(n, 6) * 60}ms` },
      el("div", { class: "release-head" },
        el("div", {}, el("h3", { text: rel.title }), el("div", { class: "release-tags" }, tags)),
        el("div", { class: "release-buttons" },
          rel.page ? el("button", { class: "gbtn small", type: "button", text: t("rel.github"), onclick: () => openUrl(rel.page) }) : null,
          el("button", { class: "gbtn small primary", type: "button", disabled: !canInstall || state.busy || !selectedInstall(),
            title: canInstall ? null : t("rel.noPackage", pkgName(state.package)),
            text: t("rel.install", pkgName(state.package)), onclick: () => installRelease(rel) }))),
      notes));
  });
}

function renderSettings() {
  const o = state.overview;
  const s = o.settings;
  const sel = $("set-game");
  sel.replaceChildren(...o.installs.map((i) => el("option", { value: i.path, selected: i.path === o.selected, text: `${i.path}  (${CHANNEL[i.channel]})` })));
  if (!o.installs.length) sel.append(el("option", { value: "", text: t("set.noInstalls") }));
  $("set-check").checked = s.check_updates_on_start;
  $("set-pre").checked = s.show_prereleases;
  $("set-close").checked = s.close_on_launch;
  $("opt-autologin").checked = s.launch.autologin;
  $("opt-share").checked = s.launch.share_archive;
  $("opt-maplo").checked = s.launch.map_load_info;
  $("opt-windowed").checked = s.launch.windowed;
  $("opt-extra").value = s.launch.extra;
  const cur = selectedInstall();
  $("set-patch").disabled = !cur || cur.channel !== "Standalone";
  $("set-open").disabled = !cur;
  $("about-version").textContent = "v" + o.app_version;
  for (const b of document.querySelectorAll("[data-lang]")) b.setAttribute("aria-pressed", String(b.dataset.lang === ui));
}

function renderPlaybar() {
  const sel = selectedInstall();
  const o = state.overview;
  const badge = $("pb-channel");
  if (!sel) {
    badge.className = "channel-badge";
    badge.textContent = "—";
    $("pb-path").textContent = t("pb.none");
    $("pb-state").textContent = t("pb.choose");
    $("play").disabled = true;
    return;
  }
  badge.className = "channel-badge " + sel.channel;
  badge.textContent = CHANNEL[sel.channel];
  $("pb-path").textContent = sel.path;
  const s = sel.status;
  const how = t("pb.opens", opensWith(sel));
  const mod = s.installed ? (s.version ? t("pb.mod", "v" + s.version) : t("pb.modInstalled")) : t("pb.noMod");
  $("pb-state").textContent = (o.game_running ? t("pb.gameOpen") : "") + how + " · " + mod;
  $("play").disabled = false;
  $("tb-info").textContent = s.installed ? (s.version ? "v" + s.version : "") + (s.package ? " · " + pkgName(s.package) : "") : "";
}

function renderAll() {
  if (!state.overview) return;
  renderHome();
  renderPackages();
  renderInstallPanel();
  renderReleases();
  renderSettings();
  renderPlaybar();
}

// ---------------------------------------------------------------- actions

async function refresh(rescan) {
  try {
    state.overview = await invoke("overview", { rescan: !!rescan, lang: ui });
    if (!state.packageTouched) state.package = state.overview.settings.package || "All";
    // The saved choice wins over the browser's language.
    const saved = state.overview.settings.language;
    if (LANGS.includes(saved) && saved !== ui) applyLang(saved);
    renderAll();
  } catch (e) {
    toast(errText(e), true);
  }
}

async function loadReleases() {
  state.loadingReleases = true;
  state.releasesError = null;
  renderAll();
  try {
    state.releases = await invoke("releases");
  } catch (e) {
    state.releasesError = errText(e);
  } finally {
    state.loadingReleases = false;
    renderAll();
  }
}

async function selectGame(path) {
  try {
    await invoke("select_game", { path });
    await refresh(false);
    if (state.releases) await loadReleases();
  } catch (e) {
    toast(errText(e), true);
  }
}

async function browse() {
  try {
    const p = await invoke("browse_game");
    if (p) {
      toast(t("toast.folderChosen", p));
      await refresh(false);
    }
  } catch (e) {
    toast(errText(e), true);
  }
}

async function play(path, patchOnly) {
  try {
    const msg = await invoke("play", { path, patchOnly: !!patchOnly });
    toast(msg);
    const btn = $("play");
    btn.disabled = true;
    setTimeout(() => { btn.disabled = false; refresh(false); }, 4000);
  } catch (e) {
    toast(errText(e), true);
  }
}

function choosePackage(id) {
  state.package = id;
  state.packageTouched = true;
  renderPackages();
  renderInstallPanel();
  renderReleases();
  saveSettings();
}

function collectSettings() {
  const s = structuredClone(state.overview.settings);
  s.check_updates_on_start = $("set-check").checked;
  s.show_prereleases = $("set-pre").checked;
  s.close_on_launch = $("set-close").checked;
  s.package = state.package;
  s.launch = {
    autologin: $("opt-autologin").checked,
    share_archive: $("opt-share").checked,
    map_load_info: $("opt-maplo").checked,
    windowed: $("opt-windowed").checked,
    extra: $("opt-extra").value.trim(),
  };
  return s;
}

let saveTimer;
/** Saves the settings shortly after the last change; `then` runs once they are saved. */
function saveSettings(then) {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(async () => {
    const s = collectSettings();
    try {
      await invoke("save_settings", { settings: s });
      state.overview.settings = s;
      renderReleases();
      if (typeof then === "function") then();
    } catch (e) {
      toast(errText(e), true);
    }
  }, 250);
}

function setBusy(b) {
  state.busy = b;
  renderInstallPanel();
  renderReleases();
}

function showLog(lines, error) {
  const log = $("log");
  log.replaceChildren(...lines.map((l) => el("li", { text: l })));
  if (error) log.append(el("li", { class: "err", text: t("err.prefix", error) }));
  log.hidden = false;
}

const STAGES = ["verify", "download", "unpack", "install"];

function progress(stage, done, total) {
  const wrap = $("progress-wrap");
  wrap.hidden = false;
  $("progress-stage").textContent = STAGES.includes(stage) ? t("stage." + stage) : stage;
  const bar = $("progress-bar");
  const box = bar.parentElement;
  if (stage === "download" && total) {
    box.classList.remove("indeterminate");
    bar.style.width = Math.min(100, (done * 100) / total).toFixed(1) + "%";
    $("progress-num").textContent = t("progress.of", fmtSize(done), fmtSize(total));
  } else if (stage === "done") {
    box.classList.remove("indeterminate");
    bar.style.width = "100%";
    $("progress-num").textContent = "";
  } else {
    box.classList.add("indeterminate");
    $("progress-num").textContent = stage === "download" ? fmtSize(done) : "";
  }
}

async function runInstall(fn, doneMsg) {
  if (state.busy) return;
  go("instalar");
  setBusy(true);
  $("log").hidden = true;
  progress("verify", 0, 0);
  try {
    const lines = await fn();
    progress("done", 1, 1);
    $("progress-stage").textContent = t("stage.done");
    showLog(lines);
    toast(doneMsg);
  } catch (e) {
    $("progress-wrap").hidden = true;
    showLog([], errText(e));
    toast(errText(e), true);
  } finally {
    setBusy(false);
    await refresh(false);
    if (state.releases) {
      // The installed version changed: recompute "update available".
      loadReleases();
    }
  }
}

// Another mod's msimg32.dll is only replaced when the player says so: null = cancelled, else the choice to send.
async function foreignChoice() {
  const sel = selectedInstall();
  if (!sel || !sel.status.foreign_dll) return false;
  const yes = await confirmDialog(t("foreign.title"), t("foreign.body"), t("foreign.ok"));
  return yes ? true : null;
}

async function installRelease(rel) {
  const sel = selectedInstall();
  if (!sel) return;
  const replaceForeign = await foreignChoice();
  if (replaceForeign === null) return;
  return runInstall(() => invoke("install_release", { tag: rel.tag, package: state.package, replaceForeign }),
    t("done.release", rel.version));
}

async function installLocal() {
  const replaceForeign = await foreignChoice();
  if (replaceForeign === null) return;
  return runInstall(() => { progress("install", 0, 0); return invoke("install_local", { replaceForeign }); }, t("done.local"));
}

async function uninstall() {
  const ok = await confirmDialog(t("uninstall.title"), t("uninstall.body"), t("uninstall.ok"));
  if (!ok) return;
  return runInstall(() => { progress("install", 0, 0); return invoke("uninstall"); }, t("done.uninstall"));
}

function openUrl(url) {
  invoke("open_url", { url }).catch((e) => toast(errText(e), true));
}

// ---------------------------------------------------------------- wiring

function wire() {
  const win = T.window.getCurrentWindow();
  $("tb-min").addEventListener("click", () => win.minimize());
  $("tb-close").addEventListener("click", () => win.close());

  for (const a of document.querySelectorAll(".mainnav a")) {
    a.addEventListener("click", (e) => {
      e.preventDefault();
      if (e.detail > 0) a.blur();
      go(a.dataset.view);
    });
  }
  document.addEventListener("click", (e) => {
    const g = e.target.closest("[data-go]");
    if (g) go(g.dataset.go);
    // Links in the release notes open in the browser.
    const a = e.target.closest(".notes a");
    if (a) {
      e.preventDefault();
      const href = a.getAttribute("href");
      if (href) openUrl(href);
    }
  });
  $("rescan").addEventListener("click", async () => { await refresh(true); toast(t("toast.scanDone")); });
  $("browse").addEventListener("click", browse);
  $("set-browse").addEventListener("click", browse);
  $("set-game").addEventListener("change", (e) => e.target.value && selectGame(e.target.value));
  $("set-open").addEventListener("click", () => {
    const s = selectedInstall();
    if (s) invoke("open_folder", { path: s.path }).catch((e) => toast(errText(e), true));
  });
  $("set-patch").addEventListener("click", () => { const s = selectedInstall(); if (s) play(s.path, true); });
  for (const id of ["set-check", "set-pre", "set-close", "opt-autologin", "opt-share", "opt-maplo", "opt-windowed"]) {
    $(id).addEventListener("change", saveSettings);
  }
  $("set-pre").addEventListener("change", () => setTimeout(renderReleases, 300));
  $("opt-extra").addEventListener("input", saveSettings);
  $("play").addEventListener("click", () => { const s = selectedInstall(); if (s) play(s.path, false); });
  $("install-gh").addEventListener("click", () => { const r = targetRelease(); if (r) installRelease(r); });
  $("install-local").addEventListener("click", installLocal);
  $("uninstall").addEventListener("click", uninstall);
  $("refresh-releases").addEventListener("click", loadReleases);
  $("update-now").addEventListener("click", () => { const r = targetRelease(); if (r) installRelease(r); });
  $("about-repo").addEventListener("click", () => openUrl("https://github.com/wilssola/GW2Enhanced"));
  $("wine-apply").addEventListener("click", applyProton);
  $("wine-copy").addEventListener("click", copyLine);
  for (const b of document.querySelectorAll("[data-lang]")) b.addEventListener("click", () => chooseLang(b.dataset.lang));
  // No browser context menu or reload in the app window.
  document.addEventListener("contextmenu", (e) => { if (!e.target.closest(".notes, input, .log, .wine-line")) e.preventDefault(); });
  document.addEventListener("keydown", (e) => { if (e.key === "F5" || (e.ctrlKey && e.key.toLowerCase() === "r")) e.preventDefault(); });

  T.event.listen("progress", (ev) => progress(ev.payload.stage, ev.payload.done, ev.payload.total));
}

/** The page in the browser's language until the settings say otherwise; the texts the script owns get theirs. */
function initialTexts() {
  applyLang(ui);
  $("t-inicio").textContent = t("home.loading");
  $("news-title").textContent = t("news.searching");
  $("install-gh").textContent = t("inst.download");
  $("install-local").textContent = t("inst.local");
  $("progress-stage").textContent = t("inst.preparing");
  $("pb-path").textContent = t("pb.none");
  $("dlg-ok").textContent = t("dlg.confirm");
}

async function start() {
  initialTexts();
  wire();
  const hash = (location.hash || "#inicio").slice(1);
  go(hash);
  await refresh(false);
  if (state.overview && state.overview.settings.check_updates_on_start) loadReleases();
  else renderAll();
  // The game may be opened or closed meanwhile.
  setInterval(() => { if (!state.busy && !document.hidden) refresh(false); }, 15000);
}

document.addEventListener("DOMContentLoaded", start);
