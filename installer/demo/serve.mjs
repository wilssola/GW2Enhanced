// Serves installer/ui in a plain browser with the demo Tauri bridge (tauri-mock.js) loaded before app.js.
// Usage: node installer/demo/serve.mjs [port]   then open http://localhost:4173/  (add ?hold or ?linux, see tauri-mock.js)
// Development aid only; nothing here is bundled with the app.
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join, normalize, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const ui = join(here, "..", "ui");
const port = Number(process.argv[2]) || 4173;
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".woff2": "font/woff2", ".txt": "text/plain" };

createServer(async (req, res) => {
  const url = new URL(req.url, "http://localhost");
  let rel = decodeURIComponent(url.pathname);
  if (rel === "/") rel = "/index.html";
  const file = rel === "/tauri-mock.js" ? join(here, "tauri-mock.js") : normalize(join(ui, rel));
  if (!file.startsWith(ui) && !file.startsWith(here)) return res.writeHead(403).end();
  try {
    let body = await readFile(file);
    if (rel === "/index.html") {
      body = body.toString().replace('<script src="app.js" defer></script>',
        '<script src="tauri-mock.js"></script>\n<script src="app.js" defer></script>');
    }
    res.writeHead(200, { "content-type": TYPES[extname(file)] || "application/octet-stream" }).end(body);
  } catch {
    res.writeHead(404).end();
  }
}).listen(port, () => console.log(`installer demo: http://localhost:${port}/`));
