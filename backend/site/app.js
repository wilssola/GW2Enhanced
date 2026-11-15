// GW2 Enhanced site. Served as /app.js (the CSP allows no inline script).
//
// XSS rule for this file: data from the API (English texts, translations,
// corrections, error codes) is only ever written with textContent / DOM
// nodes. Never use innerHTML, insertAdjacentHTML, document.write, or build
// attributes or URLs from API data.
"use strict";

(() => {
  const LANG = "pt-BR";
  const SITEKEY = document.body.dataset.sitekey || "";
  const $ = (s) => document.querySelector(s);
  const state = { sort: "recent", q: "", cursor: null, loading: false };

  const ERRORS = {
    already_voted: "Você já votou nesta tradução.",
    not_votable: "Esta tradução ainda não pode receber votos.",
    turnstile_failed: "Não foi possível confirmar que você é humano. Tente de novo.",
    turnstile_required: "Confirme que você é humano antes de enviar.",
    turnstile_not_configured: "A verificação está indisponível no momento.",
    rate_limited: "Muitas ações em pouco tempo. Espere um pouco e tente de novo.",
    banned: "Seu acesso para envio foi bloqueado.",
    markup: "A tradução precisa manter as marcações do jogo (%str1%, <c=…>, <br>, [s]…) iguais às do original.",
    fixups: "A tradução usa uma marcação [..] que não existe no original (só [s] é permitido).",
    hides_original: "A tradução não pode conter [null].",
    link: "Links não são permitidos.",
    length: "O tamanho da tradução está muito diferente do original.",
    empty: "A tradução está vazia.",
    too_long: "A tradução é longa demais.",
    control_chars: "A tradução contém caracteres invisíveis ou de controle.",
    unknown_text: "Texto não encontrado.",
    unknown_translation: "Tradução não encontrada.",
  };
  const SOURCES = { ai: "IA", argos: "Argos", user: "Comunidade" };

  // Comparison images. To add one: put the file in backend/site/img/, list it
  // in backend/src/images.rs, and add an entry here. `gallery` picks the
  // section ("texturas" or "graficos"). Optional `before`/`after` put labels
  // on the left and right halves of a side-by-side image that has none.
  const GALLERY = [
    { gallery: "texturas", src: "/img/hd-wall.jpg", alt: "Textura de muro de pedra: original à esquerda, HD à direita", caption: "Muro" },
    { gallery: "texturas", src: "/img/hd-weapon.jpg", alt: "Textura de arma: original à esquerda, HD à direita", caption: "Arma" },
    { gallery: "texturas", src: "/img/hd-building.jpg", alt: "Textura de construção: original à esquerda, HD à direita", caption: "Construção" },
    { gallery: "texturas", src: "/img/hd-city.jpg", alt: "Textura de cidade: original à esquerda, HD à direita", caption: "Cidade" },
  ];

  /** Creates an element. `attrs` values must be constants or numbers, never API data. */
  function el(tag, attrs, ...kids) {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (typeof v === "function") e.addEventListener(k.replace(/^on/, ""), v);
      else if (k === "class") e.className = v;
      else e.setAttribute(k, String(v));
    }
    for (const k of kids) if (k != null) e.append(k); // strings become text nodes
    return e;
  }
  const text = (tag, cls, value) => { const e = el(tag, cls ? { class: cls } : {}); e.textContent = value; return e; };

  function toast(msg) {
    const t = $("#toast");
    t.textContent = msg;
    t.hidden = false;
    clearTimeout(toast.timer);
    toast.timer = setTimeout(() => { t.hidden = true; }, 4000);
  }

  async function api(path, opts) {
    const r = await fetch(path, Object.assign({ credentials: "same-origin" }, opts));
    let body = null;
    try { body = await r.json(); } catch (_) { /* not JSON */ }
    if (!r.ok) {
      const code = body && typeof body.error === "string" ? body.error.split(":")[0] : "";
      throw new Error(Object.prototype.hasOwnProperty.call(ERRORS, code) ? ERRORS[code] : "Algo deu errado (" + r.status + "). Tente de novo.");
    }
    return body;
  }

  const pct = (x) => (typeof x === "number" ? x.toLocaleString("pt-BR", { maximumFractionDigits: 1 }) + "%" : "–");
  const num = (x) => (typeof x === "number" ? x.toLocaleString("pt-BR") : "–");

  async function loadStats() {
    try {
      const s = await api("/v1/stats?lang=" + encodeURIComponent(LANG));
      $("#s-translated").textContent = pct(s.translated_pct);
      $("#s-perfect").textContent = pct(s.perfect_pct);
      $("#s-texts").textContent = num(s.texts);
      $("#s-downloads").textContent = num(s.downloads);
      const w = Math.max(0, Math.min(100, Number(s.translated_pct) || 0));
      $("#s-bar").style.width = w + "%";
    } catch (_) { /* metrics are optional */ }
  }

  function voteButtons(t) {
    const up = el("button", { class: "gbtn vote", type: "button", "aria-label": "Votar a favor", title: "Boa tradução", onclick: () => vote(t.id, 1) });
    up.append(text("span", "up", "▲ " + num(t.up)));
    const down = el("button", { class: "gbtn vote", type: "button", "aria-label": "Votar contra", title: "Tradução ruim", onclick: () => vote(t.id, -1) });
    down.append(text("span", "down", "▼ " + num(t.down)));
    return [up, down];
  }

  function sourceChip(source) {
    const c = el("span", { class: "chip" });
    c.append(text("span", "", "Fonte"), text("span", "", Object.prototype.hasOwnProperty.call(SOURCES, source) ? SOURCES[source] : "?"));
    return c;
  }

  function render(item) {
    const cur = item.current;
    const en = text("div", "txt", item.en);
    en.setAttribute("lang", "en");
    const tr = cur ? text("div", "txt", cur.tr) : text("div", "txt none", "Ainda sem tradução.");
    const actions = el("div", { class: "actions" });
    if (cur) actions.append(...voteButtons(cur), sourceChip(cur.source));
    actions.append(el("span", { class: "spacer" }), el("button", { class: "gbtn", type: "button", onclick: () => correct(item) }, cur ? "Sugerir correção" : "Sugerir tradução"));
    const card = el("article", { class: "item paper" },
      el("div", { class: "pair" },
        el("div", {}, text("div", "label", "Original (inglês)"), en),
        el("div", {}, text("div", "label gold", "Tradução (português)"), tr)),
      actions);
    if (Array.isArray(item.alternatives) && item.alternatives.length) {
      const d = el("details", {}, text("summary", "", "Outras traduções (" + item.alternatives.length + ")"));
      for (const a of item.alternatives) d.append(el("div", { class: "alt" }, text("div", "txt", a.tr), ...voteButtons(a)));
      card.append(d);
    }
    return card;
  }

  async function load(reset) {
    if (state.loading) return;
    state.loading = true;
    const list = $("#list");
    if (reset) { state.cursor = null; list.replaceChildren(text("div", "msg", "Carregando…")); }
    const p = new URLSearchParams({ lang: LANG, sort: state.sort });
    if (state.q) p.set("q", state.q);
    if (state.cursor) p.set("cursor", state.cursor);
    try {
      const r = await api("/v1/texts?" + p.toString());
      if (reset) list.replaceChildren();
      for (const it of r.items) list.append(render(it));
      if (reset && !r.items.length) list.append(text("div", "msg", state.q ? "Nada encontrado." : "Nenhum texto ainda."));
      state.cursor = typeof r.next_cursor === "string" ? r.next_cursor : null;
      $("#more").hidden = !state.cursor;
    } catch (e) {
      if (reset) list.replaceChildren(text("div", "msg", e.message)); else toast(e.message);
    } finally {
      state.loading = false;
    }
  }

  // One dialog for every write: shows the Turnstile widget, sends when confirmed.
  let widget = null;
  function ask(title, bodyNodes, needsInput, send) {
    const dlg = $("#dlg"), ok = $("#dlg-ok"), note = $("#dlg-note");
    $("#dlg-title").textContent = title;
    $("#dlg-body").replaceChildren(...bodyNodes);
    note.textContent = SITEKEY ? "" : "A verificação está indisponível no momento.";
    let token = null;
    ok.disabled = true;
    const refresh = () => { ok.disabled = !token || (needsInput && !needsInput()); };
    $("#dlg-body").oninput = needsInput ? refresh : null;
    const close = () => {
      if (widget != null && window.turnstile) { window.turnstile.remove(widget); widget = null; }
      dlg.close();
    };
    $("#dlg-cancel").onclick = close;
    ok.onclick = async () => {
      ok.disabled = true;
      try { await send(token); close(); } catch (e) {
        note.textContent = e.message;
        token = null;
        if (widget != null && window.turnstile) window.turnstile.reset(widget);
      }
    };
    dlg.showModal();
    const mount = (tries) => {
      if (!window.turnstile) { if (tries < 50) setTimeout(() => mount(tries + 1), 200); return; }
      $("#ts").replaceChildren();
      widget = window.turnstile.render("#ts", {
        sitekey: SITEKEY,
        language: "pt-br",
        callback: (t) => { token = t; refresh(); },
        "expired-callback": () => { token = null; refresh(); },
        "error-callback": () => { note.textContent = "Falha na verificação. Recarregue a página."; },
      });
    };
    if (SITEKEY) mount(0);
  }

  function vote(id, value) {
    ask(value > 0 ? "Votar a favor desta tradução" : "Votar contra esta tradução",
      [text("p", "note", "Confirme que você é humano para registrar o voto.")], null,
      async (token) => {
        await api("/v1/votes", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ translation_id: id, value, turnstile: token }) });
        toast("Voto registrado. Obrigado!");
        load(true);
      });
  }

  function correct(item) {
    const ta = el("textarea", { "aria-label": "Sua tradução", maxlength: 8000 });
    ta.value = item.current ? item.current.tr : "";
    const en = text("p", "txt", item.en);
    en.setAttribute("lang", "en");
    ask("Sugerir tradução", [
      text("div", "label", "Original (inglês)"), en,
      text("div", "label gold", "Sua tradução"), ta,
      text("p", "note", "Mantenha as marcações do jogo iguais às do original. Sua sugestão passa por revisão antes de aparecer."),
    ], () => ta.value.trim() !== "" && ta.value !== (item.current && item.current.tr),
    async (token) => {
      await api("/v1/corrections", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ text_key: item.text_key, lang: LANG, tr: ta.value, turnstile: token }) });
      toast("Sugestão enviada para revisão. Obrigado!");
    });
  }

  function renderGalleries() {
    const lb = $("#lightbox"), lbImg = $("#lb-img"), lbCap = $("#lb-cap");
    $("#lb-close").addEventListener("click", () => lb.close());
    lb.addEventListener("click", (e) => { if (e.target === lb) lb.close(); });
    for (const box of document.querySelectorAll("[data-gallery]")) {
      const name = box.dataset.gallery;
      const shots = GALLERY.filter((g) => g.gallery === name);
      const empty = document.querySelector('[data-empty="' + name + '"]');
      if (empty) empty.hidden = shots.length > 0;
      box.hidden = !shots.length;
      for (const g of shots) {
        const img = el("img", { src: g.src, alt: g.alt, loading: "lazy", decoding: "async" });
        const frame = el("button", { class: "shot-frame", type: "button", "aria-label": "Ampliar: " + g.caption, onclick: () => {
          lbImg.src = g.src; lbImg.alt = g.alt; lbCap.textContent = g.caption;
          lb.showModal();
        } }, img);
        if (g.before) frame.append(text("span", "badge left", g.before));
        if (g.after) frame.append(text("span", "badge right", g.after));
        box.append(el("figure", { class: "shot paper" }, frame, text("figcaption", "", g.caption)));
      }
    }
  }

  function init() {
    renderGalleries();
    for (const b of document.querySelectorAll(".tabs button")) {
      b.addEventListener("click", () => {
        for (const o of document.querySelectorAll(".tabs button")) o.setAttribute("aria-pressed", String(o === b));
        state.sort = b.dataset.sort;
        load(true);
      });
    }
    let qTimer;
    $("#q").addEventListener("input", (e) => {
      clearTimeout(qTimer);
      qTimer = setTimeout(() => { state.q = e.target.value.trim().slice(0, 100); load(true); }, 300);
    });
    $("#more").addEventListener("click", () => load(false));
    loadStats();
    load(true);
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init); else init();
})();
