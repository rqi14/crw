# arXivQA research-recall benchmark

What it measures: given a natural-language research question, how many of the
papers that actually answer it does a retriever surface. Score is mean recall
over the 191 questions of [alphaXiv's ArXivQA
set](https://github.com/alphaXiv/retriever-sandbox)
(`db/scripts/all_train.json`), each question carrying a set of ground-truth
arXiv ids. Extra ids do not cost anything, so the game is coverage.

Published result, from `runs/2026-06-20-hosted-191.jsonl` in this directory:

| Provider | Recall |
|---|---|
| **fastCRW Research API** | **61.0%** |
| Firecrawl Research Index | 53.3% |
| Claude | 45.4% |
| Parallel | 44.3% |
| Exa | 43.4% |

Firecrawl's 53.3% is their own published figure from their Research Index
launch post. The rest is our run of the same 191 questions against each
provider's live deployed endpoint. This is research-retrieval recall, a
different measurement from the scrape truth-recall in `bench/diagnose_3way.py`;
do not conflate the two.

## Reproduce the published number

The scorer is offline and needs nothing:

```bash
python3 bench/arxivqa/score.py --selfcheck
```

Re-score the stored run. The ground truth is downloaded from alphaXiv on first
use, so the number is checked against their data, not a copy of ours:

```bash
python3 bench/arxivqa/score.py --results bench/arxivqa/runs/2026-06-20-hosted-191.jsonl
# mean recall over 191 questions: 0.6100  (61.0%)
```

## Run it yourself, end to end

1. Point the tool at an endpoint. Hosted:

   ```bash
   export FASTCRW_API_KEY="crw_live_..."      # free tier at https://fastcrw.com/pricing
   ```

   Or self-hosted, no account and no key, since the research endpoints are in
   this repository (`crates/crw-server/src/routes/research.rs`):

   ```bash
   crw serve &
   export FASTCRW_BASE_URL="http://localhost:3000"
   ```

   Check it answers:

   ```bash
   python3 bench/arxivqa/research_api.py search "speculative decoding" | head
   ```

2. Build the question file with the ground truth stripped out, so the agent
   cannot see the answers:

   ```bash
   curl -sL https://raw.githubusercontent.com/alphaXiv/retriever-sandbox/main/db/scripts/all_train.json \
     | python3 -c 'import json,sys; d=json.load(sys.stdin); print(json.dumps([{"qid": f"Q{i+1}", "query": x["query"]} for i, x in enumerate(d)]))' \
     > questions.json
   ```

3. Run one agent per question with `AGENT_PROMPT.md`, appending one JSON object
   per line to a run file. The published run used one agent per question at
   concurrency 4; any agent runner works, the prompt is the contract.

4. Score it:

   ```bash
   python3 bench/arxivqa/score.py --results your_run.jsonl
   ```

## Honest scope

- The score is an agent plus a retrieval strategy over live endpoints, not a
  pre-built paper index. The run was driven by `AGENT_PROMPT.md`, not by loading
  a skill file; the same method is documented for humans in
  [`skills/crw-research`](../../skills/crw-research/SKILL.md).
- The base pass is two steps, and both matter. `cascade-file` searches each
  exact-name reframing, ranks an id by how many reframings surfaced it, and then
  pulls the references of the top 5 ids, capped at 80 in total. That second step
  is not intent-gated, it runs on every question. Measured on the first 12
  questions with one raw query each, so that the expansion is the only variable:
  47.0% for the search union alone, 66.5% with the expansion.
- Re-running is not bit-identical. The agent is an LLM and the endpoints query
  the live web and live citation graphs, so expect a spread rather than exactly
  61.0%. The stored run file is what the published number was computed from.
- Recall is not precision. The API returns candidates; a real survey still needs
  the agent to read and filter them.
- The questions are arXiv-indexed and skew to CS and ML. Nothing here says
  anything about biomedical coverage.
- `citers` and `similar` come from a live citation graph, and a seed with a very
  large citation count can come back empty. The strategy falls back to the
  exact-name pass rather than concluding that nothing cites the paper.

## Files

| File | What it is |
|---|---|
| `score.py` | The scorer. `--selfcheck` proves it offline, `--results` scores a run |
| `research_api.py` | Deterministic CLI over `/v1/search/research/*`, the only tool the agent gets |
| `AGENT_PROMPT.md` | The per-question prompt, one agent instance per question |
| `runs/2026-06-20-hosted-191.jsonl` | The 191-question run behind the published 61.0% |
