#!/usr/bin/env python3
"""Score an arXivQA retrieval run: mean recall over ground-truth arXiv ids.

Recall for one question is the fraction of its ground-truth papers the run
surfaced. Extra ids do not cost anything, which is why the harness optimizes
for coverage. The reported score is the mean over all 191 questions, and a
question the run never answered counts as 0.

The ground truth is alphaXiv's, not ours, and is downloaded on demand:
https://github.com/alphaXiv/retriever-sandbox (db/scripts/all_train.json)

Usage:
    python3 score.py --selfcheck                       # no network, proves the scorer
    python3 score.py --results runs/2026-06-20-hosted-191.jsonl
    python3 score.py --results my_run.jsonl --test /path/to/all_train.json

Run file: one JSON object per line, {"qid": ..., "query": ..., "arxiv_ids": [...]}.
Questions are matched on the query text; qid is informational.
"""
import argparse
import json
import os
import re
import sys
import urllib.request

DATASET_URL = (
    "https://raw.githubusercontent.com/alphaXiv/retriever-sandbox/main/db/scripts/all_train.json"
)
CACHE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "arxivqa_all_train.json")


def norm_id(raw):
    """arXiv id to its comparable form: no prefix, no version, no URL tail."""
    s = str(raw).strip().lower()
    s = re.sub(r"^arxiv:\s*", "", s)
    s = s.replace("abs/", "").replace("pdf/", "")
    s = s.rsplit("/", 1)[-1]
    return re.sub(r"v\d+$", "", s).strip()


def norm_query(q):
    return re.sub(r"\s+", " ", str(q).strip().lower())


def load_test(path):
    if path:
        with open(path) as f:
            return json.load(f)
    try:
        with open(CACHE) as f:
            return json.load(f)
    except FileNotFoundError:
        pass
    with urllib.request.urlopen(DATASET_URL, timeout=60) as r:
        raw = r.read()
    with open(CACHE, "wb") as f:
        f.write(raw)
    return json.loads(raw)


def score(rows, test):
    """(per-question rows, mean recall). Unanswered questions score 0."""
    truth = {norm_query(x["query"]): [norm_id(p) for p in x["papers"]] for x in test}
    out = []
    seen = set()
    for r in rows:
        qn = norm_query(r["query"])
        seen.add(qn)
        gt = truth.get(qn)
        if gt is None:
            out.append((r.get("qid", "?"), None, 0, 0, r["query"]))
            continue
        found = {norm_id(x) for x in r.get("arxiv_ids", [])}
        hit = sum(1 for t in gt if t in found)
        out.append((r.get("qid", "?"), hit / len(gt) if gt else 0.0, hit, len(gt), r["query"]))
    for x in test:
        if norm_query(x["query"]) not in seen:
            out.append(("(none)", 0.0, 0, len(x["papers"]), "[UNANSWERED] " + x["query"]))
    scored = [r[1] for r in out if r[1] is not None]
    return out, (sum(scored) / len(scored) if scored else 0.0)


def selfcheck():
    """Prove the scorer with zero setup: no network, no run file, no API key."""
    test = [
        {"query": "  Q one ", "papers": ["2211.17192", "1706.03762"]},
        {"query": "q two", "papers": ["2005.14165"]},
        {"query": "q three, never answered", "papers": ["2404.00001"]},
    ]
    rows = [
        {"qid": "A", "query": "q ONE", "arxiv_ids": ["arXiv:2211.17192v3", "9999.99999"]},
        {"qid": "B", "query": "q two", "arxiv_ids": ["https://arxiv.org/abs/2005.14165"]},
    ]
    _, mean = score(rows, test)
    expected = (0.5 + 1.0 + 0.0) / 3
    assert abs(mean - expected) < 1e-9, f"scorer broken: {mean} != {expected}"
    print(f"selfcheck ok: mean recall {mean:.4f} (expected {expected:.4f})")
    print("  case 1: version suffix and arXiv: prefix normalized, 1 of 2 found -> 0.5")
    print("  case 2: full URL normalized to a bare id -> 1.0")
    print("  case 3: question absent from the run file -> 0.0, not skipped")
    return 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", help="run file, one JSON object per line")
    ap.add_argument("--test", help="ground truth json; downloaded from alphaXiv if omitted")
    ap.add_argument("--selfcheck", action="store_true", help="verify the scorer offline")
    ap.add_argument("--quiet", action="store_true", help="print the mean only")
    args = ap.parse_args()

    if args.selfcheck:
        return selfcheck()
    if not args.results:
        ap.error("--results is required unless --selfcheck is given")

    with open(args.results) as f:
        rows = [json.loads(ln) for ln in f if ln.strip()]
    out, mean = score(rows, load_test(args.test))

    if not args.quiet:
        print(f"{'qid':<8} {'recall':>7}  {'found/gt':>9}  query")
        print("-" * 92)
        for qid, recall, hit, total, query in out:
            if recall is None:
                print(f"{qid:<8} {'NO-MATCH':>7}             {query[:55]}")
            else:
                print(f"{qid:<8} {recall:>7.3f}  {hit:>4}/{total:<4}  {query[:55]}")
        print("-" * 92)
    n = len([r for r in out if r[1] is not None])
    print(f"mean recall over {n} questions: {mean:.4f}  ({mean * 100:.1f}%)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
