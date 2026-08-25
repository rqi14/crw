//! Transform a [`SearxngResponse`] into the public flat / grouped result
//! shapes. Direct port of `crw-saas/src/lib/search-transform.ts`.

use std::collections::HashSet;

use crw_core::types::{GroupedSearchData, ImageResult, SearchResult, SearchSource};

use crate::client::{SearxngResponse, SearxngResult};

/// Hard cap on per-source upstream rows we sort/dedupe. SearXNG with all
/// default engines tops out a couple of hundred rows per source bucket;
/// setting 500 leaves comfortable headroom while preventing a misbehaving
/// engine from turning a single search into a CPU/memory amplifier.
const MAX_UPSTREAM_ROWS: usize = 500;

fn score_or_zero(r: &SearxngResult) -> f64 {
    r.score.unwrap_or(0.0)
}

/// Predicate: row carries the load-bearing identity fields (`url`,
/// `title`, `engine`). Real upstreams sometimes emit partial rows — e.g.
/// when an engine times out mid-page — and one bad row used to fail the
/// whole search. We silently skip them and continue.
///
/// Returning a predicate (not a filtered `Vec`) lets each caller chain
/// `.filter(is_well_formed).take(MAX_UPSTREAM_ROWS).cloned()` so we never
/// clone rows that will be discarded. Callers cap *after* filtering by
/// source so a hot bucket (e.g. 600 general results) can't starve a cold
/// one (e.g. 5 news results).
fn is_well_formed(r: &SearxngResult) -> bool {
    r.url.as_deref().is_some_and(|s| !s.is_empty())
        && r.title.as_deref().is_some_and(|s| !s.is_empty())
        && r.engine.as_deref().is_some_and(|s| !s.is_empty())
}

fn url_of(r: &SearxngResult) -> &str {
    r.url.as_deref().unwrap_or("")
}

fn title_of(r: &SearxngResult) -> String {
    r.title.clone().unwrap_or_default()
}

/// Stable-sorted by descending `score` (missing scores treated as 0).
fn sort_by_score(items: &mut [SearxngResult]) {
    items.sort_by(|a, b| {
        score_or_zero(b)
            .partial_cmp(&score_or_zero(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn dedupe_by_url(items: Vec<SearxngResult>) -> Vec<SearxngResult> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let key = url_of(&item).to_string();
        if seen.insert(key) {
            out.push(item);
        }
    }
    out
}

fn to_search_result(r: &SearxngResult, position: u32) -> SearchResult {
    let description = r.content.clone().unwrap_or_default();
    SearchResult {
        url: url_of(r).to_string(),
        title: title_of(r),
        snippet: description.clone(),
        description,
        position,
        score: r.score,
        published_date: r.published_date.clone(),
        category: r.category.clone(),
        markdown: None,
        html: None,
        raw_html: None,
        links: None,
        metadata: None,
        summary: None,
        error: None,
        truncated: None,
    }
}

fn to_image_result(r: &SearxngResult, position: u32) -> ImageResult {
    ImageResult {
        url: url_of(r).to_string(),
        title: title_of(r),
        description: r.content.clone().unwrap_or_default(),
        image_url: r.img_src.clone().unwrap_or_else(|| url_of(r).to_string()),
        position,
        thumbnail_url: r.thumbnail_src.clone(),
        image_format: r.img_format.clone(),
        resolution: r.resolution.clone(),
    }
}

/// Flat output: dedupe by URL, sort by score, slice to `limit`.
///
/// Note: SaaS sorts then dedupes, so a higher-scored duplicate wins. We
/// preserve that order — see `crw-saas/src/lib/search-transform.ts:73`.
pub fn transform_flat(response: &SearxngResponse, limit: u32) -> Vec<SearchResult> {
    // Drop malformed rows, then cap the working set at `MAX_UPSTREAM_ROWS`
    // before clone+sort. A misbehaving SearXNG instance (or a query that
    // scoops thousands of rows) would otherwise amplify CPU/memory on every
    // request.
    let mut results: Vec<SearxngResult> = response
        .results
        .iter()
        .filter(|r| is_well_formed(r))
        .take(MAX_UPSTREAM_ROWS)
        .cloned()
        .collect();
    sort_by_score(&mut results);
    let deduped = dedupe_by_url(results);
    deduped
        .into_iter()
        .take(limit as usize)
        .enumerate()
        .map(|(i, r)| to_search_result(&r, (i + 1) as u32))
        .collect()
}

/// Flat output for the LLM answer / summarize path: drop malformed rows, run
/// the content-aware re-rank pipeline (RRF + junk/coverage/geo filter + BM25
/// + domain dedupe), then slice to `limit`.
///
/// Unlike [`transform_flat`] (which preserves SaaS byte-parity by sorting on
/// raw SearXNG score), this selects clean, query-relevant, geo-correct sources
/// so the top-N feeding the LLM isn't dictionary / shopping / homonym junk.
pub fn transform_flat_reranked(
    response: &SearxngResponse,
    query: &str,
    limit: u32,
    relevance: bool,
) -> Vec<SearchResult> {
    let results: Vec<SearxngResult> = response
        .results
        .iter()
        .filter(|r| is_well_formed(r))
        .take(MAX_UPSTREAM_ROWS)
        .cloned()
        .collect();
    let ranked = if relevance {
        crate::rerank::rerank_relevance(&results, query)
    } else {
        crate::rerank::rerank(&results, query)
    };
    ranked
        .into_iter()
        .take(limit as usize)
        .enumerate()
        .map(|(i, r)| to_search_result(r, (i + 1) as u32))
        .collect()
}

/// Grouped output: filter by `sources`, then per-bucket sort/dedupe/slice.
/// Limit applies **per source**, not in total — matches SaaS semantics.
pub fn transform_grouped(
    response: &SearxngResponse,
    sources: &[SearchSource],
    limit: u32,
) -> GroupedSearchData {
    let mut data = GroupedSearchData::default();
    let cap = limit as usize;

    // Per-source filter+cap on the raw response — `is_well_formed` is a
    // predicate so we never clone rows that will be discarded. Each source
    // gets its own `MAX_UPSTREAM_ROWS` budget for sort/dedupe so a hot
    // bucket (500 web rows) can't starve cold ones (5 news rows).
    if sources.contains(&SearchSource::Web) {
        let mut sorted: Vec<SearxngResult> = response
            .results
            .iter()
            .filter(|r| is_well_formed(r))
            .filter(|r| {
                let cat = r.category.as_deref();
                cat == Some("general") || (r.img_src.is_none() && cat != Some("news"))
            })
            .take(MAX_UPSTREAM_ROWS)
            .cloned()
            .collect();
        sort_by_score(&mut sorted);
        let deduped = dedupe_by_url(sorted);
        data.web = Some(
            deduped
                .into_iter()
                .take(cap)
                .enumerate()
                .map(|(i, r)| to_search_result(&r, (i + 1) as u32))
                .collect(),
        );
    }

    if sources.contains(&SearchSource::News) {
        let mut sorted: Vec<SearxngResult> = response
            .results
            .iter()
            .filter(|r| is_well_formed(r))
            .filter(|r| r.category.as_deref() == Some("news"))
            .take(MAX_UPSTREAM_ROWS)
            .cloned()
            .collect();
        sort_by_score(&mut sorted);
        let deduped = dedupe_by_url(sorted);
        data.news = Some(
            deduped
                .into_iter()
                .take(cap)
                .enumerate()
                .map(|(i, r)| to_search_result(&r, (i + 1) as u32))
                .collect(),
        );
    }

    if sources.contains(&SearchSource::Images) {
        let mut sorted: Vec<SearxngResult> = response
            .results
            .iter()
            .filter(|r| is_well_formed(r))
            .filter(|r| r.category.as_deref() == Some("images") || r.img_src.is_some())
            .take(MAX_UPSTREAM_ROWS)
            .cloned()
            .collect();
        sort_by_score(&mut sorted);
        let deduped = dedupe_by_url(sorted);
        data.images = Some(
            deduped
                .into_iter()
                .take(cap)
                .enumerate()
                .map(|(i, r)| to_image_result(&r, (i + 1) as u32))
                .collect(),
        );
    }

    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(url: &str, score: f64, content: &str) -> SearxngResult {
        SearxngResult {
            url: Some(url.into()),
            title: Some(format!("title-{url}")),
            engine: Some("test".into()),
            content: Some(content.into()),
            score: Some(score),
            engines: Vec::new(),
            positions: Vec::new(),
            category: Some("general".into()),
            template: None,
            published_date: None,
            img_src: None,
            thumbnail_src: None,
            img_format: None,
            resolution: None,
        }
    }

    fn news(url: &str, score: f64) -> SearxngResult {
        SearxngResult {
            url: Some(url.into()),
            title: Some(format!("news-{url}")),
            engine: Some("test".into()),
            content: Some("snippet".into()),
            score: Some(score),
            engines: Vec::new(),
            positions: Vec::new(),
            category: Some("news".into()),
            template: None,
            published_date: Some("2026-05-01T00:00:00Z".into()),
            img_src: None,
            thumbnail_src: None,
            img_format: None,
            resolution: None,
        }
    }

    fn image(url: &str, score: f64, img: &str) -> SearxngResult {
        SearxngResult {
            url: Some(url.into()),
            title: Some(format!("img-{url}")),
            engine: Some("test".into()),
            content: Some(String::new()),
            score: Some(score),
            engines: Vec::new(),
            positions: Vec::new(),
            category: Some("images".into()),
            template: Some("images.html".into()),
            published_date: None,
            img_src: Some(img.into()),
            thumbnail_src: Some(format!("{img}.thumb")),
            img_format: Some("jpeg".into()),
            resolution: Some("1920x1080".into()),
        }
    }

    fn resp(items: Vec<SearxngResult>) -> SearxngResponse {
        SearxngResponse {
            results: items,
            ..SearxngResponse::default()
        }
    }

    #[test]
    fn flat_sorts_by_score_desc() {
        let res = transform_flat(
            &resp(vec![r("a", 0.1, "A"), r("b", 0.9, "B"), r("c", 0.5, "C")]),
            5,
        );
        assert_eq!(
            res.iter().map(|x| x.url.as_str()).collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
        assert_eq!(res[0].position, 1);
        assert_eq!(res[1].position, 2);
        assert_eq!(res[2].position, 3);
    }

    #[test]
    fn flat_dedupe_keeps_highest_score() {
        let res = transform_flat(&resp(vec![r("a", 0.1, "low"), r("a", 0.9, "high")]), 5);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].description, "high");
    }

    #[test]
    fn flat_respects_limit() {
        let res = transform_flat(
            &resp(vec![r("a", 0.9, "A"), r("b", 0.8, "B"), r("c", 0.7, "C")]),
            2,
        );
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn flat_missing_score_treated_as_zero() {
        let mut a = r("a", 0.0, "A");
        a.score = None;
        let res = transform_flat(&resp(vec![a, r("b", 0.5, "B")]), 5);
        assert_eq!(res[0].url, "b");
    }

    #[test]
    fn grouped_web_filters_general_and_unknown() {
        let res = transform_grouped(
            &resp(vec![
                r("g", 0.9, ""),
                news("n", 0.8),
                image("i", 0.7, "https://i.img"),
            ]),
            &[SearchSource::Web],
            5,
        );
        let web = res.web.unwrap();
        assert_eq!(
            web.iter().map(|x| x.url.as_str()).collect::<Vec<_>>(),
            vec!["g"]
        );
    }

    #[test]
    fn grouped_news_only_news_category() {
        let res = transform_grouped(
            &resp(vec![r("g", 0.9, ""), news("n1", 0.8), news("n2", 0.6)]),
            &[SearchSource::News],
            5,
        );
        let n = res.news.unwrap();
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].url, "n1");
        assert!(n[0].published_date.is_some());
    }

    #[test]
    fn grouped_images_picks_image_or_img_src() {
        let mut general_with_img = r("g", 0.5, "");
        general_with_img.img_src = Some("https://x.png".into());

        let res = transform_grouped(
            &resp(vec![image("i", 0.9, "https://i.img"), general_with_img]),
            &[SearchSource::Images],
            5,
        );
        let imgs = res.images.unwrap();
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[0].url, "i");
        assert_eq!(imgs[0].image_url, "https://i.img");
    }

    #[test]
    fn grouped_image_falls_back_to_url_when_img_src_missing() {
        let mut img = image("i", 0.9, "");
        img.img_src = None; // category=images but no img_src
        let res = transform_grouped(&resp(vec![img]), &[SearchSource::Images], 5);
        let imgs = res.images.unwrap();
        assert_eq!(imgs[0].image_url, "i"); // falls back to url
    }

    #[test]
    fn grouped_limit_applies_per_source() {
        let mut items = vec![];
        for i in 0..5 {
            items.push(r(&format!("g{i}"), 1.0 - i as f64 * 0.1, ""));
            items.push(news(&format!("n{i}"), 1.0 - i as f64 * 0.1));
        }
        let res = transform_grouped(&resp(items), &[SearchSource::Web, SearchSource::News], 2);
        assert_eq!(res.web.unwrap().len(), 2);
        assert_eq!(res.news.unwrap().len(), 2);
    }

    #[test]
    fn grouped_hot_bucket_does_not_starve_cold_buckets() {
        // Regression for codex review iteration 2: the well-formed cap used
        // to be applied globally before per-source filtering, so 600 web
        // rows could push all the news rows out of the working set. Now the
        // cap is per-source — both buckets must populate.
        let mut items = Vec::new();
        for i in 0..600 {
            items.push(r(&format!("g{i}"), 1.0 - (i as f64 / 1000.0), ""));
        }
        for i in 0..3 {
            items.push(news(&format!("n{i}"), 0.5));
        }
        let res = transform_grouped(&resp(items), &[SearchSource::Web, SearchSource::News], 10);
        assert_eq!(res.web.unwrap().len(), 10);
        assert_eq!(
            res.news.unwrap().len(),
            3,
            "cold news bucket must survive a hot web bucket"
        );
    }

    #[test]
    fn malformed_rows_are_dropped_silently() {
        // Mix of well-formed and malformed rows: missing url, missing title,
        // empty engine. Only the well-formed row should survive.
        let mut bad_url = r("ok", 0.9, "ok-snippet");
        bad_url.url = None;
        let mut empty_title = r("x", 0.5, "x");
        empty_title.title = Some(String::new());
        let mut no_engine = r("y", 0.4, "y");
        no_engine.engine = None;
        let good = r("z", 0.3, "z");
        let res = transform_flat(&resp(vec![bad_url, empty_title, no_engine, good]), 10);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].url, "z");
    }

    #[test]
    fn grouped_unrequested_source_omitted() {
        let res = transform_grouped(&resp(vec![r("g", 0.9, "")]), &[SearchSource::Web], 5);
        assert!(res.web.is_some());
        assert!(res.news.is_none());
        assert!(res.images.is_none());
    }

    // ── transform_flat: remaining shapes ─────────────────────────────────

    #[test]
    fn flat_empty_response_returns_empty() {
        assert!(transform_flat(&resp(vec![]), 10).is_empty());
    }

    #[test]
    fn flat_all_malformed_returns_empty() {
        let mut bad = r("x", 0.5, "x");
        bad.title = None;
        assert!(transform_flat(&resp(vec![bad]), 10).is_empty());
    }

    #[test]
    fn flat_limit_zero_returns_empty() {
        let res = transform_flat(&resp(vec![r("a", 0.9, "A")]), 0);
        assert!(res.is_empty());
    }

    #[test]
    fn flat_limit_exceeds_available_returns_all() {
        let res = transform_flat(&resp(vec![r("a", 0.9, "A"), r("b", 0.5, "B")]), 100);
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn flat_negative_score_sorts_last() {
        let res = transform_flat(&resp(vec![r("a", -1.0, "A"), r("b", 0.1, "B")]), 5);
        assert_eq!(res[0].url, "b");
        assert_eq!(res[1].url, "a");
    }

    #[test]
    fn flat_nan_score_does_not_panic() {
        let mut a = r("a", 0.0, "A");
        a.score = Some(f64::NAN);
        // `partial_cmp` on NaN returns None -> treated as Equal; must not panic.
        let res = transform_flat(&resp(vec![a, r("b", 0.5, "B")]), 5);
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn flat_dedupe_keeps_highest_among_three_duplicates() {
        let res = transform_flat(
            &resp(vec![
                r("a", 0.1, "low"),
                r("a", 0.9, "high"),
                r("a", 0.5, "mid"),
            ]),
            5,
        );
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].description, "high");
    }

    #[test]
    fn flat_positions_start_at_one_and_increment() {
        let res = transform_flat(
            &resp(vec![r("a", 0.9, "A"), r("b", 0.8, "B"), r("c", 0.7, "C")]),
            5,
        );
        assert_eq!(
            res.iter().map(|x| x.position).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn flat_caps_upstream_rows_before_sort_and_dedupe() {
        // MAX_UPSTREAM_ROWS = 500: the cap is applied to the RAW upstream
        // order before scoring, so more than 500 unique well-formed rows
        // never all survive even with a very high `limit`.
        let items: Vec<SearxngResult> = (0..600).map(|i| r(&format!("u{i}"), 1.0, "x")).collect();
        let res = transform_flat(&resp(items), 1000);
        assert_eq!(res.len(), MAX_UPSTREAM_ROWS);
    }

    #[test]
    fn flat_unicode_title_and_description_preserved() {
        let res = transform_flat(&resp(vec![r("a", 0.9, "🦀 Ferris çalışıyor")]), 5);
        assert_eq!(res[0].description, "🦀 Ferris çalışıyor");
        assert!(res[0].title.contains('a'));
    }

    #[test]
    fn flat_snippet_mirrors_description() {
        let res = transform_flat(&resp(vec![r("a", 0.9, "the snippet text")]), 5);
        assert_eq!(res[0].snippet, res[0].description);
        assert_eq!(res[0].snippet, "the snippet text");
    }

    #[test]
    fn flat_score_and_category_are_carried_through() {
        let res = transform_flat(&resp(vec![r("a", 0.42, "A")]), 5);
        assert_eq!(res[0].score, Some(0.42));
        assert_eq!(res[0].category.as_deref(), Some("general"));
    }

    // ── transform_flat_reranked: smoke coverage (rerank internals live in
    // rerank.rs, out of scope here — just confirm this function's own wiring:
    // malformed-row filtering, limit, and the 500-row upstream cap) ─────────

    #[test]
    fn reranked_empty_response_returns_empty() {
        assert!(transform_flat_reranked(&resp(vec![]), "rust", 5, false).is_empty());
    }

    #[test]
    fn reranked_drops_malformed_rows() {
        let mut bad = r("bad", 0.9, "bad");
        bad.engine = None;
        let good = r("good", 0.1, "good");
        let res = transform_flat_reranked(&resp(vec![bad, good]), "good", 5, false);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].url, "good");
    }

    #[test]
    fn reranked_respects_limit() {
        // Distinct domains: `rerank`'s dedupe-by-registrable-domain collapses
        // rows whose URL has no `//` (like the bare "a"/"b" ids `r()` uses by
        // default) into a single empty-domain bucket, which would make this
        // assert vacuous.
        let items = vec![
            r("https://a.example/1", 0.9, "rust"),
            r("https://b.example/1", 0.8, "rust"),
            r("https://c.example/1", 0.7, "rust"),
        ];
        let res = transform_flat_reranked(&resp(items), "rust", 2, false);
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn reranked_relevance_mode_does_not_panic_and_respects_limit() {
        let items = vec![r("a", 0.9, "rust async"), r("b", 0.1, "unrelated")];
        let res = transform_flat_reranked(&resp(items), "rust async", 1, true);
        assert_eq!(res.len(), 1);
    }

    #[test]
    fn reranked_caps_upstream_rows_before_ranking() {
        let items: Vec<SearxngResult> = (0..600)
            .map(|i| r(&format!("https://u{i}.example/1"), 1.0, "rust"))
            .collect();
        let res = transform_flat_reranked(&resp(items), "rust", 1000, false);
        assert_eq!(res.len(), MAX_UPSTREAM_ROWS);
    }

    // ── transform_grouped: remaining shapes ───────────────────────────────

    #[test]
    fn grouped_web_excludes_general_row_carrying_an_image_src() {
        // A "general"-templated row that also carries img_src is treated as
        // image spillover, not a genuine web result.
        let mut spillover = r("g", 0.9, "");
        spillover.category = None;
        spillover.img_src = Some("https://x.png".into());
        let res = transform_grouped(&resp(vec![spillover]), &[SearchSource::Web], 5);
        assert!(res.web.unwrap().is_empty());
    }

    #[test]
    fn grouped_web_includes_uncategorized_row_without_image() {
        let mut uncategorized = r("g", 0.9, "");
        uncategorized.category = None;
        let res = transform_grouped(&resp(vec![uncategorized]), &[SearchSource::Web], 5);
        assert_eq!(res.web.unwrap().len(), 1);
    }

    #[test]
    fn grouped_news_excludes_general_rows() {
        let res = transform_grouped(
            &resp(vec![r("g", 0.9, ""), news("n", 0.5)]),
            &[SearchSource::News],
            5,
        );
        let n = res.news.unwrap();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].url, "n");
    }

    #[test]
    fn grouped_images_excludes_general_rows_without_img_src() {
        let res = transform_grouped(
            &resp(vec![r("g", 0.9, ""), image("i", 0.5, "https://i.png")]),
            &[SearchSource::Images],
            5,
        );
        let imgs = res.images.unwrap();
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].url, "i");
    }

    #[test]
    fn grouped_empty_sources_list_returns_all_none() {
        let res = transform_grouped(&resp(vec![r("g", 0.9, "")]), &[], 5);
        assert!(res.web.is_none());
        assert!(res.news.is_none());
        assert!(res.images.is_none());
    }

    #[test]
    fn grouped_malformed_rows_dropped_from_every_bucket() {
        let mut bad = news("bad", 0.9);
        bad.url = None;
        let res = transform_grouped(&resp(vec![bad]), &[SearchSource::News], 5);
        assert!(res.news.unwrap().is_empty());
    }

    #[test]
    fn grouped_web_dedupes_by_url_keeping_highest_score() {
        let res = transform_grouped(
            &resp(vec![r("g", 0.1, "low"), r("g", 0.9, "high")]),
            &[SearchSource::Web],
            5,
        );
        let web = res.web.unwrap();
        assert_eq!(web.len(), 1);
        assert_eq!(web[0].description, "high");
    }

    #[test]
    fn grouped_news_dedupes_by_url() {
        let res = transform_grouped(
            &resp(vec![news("n", 0.1), news("n", 0.9)]),
            &[SearchSource::News],
            5,
        );
        assert_eq!(res.news.unwrap().len(), 1);
    }

    #[test]
    fn grouped_images_dedupes_by_url() {
        let res = transform_grouped(
            &resp(vec![
                image("i", 0.1, "https://a.png"),
                image("i", 0.9, "https://b.png"),
            ]),
            &[SearchSource::Images],
            5,
        );
        let imgs = res.images.unwrap();
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].image_url, "https://b.png");
    }

    #[test]
    fn grouped_images_cap_upstream_rows_before_sort() {
        let items: Vec<SearxngResult> = (0..600)
            .map(|i| image(&format!("i{i}"), 1.0, "https://x.png"))
            .collect();
        let res = transform_grouped(&resp(items), &[SearchSource::Images], 1000);
        assert_eq!(res.images.unwrap().len(), MAX_UPSTREAM_ROWS);
    }

    #[test]
    fn grouped_all_three_sources_together() {
        let res = transform_grouped(
            &resp(vec![
                r("g", 0.9, ""),
                news("n", 0.8),
                image("i", 0.7, "https://i.png"),
            ]),
            &[SearchSource::Web, SearchSource::News, SearchSource::Images],
            5,
        );
        assert_eq!(res.web.unwrap().len(), 1);
        assert_eq!(res.news.unwrap().len(), 1);
        assert_eq!(res.images.unwrap().len(), 1);
    }

    #[test]
    fn grouped_news_carries_no_score_field_when_absent() {
        let mut n = news("n", 0.0);
        n.score = None;
        let res = transform_grouped(&resp(vec![n]), &[SearchSource::News], 5);
        assert_eq!(res.news.unwrap()[0].score, None);
    }

    #[test]
    fn grouped_web_limit_zero_returns_empty_vec_not_none() {
        let res = transform_grouped(&resp(vec![r("g", 0.9, "")]), &[SearchSource::Web], 0);
        // `sources` requested Web, so the key is Some(..), just an empty Vec.
        assert!(res.web.is_some());
        assert!(res.web.unwrap().is_empty());
    }

    #[test]
    fn grouped_images_limit_zero_returns_empty_vec() {
        let res = transform_grouped(
            &resp(vec![image("i", 0.9, "https://i.png")]),
            &[SearchSource::Images],
            0,
        );
        assert!(res.images.unwrap().is_empty());
    }

    #[test]
    fn grouped_web_position_starts_at_one() {
        let res = transform_grouped(
            &resp(vec![r("a", 0.9, ""), r("b", 0.5, "")]),
            &[SearchSource::Web],
            5,
        );
        let web = res.web.unwrap();
        assert_eq!(web[0].position, 1);
        assert_eq!(web[1].position, 2);
    }

    #[test]
    fn grouped_news_position_is_independent_of_web_position() {
        // Each bucket's position numbering restarts at 1, it does not
        // continue a global counter across web/news/images.
        let res = transform_grouped(
            &resp(vec![r("g1", 0.9, ""), r("g2", 0.8, ""), news("n", 0.5)]),
            &[SearchSource::Web, SearchSource::News],
            5,
        );
        assert_eq!(res.news.unwrap()[0].position, 1);
    }

    #[test]
    fn grouped_single_row_qualifies_for_only_its_own_bucket() {
        // A pure news row must not leak into the web bucket even though both
        // are requested.
        let res = transform_grouped(
            &resp(vec![news("n", 0.9)]),
            &[SearchSource::Web, SearchSource::News],
            5,
        );
        assert!(res.web.unwrap().is_empty());
        assert_eq!(res.news.unwrap().len(), 1);
    }

    #[test]
    fn flat_published_date_carried_through() {
        let res = transform_flat(&resp(vec![news("n", 0.9)]), 5);
        assert_eq!(
            res[0].published_date.as_deref(),
            Some("2026-05-01T00:00:00Z")
        );
    }

    #[test]
    fn flat_category_none_when_absent() {
        let mut a = r("a", 0.9, "A");
        a.category = None;
        let res = transform_flat(&resp(vec![a]), 5);
        assert_eq!(res[0].category, None);
    }

    #[test]
    fn flat_new_result_starts_with_no_enrichment_fields() {
        let res = transform_flat(&resp(vec![r("a", 0.9, "A")]), 5);
        assert!(res[0].markdown.is_none());
        assert!(res[0].html.is_none());
        assert!(res[0].raw_html.is_none());
        assert!(res[0].links.is_none());
        assert!(res[0].metadata.is_none());
        assert!(res[0].summary.is_none());
        assert!(res[0].error.is_none());
        assert!(res[0].truncated.is_none());
    }

    #[test]
    fn flat_dedupe_keeps_first_occurrence_on_equal_score_ties() {
        // Rust's `sort_by` is stable, so equal-score rows keep their original
        // relative order; dedupe then keeps the first one it sees.
        let res = transform_flat(&resp(vec![r("a", 0.5, "first"), r("a", 0.5, "second")]), 5);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].description, "first");
    }

    #[test]
    fn flat_url_with_only_whitespace_is_still_well_formed() {
        // `is_well_formed` only checks non-empty, not non-blank — documents
        // current behavior rather than a validation guarantee.
        let mut r = r("placeholder", 0.5, "x");
        r.url = Some("   ".into());
        let res = transform_flat(&resp(vec![r]), 5);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].url, "   ");
    }

    #[test]
    fn flat_uncategorized_row_still_included() {
        let mut a = r("a", 0.9, "A");
        a.category = None;
        let res = transform_flat(&resp(vec![a]), 5);
        assert_eq!(res.len(), 1);
    }

    #[test]
    fn grouped_web_excludes_rows_explicitly_categorized_news() {
        let mut news_shaped = r("n", 0.9, "");
        news_shaped.category = Some("news".into());
        let res = transform_grouped(&resp(vec![news_shaped]), &[SearchSource::Web], 5);
        assert!(res.web.unwrap().is_empty());
    }

    #[test]
    fn grouped_web_score_none_is_preserved() {
        let mut a = r("a", 0.0, "A");
        a.score = None;
        let res = transform_grouped(&resp(vec![a]), &[SearchSource::Web], 5);
        assert_eq!(res.web.unwrap()[0].score, None);
    }

    #[test]
    fn grouped_images_score_none_is_preserved() {
        let mut i = image("i", 0.0, "https://i.png");
        i.score = None;
        let res = transform_grouped(&resp(vec![i]), &[SearchSource::Images], 5);
        assert_eq!(res.images.unwrap()[0].position, 1);
    }

    #[test]
    fn grouped_news_carries_category_field() {
        let res = transform_grouped(&resp(vec![news("n", 0.9)]), &[SearchSource::News], 5);
        assert_eq!(res.news.unwrap()[0].category.as_deref(), Some("news"));
    }

    #[test]
    fn to_image_result_defaults_thumbnail_none_when_absent() {
        let mut i = image("i", 0.9, "https://i.png");
        i.thumbnail_src = None;
        i.img_format = None;
        i.resolution = None;
        let res = transform_grouped(&resp(vec![i]), &[SearchSource::Images], 5);
        let img = &res.images.unwrap()[0];
        assert!(img.thumbnail_url.is_none());
        assert!(img.image_format.is_none());
        assert!(img.resolution.is_none());
    }

    #[test]
    fn to_image_result_description_defaults_empty_when_content_absent() {
        let mut i = image("i", 0.9, "https://i.png");
        i.content = None;
        let res = transform_grouped(&resp(vec![i]), &[SearchSource::Images], 5);
        assert_eq!(res.images.unwrap()[0].description, "");
    }

    #[test]
    fn is_well_formed_rejects_empty_url() {
        let mut a = r("a", 0.9, "A");
        a.url = Some(String::new());
        assert!(transform_flat(&resp(vec![a]), 5).is_empty());
    }

    #[test]
    fn is_well_formed_rejects_empty_engine() {
        let mut a = r("a", 0.9, "A");
        a.engine = Some(String::new());
        assert!(transform_flat(&resp(vec![a]), 5).is_empty());
    }

    #[test]
    fn grouped_web_dedupe_and_sort_apply_before_the_limit_slice() {
        // Two rows share a URL and one distinct row: dedupe must run BEFORE
        // the `take(limit)` slice, or a limit of 1 could keep the loser.
        let res = transform_grouped(
            &resp(vec![
                r("dup", 0.1, "low"),
                r("dup", 0.9, "high"),
                r("z", 0.5, "z"),
            ]),
            &[SearchSource::Web],
            1,
        );
        let web = res.web.unwrap();
        assert_eq!(web.len(), 1);
        assert_eq!(web[0].description, "high");
    }

    #[test]
    fn to_image_result_maps_thumbnail_format_and_resolution() {
        let res = transform_grouped(
            &resp(vec![image("i", 0.9, "https://i.png")]),
            &[SearchSource::Images],
            5,
        );
        let img = &res.images.unwrap()[0];
        assert_eq!(img.thumbnail_url.as_deref(), Some("https://i.png.thumb"));
        assert_eq!(img.image_format.as_deref(), Some("jpeg"));
        assert_eq!(img.resolution.as_deref(), Some("1920x1080"));
    }
}
