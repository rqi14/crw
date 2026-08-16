#!/usr/bin/env bash
# Mechanical guard against CLI subcommand doc drift.
#
# Ground truth is the `Commands` enum in crates/crw-cli/src/main.rs: every
# variant there is a real, visible `crw <subcommand>`. Docs reference
# subcommands two ways: single-backtick code spans, e.g. `crw scrape`, and,
# far more commonly in this corpus, as the leading command of a line inside a
# fenced code block, e.g. a bash fence containing `crw crawl https://...`.
#
# Two failure modes:
#   1. a doc names `crw <word>` as a real command and no such subcommand exists
#      (a typo, or a command that was renamed/removed);
#   2. a real subcommand is never named anywhere in the docs (undocumented).
#
# A line is exempted from failure mode 1 when it explicitly says the command
# does NOT exist (e.g. "there is no separate `crw parse` subcommand"): that
# is the doc correctly describing an absence, not a false claim.
#
# Skips gracefully (does not fail CI) if crates/crw-cli/src/main.rs is absent,
# since command drift has nothing to check against without it.
#
# Portable: bash + python3.
set -euo pipefail

cd "${CHECK_REPO_ROOT:-$(dirname "$0")/..}"

MAIN_RS="crates/crw-cli/src/main.rs"

if [ ! -f "$MAIN_RS" ]; then
  echo "skip: ${MAIN_RS} not found, cannot derive real CLI commands" >&2
  exit 0
fi

python3 - "$MAIN_RS" <<'PY'
import re
import sys
from pathlib import Path

main_rs = Path(sys.argv[1])
text = main_rs.read_text()

m = re.search(r"enum Commands \{(.*?)\n\}", text, re.DOTALL)
if not m:
    print(f"error: could not find 'enum Commands' in {main_rs}")
    sys.exit(2)

# Each real variant: `    Scrape(commands::scrape::ScrapeArgs),` -> "scrape".
# clap's default rename for a single-word PascalCase variant is a lowercase
# kebab-case name; every variant here is one word, so lowercasing is exact.
real = {
    v.group(1).lower()
    for v in re.finditer(r"^\s{4}(\w+)\(commands::", m.group(1), re.MULTILINE)
}
if not real:
    print("error: parsed zero Commands variants, regex is out of sync with main.rs")
    sys.exit(2)

# Doc corpus: every markdown/txt file outside build output and vendored trees.
roots = ["docs", "skills", "mcp", "README.md", "README.zh-CN.md", "AGENTS.md"]
files = []
for root in roots:
    p = Path(root)
    if p.is_file():
        files.append(p)
    elif p.is_dir():
        files += list(p.rglob("*.md"))
        files += list(p.rglob("*.txt"))

NEGATIONS = (
    "there is no",
    "no separate",
    "not a subcommand",
    "no such command",
    "does not exist",
    "no equivalent",
    "not implemented",
)

CMD_RE = re.compile(r"`crw ([a-z][a-z0-9-]*)")
# A fenced-code-block line invoking crw as the leading command, optionally
# after a `$ ` shell prompt. Anchored to the start of the (stripped) line on
# purpose: that is what keeps a shell comment (`# ...`), a mid-pipe usage
# (`cat x | crw ...`), or a line-continuation argument (`  --format json`)
# from being mistaken for a command claim. The trailing `(?=\s|$)` (rather than
# `\b`) additionally rejects the bare-URL scrape shorthand `crw https://...`:
# without it, "https" from the URL scheme reads as a phantom subcommand.
FENCE_CMD_RE = re.compile(r"^\s*(?:\$\s+)?crw\s+([a-z][a-z0-9-]*)(?=\s|$)")

documented = set()
phantom = []  # (file, line_no, word)

for f in files:
    try:
        lines = f.read_text().splitlines()
    except (UnicodeDecodeError, OSError):
        continue
    in_fence = False
    for i, line in enumerate(lines, start=1):
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            continue
        words = set(CMD_RE.findall(line))
        if in_fence:
            m = FENCE_CMD_RE.match(line)
            if m:
                words.add(m.group(1))
        low = line.lower()
        negated = any(n in low for n in NEGATIONS)
        for word in words:
            if word in real:
                documented.add(word)
            elif not negated:
                phantom.append((str(f), i, word))

undocumented = sorted(real - documented)

problems = []
if phantom:
    problems.append("documented commands that do not exist:")
    for f, i, word in phantom:
        problems.append(f"    {f}:{i}: `crw {word}` (no such subcommand)")
if undocumented:
    problems.append("real subcommands never mentioned in the docs:")
    for word in undocumented:
        problems.append(f"    crw {word}")

if problems:
    print(f"FAIL: CLI command doc has drifted from {main_rs}'s Commands enum:\n")
    for p in problems:
        print(f"  {p}")
    sys.exit(1)

print(f"ok: {len(real)} CLI subcommands, all documented and no phantom commands")
PY
