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
  const REDUCE = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const HAS_IO = "IntersectionObserver" in window;
  document.documentElement.classList.add("js");

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
    { gallery: "texturas", src: "/img/hd-armor-leaves.jpg", alt: "Armadura de folhas: original à esquerda, HD à direita", caption: "Armadura de folhas" },
    { gallery: "texturas", src: "/img/hd-armor-trim.jpg", alt: "Acabamento de armadura: original à esquerda, HD à direita", caption: "Acabamento de armadura" },
    { gallery: "texturas", src: "/img/hd-creature.jpg", alt: "Criatura: original à esquerda, HD à direita", caption: "Criatura" },
    { gallery: "texturas", src: "/img/hd-fur.jpg", alt: "Pelagem: original à esquerda, HD à direita", caption: "Pelagem" },
    { gallery: "texturas", src: "/img/hd-leaf.jpg", alt: "Folha: original à esquerda, HD à direita", caption: "Folha" },
    { gallery: "texturas", src: "/img/hd-wood.jpg", alt: "Madeira: original à esquerda, HD à direita", caption: "Madeira" },
    { gallery: "texturas", src: "/img/hd-flower.jpg", alt: "Flor: original à esquerda, HD à direita", caption: "Flor" },
    { gallery: "texturas", src: "/img/hd-crystal-blade.jpg", alt: "Lâmina de cristal: original à esquerda, HD à direita", caption: "Lâmina de cristal" },
    { gallery: "texturas", src: "/img/hd-engraving.jpg", alt: "Gravura de arma: original à esquerda, HD à direita", caption: "Gravura de arma" },
    { gallery: "texturas", src: "/img/hd-blade.jpg", alt: "Lâmina e cabo: original à esquerda, HD à direita", caption: "Lâmina e cabo" },
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

  /** Runs `fn` once, the first time `node` scrolls into view (right away without IntersectionObserver). */
  function whenVisible(node, fn, margin) {
    if (!HAS_IO || !node) { fn(); return; }
    const io = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) { io.disconnect(); fn(); }
    }, { rootMargin: margin || "0px 0px -10% 0px" });
    io.observe(node);
  }

  /** Counts a number up from zero (eased), writing it with `fmt`. */
  function countUp(node, target, fmt) {
    if (typeof target !== "number" || REDUCE) { node.textContent = fmt(target); return; }
    const t0 = performance.now(), dur = 1400;
    const step = (t) => {
      const k = Math.min(1, (t - t0) / dur);
      node.textContent = fmt(k < 1 ? target * (1 - Math.pow(1 - k, 3)) : target);
      if (k < 1) requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  }

  async function loadStats() {
    try {
      const s = await api("/v1/stats?lang=" + encodeURIComponent(LANG));
      const int = (x) => num(typeof x === "number" ? Math.round(x) : x);
      whenVisible($("#metricas"), () => {
        countUp($("#s-translated"), s.translated_pct, pct);
        countUp($("#s-perfect"), s.perfect_pct, pct);
        countUp($("#s-texts"), s.texts, int);
        countUp($("#s-downloads"), s.downloads, int);
        const w = Math.max(0, Math.min(100, Number(s.translated_pct) || 0));
        $("#s-bar").style.width = w + "%";
      });
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

  // A gallery is a before/after slider (each image holds the original on its left half and the HD version on its
  // right half) plus a row of thumbnails that pick the image. "Ampliar" opens the whole image in the lightbox.
  function viewer(shots, openLightbox) {
    let current = null, touched = false, dragging = false;
    const base = el("img", { alt: "", decoding: "async" });
    const hd = el("img", { alt: "", decoding: "async", "aria-hidden": "true" });
    const left = text("span", "badge left", "Original");
    const right = text("span", "badge right", "HD");
    const range = el("input", { class: "cmp-range", type: "range", min: 0, max: 100, step: 1, value: 50, "aria-label": "Comparar original e HD" });
    const cmp = el("div", { class: "cmp" }, base, el("div", { class: "cmp-hd" }, hd),
      el("span", { class: "cmp-line", "aria-hidden": "true" }, el("span", { class: "cmp-knob" })), left, right, range);
    const cap = text("figcaption", "cmp-cap", "");
    const zoom = el("button", { class: "gbtn", type: "button", onclick: () => openLightbox(current) }, "Ampliar");
    const thumbs = el("div", { class: "thumbs", role: "group", "aria-label": "Escolher a textura" });

    // The two halves are separated by a 4 px gap: scale the image so one half fills the frame, and shift the HD copy
    // past the gap.
    base.addEventListener("load", () => {
      const half = (base.naturalWidth - 4) / 2;
      if (half <= 0) return;
      base.style.width = hd.style.width = (base.naturalWidth / half) * 100 + "%";
      hd.style.left = (-(half + 4) / half) * 100 + "%";
    });
    const setPos = (p) => {
      const v = Math.max(0, Math.min(100, p));
      cmp.style.setProperty("--pos", v + "%");
      range.value = String(Math.round(v));
    };
    const fromPointer = (e) => {
      const r = cmp.getBoundingClientRect();
      if (r.width > 0) setPos(((e.clientX - r.left) / r.width) * 100);
    };
    cmp.addEventListener("pointerdown", (e) => {
      touched = dragging = true;
      cmp.classList.add("dragging");
      try { cmp.setPointerCapture(e.pointerId); } catch (_) { /* not capturable */ }
      fromPointer(e);
    });
    cmp.addEventListener("pointermove", (e) => { if (dragging) fromPointer(e); });
    const stop = () => { dragging = false; cmp.classList.remove("dragging"); };
    cmp.addEventListener("pointerup", stop);
    cmp.addEventListener("pointercancel", stop);
    range.addEventListener("input", () => { touched = true; setPos(Number(range.value)); });

    const show = (g, animate) => {
      current = g;
      base.src = g.src; hd.src = g.src; base.alt = g.alt;
      left.textContent = g.before || "Original";
      right.textContent = g.after || "HD";
      cap.textContent = g.caption;
      zoom.setAttribute("aria-label", "Ampliar: " + g.caption);
      for (const t of thumbs.children) t.setAttribute("aria-pressed", String(t.dataset.src === g.src));
      if (animate && !REDUCE) { cmp.classList.remove("cmp-swap"); void cmp.offsetWidth; cmp.classList.add("cmp-swap"); }
    };
    for (const g of shots) {
      const b = el("button", { class: "thumb", type: "button", "aria-pressed": "false", "aria-label": "Mostrar: " + g.caption, onclick: () => show(g, true) },
        el("img", { src: g.src, alt: "", loading: "lazy", decoding: "async" }), text("span", "", g.caption));
      b.dataset.src = g.src;
      thumbs.append(b);
    }
    show(shots[0], false);
    setPos(50);

    // A short sweep the first time the slider is seen, so it is clear that it can be dragged.
    if (!REDUCE) {
      whenVisible(cmp, () => {
        const t0 = performance.now(), dur = 1800;
        const step = (t) => {
          if (touched) return;
          const k = Math.min(1, (t - t0) / dur);
          setPos(50 + 28 * Math.sin(k * Math.PI * 2) * (1 - k));
          if (k < 1) requestAnimationFrame(step);
        };
        setTimeout(() => requestAnimationFrame(step), 400);
      }, "0px 0px -25% 0px");
    }

    return el("div", { class: "viewer" },
      el("figure", { class: "cmp-wrap paper" }, cmp, el("div", { class: "cmp-bar" }, cap, zoom)),
      thumbs);
  }

  function renderGalleries() {
    const lb = $("#lightbox"), lbImg = $("#lb-img"), lbCap = $("#lb-cap");
    $("#lb-close").addEventListener("click", () => lb.close());
    lb.addEventListener("click", (e) => { if (e.target === lb) lb.close(); });
    const openLightbox = (g) => {
      if (!g) return;
      lbImg.src = g.src; lbImg.alt = g.alt; lbCap.textContent = g.caption;
      lb.showModal();
    };
    for (const box of document.querySelectorAll("[data-gallery]")) {
      const name = box.dataset.gallery;
      const shots = GALLERY.filter((g) => g.gallery === name);
      const empty = document.querySelector('[data-empty="' + name + '"]');
      if (empty) empty.hidden = shots.length > 0;
      box.hidden = !shots.length;
      if (shots.length) box.append(viewer(shots, openLightbox));
    }
  }

  // Entrance animations: elements marked .reveal rise into place when they scroll into view. Once that is over the
  // classes are dropped, so their own hover transitions apply again.
  function setupReveal() {
    const nodes = document.querySelectorAll(".reveal");
    const done = (n) => {
      n.classList.add("in");
      setTimeout(() => n.classList.remove("reveal", "in"), REDUCE ? 0 : 1300);
    };
    if (!HAS_IO || REDUCE) { nodes.forEach(done); return; }
    const io = new IntersectionObserver((entries) => {
      for (const e of entries) if (e.isIntersecting) { io.unobserve(e.target); done(e.target); }
    }, { rootMargin: "0px 0px -8% 0px" });
    nodes.forEach((n) => io.observe(n));
  }

  // Marks the navigation link of the section being read.
  function setupNav() {
    if (!HAS_IO) return;
    const links = document.querySelectorAll(".mainnav a, .topnav a");
    const io = new IntersectionObserver((entries) => {
      for (const e of entries) {
        if (!e.isIntersecting) continue;
        const href = "#" + e.target.id;
        for (const a of links) {
          if (a.getAttribute("href") === href) a.setAttribute("aria-current", "true"); else a.removeAttribute("aria-current");
        }
      }
    }, { rootMargin: "-40% 0px -55% 0px" });
    for (const id of ["recursos", "traducao", "texturas", "graficos", "instalar"]) {
      const s = document.getElementById(id);
      if (s) io.observe(s);
    }
  }

  function init() {
    renderGalleries();
    setupReveal();
    setupNav();
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
