"""Preview the backend site in a browser without Cloudflare, with sample data.

    python backend/dev/preview.py [--port 8799] [--no-open]

Serves `backend/site/` and fakes the read endpoints (`/v1/stats`, `/v1/texts`). Votes and
corrections are not simulated: the dialog opens but sending shows an error. The captcha
script is removed from the page, so nothing leaves your machine.
"""
import argparse
import json
import mimetypes
import re
import threading
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

SITE = Path(__file__).resolve().parents[1] / "site"

STATS = {"translated_pct": 62.4, "perfect_pct": 18.2, "texts": 48213, "downloads": 1240}
ITEMS = [
    {
        "text_key": "a",
        "en": "Fetch me three Ley-Line Crystals from the Dragonfall caves, and be quick about it.",
        "ids": [1],
        "current": {"id": 11, "tr": "Traga-me três Cristais de Linha Ley das cavernas de Dragonfall, e seja rápido.", "up": 42, "down": 3, "source": "ai"},
        "alternatives": [{"id": 12, "tr": "Busque três Cristais de Linha de Ley nas cavernas de Dragonfall, e depressa.", "up": 5, "down": 9, "source": "user"}],
    },
    {
        "text_key": "b",
        "en": "The Pact will not forget your sacrifice, Commander.",
        "ids": [2],
        "current": {"id": 21, "tr": "O Pacto não esquecerá o seu sacrifício, Comandante.", "up": 128, "down": 2, "source": "user"},
        "alternatives": [],
    },
    {"text_key": "c", "en": "Salvage Master's Kit", "ids": [3], "current": None, "alternatives": []},
]


TOTAL = STATS["texts"]


def texts_page(qs):
    """A page of the fake list: the sample items repeated, numbered, with the same answer shape as the server."""
    page = int((qs.get("page") or ["1"])[0])
    limit = int((qs.get("limit") or ["20"])[0])
    search = bool((qs.get("q") or [""])[0])
    total = 7 if search else TOTAL
    first = (page - 1) * limit
    items = []
    for i in range(first, min(first + limit, total)):
        it = dict(ITEMS[i % len(ITEMS)])
        it["text_key"] = f"{i:032x}"
        it["en"] = f"{it['en']} (#{i + 1})"
        items.append(it)
    more = first + limit < total
    return {
        "items": items, "page": page, "limit": limit, "offset": first, "more": more,
        "total": None if search else total, "next_cursor": str(first + limit) if more else None,
    }


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, code, body, content_type):
        self.send_response(code)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = urlparse(self.path).path
        if path == "/v1/stats":
            return self.reply(200, json.dumps(STATS).encode(), "application/json")
        if path == "/v1/texts":
            return self.reply(200, json.dumps(texts_page(parse_qs(urlparse(self.path).query))).encode(), "application/json")
        file = (SITE / ("index.html" if path == "/" else path.lstrip("/"))).resolve()
        if SITE.resolve() in file.parents and file.is_file():
            data = file.read_bytes()
            if file.name == "index.html":
                data = re.sub(rb'<script src="https://challenges[^>]*></script>', b"", data)
            return self.reply(200, data, mimetypes.guess_type(str(file))[0] or "application/octet-stream")
        self.reply(404, b'{"error":"not_found"}', "application/json")

    def do_POST(self):
        self.reply(503, b'{"error":"preview"}', "application/json")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=8799)
    ap.add_argument("--no-open", action="store_true")
    a = ap.parse_args()
    url = f"http://localhost:{a.port}/"
    print(f"Previewing {SITE}\n{url}   (Ctrl+C to stop)")
    if not a.no_open:
        threading.Timer(0.5, webbrowser.open, args=(url,)).start()
    ThreadingHTTPServer(("127.0.0.1", a.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
