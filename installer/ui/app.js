// GW2 Enhanced installer: the page. Talks to the Rust side through Tauri commands (src/app.rs).
"use strict";

const T = window.__TAURI__;
const invoke = (cmd, args) => T.core.invoke(cmd, args);
const $ = (id) => document.getElementById(id);

const PACKAGES = [
  { id: "All", name: "Completo", tags: [["Recomendado", "red"], ["Tradução"], ["Texturas HD"], ["Gráficos"]],
    text: "Os três mods: tradução para português, texturas HD e gráficos. O tradutor local vem com todos os modos (processador, CUDA e DirectML)." },
  { id: "Translation", name: "Tradução", tags: [["CPU · CUDA · DirectML", "soft"]],
    text: "Só a tradução, com o tradutor local em todos os modos. Ele usa a placa de vídeo quando der e o processador nos outros casos." },
  { id: "Translation-CPU", name: "Tradução · CPU", tags: [["Menor download", "gold"]],
    text: "Só a tradução, com o tradutor só no processador. Funciona em qualquer PC; o modelo é o pequeno." },
  { id: "Translation-CUDA", name: "Tradução · CUDA", tags: [["NVIDIA", "soft"]],
    text: "Só a tradução, com processador e CUDA: para placas NVIDIA. Usa o modelo grande, mais preciso, rodando na placa." },
  { id: "Translation-DirectML", name: "Tradução · DirectML", tags: [["AMD · Intel · NVIDIA", "soft"]],
    text: "Só a tradução, com processador e DirectML: qualquer placa com DirectX 12 no Windows (AMD, Intel ou NVIDIA). Modelo grande." },
  { id: "Textures", name: "Texturas HD", tags: [["Pacote HD à parte", "soft"]],
    text: "Só o motor das texturas HD. O pacote de texturas (cerca de 95 GB) é baixado à parte, no Nexus Mods." },
  { id: "Graphics", name: "Gráficos", tags: [["HDR", "soft"]],
    text: "Só os gráficos: HDR de verdade, bloom, oclusão de ambiente, reflexos, raios de luz, efeitos de lente e correção de cor." },
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
    $("dlg-ok").textContent = okText || "Confirmar";
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
  return isNaN(d) ? "" : d.toLocaleDateString("pt-BR", { day: "numeric", month: "long", year: "numeric" });
}
function fmtSize(n) {
  if (!n) return "";
  if (n >= 1024 ** 3) return (n / 1024 ** 3).toFixed(1).replace(".", ",") + " GB";
  return Math.max(1, Math.round(n / 1024 ** 2)) + " MB";
}
function pkgName(id) { return (PACKAGES.find((p) => p.id === id) || { name: id }).name; }

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
  if (i.channel === "Steam") return linux ? "pela Steam (Proton)" : "pela Steam";
  if (i.channel === "Epic") return linux ? "pelo Heroic (Epic Games)" : "pela Epic Games";
  return linux ? "pelo Wine" : "pelo Gw2-64.exe";
}

function statusText(st) {
  if (st.installed) {
    const v = st.version ? "v" + st.version : "versão desconhecida";
    return { title: "Instalado · " + v, kind: "ok" };
  }
  if (st.legacy_remastered) return { title: "GW2 Remastered antigo", kind: "warn" };
  if (st.legacy_translated) return { title: "GW2 Translated antigo", kind: "warn" };
  return { title: "Não instalado", kind: "bad" };
}

function renderHome() {
  const o = state.overview;
  const sel = selectedInstall();
  const title = document.querySelector(".hero-title");
  const sub = $("hero-sub");
  const chips = $("hero-chips");
  chips.replaceChildren();
  if (!sel) {
    title.textContent = "Guild Wars 2 não encontrado";
    sub.textContent = o.platform === "linux"
      ? "Não achei o jogo na Steam (nativa, Flatpak ou Snap) nem em prefixos do Wine ou do Lutris. Escolha a pasta onde fica o Gw2-64.exe."
      : "Não achei o jogo na Steam, na Epic Games nem nas pastas padrão. Escolha a pasta onde fica o Gw2-64.exe.";
  } else {
    const st = sel.status;
    if (st.installed) {
      title.textContent = "GW2 Enhanced " + (st.version ? "v" + st.version : "instalado");
      sub.textContent = st.package ? "Pacote " + pkgName(st.package) + " em " + sel.path : "Instalado em " + sel.path;
      chips.append(chip("Instalado", "ok"));
      if (!st.version) chips.append(chip("Versão desconhecida", "warn"));
    } else {
      title.textContent = "Mod não instalado";
      sub.textContent = "Escolha um pacote e instale em " + sel.path + ".";
      chips.append(chip("Não instalado", "bad"));
    }
    for (const d of st.data_dirs) chips.append(chip(d.replace("GW2", "")));
    if (st.legacy_translated) chips.append(chip("GW2 Translated antigo: será trocado", "warn"));
    if (st.legacy_remastered) chips.append(chip("GW2 Remastered antigo: será trocado", "warn"));
    if (st.foreign_dll) chips.append(chip("msimg32.dll de outro mod", "warn"));
    if (st.backup) chips.append(chip("Backup de outro mod guardado"));
  }
  if (o.game_running) chips.append(chip("Jogo aberto", "dark"));
  renderWine(sel);

  // Update banner.
  const r = state.releases;
  const banner = $("update-banner");
  const latest = targetRelease();
  const installed = sel && sel.status.installed;
  if (r && latest && installed && r.update_available) {
    $("update-title").textContent = "Nova versão: v" + latest.version;
    $("update-text").textContent = "Você tem a v" + r.installed + ". Baixe e instale com um clique.";
    banner.hidden = false;
  } else if (r && latest && sel && !installed) {
    $("update-title").textContent = "Versão v" + latest.version + " disponível";
    $("update-text").textContent = "Instale o pacote " + pkgName(state.package) + " direto do GitHub.";
    banner.hidden = false;
  } else {
    banner.hidden = true;
  }
  $("nav-dot").hidden = !(r && r.update_available);

  // News.
  $("news-more").hidden = !latest || !!state.releasesError;
  if (state.releasesError) {
    $("news-title").textContent = "Não foi possível consultar";
    $("news-date").textContent = "";
    $("news-notes").replaceChildren(el("p", { text: state.releasesError }));
  } else if (latest) {
    $("news-title").textContent = latest.title;
    $("news-date").textContent = fmtDate(latest.published_at);
    $("news-notes").innerHTML = latest.notes_html || "<p><em>Sem notas.</em></p>";
  } else if (r) {
    $("news-title").textContent = "Nenhuma versão publicada ainda";
    $("news-date").textContent = "";
    $("news-notes").replaceChildren();
  } else if (!o.settings.check_updates_on_start && !state.loadingReleases) {
    $("news-title").textContent = "Verificação automática desligada";
    $("news-date").textContent = "";
    $("news-notes").replaceChildren(el("p", { text: "Abra “Atualizações” para consultar as versões." }));
  }

  // Installs.
  const list = $("installs");
  list.replaceChildren();
  if (!o.installs.length) {
    list.append(el("div", { class: "paper empty-card" },
      el("p", { text: "Nenhuma instalação encontrada. Use “Escolher pasta…” e selecione o Gw2-64.exe." })));
  }
  o.installs.forEach((i, n) => {
    const s = statusText(i.status);
    const isSel = i.path === o.selected;
    const card = el("div", { class: "paper install" + (isSel ? " selected" : ""), style: `animation-delay:${n * 60}ms` },
      el("div", { class: "channel-icon " + i.channel, text: CHANNEL_SHORT[i.channel], title: CHANNEL[i.channel] }),
      el("div", { class: "meta" },
        el("div", { class: "path", text: i.path }),
        el("div", { class: "sub", text: "Encontrada por: " + i.source + " · abre " + opensWith(i) }),
        el("div", { class: "chips" }, chip(s.title, s.kind), i.running ? chip("Jogo aberto", "dark") : null, isSel ? chip("Em uso", "dark") : null,
          i.proton ? chip(i.proton.override_set ? "Proton configurado" : "Proton: falta o ajuste", i.proton.override_set ? "ok" : "warn") : null)),
      el("div", { class: "actions" },
        isSel ? null : el("button", { class: "gbtn small", type: "button", text: "Usar esta", onclick: () => selectGame(i.path) }),
        el("button", { class: "gbtn small", type: "button", text: "Pasta", onclick: () => invoke("open_folder", { path: i.path }).catch((e) => toast(errText(e), true)) }),
        el("button", { class: "gbtn small primary", type: "button", text: "Jogar", onclick: () => play(i.path, false) })));
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
  const why = "No Linux o jogo roda pelo Wine, que usa a própria cópia do msimg32.dll e ignora a do mod. " +
    "O ajuste msimg32=n,b manda usar primeiro a do mod (n = nativa) e depois a do Wine (b = embutida).";
  let ok = false, line = o.proton_line, canApply = false;
  if (sel.channel === "Steam" && p) {
    ok = p.override_set;
    line = p.line;
    canApply = !ok && p.accounts > 0;
    $("wine-title").textContent = ok ? "Proton configurado" : "Falta um ajuste na Steam (Proton)";
    $("wine-text").textContent = ok
      ? "As opções de inicialização da Steam já têm o ajuste msimg32=n,b: o mod vai carregar pelo Proton."
      : why + " Na Steam: botão direito no Guild Wars 2 › Propriedades › Geral › Opções de inicialização, e cole a linha abaixo." +
        (p.steam_running ? " Para eu configurar sozinho, feche a Steam antes." : "");
  } else {
    $("wine-title").textContent = "Wine ou Lutris: ajuste do msimg32";
    $("wine-text").textContent = why + (sel.wine_prefix ? " O botão Jogar já abre com o ajuste (prefixo " + sel.wine_prefix + ")." :
      " O botão Jogar já abre com o ajuste.") + " No Lutris, coloque WINEDLLOVERRIDES = msimg32=n,b nas variáveis de ambiente do jogo, ou msimg32 = n,b em Opções do Wine › Substituições de DLL.";
    line = "WINEDLLOVERRIDES=\"msimg32=n,b\" wine Gw2-64.exe";
  }
  box.classList.toggle("ok", ok);
  $("wine-line").textContent = line;
  $("wine-line-row").hidden = ok;
  $("wine-actions").hidden = !canApply;
  $("wine-apply").disabled = state.busy || (p && p.steam_running);
  $("wine-apply").title = p && p.steam_running ? "Feche a Steam primeiro" : "";
}

async function applyProton() {
  const sel = selectedInstall();
  if (!sel) return;
  const yes = await confirmDialog("Configurar a Steam",
    "Vou adicionar WINEDLLOVERRIDES=\"msimg32=n,b\" às opções de inicialização do Guild Wars 2 em cada conta da Steam deste computador. " +
    "Uma cópia do arquivo original fica ao lado (localconfig.vdf.antes-do-gw2enhanced). A Steam precisa estar fechada.", "Configurar");
  if (!yes) return;
  try {
    const log = await invoke("set_proton_override", { path: sel.path });
    toast("Steam configurada (" + log.join("; ") + "). Abra a Steam e jogue.");
  } catch (e) { toast(errText(e), true); }
  refresh(false);
}

async function copyLine() {
  const text = $("wine-line").textContent;
  try { await navigator.clipboard.writeText(text); toast("Linha copiada"); }
  catch {
    const r = document.createRange(); r.selectNodeContents($("wine-line"));
    const s = getSelection(); s.removeAllRanges(); s.addRange(r);
    toast("Selecionei a linha: copie com Ctrl+C");
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
      p.tags.map(([t, k]) => el("span", { class: "tag " + (k || ""), text: t })),
      st && st.installed && st.package === p.id ? el("span", { class: "tag gold", text: "Instalado" }) : null,
      size ? el("span", { class: "tag soft", text: size }) : null,
      missing ? el("span", { class: "tag", text: "Fora desta versão" }) : null);
    const b = el("button", { class: "paper pkg", type: "button", role: "radio", "aria-checked": on ? "true" : "false",
      onclick: () => choosePackage(p.id) },
      el("div", { class: "pkg-head" }, el("b", { text: p.name }), el("span", { class: "radio", "aria-hidden": "true" })),
      el("p", { text: p.text }), foot);
    box.append(b);
  });
}

function renderInstallPanel() {
  const sel = selectedInstall();
  const o = state.overview;
  const rel = targetRelease();
  $("install-path").textContent = sel ? sel.path : "Nenhuma pasta escolhida";
  const notes = [];
  if (sel) {
    const s = sel.status;
    notes.push(s.installed ? "Instalado: " + (s.version ? "v" + s.version : "versão desconhecida") + (s.package ? " · " + pkgName(s.package) : "") + "." : "O mod não está instalado nesta pasta.");
    if (s.foreign_dll) notes.push("Já existe um msimg32.dll de outro mod nesta pasta: ao instalar, você escolhe se quer substituí-lo.");
  }
  if (o.game_running) notes.push("Feche o jogo antes de instalar.");
  $("install-state").textContent = notes.join(" ");
  const gh = $("install-gh");
  if (state.loadingReleases) gh.textContent = "Consultando o GitHub…";
  else if (rel) gh.textContent = "Baixar e instalar v" + rel.version;
  else gh.textContent = "Baixar e instalar";
  gh.disabled = state.busy || !sel || !rel || !rel.packages.includes(state.package);
  const local = o.local_package;
  const lb = $("install-local");
  lb.hidden = !local;
  if (local) lb.textContent = "Instalar do pacote local" + (local.version ? " (v" + local.version + (local.package ? " · " + pkgName(local.package) : "") + ")" : "");
  lb.disabled = state.busy || !sel;
  $("uninstall").disabled = state.busy || !sel || !(sel.status.installed || sel.status.legacy_translated || sel.status.legacy_remastered);
}

function renderReleases() {
  const box = $("releases");
  box.replaceChildren();
  if (state.loadingReleases && !state.releases) {
    box.append(el("p", { class: "empty", text: "Consultando as versões no GitHub…" }));
    return;
  }
  if (state.releasesError) {
    box.append(el("div", { class: "paper error-card" },
      el("h3", { text: "Não consegui consultar as versões" }),
      el("p", { text: state.releasesError })));
    return;
  }
  const list = visibleReleases();
  if (!list.length) {
    box.append(el("p", { class: "empty", text: "Nenhuma versão publicada ainda." }));
    return;
  }
  const r = state.releases;
  list.forEach((rel, n) => {
    const tags = [];
    if (rel.version === r.latest) tags.push(el("span", { class: "tag red", text: "Mais recente" }));
    if (r.installed && rel.version === r.installed) tags.push(el("span", { class: "tag gold", text: "Instalada" }));
    if (rel.prerelease) tags.push(el("span", { class: "tag", text: "Pré-lançamento" }));
    if (!rel.signed) tags.push(el("span", { class: "tag", text: "Sem assinatura" }));
    tags.push(el("span", { class: "tag soft", text: fmtDate(rel.published_at) }));
    const canInstall = rel.signed && rel.packages.includes(state.package);
    const notes = el("div", { class: "notes" });
    notes.innerHTML = rel.notes_html || "<p><em>Sem notas.</em></p>";
    box.append(el("article", { class: "paper release", style: `animation-delay:${Math.min(n, 6) * 60}ms` },
      el("div", { class: "release-head" },
        el("div", {}, el("h3", { text: rel.title }), el("div", { class: "release-tags" }, tags)),
        el("div", { class: "release-buttons" },
          rel.page ? el("button", { class: "gbtn small", type: "button", text: "Ver no GitHub", onclick: () => openUrl(rel.page) }) : null,
          el("button", { class: "gbtn small primary", type: "button", disabled: !canInstall || state.busy || !selectedInstall(),
            title: canInstall ? null : "Esta versão não tem o pacote " + pkgName(state.package) + " assinado",
            text: "Instalar " + pkgName(state.package), onclick: () => installRelease(rel) }))),
      notes));
  });
}

function renderSettings() {
  const o = state.overview;
  const s = o.settings;
  const sel = $("set-game");
  sel.replaceChildren(...o.installs.map((i) => el("option", { value: i.path, selected: i.path === o.selected, text: `${i.path}  (${CHANNEL[i.channel]})` })));
  if (!o.installs.length) sel.append(el("option", { value: "", text: "Nenhuma instalação encontrada" }));
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
}

function renderPlaybar() {
  const sel = selectedInstall();
  const o = state.overview;
  const badge = $("pb-channel");
  if (!sel) {
    badge.className = "channel-badge";
    badge.textContent = "—";
    $("pb-path").textContent = "Nenhuma instalação escolhida";
    $("pb-state").textContent = "Escolha a pasta do jogo";
    $("play").disabled = true;
    return;
  }
  badge.className = "channel-badge " + sel.channel;
  badge.textContent = CHANNEL[sel.channel];
  $("pb-path").textContent = sel.path;
  const s = sel.status;
  const how = "Abre " + opensWith(sel);
  $("pb-state").textContent = (o.game_running ? "Jogo aberto · " : "") + how + " · " + (s.installed ? "mod " + (s.version ? "v" + s.version : "instalado") : "sem o mod");
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
    state.overview = await invoke("overview", { rescan: !!rescan });
    if (!state.packageTouched) state.package = state.overview.settings.package || "All";
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
      toast("Pasta escolhida: " + p);
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
function saveSettings() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(async () => {
    const s = collectSettings();
    try {
      await invoke("save_settings", { settings: s });
      state.overview.settings = s;
      renderReleases();
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
  if (error) log.append(el("li", { class: "err", text: "Erro: " + error }));
  log.hidden = false;
}

const STAGES = { verify: "Verificando a assinatura", download: "Baixando", unpack: "Descompactando", install: "Instalando" };

function progress(stage, done, total) {
  const wrap = $("progress-wrap");
  wrap.hidden = false;
  $("progress-stage").textContent = STAGES[stage] || stage;
  const bar = $("progress-bar");
  const box = bar.parentElement;
  if (stage === "download" && total) {
    box.classList.remove("indeterminate");
    bar.style.width = Math.min(100, (done * 100) / total).toFixed(1) + "%";
    $("progress-num").textContent = fmtSize(done) + " de " + fmtSize(total);
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
    $("progress-stage").textContent = "Concluído";
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
  const yes = await confirmDialog("Já existe um msimg32.dll de outro mod",
    "Esta pasta já tem um msimg32.dll que não é do GW2 Enhanced (outro mod usa o mesmo nome). Substituir? O arquivo dele fica guardado como "
    + "msimg32.dll.antes-do-gw2enhanced e volta ao desinstalar o GW2 Enhanced; enquanto isso, aquele mod não carrega.",
    "Substituir");
  return yes ? true : null;
}

async function installRelease(rel) {
  const sel = selectedInstall();
  if (!sel) return;
  const replaceForeign = await foreignChoice();
  if (replaceForeign === null) return;
  return runInstall(() => invoke("install_release", { tag: rel.tag, package: state.package, replaceForeign }),
    `GW2 Enhanced v${rel.version} instalado`);
}

async function installLocal() {
  const replaceForeign = await foreignChoice();
  if (replaceForeign === null) return;
  return runInstall(() => { progress("install", 0, 0); return invoke("install_local", { replaceForeign }); }, "Pacote local instalado");
}

async function uninstall() {
  const ok = await confirmDialog("Desinstalar o GW2 Enhanced?",
    "O msimg32.dll do mod é removido e o de outro mod, se havia, volta para o lugar. Suas configurações, traduções e logs ficam na pasta do jogo.",
    "Desinstalar");
  if (!ok) return;
  return runInstall(() => { progress("install", 0, 0); return invoke("uninstall"); }, "GW2 Enhanced desinstalado");
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
  $("rescan").addEventListener("click", async () => { await refresh(true); toast("Busca concluída"); });
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
  // No browser context menu or reload in the app window.
  document.addEventListener("contextmenu", (e) => { if (!e.target.closest(".notes, input, .log, .wine-line")) e.preventDefault(); });
  document.addEventListener("keydown", (e) => { if (e.key === "F5" || (e.ctrlKey && e.key.toLowerCase() === "r")) e.preventDefault(); });

  T.event.listen("progress", (ev) => progress(ev.payload.stage, ev.payload.done, ev.payload.total));
}

async function start() {
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
