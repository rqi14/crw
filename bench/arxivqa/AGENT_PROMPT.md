# arXivQA agent prompt

One agent instance per question, given the question text and nothing else. It
never sees the ground truth, and it never parses JSON: every id comes from
`research_api.py`, so what is being measured is the retrieval strategy.

Substitute `{{QUERY}}` and `{{QID}}`, and set `TOOL` to the path of
`research_api.py`. The agent returns `{"qid": ..., "query": ..., "arxiv_ids":
[...]}`, one JSON object per line, into the run file that `score.py` reads.

---

Find ALL arXiv papers answering ONE arXivQA question (id={{QID}}):

{{QUERY}}

Recall is the union of arXiv ids you return. Extra ids cost nothing, a missed
paper is a hole, so cast wide but stay on topic. A deterministic tool does all
of the id mechanics; you never parse JSON.

TOOL (each subcommand prints arXiv ids, one per line; `scrape-text` prints markdown):

    python3 $TOOL cascade-file <file>   # searches your reframings, then expands the top papers' references, capped at ~80 ids
    python3 $TOOL search "<q>"          # ids for one query
    python3 $TOOL refs <arxiv_id> references   # what that paper cites (compare-against)
    python3 $TOOL refs <arxiv_id> citers       # what cites that paper (using / extending)
    python3 $TOOL scrape <url>          # every arXiv id on a page (survey, awesome-list, leaderboard)
    python3 $TOOL scrape-text <url>     # raw markdown to read (leaderboard model names)

CLASSIFY THE QUESTION, then apply the matching method. This is the whole game.

A) ALWAYS, as the base pass. Write 8 to 12 exact-name keyword reframings:
   specific method, model, dataset, and benchmark NAMES, not broad phrases.
   Write them one per line to a file, then run `cascade-file` on it. Exact-name
   decomposition is the single largest recall lever, because one broad query
   misses the niche papers.

B) COMPARE-AGAINST ("what does X compare to, build on, baseline against").
   Resolve X to its arXiv id, then `refs <X> references`. The answer is in X's
   own bibliography, not in a topical search.

C) USING OR EXTENDING X ("models that adopt X"). `refs <X> citers`, plus
   exact-name searches for the adopters you already know.

D) BEST-ON-BENCHMARK ("which models score best on Y", "largest open model").
   Find the leaderboard, `scrape-text` it, read the OPEN model names off it
   (closed models rarely have a paper to retrieve), then search
   "<model family> technical report" for each.

E) NICHE ENUMERATION ("papers that do X"). The exact-name pass in A is primary.
   A tight, on-topic survey or awesome-list adds its ids as a bonus.

RULES
- Recent ids (25xx, 26xx) are real papers. Keep them.
- A specific-sounding question usually still has a family of papers behind it.
  Surface the family. Only a question naming one paper by title is single-answer.
- Merge all ids from every step. Rank the method-targeted hits (references,
  citers, leaderboard) and exact-name hits above the broad cascade tail.
- Never invent an id. Report only ids the tool returned.
