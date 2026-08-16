#!/usr/bin/env bash
# Mechanical guard against the crate-graph doc going stale.
#
# docs/docs/architecture.md hand-documents the workspace crate list and the
# intra-workspace dependency edges (its "Crate Structure" fenced block). That
# table has drifted from reality before: a crate gets added/removed, or a
# dependency edge changes, and nobody remembers to update the prose diagram.
#
# Ground truth is `cargo metadata --no-deps`, not the doc. This script derives
# the real crate set, the real intra-workspace NORMAL-dependency edges
# (dev-dependencies excluded, so crw-server's self path dev-dependency does not
# show up as a self-loop), and the real binary targets (targets[].kind), and
# fails if the doc's fenced block or binaries prose disagrees. The binary
# check exists because crate/dependency drift is not the only way this doc
# goes stale: it can also assert a wrong binary count, or claim a crate is
# "library only" when cargo actually auto-discovers a `[[bin]]` for it from a
# `src/main.rs` the doc's author didn't expect.
#
# Portable: bash + python3 (present on ubuntu-latest and macOS).
set -euo pipefail

cd "${CHECK_REPO_ROOT:-$(dirname "$0")/..}"

DOC="docs/docs/architecture.md"

if [ ! -f "$DOC" ]; then
  echo "error: ${DOC} not found" >&2
  exit 2
fi

METADATA_JSON="$(mktemp -t crw-crate-graph-metadata.XXXXXX.json)"
trap 'rm -f "$METADATA_JSON"' EXIT

cargo metadata --no-deps --format-version=1 > "$METADATA_JSON"

python3 - "$DOC" "$METADATA_JSON" <<'PY'
import json
import re
import sys

doc_path, metadata_path = sys.argv[1], sys.argv[2]

with open(metadata_path) as f:
    meta = json.load(f)

packages = meta["packages"]
real_names = {p["name"] for p in packages}

# Real binary-producing crates: any crate with a target whose kind list
# contains "bin". Cargo auto-discovers a binary from src/main.rs even with no
# explicit [[bin]] section, which is exactly the drift class this check
# exists to catch (a crate the doc calls "library only" that actually ships
# a main.rs).
real_bin_crates = {
    p["name"] for p in packages if any("bin" in t["kind"] for t in p["targets"])
}

# Real edges: normal (non-dev, non-build) dependencies whose target is also a
# workspace crate. This excludes crw-server's self dev-dependency, which cargo
# metadata otherwise reports as a self-loop.
real_edges = {}
for p in packages:
    deps = sorted(
        {
            d["name"]
            for d in p["dependencies"]
            if d.get("kind") is None and d["name"] in real_names
        }
    )
    real_edges[p["name"]] = deps

doc_text = open(doc_path).read()

problems = []

# Prose crate count: "crw is a Rust workspace with N crates"
m = re.search(r"Rust workspace with (\d+) crates", doc_text)
if not m:
    problems.append("could not find the 'Rust workspace with N crates' sentence")
elif int(m.group(1)) != len(real_names):
    problems.append(
        f"doc says {m.group(1)} crates, cargo metadata reports {len(real_names)}"
    )

# The fenced code block under "## Crate Structure".
block = re.search(r"## Crate Structure\n.*?```\n(.*?)```", doc_text, re.DOTALL)
if not block:
    problems.append("could not find the fenced crate-structure block")
    doc_edges = {}
else:
    doc_edges = {}
    for line in block.group(1).splitlines():
        line = line.strip()
        if not line:
            continue
        m = re.match(r"^(crw-[a-z-]+)\s+(.*?)(?:→\s*(.+))?$", line)
        if not m:
            problems.append(f"could not parse crate-structure line: {line!r}")
            continue
        name, desc, deps_raw = m.group(1), m.group(2), m.group(3)
        deps = sorted(d.strip() for d in deps_raw.split(",")) if deps_raw else []
        doc_edges[name] = deps

        # The description column claims a crate ships no binary. Verify that
        # against cargo metadata's real targets, not just trust the prose:
        # cargo auto-discovers a `[[bin]]` from a bare src/main.rs even with
        # no explicit [[bin]] section in Cargo.toml.
        desc_low = desc.lower()
        if ("library only" in desc_low or "no [[bin]]" in desc_low) and name in real_bin_crates:
            problems.append(
                f"{name}: doc's structure block calls it library-only / "
                "no [[bin]], but cargo metadata reports a real bin target for it"
            )

    doc_names = set(doc_edges)
    missing_from_doc = real_names - doc_names
    extra_in_doc = doc_names - real_names
    if missing_from_doc:
        problems.append(
            f"crates missing from the doc's structure block: {sorted(missing_from_doc)}"
        )
    if extra_in_doc:
        problems.append(
            f"crates in the doc's structure block that do not exist: {sorted(extra_in_doc)}"
        )

    for name in sorted(doc_names & real_names):
        if doc_edges[name] != real_edges[name]:
            problems.append(
                f"{name}: doc says deps={doc_edges[name]}, "
                f"cargo metadata says deps={real_edges[name]}"
            )

# Prose binaries claim, e.g. "The workspace produces three binaries: `crw`
# (from `crw-cli`), `crw-mcp` (from `crw-mcp`), and `crw-browse` (from
# `crw-browse`, ...)." Verify both the count and the exact crate set against
# cargo metadata's real bin targets.
NUMBER_WORDS = {
    "one": 1,
    "two": 2,
    "three": 3,
    "four": 4,
    "five": 5,
    "six": 6,
    "seven": 7,
    "eight": 8,
    "nine": 9,
    "ten": 10,
}
m = re.search(r"workspace\s+produces\s+(\w+)\s+binaries?", doc_text, re.IGNORECASE)
if not m:
    problems.append("could not find the 'workspace produces N binaries' sentence")
else:
    word = m.group(1).lower()
    claimed_count = NUMBER_WORDS.get(word, int(word) if word.isdigit() else None)
    if claimed_count is None:
        problems.append(f"could not parse the binaries count word {m.group(1)!r}")
    elif claimed_count != len(real_bin_crates):
        problems.append(
            f"doc says '{word}' binaries, cargo metadata reports "
            f"{len(real_bin_crates)}: {sorted(real_bin_crates)}"
        )

    # Crate names cited via "from `crate-name`" in the same paragraph as the
    # count, so a mismatched or incomplete list is caught even when the
    # count itself happens to still be right.
    para_start = doc_text.rfind("\n\n", 0, m.start()) + 2
    para_end = doc_text.find("\n\n", m.end())
    if para_end == -1:
        para_end = len(doc_text)
    paragraph = doc_text[para_start:para_end]
    claimed_crates = set(re.findall(r"from\s+`([a-z0-9-]+)`", paragraph))
    missing = real_bin_crates - claimed_crates
    extra = claimed_crates - real_bin_crates
    if missing:
        problems.append(
            f"binaries sentence never names crate(s) that really do produce "
            f"a binary: {sorted(missing)}"
        )
    if extra:
        problems.append(
            f"binaries sentence names crate(s) that do not produce a "
            f"binary: {sorted(extra)}"
        )

if problems:
    print(f"FAIL: {doc_path} has drifted from the real crate graph:\n")
    for p in problems:
        print(f"  - {p}")
    print(
        f"\nFix: update {doc_path} to match `cargo metadata --no-deps` "
        "(normal, non-dev, intra-workspace dependency edges)."
    )
    sys.exit(1)

print(
    f"ok: {doc_path} matches cargo metadata "
    f"({len(real_names)} crates, {sum(len(v) for v in real_edges.values())} edges)"
)
PY
