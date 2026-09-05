#!/usr/bin/env python3
"""Refresh the dated project snapshot; a failed/incomplete crawl never replaces it."""
import argparse
from datetime import datetime, timezone
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import re
import tempfile
from urllib.parse import parse_qs, urlencode, urljoin, urlsplit, urlunsplit
from urllib.request import Request, urlopen

BASE = "https://github.com/release-plz/action/network/dependents"
ALIASES = ("UGFja2FnZS0zMDY0NDU2NDU0", "UGFja2FnZS01NTY5MDk1NDUw")
ROOT = Path(__file__).resolve().parents[1]
CACHE = ROOT / "src/data/project-snapshot.json"
CURATED = ROOT / "src/data/curated-projects.json"
VOID = {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"}


class Node:
    def __init__(self, tag="", attrs=(), parent=None):
        self.tag, self.attrs, self.parent, self.children = tag, dict(attrs), parent, []

    def walk(self):
        yield self
        for child in self.children:
            if isinstance(child, Node):
                yield from child.walk()

    def text(self):
        return "".join(child.text() if isinstance(child, Node) else child for child in self.children)


class Document(HTMLParser):
    def __init__(self, html):
        super().__init__(convert_charrefs=True)
        self.root = Node()
        self.current = self.root
        self.feed(html)

    def handle_starttag(self, tag, attrs):
        node = Node(tag, attrs, self.current)
        self.current.children.append(node)
        if tag not in VOID:
            self.current = node

    def handle_endtag(self, tag):
        node = self.current
        while node.parent is not None:
            if node.tag == tag:
                self.current = node.parent
                return
            node = node.parent

    def handle_startendtag(self, tag, attrs):
        self.current.children.append(Node(tag, attrs, self.current))

    def handle_data(self, data):
        self.current.children.append(data)


def number(text):
    value = text.strip().lower().replace(",", "")
    if not re.fullmatch(r"\d+(?:\.\d+)?[km]?", value):
        raise ValueError(f"Unrecognized GitHub count: {value!r}")
    multiplier = {"k": 1000, "m": 1000000}.get(value[-1], 1)
    return int(float(value[:-1] if multiplier != 1 else value) * multiplier)


def page_url(url, alias):
    parts = urlsplit(urljoin(BASE, url))
    if parts.scheme != "https" or parts.netloc != "github.com" or parts.path != urlsplit(BASE).path:
        raise ValueError("Unexpected dependents pagination URL")
    query = parse_qs(parts.query)
    if query.get("package_id", [alias]) != [alias]:
        raise ValueError("Pagination changed the Action package alias")
    query["package_id"], query["dependent_type"] = [alias], ["REPOSITORY"]
    return urlunsplit((parts.scheme, parts.netloc, parts.path, urlencode(sorted((k, v) for k, values in query.items() for v in values)), ""))


def parse_page(html):
    document = Document(html)
    nodes = list(document.root.walk())
    projects, next_url, reported = [], None, None
    for node in nodes:
        if node.attrs.get("data-test-id") == "dg-repo-pkg-dependent":
            links = [child for child in node.walk() if child.tag == "a" and child.attrs.get("data-hovercard-type") == "repository"]
            stars = [child for child in node.walk() if "octicon-star" in child.attrs.get("class", "").split()]
            if len(links) != 1 or len(stars) != 1:
                raise ValueError("Dependent repository markup changed")
            name = links[0].attrs.get("href", "").strip("/")
            if not re.fullmatch(r"[\w.-]+/[\w.-]+", name):
                raise ValueError("Invalid dependent repository name")
            projects.append({"name": name, "url": f"https://github.com/{name}", "stars": number(stars[0].parent.text()), "usage_url": BASE})
        if node.tag == "a":
            text = node.text().strip()
            if text == "Next" and node.attrs.get("href") and node.attrs.get("aria-disabled") != "true":
                if node.parent.attrs.get("data-test-selector") == "pagination":
                    next_url = node.attrs["href"]
            if "selected" in node.attrs.get("class", "").split() and "Repositories" in text:
                match = re.search(r"([\d,.]+[kKmM]?)\s+Repositories", text)
                if match:
                    reported = number(match[1])
    if not projects and reported != 0:
        raise ValueError("No repository rows/count: GitHub may be unavailable or its markup changed")
    return projects, next_url, reported


def fetch(url):
    headers = {"User-Agent": "release-plz-website-snapshot", "Accept": "application/json" if url.startswith("https://api.github.com/") else "text/html"}
    # Public metadata only; tokens are never placed in URLs or log messages.
    if url.startswith("https://api.github.com/") and os.environ.get("GITHUB_TOKEN"):
        headers["Authorization"] = f"Bearer {os.environ['GITHUB_TOKEN']}"
    with urlopen(Request(url, headers=headers), timeout=30) as response:
        return response.read().decode("utf-8")


def crawl(fetch_page=fetch, aliases=ALIASES, max_pages=300):
    repositories, sources, page_counts = {}, [], {}
    for alias in aliases:
        url, seen, observed_in_alias = page_url(BASE, alias), set(), set()
        sources.append(url)
        for _ in range(max_pages):
            if url in seen:
                raise ValueError("Pagination cycle detected")
            seen.add(url)
            projects, next_url, _ = parse_page(fetch_page(url))
            names = {project["name"].casefold() for project in projects}
            if names and names <= observed_in_alias:
                raise ValueError("Pagination repeated repository contents")
            observed_in_alias.update(names)
            for project in projects:
                project["usage_url"] = sources[-1]
                repositories[project["name"].casefold()] = project
            if next_url is None:
                break
            url = page_url(next_url, alias)
        else:
            raise ValueError("Pagination limit reached; refusing to save an incomplete snapshot")
        page_counts[alias] = len(seen)
    return repositories, sources, page_counts


def atomic_write(path, snapshot):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as output:
            temporary = output.name
            json.dump(snapshot, output, indent=2, ensure_ascii=False)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.chmod(temporary, 0o644)
        os.replace(temporary, path)
    finally:
        if temporary and os.path.exists(temporary):
            os.unlink(temporary)


def refresh(cache=CACHE, curated_path=CURATED, fetch_page=fetch, max_pages=300):
    repositories, sources, page_counts = crawl(fetch_page, max_pages=max_pages)
    observed = len(repositories)
    for project in json.loads(curated_path.read_text()):
        metadata = json.loads(fetch_page(f"https://api.github.com/repos/{project['name']}"))
        stars = metadata["stargazers_count"]
        if not isinstance(stars, int) or stars < 0:
            raise ValueError("Invalid repository star count")
        repositories[project["name"].casefold()] = {**project, "stars": stars}
    snapshot = {"updated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                "discovery_complete": True, "repository_count": observed,
                "reported_count": observed, "sources": sources, "source_page_counts": page_counts,
                "projects": sorted(repositories.values(), key=lambda project: (-project["stars"], project["name"].casefold()))}
    atomic_write(cache, snapshot)
    return snapshot


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--max-pages", type=int, default=300)
    args = parser.parse_args()
    try:
        snapshot = refresh(max_pages=args.max_pages)
    except Exception as error:
        # The caller may retain the committed dated cache and continue the build.
        parser.exit(1, f"Project refresh failed; cached snapshot was retained: {type(error).__name__}\n")
    print(f"Saved {snapshot['repository_count']} distinct Action dependents; pages per alias: {snapshot['source_page_counts']}")


if __name__ == "__main__":
    main()
