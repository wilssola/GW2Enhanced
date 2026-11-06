// Translates the text of the game's web pages (Trading Post, Gem Store, Wizard's Vault).
//
// Runs inside the page. Called with {"en": "translation"} once per chunk: the first holds the
// pages' own texts, the second (with `complete` set) the game dictionary. Texts still without a
// translation after that are reported to the DLL through the console, which translates them on
// the player's PC and sends them back through `add`. Only text nodes and a few attributes are
// rewritten: values typed by the player and anything the page reads from its own data stay as
// they are. The pages redraw from their own state and only write a text node when it changed, so a
// translated node stays translated; when they do write English again, the observer catches it.
(function (entries, complete) {
  "use strict";
  if (window.__gw2t) {
    window.__gw2t.add(entries, complete);
    return;
  }

  const exact = new Map();
  const results = new Set();
  const patterns = [];
  const cache = new Map();
  const SKIP = new Set(["SCRIPT", "STYLE", "TEXTAREA", "NOSCRIPT"]);
  const ATTRIBUTES = ["placeholder", "title"];

  const normalize = (s) => s.replace(/\s+/g, " ").trim();
  const escape = (s) => s.replace(/[.*+?^$()|[\]\\{}]/g, "\\$&");
  const PLACEHOLDER = /\$?\{(\w+)\}/g;
  // Prefix of the console messages that carry missing texts (the DLL reads and hides them).
  const MARK = "\u0001gw2t-missing:";
  const MAX_REPORTED = 5000;
  const MAX_LENGTH = 2000;
  const missing = new Set();
  const reported = new Set();
  let ready = false;
  let timer = 0;

  function flush() {
    timer = 0;
    const batch = [];
    for (const key of missing) {
      if (translated(key) === null && reported.size < MAX_REPORTED) {
        reported.add(key);
        batch.push(key);
      }
    }
    missing.clear();
    if (batch.length) console.debug(MARK + JSON.stringify(batch));
  }

  function report(key) {
    if (reported.has(key) || key.length > MAX_LENGTH) return;
    missing.add(key);
    if (ready && !timer) timer = setTimeout(flush, 300);
  }

  function compile(en, tr) {
    const names = [];
    const source = en
      .split(PLACEHOLDER)
      .map((part, i) => {
        if (i % 2) {
          names.push(part);
          return "(.+?)";
        }
        return escape(part);
      })
      .join("");
    patterns.push({ regex: new RegExp("^" + source + "$"), names, tr, size: en.length });
  }

  function add(items) {
    let compiled = false;
    for (const [en, tr] of Object.entries(items)) {
      const key = normalize(en);
      if (PLACEHOLDER.test(key)) {
        compile(key, tr);
        compiled = true;
      } else {
        exact.set(key, tr);
      }
      PLACEHOLDER.lastIndex = 0;
      results.add(normalize(tr));
    }
    if (compiled) patterns.sort((a, b) => b.size - a.size);
    cache.clear();
  }

  function translated(key) {
    if (results.has(key) && !exact.has(key)) return null;
    const direct = exact.get(key);
    if (direct !== undefined) return direct;
    for (const p of patterns) {
      const m = p.regex.exec(key);
      if (m) {
        const values = Object.fromEntries(p.names.map((name, i) => [name, m[i + 1]]));
        return p.tr.replace(PLACEHOLDER, (all, name) => (name in values ? values[name] : all));
      }
    }
    return null;
  }

  // The translation of `text` keeping its surrounding spaces; null when there is none.
  function translate(text) {
    const key = normalize(text);
    if (!key || !/\p{L}/u.test(key)) return null;
    let tr = cache.get(key);
    if (tr === undefined) {
      tr = translated(key);
      cache.set(key, tr);
    }
    if (tr === null && !results.has(key)) report(key);
    if (tr === null || tr === key) return null;
    const [lead] = text.match(/^\s*/);
    const [trail] = text.match(/\s*$/);
    return lead + tr + trail;
  }

  function textNode(node) {
    const parent = node.parentNode;
    if (!parent || SKIP.has(parent.nodeName) || parent.isContentEditable) return;
    const tr = translate(node.nodeValue);
    if (tr !== null) node.nodeValue = tr;
  }

  function element(el) {
    for (const name of ATTRIBUTES) {
      const value = el.getAttribute(name);
      if (value) {
        const tr = translate(value);
        if (tr !== null) el.setAttribute(name, tr);
      }
    }
  }

  function walk(root) {
    if (root.nodeType === 3) return textNode(root);
    if (root.nodeType !== 1 || SKIP.has(root.nodeName)) return;
    element(root);
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT | NodeFilter.SHOW_ELEMENT);
    while (walker.nextNode()) {
      const n = walker.currentNode;
      if (n.nodeType === 3) textNode(n);
      else element(n);
    }
  }

  window.__gw2t = {
    translate,
    add(items, last) {
      add(items);
      if (typeof document !== "undefined" && document.documentElement) walk(document.documentElement);
      if (last && !ready) {
        ready = true;
        if (missing.size && !timer) timer = setTimeout(flush, 300);
      }
    },
  };

  add(entries);
  ready = !!complete;
  if (typeof document === "undefined" || !document.documentElement) return;
  walk(document.documentElement);
  new MutationObserver((records) => {
    for (const r of records) {
      if (r.type === "characterData") textNode(r.target);
      else if (r.type === "attributes") element(r.target);
      else r.addedNodes.forEach(walk);
    }
  }).observe(document.documentElement, {
    subtree: true,
    childList: true,
    characterData: true,
    attributes: true,
    attributeFilter: ATTRIBUTES,
  });
})
