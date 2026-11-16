"""Generate detailed release notes from Git history using only the standard library.

Adapted from Zapive's release tooling. Writes the release notes and SHA256SUMS.txt (in `sha256sum` format, which
the in-game updater parses) for the GW2Enhanced-<Package>-<version>.zip assets.

Output layout (Markdown, English):

    ## Breaking changes          (with migration text)
    ## New features / Improvements / Fixes / Security / Other changes
    ## Contributors              (+ full compare link)
    ## Downloads and verification (SHA-256)   (only with --assets-dir)

Everything that is only useful on the GitHub page (the download table and the
verification steps) lives under the SHA-256 heading, so an in-game "what's new"
view can drop it by that heading (Zapive's updater does). Do not rename it
without checking enhanced/src/update.rs.
"""

import argparse
from collections import namedtuple
import hashlib
import os
from pathlib import Path
import re
import subprocess

BREAKING_SECTION = "Breaking changes"
SECTIONS = (
    BREAKING_SECTION,
    "New features",
    "Improvements",
    "Fixes",
    "Security",
    "Other changes",
)
IGNORED_TYPES = {"chore", "ci", "build", "test", "docs", "style", "bump"}
CATEGORIES = {
    "feat": "New features",
    "perf": "Improvements",
    "refactor": "Improvements",
    "fix": "Fixes",
    "security": "Security",
}
# Types whose commit body (first paragraph) is shown under the entry.
DETAILED_TYPES = {"feat", "fix", "perf", "security"}
DETAIL_LIMIT = 400
DOWNLOADS_HEADING = "Downloads and verification (SHA-256)"

CONVENTIONAL = re.compile(
    r"^(?P<type>[a-z]+)(?:\((?P<scope>[^()\r\n]+)\))?(?P<breaking>!)?:\s+(?P<text>\S.*)$"
)
BREAKING = re.compile(r"^BREAKING(?: CHANGE|-CHANGE):\s*\S", re.MULTILINE)
BREAKING_TEXT = re.compile(
    r"^BREAKING(?: CHANGE|-CHANGE):[ \t]*(?P<text>.*?)(?:\n[ \t]*\n|\Z)",
    re.MULTILINE | re.DOTALL,
)
TRAILER = re.compile(
    r"^(?:Co-authored-by|Signed-off-by|Reviewed-by|Acked-by|Fixes|Closes|Resolves|Refs?)\b[:\s]",
    re.IGNORECASE,
)
PR_SUFFIX = re.compile(r"\s*\(#(?P<pr>\d+)\)\s*$")
MERGE_PR = re.compile(r"^Merge pull request #(?P<pr>\d+)\b")
MERGE = re.compile(r"^Merge (?:pull request|branch|remote-tracking branch|tag)\b")
NOREPLY = re.compile(r"^(?:\d+\+)?(?P<login>[A-Za-z0-9-]+)@users\.noreply\.github\.com$")
MENTION = re.compile(r"(?<![\w`/])@(?=[A-Za-z0-9])")

Commit = namedtuple("Commit", "subject body sha author email pr", defaults=("", "", "", "", "", None))


def _coerce(item):
    """Accept bare (subject, body) pairs as well as full Commit records."""
    if isinstance(item, Commit):
        return item
    return Commit(*item)


def _safe(text):
    """Keep GitHub from notifying people through @mentions in commit text."""
    return MENTION.sub("@\u200b", text)


def _first_paragraph(body):
    for paragraph in re.split(r"\n\s*\n", body.strip()):
        paragraph = paragraph.strip()
        if not paragraph or paragraph.startswith("```") or paragraph.startswith("~~~"):
            continue
        if TRAILER.match(paragraph) or BREAKING.match(paragraph):
            continue
        lines = [line for line in paragraph.splitlines() if not TRAILER.match(line.strip())]
        text = " ".join(" ".join(lines).split())
        if not text:
            continue
        if len(text) > DETAIL_LIMIT:
            text = text[:DETAIL_LIMIT].rsplit(" ", 1)[0].rstrip(".,;:") + "…"
        return text
    return ""


def _migration(body):
    match = BREAKING_TEXT.search(body)
    if not match:
        return ""
    return " ".join(match.group("text").split())


def _build_entries(commits):
    entries = {}
    for item in commits:
        commit = _coerce(item)
        subject = commit.subject.strip()
        if not subject or MERGE.match(subject):
            continue
        body = commit.body or ""
        match = CONVENTIONAL.fullmatch(subject)
        breaking = bool(BREAKING.search(body)) or bool(match and match.group("breaking"))
        kind = scope = None
        if match:
            kind = match.group("type")
            if kind in IGNORED_TYPES and not breaking:
                continue
            text = match.group("text")
            scope = (match.group("scope") or "").strip() or None
            section = CATEGORIES.get(kind, "Other changes")
        else:
            text = subject
            section = "Other changes"
        pr = commit.pr
        suffix = PR_SUFFIX.search(text)
        if suffix:
            pr = pr or int(suffix.group("pr"))
            text = text[: suffix.start()].rstrip() or text
        if breaking:
            section = BREAKING_SECTION
        entry = {
            "section": section,
            "text": text,
            "scope": scope,
            "sha": commit.sha,
            "pr": pr,
            "author": (commit.author, commit.email),
            "detail": _first_paragraph(body) if kind in DETAILED_TYPES else "",
            "migration": _migration(body) if breaking else "",
        }
        key = " ".join(text.split()).casefold()
        if key not in entries or (breaking and entries[key]["section"] != BREAKING_SECTION):
            entries[key] = entry
    return list(entries.values())


def _links(entry, repository):
    parts = []
    if repository:
        base = "https://github.com/" + repository
        if entry["pr"]:
            parts.append("[#%d](%s/pull/%d)" % (entry["pr"], base, entry["pr"]))
        if entry["sha"]:
            parts.append("[`%s`](%s/commit/%s)" % (entry["sha"][:7], base, entry["sha"]))
    else:
        if entry["pr"]:
            parts.append("#%d" % entry["pr"])
        if entry["sha"]:
            parts.append("`%s`" % entry["sha"][:7])
    return " (" + ", ".join(parts) + ")" if parts else ""


def _render_entry(entry, repository):
    label = "**%s**: " % entry["scope"] if entry["scope"] else ""
    lines = ["- " + label + _safe(entry["text"]) + _links(entry, repository)]
    if entry["detail"]:
        lines.append("  - " + _safe(entry["detail"]))
    if entry["migration"]:
        lines.append("  - **Migration:** " + _safe(entry["migration"]))
    return "\n".join(lines)


def _contributors(entries):
    people = {}
    for entry in entries:
        name, email = entry["author"]
        if (name or "").strip().endswith("[bot]"):
            continue
        name = re.sub(r"[\[\]*_<>`|]", "", name or "").strip()
        if not name:
            continue
        login = NOREPLY.match(email or "")
        people.setdefault(name.casefold(), (name, login.group("login") if login else None))
    result = []
    for name, login in people.values():
        result.append("[@%s](https://github.com/%s)" % (login, login) if login else name)
    return result


def generate_notes(commits, repository=None, compare=None):
    """Group commits into sections; descriptions are never rewritten.

    `commits` holds Commit records or bare (subject, body) pairs. `repository`
    ("owner/name") turns commit hashes and PR numbers into links; `compare`
    is an optional (previous_tag, tag) pair rendered as the full-changelog link.
    """
    entries = _build_entries(commits)
    blocks = []
    for section in SECTIONS:
        items = [entry for entry in entries if entry["section"] == section]
        if items:
            blocks.append(
                "## " + section + "\n\n" + "\n".join(_render_entry(e, repository) for e in items)
            )
    if not blocks:
        return ""
    footer = []
    people = _contributors(entries)
    if people:
        footer.append("## Contributors\n\n" + "Thanks to " + ", ".join(people) + ".")
    if repository and compare:
        previous, tag = compare
        base = "https://github.com/" + repository
        if previous:
            footer.append(
                "**Full changelog**: [%s...%s](%s/compare/%s...%s)" % (previous, tag, base, previous, tag)
            )
        else:
            footer.append("**Code of this version**: [%s](%s/tree/%s)" % (tag, base, tag))
    if footer:
        # The compare link belongs to the contributors block, not a section of its own.
        blocks.append("\n\n".join(footer))
    return "\n\n".join(blocks) + "\n"


# Package (as in GW2Enhanced-<Package>-<version>.zip, README "Downloads") -> what it has. Order = table order.
PACKAGES = (
    ("All", "All three mods; local translator with every mode (CPU, CUDA, DirectML)"),
    ("Translation", "Translation only; local translator with every mode"),
    ("Translation-CPU", "Translation only; local translator on the processor (the smallest)"),
    ("Translation-CUDA", "Translation only; local translator on NVIDIA cards (CUDA)"),
    ("Translation-DirectML", "Translation only; local translator on any DirectX 12 card (AMD, Intel, NVIDIA)"),
    ("Textures", "HD textures runtime only (the HD pack is a separate download, on Nexus Mods)"),
    ("Graphics", "Graphics only (HDR, bloom, occlusion, reflections...)"),
)
ASSET = re.compile(r"GW2Enhanced-(?P<package>[A-Za-z]+(?:-[A-Za-z]+)?)-(?P<version>\d[^/\\]*)\.zip")


def _platform(name):
    """Map a release asset name to (sort key, package, description) or None."""
    match = ASSET.fullmatch(name)
    if not match:
        return None
    for index, (package, description) in enumerate(PACKAGES):
        if match.group("package") == package:
            return index, package, description
    return None


def _format_size(size):
    value = float(size)
    for unit in ("B", "KiB", "MiB", "GiB"):
        if value < 1024 or unit == "GiB":
            return "%d B" % value if unit == "B" else "%.1f %s" % (value, unit)
        value /= 1024


def checksum_files(assets_dir):
    """Return sorted (name, sha256, size) for the release assets in a directory."""
    files = []
    for path in sorted(Path(assets_dir).iterdir(), key=lambda p: p.name):
        if not path.is_file() or not ASSET.fullmatch(path.name):
            continue
        digest = hashlib.sha256()
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
        files.append((path.name, digest.hexdigest(), path.stat().st_size))
    return files


def checksums_text(files):
    """SHA256SUMS.txt in the `sha256sum` format the in-app updater parses."""
    return "".join("%s  %s\n" % (digest, name) for name, digest, _ in files)


def downloads_section(files, repository=None, tag=None):
    """Download table plus verification steps, as the last release-note section."""
    rows = []
    for name, _, size in files:
        info = _platform(name)
        if info:
            url = "https://github.com/%s/releases/download/%s/%s" % (repository, tag, name) if repository and tag else None
            rows.append((info, "[%s](%s)" % (name, url) if url else "`%s`" % name, size))
    lines = ["## " + DOWNLOADS_HEADING, ""]
    if rows:
        lines += ["| Package | File | Contents | Size |", "| --- | --- | --- | --- |"]
        for (_, package, kind), link, size in sorted(rows, key=lambda r: (r[0][0], r[1])):
            lines.append("| %s | %s | %s | %s |" % (package, link, kind, _format_size(size)))
        lines.append("")
    attest = "gh attestation verify GW2Enhanced-<package>-<version>.zip --repo " + (repository or "<owner>/<repo>")
    lines += [
        "Installing: close the game and extract the zip into the game folder (the one with `Gw2-64.exe`), or run "
        "the `GW2Enhanced-Installer.exe` inside it. Players who already use GW2 Enhanced get this version through "
        "the automatic update, which downloads the same package and checks its SHA-256 before installing.",
        "",
        "### How to check the download",
        "",
        "Download `SHA256SUMS.txt` too and compare:",
        "",
        "```powershell",
        "# Windows (PowerShell): the result must equal the file's line in SHA256SUMS.txt",
        "Get-FileHash .\\GW2Enhanced-<package>-<version>.zip -Algorithm SHA256",
        "```",
        "",
        "```sh",
        "# Linux/macOS/Git Bash (checks only the files you downloaded)",
        "sha256sum --ignore-missing -c SHA256SUMS.txt",
        "```",
        "",
        "Every zip has a provenance attestation (which commit and which GitHub Actions run produced it):",
        "",
        "```sh",
        attest,
        "```",
    ]
    if files:
        lines += ["", "### SHA-256", "", "```", checksums_text(files).rstrip("\n"), "```"]
    return "\n".join(lines) + "\n"


def git(repo, *args):
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        encoding="utf-8",
    ).stdout.strip()


def resolve_range(repo, to_tag="HEAD", from_tag=None):
    """Return (end sha, start sha or None, revision) for the exclusive-lower-bound range."""
    end = git(repo, "rev-parse", "--verify", "--end-of-options", to_tag + "^{commit}")
    if from_tag is None:
        try:
            from_tag = git(repo, "describe", "--tags", "--abbrev=0", "--match", "v*", end + "^")
        except subprocess.CalledProcessError:
            from_tag = None
    if not from_tag:
        return end, None, end
    start = git(repo, "rev-parse", "--verify", "--end-of-options", from_tag + "^{commit}")
    return end, from_tag, start + ".." + end


def previous_tag(repo, to_tag="HEAD", from_tag=None):
    return resolve_range(repo, to_tag, from_tag)[1]


def _pull_requests(repo, revision):
    """Map commit sha -> PR number for commits that came in through a merged PR."""
    mapping = {}
    log = git(repo, "log", "--merges", "--format=%H%x00%P%x00%s%x1e", revision, "--")
    for record in log.split("\x1e"):
        fields = record.strip().split("\0")
        if len(fields) != 3:
            continue
        match = MERGE_PR.match(fields[2])
        parents = fields[1].split()
        if not match or len(parents) < 2:
            continue
        try:
            merged = git(repo, "rev-list", parents[0] + ".." + parents[1]).split()
        except subprocess.CalledProcessError:
            continue
        for sha in merged:
            mapping.setdefault(sha, int(match.group("pr")))
    return mapping


def read_commits(repo, to_tag="HEAD", from_tag=None):
    """Use an exclusive lower bound; without one, discover the previous v* tag."""
    _, _, revision = resolve_range(repo, to_tag, from_tag)
    log = git(
        repo, "log", "--reverse", "--topo-order",
        "--format=%H%x00%an%x00%ae%x00%s%x00%b%x1e", revision, "--",
    )
    prs = _pull_requests(repo, revision)
    commits = []
    for record in log.split("\x1e"):
        fields = record.lstrip("\n").split("\0")
        if len(fields) != 5:
            continue
        sha, author, email, subject, body = fields
        commits.append(Commit(subject.strip(), body, sha, author, email, prs.get(sha)))
    return commits


def repository_from_remote(repo):
    """owner/name from the origin remote, or None."""
    try:
        url = git(repo, "remote", "get-url", "origin")
    except subprocess.CalledProcessError:
        return None
    match = re.search(r"github\.com[:/](?P<slug>[^/\s]+/[^/\s]+?)(?:\.git)?/?$", url)
    return match.group("slug") if match else None


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", default=".")
    parser.add_argument("--from-tag", help="Exclusive lower bound (default: previous v* tag)")
    parser.add_argument("--to-tag", default="HEAD", help="Inclusive upper bound")
    parser.add_argument("--repository", help="owner/name for links (default: GITHUB_REPOSITORY or origin)")
    parser.add_argument("--assets-dir", help="Directory with release assets; adds the downloads section")
    parser.add_argument("--checksums-output", help="Where to write SHA256SUMS.txt (needs --assets-dir)")
    parser.add_argument("--output", required=True)
    args = parser.parse_args()

    repository = args.repository or os.environ.get("GITHUB_REPOSITORY") or repository_from_remote(args.repo)
    _, previous, _ = resolve_range(args.repo, args.to_tag, args.from_tag)
    commits = read_commits(args.repo, args.to_tag, args.from_tag)
    notes = generate_notes(commits, repository, (previous, args.to_tag) if args.to_tag != "HEAD" else None)
    if args.assets_dir:
        files = checksum_files(args.assets_dir)
        if args.checksums_output:
            Path(args.checksums_output).write_text(checksums_text(files), encoding="utf-8", newline="\n")
        notes = (notes + "\n" if notes else "") + downloads_section(files, repository, args.to_tag)
    Path(args.output).write_text(notes, encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
