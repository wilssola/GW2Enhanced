// GW2 Enhanced site. Served as /app.js (the CSP allows no inline script).
//
// XSS rule for this file: data from the API (English texts, translations,
// corrections, error codes) is only ever written with textContent / DOM
// nodes. Never use innerHTML, insertAdjacentHTML, document.write, or build
// attributes or URLs from API data.
"use strict";

(() => {
  // The dictionary's language (the API's `lang`), not the language of the page.
  const DICT_LANG = "pt-BR";
  const SITEKEY = document.body.dataset.sitekey || "";
  const $ = (s) => document.querySelector(s);
  // The list's state lives in the URL (#dicionario?sort=best&p=3&n=50&q=…), so links, back and forward work.
  const SORTS = ["recent", "oldest", "best", "worst"];
  const SIZES = [20, 50, 100];
  const state = { sort: "recent", p: 1, n: 20, q: "", total: null, more: false, seq: 0, items: null, err: null };
  const REDUCE = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const HAS_IO = "IntersectionObserver" in window;
  document.documentElement.classList.add("js");

  // ---- languages ----
  //
  // index.html is written in Portuguese and stays readable without JS. Its translatable parts carry
  // data-i18n="key" (the element's content) or data-i18n-attr="attr:key ..." (attributes). Their Portuguese is the
  // markup itself; STR.en holds the English. In a page template, {0}, {1}… stand for the element's child elements,
  // in order (links, <b>, <code>…), which keep their own data-i18n. Strings built here have both languages in STR,
  // and {0}, {1}… in them are the arguments of t().
  const LANGS = ["pt-BR", "en"];
  const LANG_KEY = "gw2e-lang";
  const STR = {
    "pt-BR": {
      "err.already_voted": "Você já votou nesta tradução.",
      "err.not_votable": "Esta tradução ainda não pode receber votos.",
      "err.turnstile_failed": "Não foi possível confirmar que você é humano. Tente de novo.",
      "err.turnstile_required": "Confirme que você é humano antes de enviar.",
      "err.turnstile_not_configured": "A verificação está indisponível no momento.",
      "err.rate_limited": "Muitas ações em pouco tempo. Espere um pouco e tente de novo.",
      "err.banned": "Seu acesso para envio foi bloqueado.",
      "err.markup": "A tradução precisa manter as marcações do jogo (%str1%, <c=…>, <br>, [s]…) iguais às do original.",
      "err.fixups": "A tradução usa uma marcação [..] que não existe no original (só [s] é permitido).",
      "err.hides_original": "A tradução não pode conter [null].",
      "err.link": "Links não são permitidos.",
      "err.length": "O tamanho da tradução está muito diferente do original.",
      "err.empty": "A tradução está vazia.",
      "err.too_long": "A tradução é longa demais.",
      "err.control_chars": "A tradução contém caracteres invisíveis ou de controle.",
      "err.unknown_text": "Texto não encontrado.",
      "err.unknown_translation": "Tradução não encontrada.",
      "err.bad_page": "Esta página não existe.",
      "err.bad_limit": "Tamanho de página inválido.",
      "err.other": "Algo deu errado ({0}). Tente de novo.",
      "src.ai": "IA",
      "src.argos": "Argos",
      "src.user": "Comunidade",
      "vote.up": "Votar a favor",
      "vote.down": "Votar contra",
      "vote.upTitle": "Boa tradução",
      "vote.downTitle": "Tradução ruim",
      "vote.mine": "Seu voto",
      "item.source": "Fonte",
      "item.none": "Ainda sem tradução.",
      "item.correct": "Sugerir correção",
      "item.suggest": "Sugerir tradução",
      "item.en": "Original (inglês)",
      "item.tr": "Tradução (português)",
      "item.alts": "Outras traduções ({0})",
      "pager.first": "Primeira página",
      "pager.prev": "Página anterior",
      "pager.page": "Página {0}",
      "pager.next": "Próxima página",
      "pager.last": "Última página ({0})",
      "pager.of": "Mostrando {0}–{1} de {2}",
      "pager.search": "Mostrando {0}–{1} resultados da busca",
      "pager.range": "Mostrando {0}–{1}",
      "list.emptyPage": "Esta página está vazia.",
      "list.noMatch": "Nada encontrado.",
      "list.empty": "Nenhum texto ainda.",
      "jump.invalid": "Digite o número de uma página.",
      "dlg.verifyOff": "A verificação está indisponível no momento.",
      "dlg.verifyFail": "Falha na verificação. Recarregue a página.",
      "vote.titleUp": "Votar a favor desta tradução",
      "vote.titleDown": "Votar contra esta tradução",
      "vote.confirm": "Confirme que você é humano para registrar o voto.",
      "vote.done": "Voto registrado. Obrigado!",
      "fix.yours": "Sua tradução",
      "fix.title": "Sugerir tradução",
      "fix.note": "Mantenha as marcações do jogo iguais às do original. Sua sugestão passa por revisão antes de aparecer.",
      "fix.done": "Sugestão enviada para revisão. Obrigado!",
      "fix.same": "Igual à tradução atual: mude o texto para enviar uma correção.",
      "fix.breaks": "O original tem {0} quebra(s) de linha e a sua tradução tem {1}: use a mesma quantidade.",
      "err.line_breaks": "A tradução precisa ter a mesma quantidade de quebras de linha do original.",
      "err.same_as_current": "Essa sugestão é igual à tradução atual (ou a uma já enviada).",
      "cmp.before": "Original",
      "cmp.after": "HD",
      "cmp.range": "Comparar original e HD",
      "cmp.zoom": "Ampliar",
      "cmp.zoomOf": "Ampliar: {0}",
      "cmp.pick": "Escolher a textura",
      "cmp.show": "Mostrar: {0}",
      "img.wall": "Muro",
      "img.wall.alt": "Textura de muro de pedra: original à esquerda, HD à direita",
      "img.weapon": "Arma",
      "img.weapon.alt": "Textura de arma: original à esquerda, HD à direita",
      "img.building": "Construção",
      "img.building.alt": "Textura de construção: original à esquerda, HD à direita",
      "img.city": "Cidade",
      "img.city.alt": "Textura de cidade: original à esquerda, HD à direita",
      "img.armorLeaves": "Armadura de folhas",
      "img.armorLeaves.alt": "Armadura de folhas: original à esquerda, HD à direita",
      "img.armorTrim": "Acabamento de armadura",
      "img.armorTrim.alt": "Acabamento de armadura: original à esquerda, HD à direita",
      "img.creature": "Criatura",
      "img.creature.alt": "Criatura: original à esquerda, HD à direita",
      "img.fur": "Pelagem",
      "img.fur.alt": "Pelagem: original à esquerda, HD à direita",
      "img.leaf": "Folha",
      "img.leaf.alt": "Folha: original à esquerda, HD à direita",
      "img.wood": "Madeira",
      "img.wood.alt": "Madeira: original à esquerda, HD à direita",
      "img.flower": "Flor",
      "img.flower.alt": "Flor: original à esquerda, HD à direita",
      "img.crystalBlade": "Lâmina de cristal",
      "img.crystalBlade.alt": "Lâmina de cristal: original à esquerda, HD à direita",
      "img.engraving": "Gravura de arma",
      "img.engraving.alt": "Gravura de arma: original à esquerda, HD à direita",
      "img.blade": "Lâmina e cabo",
      "img.blade.alt": "Lâmina e cabo: original à esquerda, HD à direita",
      "img.instHome": "Início: situação do mod, aviso de nova versão e o botão Jogar",
      "img.instHome.alt": "Tela inicial do instalador do GW2 Enhanced, com dados de exemplo",
      "img.instPackages": "Escolha o pacote: completo, só tradução, texturas HD ou gráficos",
      "img.instPackages.alt": "Escolha de pacote no instalador do GW2 Enhanced",
      "img.instInstall": "Baixa e instala, conferindo que o arquivo é oficial",
      "img.instInstall.alt": "Instalador do GW2 Enhanced baixando uma versão",
      "img.instSettings": "Configurações: pasta do jogo, atualizações e opções de inicialização",
      "img.instSettings.alt": "Configurações do instalador do GW2 Enhanced",
    },
    en: {
      "err.already_voted": "You have already voted on this translation.",
      "err.not_votable": "This translation can't receive votes yet.",
      "err.turnstile_failed": "We couldn't confirm that you're human. Please try again.",
      "err.turnstile_required": "Confirm that you're human before sending.",
      "err.turnstile_not_configured": "Verification is unavailable right now.",
      "err.rate_limited": "Too many actions in a short time. Wait a moment and try again.",
      "err.banned": "Your access to sending has been blocked.",
      "err.markup": "The translation must keep the game's markup (%str1%, <c=…>, <br>, [s]…) the same as the original.",
      "err.fixups": "The translation uses a [..] tag that isn't in the original (only [s] is allowed).",
      "err.hides_original": "The translation can't contain [null].",
      "err.link": "Links are not allowed.",
      "err.length": "The translation's length is too different from the original.",
      "err.empty": "The translation is empty.",
      "err.too_long": "The translation is too long.",
      "err.control_chars": "The translation contains invisible or control characters.",
      "err.unknown_text": "Text not found.",
      "err.unknown_translation": "Translation not found.",
      "err.bad_page": "This page doesn't exist.",
      "err.bad_limit": "Invalid page size.",
      "err.other": "Something went wrong ({0}). Please try again.",
      "src.ai": "AI",
      "src.argos": "Argos",
      "src.user": "Community",
      "vote.up": "Vote for",
      "vote.down": "Vote against",
      "vote.upTitle": "Good translation",
      "vote.downTitle": "Bad translation",
      "vote.mine": "Your vote",
      "item.source": "Source",
      "item.none": "Not translated yet.",
      "item.correct": "Suggest a correction",
      "item.suggest": "Suggest a translation",
      "item.en": "Original (English)",
      "item.tr": "Translation (Portuguese)",
      "item.alts": "Other translations ({0})",
      "pager.first": "First page",
      "pager.prev": "Previous page",
      "pager.page": "Page {0}",
      "pager.next": "Next page",
      "pager.last": "Last page ({0})",
      "pager.of": "Showing {0}–{1} of {2}",
      "pager.search": "Showing search results {0}–{1}",
      "pager.range": "Showing {0}–{1}",
      "list.emptyPage": "This page is empty.",
      "list.noMatch": "Nothing found.",
      "list.empty": "No texts yet.",
      "jump.invalid": "Enter a page number.",
      "dlg.verifyOff": "Verification is unavailable right now.",
      "dlg.verifyFail": "Verification failed. Reload the page.",
      "vote.titleUp": "Vote for this translation",
      "vote.titleDown": "Vote against this translation",
      "vote.confirm": "Confirm that you're human to cast your vote.",
      "vote.done": "Vote recorded. Thank you!",
      "fix.yours": "Your translation",
      "fix.title": "Suggest a translation",
      "fix.note": "Keep the game's markup the same as the original. Your suggestion is reviewed before it appears.",
      "fix.done": "Suggestion sent for review. Thank you!",
      "fix.same": "Same as the current translation: change the text to send a correction.",
      "fix.breaks": "The original has {0} line break(s) and your translation has {1}: use the same number.",
      "err.line_breaks": "The translation must have as many line breaks as the original.",
      "err.same_as_current": "This suggestion is the same as the current translation (or one already sent).",
      "cmp.before": "Original",
      "cmp.after": "HD",
      "cmp.range": "Compare original and HD",
      "cmp.zoom": "Enlarge",
      "cmp.zoomOf": "Enlarge: {0}",
      "cmp.pick": "Choose the texture",
      "cmp.show": "Show: {0}",
      "img.wall": "Wall",
      "img.wall.alt": "Stone wall texture: original on the left, HD on the right",
      "img.weapon": "Weapon",
      "img.weapon.alt": "Weapon texture: original on the left, HD on the right",
      "img.building": "Building",
      "img.building.alt": "Building texture: original on the left, HD on the right",
      "img.city": "City",
      "img.city.alt": "City texture: original on the left, HD on the right",
      "img.armorLeaves": "Leaf armor",
      "img.armorLeaves.alt": "Leaf armor: original on the left, HD on the right",
      "img.armorTrim": "Armor trim",
      "img.armorTrim.alt": "Armor trim: original on the left, HD on the right",
      "img.creature": "Creature",
      "img.creature.alt": "Creature: original on the left, HD on the right",
      "img.fur": "Fur",
      "img.fur.alt": "Fur: original on the left, HD on the right",
      "img.leaf": "Leaf",
      "img.leaf.alt": "Leaf: original on the left, HD on the right",
      "img.wood": "Wood",
      "img.wood.alt": "Wood: original on the left, HD on the right",
      "img.flower": "Flower",
      "img.flower.alt": "Flower: original on the left, HD on the right",
      "img.crystalBlade": "Crystal blade",
      "img.crystalBlade.alt": "Crystal blade: original on the left, HD on the right",
      "img.engraving": "Weapon engraving",
      "img.engraving.alt": "Weapon engraving: original on the left, HD on the right",
      "img.blade": "Blade and hilt",
      "img.blade.alt": "Blade and hilt: original on the left, HD on the right",
      "img.instHome": "Home: mod status, update notice and the Play button",
      "img.instHome.alt": "GW2 Enhanced installer home screen, with sample data",
      "img.instPackages": "Pick a package: full, translation only, HD textures or graphics",
      "img.instPackages.alt": "Package choice in the GW2 Enhanced installer",
      "img.instInstall": "Downloads and installs, checking the file is official",
      "img.instInstall.alt": "GW2 Enhanced installer downloading a release",
      "img.instSettings": "Settings: game folder, updates and launch options",
      "img.instSettings.alt": "GW2 Enhanced installer settings",

      // index.html (data-i18n / data-i18n-attr)
      "meta.title": "GW2 Enhanced — Guild Wars 2 in Brazilian Portuguese, in HD and with better graphics",
      "meta.desc": "Free mod for Guild Wars 2: the whole game in Brazilian Portuguese, HD textures and better-looking graphics. All in a single mod, without changing any game file.",
      "skip": "Skip to content",
      "nav.aria": "Shortcuts",
      "nav.translation": "Translation",
      "nav.textures": "Textures",
      "nav.texturesHd": "HD textures",
      "nav.graphics": "Graphics",
      "nav.install": "Install",
      "nav.source": "Source code",
      "nav.sections": "Sections",
      "lang.aria": "Language",
      "lang.label": "Language",
      "dl": "Download",
      "hero.kicker": "Free mod for Guild Wars 2",
      "hero.tagline": "The game in Brazilian Portuguese, with HD textures and better-looking graphics. All in a single mod, and no game file is changed.",
      "hero.dl": "Download now",
      "hero.more": "Explore the mod",
      "promise.aria": "In short",
      "promise.1": "No game file is changed",
      "promise.2": "Every feature can be turned on and off",
      "promise.3": "Updates itself, if you want",
      "promise.4": "Free and open source",
      "res.kicker": "One mod, three features",
      "res.title": "All in a single mod",
      "res.lead": "Install once and use what you want. Each feature works alone or together with the others and can be turned on and off at any time. Uninstall it and the game is exactly as it was.",
      "feat.1": "Feature I",
      "feat.2": "Feature II",
      "feat.3": "Feature III",
      "feat.1.title": "Translation",
      "feat.1.text": "The whole game in Brazilian Portuguese, with a dictionary made by the community. Whatever is still missing is translated on the fly, on your own PC.",
      "feat.1.more": "See the translation",
      "feat.2.title": "HD textures",
      "feat.2.text": "The game's own textures, much sharper up close. Same colors and the same hand-painted style: nothing is redrawn.",
      "feat.2.more": "See the comparison",
      "feat.3.title": "Graphics",
      "feat.3.text": "Better lighting, true HDR and modern effects, rendered inside the game itself and light on your computer.",
      "feat.3.more": "See the effects",
      "tr.title": "Translation into Portuguese",
      "tr.lead": "Quests, dialogue, items, skills, the interface and even the Trading Post, the Gem Store and the Wizard's Vault: the whole game in Brazilian Portuguese.",
      "tr.p1.title": "Community dictionary",
      "tr.p1.text": "The mod already ships with about 205,000 texts translated, reviewed and voted on by the players themselves, here on this site. A new version with the best translations comes out every day.",
      "tr.p2.title": "Translates on your PC",
      "tr.p2.text": "Whatever the dictionary doesn't have yet is translated on the fly by a translation model that runs on your computer, offline, on the processor or the graphics card.",
      "tr.p3.title": "Help, if you want",
      "tr.p3.text": "Only with your permission, the mod sends the texts it finds to improve the dictionary. Nothing goes straight in: everything is checked and voted on by the community first.",
      "tr.p4.title": "Always the official version",
      "tr.p4.text": "Every new version of the translation is checked by the mod, to make sure it really came from this site before it is used.",
      "tr.privacy.title": "Your privacy",
      "tr.privacy.text": "{0} If you agree to help (the mod asks the first time), it sends only the original English text and the translations made on your PC. The Trading Post, Gem Store and Wizard's Vault pages are not sent either. You can change your mind at any time, in the mod's panel. This site does not store your IP address.",
      "tr.privacy.bold": "Chat between players is never read or sent.",
      "tech": "Technical details",
      "tr.tech.1": "The mod swaps the text in memory, by each text's number, at the moment the game is about to show it. {0} is never changed, and with the game in another language everything that has a translation appears in Portuguese.",
      "tr.tech.2": "The translation doesn't have to fit in the space of the English text. The game's protected texts are translated as soon as the server releases their key.",
      "tr.tech.3": "Every dictionary version published here is signed (Ed25519), and the mod checks the signature before using it.",
      "tr.tech.4": "The local translator is an OPUS-MT model running on CTranslate2, in a separate process: on the processor, on NVIDIA cards (CUDA) or on any DirectX 12 card (DirectML). Map dialogue is translated before it appears.",
      "tr.tech.5": "Everything that is sent goes through rules (game markup, links, length). An English text is only accepted when different players send the same one or when the automatic review recognizes it, and every translation is reviewed before it can receive votes.",
      "tr.tech.6": "Instead of the IP, the site stores only a code derived from it (HMAC-SHA256 with a secret key), used to limit abuse and to allow one vote per person. Votes and suggestions require Cloudflare's anti-bot check (Turnstile).",
      "stats.kicker": "Community progress",
      "stats.title": "The dictionary so far",
      "stats.translated": "of texts translated",
      "stats.perfect": "approved by the community",
      "stats.texts": "known texts",
      "stats.downloads": "mod downloads",
      "dict.kicker": "Original and translation side by side",
      "dict.title": "Vote and suggest",
      "dict.hint": "Compare the original English text with the translation, vote for the best ones and suggest corrections. Each person gets one vote per translation, and suggestions are checked before they appear.",
      "sort.aria": "Sort",
      "sort.recent": "Newest",
      "sort.oldest": "Oldest",
      "sort.best": "Best",
      "sort.worst": "Worst",
      "dict.search": "Search",
      "dict.placeholder": "Search in English or Portuguese…",
      "dict.perPage": "Per page",
      "pager.aria": "Pages",
      "dict.jump": "Go to page",
      "dict.go": "Go",
      "tex.title": "HD textures",
      "tex.lead": "Guild Wars 2's own textures, upscaled and much sharper up close. The game still looks like itself: same colors, same hand-painted style.",
      "tex.cmp.kicker": "Original and HD",
      "tex.cmp.title": "Drag to compare",
      "tex.cmp.hint": "On the left, the game's original texture; on the right, the same texture in HD. Pick another texture below.",
      "tex.p1.title": "The original style",
      "tex.p1.text": "Nothing is redrawn, recolored or swapped for a “realistic” look. The art is still ArenaNet's; the pack only makes it sharper.",
      "tex.p2.title": "Super-resolution AI, not generative",
      "tex.p2.text": "The textures were upscaled by a super-resolution AI model (UltraSharp 4x). It only takes the original texture, has no prompt and creates no new content: it rebuilds edges and fine detail that ordinary upscaling would blur.",
      "tex.p3.title": "Your character in 4x",
      "tex.p3.text": "Scenery, creatures, weapons and armor get twice the resolution, and your own character four times as much.",
      "tex.p4.title": "Pack on Nexus Mods",
      "tex.p4.text": "The HD pack is large (about 95 GB) and is hosted on {0}: one main pack and two optional ones with even more detail. Just extract it into the game folder.",
      "tex.p4.dl": "Download the textures on Nexus Mods →",
      "tex.tech.1": "When the game is about to read a texture from {0}, the mod hands it the HD version from the pack, if there is one; otherwise, the original. {1} is not changed.",
      "tex.tech.2": "Of the game's roughly 432,000 textures, about 231,000 go into the pack. Each one is classified by what uses it (character, weapons and armor, scenery, creatures, effects, terrain) and gets a target size. Interface, cinematics and textures that are small or already large stay as they are.",
      "tex.tech.3": "Color textures go through {0} (super-resolution with the ESRGAN architecture, image to image, running locally on the GPU). Normal maps and masks don't go through any model: they are resized with Lanczos.",
      "tex.tech.4": "Every result goes back to the same GPU format the game uses (BC1/BC3/BC5/BC7), and the pack is stored in a few large files with an index. Nothing is converted while you play.",
      "tex.tech.5": "The mod looks for the {0} folder inside the game folder, or the folder you choose in the panel.",
      "tex.credit": "Upscaled with {0} by Kim2091, under {1}: the HD pack is free and non-commercial.",
      "gfx.title": "Better graphics",
      "gfx.lead": "Better lighting, true HDR and modern effects, rendered inside the game itself, not as a filter over the finished image.",
      "gfx.p1.title": "Light on your PC",
      "gfx.p1.text": "The effects run on your graphics card along with the game, with no AI needed. Performance barely changes.",
      "gfx.p2.title": "True HDR",
      "gfx.p2.text": "On HDR monitors, the glow of spells, the sun and fire goes beyond white without blowing out the image.",
      "gfx.p3.title": "Interface untouched",
      "gfx.p3.text": "The effects apply only to the game world: the interface stays sharp, and nothing the game shows gets hidden.",
      "gfx.p4.title": "Your way",
      "gfx.p4.text": "Every effect has its own settings in the mod's panel, inside the game, and can be turned on or off.",
      "fx.kicker": "What's included",
      "fx.title": "Effects",
      "fx.hdr": "Real HDR",
      "fx.hdr.d": "brightness beyond white",
      "fx.bloom": "Bloom",
      "fx.bloom.d": "the glow of lights",
      "fx.ao": "Ambient occlusion",
      "fx.ao.d": "shadows in corners and crevices",
      "fx.refl": "Reflections",
      "fx.refl.d": "surfaces that reflect the scene",
      "fx.rays": "Light shafts",
      "fx.rays.d": "light streaming through the scene",
      "fx.contact": "Contact shadows",
      "fx.contact.d": "where objects touch",
      "fx.lens": "Lens effects",
      "fx.lens.d": "like a real camera",
      "fx.color": "Color grading",
      "fx.color.d": "fine-tuning the colors",
      "fx.photo": "Photo mode",
      "fx.photo.d": "for screenshots",
      "gfx.cmp.kicker": "Before and after",
      "gfx.cmp.title": "In game",
      "gfx.cmp.empty": "Before and after screenshots, with and without the effects, coming soon.",
      "gfx.tech.1": "The mod follows the steps the game (Direct3D 11) uses to draw each frame and recognizes them by their shaders. On loading screens, character select and cinematics, the effects turn off.",
      "gfx.tech.2": "The game draws the scene with 8 bits per color and clips everything at white. The mod replaces those intermediate images with 16-bit floating point, so brightness goes past white without blowing out.",
      "gfx.tech.3": "The effects slot in between the game's own steps and use its depth buffer, so they respect the scene's geometry. The interface is drawn afterwards and is not affected.",
      "gfx.tech.4": "With Windows HDR on, the image is output in scRGB (or HDR10, in testing), up to your monitor's peak. In SDR, the result is clipped as before.",
      "inst.kicker": "Installation",
      "inst.title": "Get started in three steps",
      "inst.s1.title": "Download and open the installer",
      "inst.s1.text": "With the game closed. It finds Guild Wars 2 on its own, whether on Steam or from the ArenaNet installer, and installs the mod.",
      "inst.s2.title": "Open the game",
      "inst.s2.text": "The mod starts with the game. {0}+{1} opens the panel, with a tab for each feature.",
      "inst.s3.title": "Choose what to use",
      "inst.s3.text": "Turn each feature on and off whenever you want. For the HD textures, also download the {0}.",
      "inst.s3.link": "pack on Nexus Mods",
      "inst.upd.title": "Automatic updates",
      "inst.upd.text": "When a new version comes out, the mod lets you know in game and asks whether you want to update. Before installing, it checks that the update is official. Your settings and translations stay as they are.",
      "inst.uninst.title": "Uninstall",
      "inst.uninst.text": "Open the installer and choose {0}. The game goes back to exactly the original.",
      "inst.uninst.word": "Uninstall",
      "inst.shots.kicker": "The installer",
      "inst.shots.title": "Installs, updates and starts the game",
      "inst.tech.1": "The mod is a single file, {0}, placed next to {1}. If another mod already uses that name, the installer warns and asks before replacing it (and keeps it, to put back on uninstall). Deleting the file restores the original game.",
      "inst.tech.2": "There are also standalone packages: only the translation (with the translator on the processor, on NVIDIA cards or on any DirectX 12 card), only the textures, only the graphics, or everything together.",
      "inst.tech.3": "Every version on GitHub comes with the SHA-256 checksum of all files and a provenance attestation. Automatic updates are only installed if the downloaded file matches the checksum.",
      "band.title": "Ready to play in Portuguese?",
      "band.text": "Free and open source. The button opens the latest version on GitHub: installer for Windows, AppImage and .deb for Linux. Every version is listed in the {0}, and developers can read the {1}.",
      "band.releases": "GitHub releases",
      "band.source": "source code",
      "foot.1": "{0} It is not made, endorsed, supported by or affiliated with ArenaNet, LLC or NCSOFT Corporation. It only changes what your computer displays: it sends nothing to the game servers and gives no one an advantage. Third-party programs that connect to the game are used {1}; read {2} and the game's User Agreement before using it.",
      "foot.1.bold": "GW2 Enhanced is an unofficial fan project.",
      "foot.1.risk": "at your own risk",
      "foot.1.policy": "ArenaNet's policy on third-party programs",
      "foot.2": "© ArenaNet, LLC. All rights reserved. NCSOFT, the NC logo, ArenaNet, Guild Wars, Guild Wars 2 and all associated logos and designs are trademarks or registered trademarks of NCSOFT Corporation. All other trademarks are the property of their respective owners.",
      "foot.3": "Code under the MIT license · Cinzel, EB Garamond and Barlow Condensed fonts under the SIL Open Font License · {0}",
      "foot.3.source": "Source code on GitHub",
      "dlg.cancel": "Cancel",
      "dlg.send": "Send",
      "lb.aria": "Enlarged image",
      "lb.close": "Close",
    },
  };

  /** The saved choice, else the first of the browser's languages that is Portuguese or English; English otherwise. */
  function pickLang() {
    try { const s = localStorage.getItem(LANG_KEY); if (LANGS.includes(s)) return s; } catch (_) { /* storage unavailable */ }
    const prefs = navigator.languages && navigator.languages.length ? navigator.languages : [navigator.language || ""];
    for (const l of prefs) {
      const b = String(l).toLowerCase();
      if (b === "pt" || b.startsWith("pt-")) return "pt-BR";
      if (b === "en" || b.startsWith("en-")) return "en";
    }
    return "en";
  }
  let ui = pickLang();

  /** The string `key` in the page's language, with {0}, {1}… replaced by `args`. */
  function t(key, ...args) {
    const d = STR[ui], v = Object.prototype.hasOwnProperty.call(d, key) ? d[key] : STR["pt-BR"][key];
    if (typeof v !== "string") return key;
    return args.length ? v.replace(/\{(\d+)\}/g, (m, i) => (i < args.length ? String(args[i]) : m)) : v;
  }
  const has = (key) => Object.prototype.hasOwnProperty.call(STR[ui], key) || Object.prototype.hasOwnProperty.call(STR["pt-BR"], key);

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
    const en = STR[ui] === STR.en;
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

  // Parts built by this script redraw their texts through these when the language changes.
  const relabel = [];

  function setLang(l, save) {
    if (!LANGS.includes(l)) return;
    const changed = l !== ui;
    ui = l;
    if (save) { try { localStorage.setItem(LANG_KEY, l); } catch (_) { /* storage unavailable */ } }
    document.documentElement.lang = l;
    translatePage();
    for (const b of document.querySelectorAll("[data-lang]")) b.setAttribute("aria-pressed", String(b.dataset.lang === l));
    if (changed) for (const f of relabel) f();
  }

  function setupLang() {
    for (const b of document.querySelectorAll("[data-lang]")) {
      b.hidden = false;
      b.addEventListener("click", () => setLang(b.dataset.lang, true));
    }
    setLang(ui, false);
  }

  // Comparison images. To add one: put the file in backend/site/img/, list it
  // in backend/src/images.rs, and add an entry here plus its caption and alt
  // text ("img.<key>", "img.<key>.alt") to STR in both languages. `gallery`
  // picks the section ("texturas" or "graficos"). Optional `before`/`after`
  // (STR keys) put labels on the left and right halves of a side-by-side
  // image that has none. `plain` shows a single screenshot, with no before/after slider.
  const GALLERY = [
    { gallery: "texturas", src: "/img/hd-wall.jpg", key: "wall" },
    { gallery: "texturas", src: "/img/hd-weapon.jpg", key: "weapon" },
    { gallery: "texturas", src: "/img/hd-building.jpg", key: "building" },
    { gallery: "texturas", src: "/img/hd-city.jpg", key: "city" },
    { gallery: "texturas", src: "/img/hd-armor-leaves.jpg", key: "armorLeaves" },
    { gallery: "texturas", src: "/img/hd-armor-trim.jpg", key: "armorTrim" },
    { gallery: "texturas", src: "/img/hd-creature.jpg", key: "creature" },
    { gallery: "texturas", src: "/img/hd-fur.jpg", key: "fur" },
    { gallery: "texturas", src: "/img/hd-leaf.jpg", key: "leaf" },
    { gallery: "texturas", src: "/img/hd-wood.jpg", key: "wood" },
    { gallery: "texturas", src: "/img/hd-flower.jpg", key: "flower" },
    { gallery: "texturas", src: "/img/hd-crystal-blade.jpg", key: "crystalBlade" },
    { gallery: "texturas", src: "/img/hd-engraving.jpg", key: "engraving" },
    { gallery: "texturas", src: "/img/hd-blade.jpg", key: "blade" },
    { gallery: "instalador", src: "/img/installer-home.webp", key: "instHome", plain: true },
    { gallery: "instalador", src: "/img/installer-packages.webp", key: "instPackages", plain: true },
    { gallery: "instalador", src: "/img/installer-install.webp", key: "instInstall", plain: true },
    { gallery: "instalador", src: "/img/installer-settings.webp", key: "instSettings", plain: true },
  ];
  const caption = (g) => t("img." + g.key);
  const altText = (g) => t("img." + g.key + ".alt");

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
    const tst = $("#toast");
    tst.textContent = msg;
    tst.hidden = false;
    clearTimeout(toast.timer);
    toast.timer = setTimeout(() => { tst.hidden = true; }, 4000);
  }

  async function api(path, opts) {
    const r = await fetch(path, Object.assign({ credentials: "same-origin" }, opts));
    let body = null;
    try { body = await r.json(); } catch (_) { /* not JSON */ }
    if (!r.ok) {
      const code = body && typeof body.error === "string" ? body.error.split(":")[0] : "";
      const err = new Error(code);
      err.code = code;
      err.status = r.status;
      throw err;
    }
    return body;
  }

  /** The message for an error, in the current language: API error codes are looked up, anything else keeps its own. */
  function errorText(e) {
    if (e && typeof e.status === "number") return /^[a-z_]+$/.test(e.code) && has("err." + e.code) ? t("err." + e.code) : t("err.other", e.status);
    return e && e.message ? e.message : t("err.other", "?");
  }

  const pct = (x) => (typeof x === "number" ? x.toLocaleString(ui, { maximumFractionDigits: 1 }) + "%" : "–");
  const num = (x) => (typeof x === "number" ? x.toLocaleString(ui) : "–");

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
    const step = (tm) => {
      const k = Math.min(1, (tm - t0) / dur);
      node.textContent = fmt(k < 1 ? target * (1 - Math.pow(1 - k, 3)) : target);
      if (k < 1) requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  }

  async function loadStats() {
    try {
      const s = await api("/v1/stats?lang=" + encodeURIComponent(DICT_LANG));
      const int = (x) => num(typeof x === "number" ? Math.round(x) : x);
      const fields = [["#s-translated", s.translated_pct, pct], ["#s-perfect", s.perfect_pct, pct], ["#s-texts", s.texts, int], ["#s-downloads", s.downloads, int]];
      let shown = false;
      whenVisible($("#metricas"), () => {
        shown = true;
        for (const [id, v, fmt] of fields) countUp($(id), v, fmt);
        const w = Math.max(0, Math.min(100, Number(s.translated_pct) || 0));
        $("#s-bar").style.width = w + "%";
      });
      // The formatters read the current language, so a running count-up switches by itself; a finished one is rewritten.
      relabel.push(() => { if (shown) for (const [id, v, fmt] of fields) $(id).textContent = fmt(v); });
    } catch (_) { /* metrics are optional */ }
  }

  // The votes cast from this browser (translation id -> 1 / -1), so a voted item stays marked after the list is fetched
  // again or the page is reloaded. Storage may be unavailable (private mode): then it lasts as long as the page.
  const VOTED_KEY = "gw2e-voted";
  const voted = (() => {
    try { const v = JSON.parse(localStorage.getItem(VOTED_KEY) || "{}"); return v && typeof v === "object" ? v : {}; } catch (_) { return {}; }
  })();
  // The counts a vote answered with: list pages are cached at the edge for a few minutes, so a reload can bring counts
  // from before the vote; the larger of the two is shown.
  const COUNTS_KEY = "gw2e-vote-counts";
  const votedCounts = (() => {
    try { const v = JSON.parse(localStorage.getItem(COUNTS_KEY) || "{}"); return v && typeof v === "object" ? v : {}; } catch (_) { return {}; }
  })();
  function rememberVote(id, value, counts) {
    voted[id] = value;
    if (counts) votedCounts[id] = counts;
    try {
      localStorage.setItem(VOTED_KEY, JSON.stringify(voted));
      localStorage.setItem(COUNTS_KEY, JSON.stringify(votedCounts));
    } catch (_) { /* storage unavailable */ }
  }
  // Counts to show for a translation: never fewer than this browser's own vote, nor than the vote's answer.
  function shownCounts(tr) {
    let up = tr.up || 0, down = tr.down || 0;
    const c = votedCounts[tr.id];
    if (c) { up = Math.max(up, c.up || 0); down = Math.max(down, c.down || 0); }
    if (voted[tr.id] === 1) up = Math.max(up, 1);
    if (voted[tr.id] === -1) down = Math.max(down, 1);
    return { up, down };
  }

  // Vote buttons. A vote updates its counts in place from the answer and marks the chosen button; nothing else is
  // redrawn, so the list stays where it is.
  function voteButtons(tr) {
    const shown = shownCounts(tr);
    const upN = text("span", "up", "▲ " + num(shown.up));
    const downN = text("span", "down", "▼ " + num(shown.down));
    const up = el("button", { class: "gbtn vote", type: "button", "aria-label": t("vote.up"), title: t("vote.upTitle") }, upN);
    const down = el("button", { class: "gbtn vote", type: "button", "aria-label": t("vote.down"), title: t("vote.downTitle") }, downN);
    const mark = (value) => {
      for (const [b, v] of [[up, 1], [down, -1]]) {
        b.disabled = true;
        b.classList.toggle("voted", v === value);
        if (v === value) { b.setAttribute("aria-pressed", "true"); b.title = t("vote.mine"); }
      }
    };
    const done = (r, value) => {
      const counts = r && typeof r.up === "number" && typeof r.down === "number" ? { up: r.up, down: r.down } : null;
      rememberVote(tr.id, value, counts);
      const s = shownCounts(tr);
      upN.textContent = "▲ " + num(s.up);
      downN.textContent = "▼ " + num(s.down);
      mark(value);
    };
    up.onclick = () => vote(tr.id, 1, done);
    down.onclick = () => vote(tr.id, -1, done);
    if (voted[tr.id] === 1 || voted[tr.id] === -1) mark(voted[tr.id]);
    return [up, down];
  }

  function sourceChip(source) {
    const c = el("span", { class: "chip" });
    c.append(text("span", "", t("item.source")), text("span", "", /^[a-z]+$/.test(source) && has("src." + source) ? t("src." + source) : "?"));
    return c;
  }

  function render(item) {
    const cur = item.current;
    const en = text("div", "txt", item.en);
    en.setAttribute("lang", "en");
    const tr = cur ? text("div", "txt", cur.tr) : text("div", "txt none", t("item.none"));
    if (cur) tr.setAttribute("lang", DICT_LANG);
    const actions = el("div", { class: "actions" });
    if (cur) actions.append(...voteButtons(cur), sourceChip(cur.source));
    actions.append(el("span", { class: "spacer" }), el("button", { class: "gbtn", type: "button", onclick: () => correct(item) }, cur ? t("item.correct") : t("item.suggest")));
    const card = el("article", { class: "item paper" },
      el("div", { class: "pair" },
        el("div", {}, text("div", "label", t("item.en")), en),
        el("div", {}, text("div", "label gold", t("item.tr")), tr)),
      actions);
    if (Array.isArray(item.alternatives) && item.alternatives.length) {
      const d = el("details", {}, text("summary", "", t("item.alts", num(item.alternatives.length))));
      for (const a of item.alternatives) {
        const alt = text("div", "txt", a.tr);
        alt.setAttribute("lang", DICT_LANG);
        d.append(el("div", { class: "alt" }, alt, ...voteButtons(a)));
      }
      card.append(d);
    }
    return card;
  }

  // ---- dictionary list: numbered pages, state in the URL hash ----

  const HASH = "#dicionario";

  /** Reads sort / page / size / search from `#dicionario?…`; anything invalid falls back to the default. */
  function readHash() {
    const h = location.hash;
    const s = { sort: "recent", p: 1, n: 20, q: "" };
    if (!h.startsWith(HASH + "?")) return null;
    const u = new URLSearchParams(h.slice(HASH.length + 1));
    if (SORTS.includes(u.get("sort"))) s.sort = u.get("sort");
    const p = Number(u.get("p"));
    if (Number.isInteger(p) && p >= 1 && p <= 1e6) s.p = p;
    const n = Number(u.get("n"));
    if (SIZES.includes(n)) s.n = n;
    s.q = (u.get("q") || "").trim().slice(0, 100);
    return s;
  }

  function hashFor(s) {
    const u = new URLSearchParams({ sort: s.sort, p: String(s.p), n: String(s.n) });
    if (s.q) u.set("q", s.q);
    return HASH + "?" + u.toString();
  }

  /** Moves the list to `next` (merged into the current state). `replace` keeps it out of the history (typing). */
  function go(next, replace) {
    const s = Object.assign({ sort: state.sort, p: state.p, n: state.n, q: state.q }, next);
    const h = hashFor(s);
    if (replace) { history.replaceState(null, "", h); apply(s, false); }
    else if (location.hash !== h) location.hash = h; // hashchange does the rest
    else apply(s, true);
  }

  function apply(s, scroll) {
    Object.assign(state, s);
    for (const b of document.querySelectorAll(".tabs button")) b.setAttribute("aria-pressed", String(b.dataset.sort === state.sort));
    $("#pg-size").value = String(state.n);
    if ($("#q").value.trim() !== state.q) $("#q").value = state.q;
    load(scroll);
  }

  const lastPage = () => (typeof state.total === "number" ? Math.max(1, Math.ceil(state.total / state.n)) : null);

  /** Page numbers to show: first, last and two around the current one, with gaps as null. */
  function pageList(cur, last) {
    const w = window.innerWidth, r = w < 400 ? 0 : w < 600 ? 1 : 2; // fewer neighbours on phones, so the pager fits one line
    const near = [];
    for (let p = cur - r; p <= cur + r; p++) near.push(p);
    const want = new Set([1, last, ...near].filter((p) => p >= 1 && p <= last));
    const out = [];
    let prev = 0;
    for (const p of [...want].sort((a, b) => a - b)) {
      if (p - prev > 2) out.push(null); else if (p - prev === 2) out.push(prev + 1);
      out.push(p);
      prev = p;
    }
    return out;
  }

  function renderPager(count) {
    const nav = $("#pager"), cur = state.p, last = lastPage();
    // A search has no total: number the pages seen so far, plus the next one when there is more.
    const end = last != null ? last : cur + (state.more ? 1 : 0);
    const link = (label, p, aria, cls) => {
      const off = p < 1 || p > end || p === cur;
      const b = el("button", { class: "pg " + (cls || ""), type: "button", "aria-label": aria }, label);
      if (p === cur && !cls) { b.setAttribute("aria-current", "page"); b.classList.add("on"); }
      if (off) b.disabled = true; else b.addEventListener("click", () => go({ p }));
      return b;
    };
    const kids = [link("«", 1, t("pager.first"), "edge far"), link("‹", cur - 1, t("pager.prev"), "edge")];
    for (const p of pageList(cur, Math.max(end, cur))) {
      kids.push(p == null ? text("span", "pg-gap", "…") : link(num(p), p, t("pager.page", num(p))));
    }
    kids.push(link("›", cur + 1, t("pager.next"), "edge"));
    if (last != null) kids.push(link("»", last, t("pager.last", num(last)), "edge far"));
    nav.replaceChildren(...kids);
    nav.hidden = end <= 1 && cur === 1;
    $("#pg-jump").hidden = last == null || last <= 1;
    $("#pg-input").max = last != null ? String(last) : "";
    const from = (cur - 1) * state.n + 1, to = from + count - 1;
    $("#pg-info").textContent = !count ? ""
      : typeof state.total === "number" ? t("pager.of", num(from), num(to), num(state.total))
        : state.q ? t("pager.search", num(from), num(to)) : t("pager.range", num(from), num(to));
  }

  /** Draws the last answer of the list (or its error) and the pager. */
  function drawList() {
    const list = $("#list");
    if (state.err) {
      list.replaceChildren(text("div", "msg", errorText(state.err)));
      renderPager(0);
      return;
    }
    const items = state.items;
    if (!items) return;
    // Revalidation without a full redraw: a card whose data (and language) did not change keeps its node (scroll
    // position, an open "other translations", focus and vote marks stay); only new or changed cards are built.
    const old = new Map();
    for (const c of list.children) if (c.dataset && c.dataset.sig) old.set(c.dataset.sig, c);
    list.replaceChildren(...items.map((item) => {
      const sig = ui + " " + JSON.stringify(item);
      const keep = old.get(sig);
      if (keep) return keep;
      const card = render(item);
      card.dataset.sig = sig;
      return card;
    }));
    if (!items.length) {
      list.append(text("div", "msg", state.p > 1 ? t("list.emptyPage") : state.q ? t("list.noMatch") : t("list.empty")));
    }
    renderPager(items.length);
  }

  async function load(scroll) {
    const seq = ++state.seq; // a newer request wins over a slower older one
    const list = $("#list");
    list.setAttribute("aria-busy", "true");
    list.classList.add("busy");
    if (scroll) $("#dicionario").scrollIntoView({ behavior: REDUCE ? "auto" : "smooth", block: "start" });
    const p = new URLSearchParams({ lang: DICT_LANG, sort: state.sort, page: String(state.p), limit: String(state.n) });
    if (state.q) p.set("q", state.q);
    try {
      const r = await api("/v1/texts?" + p.toString());
      if (seq !== state.seq) return;
      state.total = typeof r.total === "number" ? r.total : null;
      state.more = r.more === true || typeof r.next_cursor === "string";
      state.items = r.items;
      state.err = null;
      drawList();
    } catch (e) {
      if (seq !== state.seq) return;
      state.err = e;
      state.items = null;
      state.more = false;
      drawList();
    } finally {
      if (seq === state.seq) { list.removeAttribute("aria-busy"); list.classList.remove("busy"); }
    }
  }

  function setupList() {
    for (const b of document.querySelectorAll(".tabs button")) {
      b.addEventListener("click", () => go({ sort: b.dataset.sort, p: 1 }));
    }
    $("#pg-size").addEventListener("change", (e) => {
      const n = Number(e.target.value);
      // Keep the first row in view: the page that holds it at the new size.
      if (SIZES.includes(n)) go({ n, p: Math.floor(((state.p - 1) * state.n) / n) + 1 });
    });
    let qTimer;
    $("#q").addEventListener("input", (e) => {
      clearTimeout(qTimer);
      qTimer = setTimeout(() => go({ q: e.target.value.trim().slice(0, 100), p: 1 }, true), 300);
    });
    $("#pg-jump").addEventListener("submit", (e) => {
      e.preventDefault();
      const last = lastPage(), v = Number($("#pg-input").value);
      if (!Number.isInteger(v) || v < 1) { toast(t("jump.invalid")); return; }
      go({ p: last != null ? Math.min(v, last) : v });
      $("#pg-input").value = "";
    });
    window.addEventListener("hashchange", () => { const s = readHash(); if (s) apply(s, true); });
    relabel.push(drawList);
    const s = readHash();
    apply(s || { sort: "recent", p: 1, n: 20, q: "" }, !!s);
  }

  // One dialog for every write: shows the Turnstile widget, sends when confirmed. The dialog is modal, so the language
  // cannot change while it is open.
  let widget = null;
  function ask(title, bodyNodes, needsInput, send) {
    const dlg = $("#dlg"), ok = $("#dlg-ok"), note = $("#dlg-note");
    $("#dlg-title").textContent = title;
    $("#dlg-body").replaceChildren(...bodyNodes);
    note.textContent = SITEKEY ? "" : t("dlg.verifyOff");
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
        note.textContent = errorText(e);
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
        language: ui === "en" ? "en" : "pt-br",
        callback: (tk) => { token = tk; refresh(); },
        "expired-callback": () => { token = null; refresh(); },
        "error-callback": () => { note.textContent = t("dlg.verifyFail"); },
      });
    };
    if (SITEKEY) mount(0);
  }

  function vote(id, value, done) {
    ask(value > 0 ? t("vote.titleUp") : t("vote.titleDown"),
      [text("p", "note", t("vote.confirm"))], null,
      async (token) => {
        let r;
        try {
          r = await api("/v1/votes", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ translation_id: id, value, turnstile: token }) });
        } catch (e) {
          // Voted before (another visit, another browser on the same network): mark it and close, it is not an error.
          if (e.code === "already_voted") { if (done) done(null, value); toast(errorText(e)); return; }
          throw e;
        }
        if (done) done(r, value);
        toast(t("vote.done"));
      });
  }

  function correct(item) {
    const ta = el("textarea", { "aria-label": t("fix.yours"), maxlength: 8000, lang: DICT_LANG });
    ta.value = item.current ? item.current.tr : "";
    const en = text("p", "txt", item.en);
    en.setAttribute("lang", "en");
    // Spacing alone does not make a new translation (the server compares the same way).
    const words = (s) => (s || "").trim().split(/\s+/).join(" ");
    const same = () => item.current != null && words(ta.value) === words(item.current.tr);
    // The game lays the text out by its line breaks: a correction keeps exactly as many as the original.
    const breaks = (s) => ((s || "").replace(/\r\n?/g, "\n").match(/\n/g) || []).length;
    const want = breaks(item.en);
    const breaksOk = () => breaks(ta.value) === want;
    const hint = text("p", "note warn", "");
    hint.hidden = true;
    ta.addEventListener("input", () => {
      const msg = same() ? t("fix.same") : !breaksOk() ? t("fix.breaks", num(want), num(breaks(ta.value))) : "";
      hint.textContent = msg;
      hint.hidden = !msg;
    });
    ask(t("fix.title"), [
      text("div", "label", t("item.en")), en,
      text("div", "label gold", t("fix.yours")), ta, hint,
      text("p", "note", t("fix.note")),
    ], () => ta.value.trim() !== "" && !same() && breaksOk(),
    async (token) => {
      await api("/v1/corrections", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ text_key: item.text_key, lang: DICT_LANG, tr: ta.value, turnstile: token }) });
      toast(t("fix.done"));
    });
  }

  // A gallery is a before/after slider (each image holds the original on its left half and the HD version on its
  // right half) plus a row of thumbnails that pick the image. The zoom button opens the whole image in the lightbox.
  function viewer(shots, openLightbox) {
    let current = null, touched = false, dragging = false;
    const base = el("img", { alt: "", decoding: "async" });
    const hd = el("img", { alt: "", decoding: "async", "aria-hidden": "true" });
    const left = text("span", "badge left", "");
    const right = text("span", "badge right", "");
    const range = el("input", { class: "cmp-range", type: "range", min: 0, max: 100, step: 1, value: 50 });
    const cmp = el("div", { class: "cmp" }, base, el("div", { class: "cmp-hd" }, hd),
      el("span", { class: "cmp-line", "aria-hidden": "true" }, el("span", { class: "cmp-knob" })), left, right, range);
    const cap = text("figcaption", "cmp-cap", "");
    const zoom = el("button", { class: "gbtn", type: "button", onclick: () => openLightbox(current) });
    const thumbs = el("div", { class: "thumbs", role: "group" });

    // The two halves are separated by a 4 px gap: scale the image so one half fills the frame, and shift the HD copy
    // past the gap.
    base.addEventListener("load", () => {
      if (current && current.plain) { base.style.width = ""; return; }
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

    // Every text of the viewer, in the current language.
    const labels = () => {
      const g = current;
      base.alt = altText(g);
      left.textContent = t(g.before || "cmp.before");
      right.textContent = t(g.after || "cmp.after");
      cap.textContent = caption(g);
      range.setAttribute("aria-label", t("cmp.range"));
      zoom.textContent = t("cmp.zoom");
      zoom.setAttribute("aria-label", t("cmp.zoomOf", caption(g)));
      thumbs.setAttribute("aria-label", t("cmp.pick"));
      for (const b of thumbs.children) {
        const s = shots.find((x) => x.src === b.dataset.src);
        b.setAttribute("aria-label", t("cmp.show", caption(s)));
        b.lastChild.textContent = caption(s);
      }
    };
    const show = (g, animate) => {
      current = g;
      base.src = g.src; hd.src = g.src;
      cmp.classList.toggle("plain", !!g.plain);
      range.disabled = !!g.plain;
      labels();
      for (const b of thumbs.children) b.setAttribute("aria-pressed", String(b.dataset.src === g.src));
      if (animate && !REDUCE) { cmp.classList.remove("cmp-swap"); void cmp.offsetWidth; cmp.classList.add("cmp-swap"); }
    };
    for (const g of shots) {
      const b = el("button", { class: "thumb", type: "button", "aria-pressed": "false", onclick: () => show(g, true) },
        el("img", { src: g.src, alt: "", loading: "lazy", decoding: "async" }), text("span", "", ""));
      b.dataset.src = g.src;
      thumbs.append(b);
    }
    show(shots[0], false);
    setPos(50);
    relabel.push(labels);

    // A short sweep the first time the slider is seen, so it is clear that it can be dragged.
    if (!REDUCE) {
      whenVisible(cmp, () => {
        const t0 = performance.now(), dur = 1800;
        const step = (tm) => {
          if (touched) return;
          const k = Math.min(1, (tm - t0) / dur);
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
    let shown = null;
    $("#lb-close").addEventListener("click", () => lb.close());
    lb.addEventListener("click", (e) => { if (e.target === lb) lb.close(); });
    const lbLabels = () => { if (shown) { lbImg.alt = altText(shown); lbCap.textContent = caption(shown); } };
    const openLightbox = (g) => {
      if (!g) return;
      shown = g;
      lbImg.src = g.src;
      lbLabels();
      lb.showModal();
    };
    relabel.push(lbLabels);
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
    setupLang();
    renderGalleries();
    setupReveal();
    setupNav();
    setupList();
    loadStats();
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init); else init();
})();
