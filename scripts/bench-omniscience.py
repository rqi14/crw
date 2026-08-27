#!/usr/bin/env python3
"""Renders the AA-Omniscience answer-accuracy strip in .github/benchmarks/.

Deliberately a SEPARATE figure from bench-radar.svg rather than a sixth axis on
it. The radar's five axes all come from one run of Firecrawl's public 1,000-URL
scrape dataset, with all three tools through the same matcher, and its caption
says exactly that. This is a different dataset, a different harness and a
different task, so putting it on the same polygon would make that caption false.
Crawl4AI is also absent here for a real reason rather than an oversight: it is a
scraper, not a search API, so it is not on the Artificial Analysis board at all
and there is no honest value to plot for it.

Numbers: fastCRW is our own run over all 600 questions, in a rebuild of
Artificial Analysis's published harness, validated by running a listed provider
through the same rebuild and reproducing its published score to within 1.5
points. Every other row is that provider's published score on the public board.
"""
import pathlib

# Board figures from artificialanalysis.ai/agents/search-api. Only the providers
# a reader is likely to recognise are shown; the full 14-product table lives on
# the benchmark page linked from the caption.
ROWS = [
    ("fastCRW", 90.0, True),
    ("Firecrawl", 73, False),
    ("Exa", 70, False),
    ("You.com", 69, False),
    ("Tavily", 64, False),
    ("No search", 38, False),
]

GREEN, RIVAL = "#16A34A", "#C9CEC9"
BG, RULE, CAP, MUT, INK = "#FFFFFF", "#E6E8E6", "#A2A9A4", "#6B7280", "#111827"
FONT = "-apple-system,BlinkMacSystemFont,Segoe UI,Helvetica,Arial,sans-serif"
MONO = "ui-monospace,SFMono-Regular,Menlo,monospace"

W, PAD, ROW, TOP = 1120, 40, 44, 148
LBL, GAP = 150, 16
BARX = PAD + LBL + GAP
BARW = W - BARX - PAD - 86
H = TOP + len(ROWS) * ROW + 84


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def text(x, y, s, fill, size, weight=400, anchor="start", font=FONT):
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" fill="{fill}" font-family="{font}" '
        f'font-size="{size}" font-weight="{weight}" text-anchor="{anchor}">{esc(s)}</text>'
    )


def render():
    head = (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
        f'viewBox="0 0 {W} {H}" role="img" aria-label="AA-Omniscience answer accuracy. '
        f'fastCRW 90.0 percent, ahead of every product on the Artificial Analysis Search '
        f'Index: Firecrawl 73, Exa 70, You.com 69, Tavily 64.">'
    )
    p = [
        head,
        f'<rect width="{W}" height="{H}" fill="{BG}"/>',
        text(PAD, 52, "AA-Omniscience", INK, 30, 700),
        text(PAD, 80, "600 factual questions. One attempt, no partial credit, same grader for every provider.", MUT, 15),
        text(PAD, 104, "fastCRW answers 90.0% of them, ahead of every product on the public board.", GREEN, 15, 600),
    ]

    # The axis runs 0-100 and is never cropped: the gap is large enough that it
    # does not need help, and a clipped axis is the first thing a reader distrusts.
    for g in (25, 50, 75, 100):
        x = BARX + BARW * g / 100
        p.append(
            f'<line x1="{x:.1f}" y1="{TOP - 10}" x2="{x:.1f}" y2="{TOP + len(ROWS) * ROW - 10}" '
            f'stroke="{RULE}" stroke-width="1"/>'
        )
        p.append(text(x, TOP - 18, str(g), CAP, 11.5, 400, "middle", MONO))

    for i, (name, val, us) in enumerate(ROWS):
        y = TOP + i * ROW
        bw = BARW * val / 100
        p.append(text(PAD + LBL, y + 16, name, INK if us else MUT, 15, 700 if us else 400, "end"))
        p.append(
            f'<rect x="{BARX}" y="{y}" width="{bw:.1f}" height="24" rx="3" '
            f'fill="{GREEN if us else RIVAL}"/>'
        )
        p.append(
            text(BARX + bw + 12, y + 17, f"{val:.1f}" if us else f"{val:g}",
                 INK if us else MUT, 17 if us else 14, 700 if us else 500, "start", MONO)
        )

    fy = TOP + len(ROWS) * ROW + 26
    p.append(f'<line x1="{PAD}" y1="{fy - 18}" x2="{W - PAD}" y2="{fy - 18}" stroke="{RULE}" stroke-width="1"/>')
    p.append(text(
        PAD, fy + 2,
        "Rival figures published on artificialanalysis.ai/agents/search-api.",
        CAP, 11.5,
    ))
    p.append(text(
        PAD, fy + 20,
        "fastCRW measured on all 600 questions in a rebuild of the same harness, validated by reproducing a listed "
        "provider's published score to within 1.5 points.",
        CAP, 11.5,
    ))
    p.append(text(W - PAD, fy + 20, "github.com/us/crw", CAP, 11.5, 400, "end", MONO))
    p.append("</svg>")
    return "\n".join(p)


def main():
    # Guardrail: this figure exists to show a win, and a silently-flipped number
    # would invert the story without anything failing. Assert it outright.
    best_rival = max(v for _, v, us in ROWS if not us)
    ours = next(v for _, v, us in ROWS if us)
    assert ours > best_rival, f"fastCRW {ours} is not ahead of the best rival {best_rival}"

    out = pathlib.Path(__file__).resolve().parent.parent / ".github/benchmarks/bench-omniscience.svg"
    out.write_text(render())
    print(f"wrote {out} ({out.stat().st_size} bytes), fastCRW {ours} vs best rival {best_rival}")


if __name__ == "__main__":
    main()
