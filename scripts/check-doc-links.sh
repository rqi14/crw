#!/usr/bin/env bash
# Mechanical guard against broken internal links.
#
# Two link shapes are checked, both under docs/docs/ and skills/:
#   1. Relative markdown links: an explicit `[x](../COMPATIBILITY-firecrawl.md)`
#      or `[x](./recipe-batch.md)`, AND a bare `[x](crawling.md)` or
#      `[x](crawling.md#parameters)` with no ./ or ../ prefix (the common style
#      in this corpus). All three resolve to a real file on disk relative to
#      the linking file's own directory.
#   2. Docs-site links, e.g. `[x](/docs/architecture)`: the slug after
#      `/docs/` must be a real `slug:` entry in docs/site.config.js, the
#      sidebar that drives the docs site's routing.
#
# A link that is neither shape (an external http(s) URL, a same-file `#anchor`
# link, a bare slug link like `[x](configuration)` with no .md extension, or
# an absolute non-/docs path) is out of scope and skipped.
#
# Skips gracefully (does not fail CI) if docs/site.config.js is absent.
#
# Portable: bash + python3.
set -euo pipefail

cd "${CHECK_REPO_ROOT:-$(dirname "$0")/..}"

SITE_CONFIG="docs/site.config.js"

if [ ! -f "$SITE_CONFIG" ]; then
  echo "skip: ${SITE_CONFIG} not found, cannot derive the real docs slug set" >&2
  exit 0
fi

python3 - "$SITE_CONFIG" <<'PY'
import re
import sys
from pathlib import Path

site_config = Path(sys.argv[1])
slugs = set(re.findall(r'slug:\s*"([a-zA-Z0-9-]+)"', site_config.read_text()))
if not slugs:
    print(f"error: parsed zero slugs from {site_config}, regex is out of sync")
    sys.exit(2)

roots = [p for p in (Path("docs/docs"), Path("skills")) if p.is_dir()]
files = []
for root in roots:
    files += list(root.rglob("*.md"))

LINK_RE = re.compile(r"\]\(([^)#\s]+)(?:#[^)]*)?\)")

broken_relative = []  # (file, line_no, target)
broken_slug = []  # (file, line_no, slug)

for f in files:
    lines = f.read_text().splitlines()
    for i, line in enumerate(lines, start=1):
        for target in LINK_RE.findall(line):
            if target.startswith(("http://", "https://", "mailto:")):
                continue
            if target.startswith("/docs/"):
                slug = target[len("/docs/") :].split("/")[0]
                if slug not in slugs:
                    broken_slug.append((str(f), i, target))
                continue
            if target.startswith(("./", "../")):
                dest = (f.parent / target).resolve()
                if not dest.exists():
                    broken_relative.append((str(f), i, target))
                continue
            if not target.startswith("/") and target.endswith(".md"):
                # Bare relative markdown link, e.g. `crawling.md` or
                # `crawling.md#parameters` (the anchor is already stripped by
                # LINK_RE above). Resolve exactly like an explicit ./ or ../
                # target: relative to the linking file's own directory.
                dest = (f.parent / target).resolve()
                if not dest.exists():
                    broken_relative.append((str(f), i, target))
                continue
            # Any other absolute path (/foo), bare slug link (e.g.
            # `(configuration)` matching a site.config.js slug, no .md
            # extension), or non-doc scheme is out of scope for this check.

problems = []
if broken_relative:
    problems.append("relative links pointing at a file that does not exist:")
    for f, i, target in broken_relative:
        problems.append(f"    {f}:{i}: {target}")
if broken_slug:
    problems.append("links to a /docs/ slug not in docs/site.config.js:")
    for f, i, target in broken_slug:
        problems.append(f"    {f}:{i}: {target}")

if problems:
    print("FAIL: broken internal link(s) found:\n")
    for p in problems:
        print(f"  {p}")
    sys.exit(1)

print(f"ok: {len(files)} files scanned, {len(slugs)} known slugs, no broken internal links")
PY
