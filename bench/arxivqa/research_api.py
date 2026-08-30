#!/usr/bin/env python3
"""Deterministic CLI over the fastCRW Research API, used by the arXivQA harness.

The benchmark agent never parses JSON: it calls these subcommands and reads
arXiv ids, one per line. That keeps the measured variable the retrieval
strategy, not the agent's JSON handling.

Endpoints used (all GET, all in the open-core server, see
`crates/crw-server/src/routes/research.rs`):
    /v1/search/research/papers
    /v1/search/research/papers/{id}/similar?mode=references|citers|similar

Configuration, both optional:
    FASTCRW_BASE_URL  default https://api.fastcrw.com; point it at your own
                      `crw serve` to run the whole benchmark self-hosted
    FASTCRW_API_KEY   bearer token; the hosted API requires one, a self-hosted
                      server started without auth does not

Usage:
    python3 research_api.py search "<query>"
    python3 research_api.py cascade-file <file-of-queries>
    python3 research_api.py refs <arxiv_id> [references|citers|similar]
    python3 research_api.py scrape <url>
    python3 research_api.py scrape-text <url>
"""
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from collections import Counter

BASE = os.environ.get("FASTCRW_BASE_URL", "https://api.fastcrw.com").rstrip("/")
KEY = os.environ.get("FASTCRW_API_KEY", "")
HEADERS = {"Authorization": f"Bearer {KEY}"} if KEY else {}
ARXIV = re.compile(r"(\d{4}\.\d{4,5})")


def _norm(x):
    return re.sub(r"v\d+$", "", str(x).strip().lower())


def _request(path, body=None, tries=5, timeout=90):
    """GET, or POST when `body` is given, with backoff on 429 and 5xx."""
    url = BASE + path
    data = json.dumps(body).encode() if body is not None else None
    headers = dict(HEADERS)
    if data is not None:
        headers["Content-Type"] = "application/json"
    for attempt in range(tries):
        try:
            req = urllib.request.Request(url, data=data, headers=headers)
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return json.loads(r.read())
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503, 504) and attempt < tries - 1:
                time.sleep(2 * (2**attempt))
                continue
            return None
        except Exception:
            if attempt < tries - 1:
                time.sleep(2 * (2**attempt))
                continue
            return None
    return None


def _ids(payload):
    """arXiv ids out of a {results:[{ids:{arxiv:[..]}, primaryId}]} response."""
    if not payload:
        return []
    out = []
    for r in payload.get("results", []):
        arxiv = (r.get("ids") or {}).get("arxiv")
        if arxiv:
            out += [_norm(x) for x in arxiv]
        elif str(r.get("primaryId", "")).startswith("arxiv:"):
            out.append(_norm(r["primaryId"].split(":", 1)[1]))
    if not out:
        out = [_norm(x) for x in ARXIV.findall(json.dumps(payload))]
    return list(dict.fromkeys(out))


def search(query, k=40):
    qs = urllib.parse.urlencode({"query": query, "k": k})
    return _ids(_request(f"/v1/search/research/papers?{qs}"))


def refs(arxiv_id, mode="references", k=60):
    qs = urllib.parse.urlencode({"intent": "related work", "mode": mode, "k": k})
    return _ids(_request(f"/v1/search/research/papers/arxiv:{_norm(arxiv_id)}/similar?{qs}"))


def scrape_text(url):
    d = _request("/v1/scrape", {"url": url, "formats": ["markdown"]})
    if not d:
        return ""
    return (d.get("data") or d).get("markdown") or ""


def scrape_ids(url):
    return list(dict.fromkeys(_norm(x) for x in ARXIV.findall(scrape_text(url))))


def cascade(queries, top_anchors=5, cap=80):
    """Search every query, then expand the references of the top anchors.

    Ranking is by how many of the queries surfaced an id, which is why the
    exact-name decomposition in the prompt matters: an id that several
    independent specific queries agree on ranks above a single broad hit.
    """
    freq = Counter()
    for q in queries:
        for i in search(q):
            freq[i] += 1
    expanded = Counter()
    for anchor, _ in freq.most_common(top_anchors):
        for i in refs(anchor):
            if i not in freq:
                expanded[i] += 1
    ranked = [a for a, _ in freq.most_common()] + [a for a, _ in expanded.most_common()]
    return ranked[:cap]


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    cmd = argv[1]
    if cmd == "cascade-file":
        with open(argv[2]) as f:
            out = cascade([ln.strip() for ln in f if ln.strip()])
    elif cmd == "cascade":
        out = cascade(argv[2:])
    elif cmd == "search":
        out = search(argv[2])
    elif cmd == "refs":
        out = refs(argv[2], argv[3] if len(argv) > 3 else "references")
    elif cmd == "scrape":
        out = scrape_ids(argv[2])
    elif cmd == "scrape-text":
        print(scrape_text(argv[2])[:9000])
        return 0
    else:
        print(f"unknown command: {cmd}\n{__doc__}")
        return 2
    print("\n".join(out))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
