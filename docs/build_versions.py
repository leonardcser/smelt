#!/usr/bin/env python3
"""Build the docs for main and every release tag into one versioned site.

Each version lands in `site/<tag>/` and main in `site/main/`; `site/latest/`
holds the newest release. Every version shows the landing page from main.
`versions.json` feeds the theme's version selector in
the format mike writes. The root, and any link from before the site had
versions, redirects to `latest/`.

Run from anywhere: `python3 docs/build_versions.py [OUT]` writes to
`docs/site` unless given another folder, and `--serve [PORT]` previews the
site on port 8000 from a temporary folder. It needs the release tags, so CI
checks out the full history. For writing docs, `zensical serve` reloads on
every edit but has no version selector.
"""

import argparse
import functools
import http.server
import io
import json
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

DOCS = Path(__file__).resolve().parent
REPO = DOCS.parent
SITE = DOCS / "site"
URL = "https://smeltagent.dev/"
# The first release whose docs the selector offers.
FIRST = (0, 6, 0)
# What a docs build reads from a checkout.
TREES = ("docs", "runtime")
SELECTOR_CSS = "stylesheets/versions.css"
# The landing page, relative to `docs/`. It belongs to the site, not to any one
# version, so every build takes it from main, with the replays it embeds.
HOME = ("overrides/home.html", "docs/index.md", "docs/javascripts/home.js")


def git(*args: str) -> bytes:
    return subprocess.run(["git", *args], cwd=REPO, check=True, capture_output=True).stdout


def release_tags() -> list[str]:
    """Release tags from `FIRST` on, newest first; prereleases have no docs of their own."""
    tags = git("tag", "--list", "v*", "--sort=-v:refname").decode().split()
    return [
        tag
        for tag in tags
        if re.fullmatch(r"v\d+\.\d+\.\d+", tag)
        and tuple(int(part) for part in tag[1:].split(".")) >= FIRST
    ]


def home_files() -> list[str]:
    """`HOME` and the replays the landing page embeds."""
    replays = re.findall(r'data-replay="(assets/replays/[^"]+)"', (DOCS / HOME[0]).read_text())
    return [*HOME, *(f"docs/{replay}" for replay in replays)]


def build(source: Path, name: str, strict: bool) -> bool:
    """Build the docs tree at `source` into `site/<name>/`."""
    config = source / "zensical.toml"
    text = re.sub(r"^site_url = .*$", f'site_url = "{URL}{name}/"', config.read_text(), count=1, flags=re.M)
    # The selector belongs to the versioned site, not to any one version, so
    # every build gets it here and `zensical serve` goes without.
    text += '\n[project.extra.version]\nprovider = "mike"\n'
    text = re.sub(r"^extra_css = \[", f'extra_css = ["{SELECTOR_CSS}", ', text, count=1, flags=re.M)
    shutil.copy(DOCS / "versions.css", source / "docs" / SELECTOR_CSS)
    for path in home_files():
        (source / path).parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(DOCS / path, source / path)
    config.write_text(text)
    command = ["zensical", "build", "--strict"] if strict else ["zensical", "build"]
    if subprocess.run(command, cwd=source).returncode != 0:
        print(f"warning: the docs for {name} did not build, leaving them out", file=sys.stderr)
        return False
    shutil.copytree(source / "site", SITE / name)
    return True


def checkout(ref: str | None, into: Path) -> Path:
    """The docs of `ref`, or of the working tree when `ref` is None, with the
    runtime they take snippets from."""
    if ref is None:
        for tree in TREES:
            shutil.copytree(REPO / tree, into / tree, ignore=shutil.ignore_patterns("site"))
    else:
        with tarfile.open(fileobj=io.BytesIO(git("archive", ref, *TREES))) as archive:
            archive.extractall(into, filter="data")
    return into / "docs"


def redirect(to: str) -> str:
    return (
        '<!doctype html><meta charset="utf-8"><title>smelt docs</title>'
        f'<meta http-equiv="refresh" content="0; url={to}">'
        f'<script>location.replace("{to}" + location.hash)</script>'
    )


def main() -> None:
    if SITE.exists():
        shutil.rmtree(SITE)
    SITE.mkdir()
    tags = release_tags()
    versions = []
    with tempfile.TemporaryDirectory() as tmp:
        if not build(checkout(None, Path(tmp) / "main"), "main", strict=True):
            sys.exit("the docs on main must build")
        versions.append({"version": "main", "title": "main (unreleased)", "aliases": []})
        latest = None
        for tag in tags:
            # The newest release is built as `latest`, so its pages name that
            # as their canonical address, and copied to its tag.
            name = "latest" if latest is None else tag
            if not build(checkout(tag, Path(tmp) / tag), name, strict=False):
                continue
            if latest is None:
                latest = tag
                shutil.copytree(SITE / "latest", SITE / tag)
            versions.append(
                {"version": tag, "title": tag, "aliases": ["latest"] if tag == latest else []}
            )
    (SITE / "versions.json").write_text(json.dumps(versions, indent=2) + "\n")

    home = "latest/" if latest else "main/"
    (SITE / "index.html").write_text(redirect(home))
    # Links from before the site had versions point at the newest release.
    known = json.dumps(["latest", *(v["version"] for v in versions)])
    (SITE / "404.html").write_text(
        '<!doctype html><meta charset="utf-8"><title>Page not found</title>'
        f"<script>const known = {known};"
        'const path = location.pathname.replace(/^\\//, "");'
        'if (!known.includes(path.split("/")[0]))'
        f' location.replace("/{home}" + path + location.search + location.hash);</script>'
        f'<p>Page not found. <a href="/{home}">Go to the docs</a>.</p>'
    )
    print(f"built {', '.join(v['version'] for v in versions)} into {SITE}")


def serve(port: int) -> None:
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=SITE)
    with http.server.ThreadingHTTPServer(("0.0.0.0", port), handler) as server:
        print(f"serving the versioned docs on http://localhost:{port}/")
        server.serve_forever()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Build the docs for main and every release.")
    parser.add_argument("out", nargs="?", type=Path, help="where to write the site (default docs/site)")
    parser.add_argument(
        "--serve", type=int, nargs="?", const=8000, metavar="PORT", help="then serve it (default port 8000)"
    )
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as preview:
        # A preview stays out of docs/site, which `zensical serve` owns.
        SITE = (args.out or (Path(preview) / "site" if args.serve else SITE)).resolve()
        main()
        if args.serve:
            try:
                serve(args.serve)
            except KeyboardInterrupt:
                pass
