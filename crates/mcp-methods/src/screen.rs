//! Stargazer screening — bulk-fetch a repo's stargazers and their public
//! repo portfolios over cheap REST, classify each person into an
//! archetype, and render a compact overview the agent can drill into.
//!
//! Cost model (the whole point): one REST request per stargazer-page +
//! one per stargazer for their owned-repos list. No GraphQL, no
//! per-repo calls, no READMEs in the bulk pass. The raw payloads
//! (~550 KB for a prolific user) are projected down to a handful of
//! fields before anything leaves Rust; the agent-facing overview stays
//! ~2 KB regardless of how many people starred the repo.
//!
//! The value-bearing logic (`project_repo`, `profile_user`,
//! `build_overview`) is pure over already-fetched JSON, so it unit-tests
//! without the network. `screen_repo` is the orchestrator that fetches.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::github;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Tuning knobs for a screen. Defaults are chosen for the kglite-scale
/// case (dozens-to-low-hundreds of stargazers).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenConfig {
    /// Max stargazers to screen (most-recent first). `None` = all.
    pub max_stargazers: Option<usize>,
    /// Max owned repos to pull per user (sorted by most-recently pushed).
    pub max_repos_per_user: usize,
    /// Lowercase keywords for the relevance gate, matched against repo
    /// name / description / topics / language.
    pub relevance_keywords: Vec<String>,
    /// Languages that define the seed project's stack (e.g. Rust + Python
    /// for a PyO3 lib). Stargazers who use *all* of them are flagged as a
    /// "stack match" — an architectural signal that needs no keywords and
    /// catches relevant devs whose repo names don't contain the keywords.
    pub stack_languages: Vec<String>,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            max_stargazers: None,
            max_repos_per_user: 100,
            relevance_keywords: Vec::new(),
            stack_languages: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Projected types
// ---------------------------------------------------------------------------

/// The cheap fields we keep from a repo object — everything else (URLs,
/// owner blob, permissions) is dropped at projection time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoLite {
    pub name: String,
    pub fork: bool,
    pub archived: bool,
    pub lang: Option<String>,
    pub stars: u64,
    pub forks: u64,
    pub pushed: String,  // YYYY-MM-DD
    pub created: String, // YYYY-MM-DD
    pub topics: Vec<String>,
    pub desc: Option<String>,
    /// Repo size in KB — a cheap proxy for code volume / effort.
    #[serde(default)]
    pub size: u64,
}

/// Normalized 0–1 metric vector for a person, percentile-ranked within the
/// screened set. The basis axes for ranking and fan-out gating: they
/// disagree with each other (effort ≠ popularity ≠ recency ≠ relatedness),
/// which is what makes each a real axis rather than a derived view.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Scores {
    pub relatedness: f64,
    pub popularity: f64,
    pub effort: f64,
    pub recency: f64,
}

/// One classified stargazer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    pub login: String,
    pub archetype: Archetype,
    pub repos_seen: usize,
    pub capped: bool, // hit the per-user page cap (has more repos)
    pub original_count: usize,
    pub fork_count: usize,
    pub max_stars: u64,
    pub total_stars: u64,
    pub top_langs: Vec<String>,
    pub last_active: String,
    pub flagship: Option<RepoLite>,
    /// Repos (non-fork) that matched the relevance keywords, best-first.
    pub relevant: Vec<RepoLite>,
    /// True when the top relevant repo names the topic (score ≥ 2) or has
    /// real traction — i.e. worth surfacing, not language-only noise.
    pub strong_hit: bool,
    /// Relevance score of the best matching repo (match density).
    pub hit_score: u32,
    /// Keywords that matched the best repo (for the legibility annotation).
    pub hit_terms: Vec<String>,
    /// Uses every language in the seed project's stack (cheap PyO3-style
    /// architectural signal, independent of keywords).
    pub stack_match: bool,
    /// Per-stack-language count of original repos, in `stack_languages`
    /// order. The depth of stack commitment — a serial Rust+Python builder
    /// (the kglite/maturin pattern) ranks above an incidental one. This is
    /// the signal that catches keyword-invisible architectural peers.
    #[serde(default)]
    pub stack_lang_counts: Vec<(String, usize)>,
    // ── Shortlist enrichments (filled only for leads + stack candidates) ──
    /// Follower count (reach), if enriched. [item 6]
    #[serde(default)]
    pub followers: Option<u64>,
    /// This stargazer's repo(s) declare the seed package as a dependency —
    /// an actual adopter, not just a watcher. [item 2]
    #[serde(default)]
    pub adopter: bool,
    /// Evidence line for the adoption flag (the manifest match).
    #[serde(default)]
    pub adoption_evidence: Option<String>,
    /// Count of original repos that combine *all* stack languages in one
    /// repo — the true PyO3/maturin co-location signal. [item 4]
    #[serde(default)]
    pub colocated_repos: Option<usize>,
    /// Repos this person contributes to but does not own (from public
    /// events) — surfaces relevance the owned-repo list can't see. [item 7]
    #[serde(default)]
    pub contributes_to: Vec<String>,
    /// Normalized metric vector (filled by `normalize_scores` after the
    /// whole set is classified + enriched).
    #[serde(default)]
    pub scores: Scores,
    /// All projected repos, kept for drill-down.
    pub repos: Vec<RepoLite>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Archetype {
    /// High-traction author: a very popular repo or several notable ones.
    Established,
    /// One repo stands far above the rest.
    SingleProject,
    /// Many original repos, low traction — a builder/tinkerer.
    Prolific,
    /// A few modest original repos.
    Casual,
    /// Mostly forks / nothing original — a consumer.
    Consumer,
    /// Has original work but no public push in a long time.
    Dormant,
}

impl Archetype {
    pub fn label(self) -> &'static str {
        match self {
            Archetype::Established => "Established authors",
            Archetype::SingleProject => "Single-project devs",
            Archetype::Prolific => "Prolific builders",
            Archetype::Casual => "Casual devs",
            Archetype::Consumer => "Consumers / lurkers",
            Archetype::Dormant => "Dormant",
        }
    }
    /// Display ordering for the overview (most interesting first).
    fn rank(self) -> u8 {
        match self {
            Archetype::Established => 0,
            Archetype::SingleProject => 1,
            Archetype::Prolific => 2,
            Archetype::Casual => 3,
            Archetype::Dormant => 4,
            Archetype::Consumer => 5,
        }
    }
}

// ---------------------------------------------------------------------------
// Projection
// ---------------------------------------------------------------------------

fn date10(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| s.chars().take(10).collect())
        .unwrap_or_default()
}

/// Project a raw GitHub repo object down to the cheap fields.
pub fn project_repo(raw: &Value) -> RepoLite {
    let topics = raw
        .get("topics")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    RepoLite {
        name: raw
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        fork: raw.get("fork").and_then(Value::as_bool).unwrap_or(false),
        archived: raw.get("archived").and_then(Value::as_bool).unwrap_or(false),
        lang: raw
            .get("language")
            .and_then(Value::as_str)
            .map(String::from),
        stars: raw
            .get("stargazers_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        forks: raw.get("forks_count").and_then(Value::as_u64).unwrap_or(0),
        pushed: date10(raw, "pushed_at"),
        created: date10(raw, "created_at"),
        topics,
        desc: raw
            .get("description")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        size: raw.get("size").and_then(Value::as_u64).unwrap_or(0),
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Split text into lowercase alphanumeric word tokens.
fn tokenize(s: &str) -> std::collections::HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// Whole-word (with naive plural stemming) match: `graph` matches `graph`,
/// `graphs`, and the `graph` token inside `knowledge-graph` — but NOT
/// `graphql` (no boundary) and NOT `storage` for `rag`. Kills the
/// substring false positives the first tester flagged.
fn token_set_has(tokens: &std::collections::HashSet<String>, kw: &str) -> bool {
    tokens.iter().any(|t| {
        t == kw || t.strip_suffix('s') == Some(kw) || t.strip_prefix(kw) == Some("s")
    })
}

/// Weighted relevance of a repo against the keyword list, plus the distinct
/// keywords that matched (for the legibility annotation). Name + topic are
/// the strongest "about-ness" signal; description medium; a bare language
/// match is weakest (matching "rust"-the-language alone would flag every
/// Rust repo, so it scores 1 and the overview filters those out unless the
/// repo earns traction).
fn repo_relevance(r: &RepoLite, kws: &[String]) -> (u32, Vec<String>) {
    if kws.is_empty() {
        return (0, Vec::new());
    }
    let name = tokenize(&r.name);
    let desc = tokenize(r.desc.as_deref().unwrap_or(""));
    let topics: std::collections::HashSet<String> =
        r.topics.iter().flat_map(|t| tokenize(t)).collect();
    let lang = r.lang.as_deref().unwrap_or("").to_lowercase();

    let mut score = 0u32;
    let mut matched = Vec::new();
    for k in kws {
        let mut hit = false;
        if token_set_has(&name, k) {
            score += 3;
            hit = true;
        }
        if token_set_has(&topics, k) {
            score += 3;
            hit = true;
        }
        if token_set_has(&desc, k) {
            score += 2;
            hit = true;
        }
        if &lang == k {
            score += 1;
            hit = true;
        }
        if hit {
            matched.push(k.clone());
        }
    }
    (score, matched)
}

/// Classify a user from their (already projected) owned repos.
pub fn profile_user(login: &str, repos: Vec<RepoLite>, capped: bool, cfg: &ScreenConfig) -> UserProfile {
    let originals: Vec<&RepoLite> = repos.iter().filter(|r| !r.fork).collect();
    let original_count = originals.len();
    let fork_count = repos.len() - original_count;

    let max_stars = originals.iter().map(|r| r.stars).max().unwrap_or(0);
    let total_stars: u64 = originals.iter().map(|r| r.stars).sum();
    let last_active = repos.iter().map(|r| r.pushed.clone()).max().unwrap_or_default();

    // Top languages over original repos.
    let mut lang_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for r in &originals {
        if let Some(l) = &r.lang {
            *lang_counts.entry(l.clone()).or_default() += 1;
        }
    }
    let mut langs: Vec<(String, usize)> = lang_counts.into_iter().collect();
    langs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let top_langs: Vec<String> = langs.into_iter().take(3).map(|(l, _)| l).collect();

    // Flagship = the most-starred original, if it stands out.
    let mut by_stars: Vec<&RepoLite> = originals.clone();
    by_stars.sort_by(|a, b| b.stars.cmp(&a.stars));
    let second = by_stars.get(1).map(|r| r.stars).unwrap_or(0);
    let flagship = by_stars.first().copied().filter(|r| r.stars >= 10).cloned();
    let flagship_dominant = flagship
        .as_ref()
        .map(|f| f.stars >= 25 && f.stars >= second.saturating_mul(3).max(25))
        .unwrap_or(false);

    let notable = originals.iter().filter(|r| r.stars >= 50).count();
    let established = max_stars >= 500 || notable >= 2;

    // Recency: dormant if the newest push is old. Compare lexically on
    // YYYY-MM-DD against a rough "2 years before the freshest seen" is
    // overkill here; use a fixed cutoff the caller can revisit.
    let dormant = !last_active.is_empty() && last_active.as_str() < "2024-01-01";

    let archetype = if original_count == 0 {
        Archetype::Consumer
    } else if established {
        Archetype::Established
    } else if flagship_dominant {
        Archetype::SingleProject
    } else if dormant {
        Archetype::Dormant
    } else if original_count >= 6 {
        Archetype::Prolific
    } else if fork_count > original_count && max_stars < 5 {
        Archetype::Consumer
    } else {
        Archetype::Casual
    };

    // Relevant repos, scored and sorted best-first (highest score, then
    // most stars). `relevant[0]` is the repo to show as the dev's hit.
    let mut scored: Vec<(u32, Vec<String>, &&RepoLite)> = originals
        .iter()
        .map(|r| {
            let (s, terms) = repo_relevance(r, &cfg.relevance_keywords);
            (s, terms, r)
        })
        .filter(|(s, _, _)| *s > 0)
        .collect();
    // Rank by *breadth* first — how many distinct keywords the repo hits —
    // then by weighted score, then traction. Breadth is the quality signal:
    // a repo matching graph+knowledge+database is genuinely on-domain; one
    // matching only "database" is almost always a coincidence.
    scored.sort_by(|a, b| {
        b.1.len()
            .cmp(&a.1.len())
            .then(b.0.cmp(&a.0))
            .then(b.2.stars.cmp(&a.2.stars))
    });
    let relevant: Vec<RepoLite> = scored.iter().map(|(_, _, r)| (**r).clone()).collect();
    let best_relevant_score = scored.first().map(|(s, _, _)| *s).unwrap_or(0);
    let hit_terms: Vec<String> = scored.first().map(|(_, t, _)| t.clone()).unwrap_or_default();

    // Stack match: count original repos in each of the seed project's
    // languages. A dev is a stack match if they have ≥1 repo in *every*
    // stack language; the counts give the depth to rank by.
    let stack_lang_counts: Vec<(String, usize)> = cfg
        .stack_languages
        .iter()
        .map(|sl| {
            let n = originals
                .iter()
                .filter(|r| r.lang.as_deref().map(|l| l.eq_ignore_ascii_case(sl)).unwrap_or(false))
                .count();
            (sl.clone(), n)
        })
        .collect();
    let stack_match = !stack_lang_counts.is_empty()
        && stack_lang_counts.iter().all(|(_, n)| *n >= 1);

    UserProfile {
        login: login.to_string(),
        archetype,
        repos_seen: repos.len(),
        capped,
        original_count,
        fork_count,
        max_stars,
        total_stars,
        top_langs,
        last_active,
        flagship,
        // A lead must hit ≥2 distinct keywords. Single-keyword matches
        // ("database" on a file-manager plugin) are demoted to a weak-match
        // footnote — they were the dominant noise the testers flagged.
        strong_hit: hit_terms.len() >= 2,
        hit_score: best_relevant_score,
        hit_terms,
        stack_match,
        stack_lang_counts,
        followers: None,
        adopter: false,
        adoption_evidence: None,
        colocated_repos: None,
        contributes_to: Vec::new(),
        scores: Scores::default(),
        relevant,
        repos,
    }
}

/// Recency of a date string "YYYY-MM-DD" as a sortable ordinal (higher =
/// more recent). Empty → 0.
fn date_ordinal(d: &str) -> f64 {
    let mut it = d.split('-');
    let y: f64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let m: f64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let day: f64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    y * 372.0 + m * 31.0 + day
}

/// Assign each value its percentile rank in [0,1] (ties share the average
/// rank). The normalization that lets power-law axes (stars, size) be
/// blended/compared with each other.
fn percentile_ranks(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    if n <= 1 {
        return vec![1.0; n];
    }
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| values[a].total_cmp(&values[b]));
    let mut out = vec![0.0; n];
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && values[idx[j + 1]] == values[idx[i]] {
            j += 1;
        }
        // average rank position for the tie group
        let rank = (i + j) as f64 / 2.0;
        let pct = rank / (n - 1) as f64;
        for k in i..=j {
            out[idx[k]] = pct;
        }
        i = j + 1;
    }
    out
}

/// Compute the normalized score vector for every profile from cheap raw
/// signals: relatedness (keyword score), popularity (stars+followers),
/// effort (repo size + breadth), recency (latest push). Percentile-ranked
/// across the set so axes are comparable. Call after enrichment.
pub fn normalize_scores(profiles: &mut [UserProfile]) {
    let n = profiles.len();
    if n == 0 {
        return;
    }
    let rel: Vec<f64> = profiles.iter().map(|p| p.hit_score as f64).collect();
    let pop: Vec<f64> = profiles
        .iter()
        .map(|p| ((p.total_stars + p.followers.unwrap_or(0)) as f64 + 1.0).ln())
        .collect();
    let eff: Vec<f64> = profiles
        .iter()
        .map(|p| {
            let size: u64 = p.repos.iter().filter(|r| !r.fork).map(|r| r.size).sum();
            (size as f64 + 1.0).ln() + p.original_count as f64
        })
        .collect();
    let rec: Vec<f64> = profiles.iter().map(|p| date_ordinal(&p.last_active)).collect();

    let (rel, pop, eff, rec) = (
        percentile_ranks(&rel),
        percentile_ranks(&pop),
        percentile_ranks(&eff),
        percentile_ranks(&rec),
    );
    for (i, p) in profiles.iter_mut().enumerate() {
        p.scores = Scores {
            relatedness: rel[i],
            popularity: pop[i],
            effort: eff[i],
            recency: rec[i],
        };
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn repo_line(r: &RepoLite) -> String {
    let lang = r.lang.as_deref().unwrap_or("—");
    let desc = r.desc.as_deref().map(|d| trunc(d, 70)).unwrap_or_default();
    let topics = if r.topics.is_empty() {
        String::new()
    } else {
        format!(" [{}]", r.topics.iter().take(4).cloned().collect::<Vec<_>>().join(","))
    };
    format!("{} {}★ ({}) \"{}\"{}", r.name, r.stars, lang, desc, topics)
}

/// One-line summary of a user for the overview.
fn user_overview_line(p: &UserProfile) -> String {
    match p.archetype {
        Archetype::Established | Archetype::SingleProject => {
            let f = p
                .flagship
                .as_ref()
                .map(repo_line)
                .unwrap_or_else(|| "—".into());
            format!("{} — {}", p.login, f)
        }
        _ => {
            let langs = if p.top_langs.is_empty() {
                "—".into()
            } else {
                p.top_langs.join("/")
            };
            let more = if p.capped { "+" } else { "" };
            format!(
                "{} — {}{} repos · {} · active {} · {}★ max",
                p.login, p.original_count, more, langs, p.last_active, p.max_stars
            )
        }
    }
}

/// Disclosure scale — borrowed from kglite's `GraphScale`. The detail
/// level adapts to how many stargazers there are: a small set shows every
/// notable person inline; a huge set collapses to statistics + drill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    Small,   // ≤40   — full inline detail
    Medium,  // ≤150  — established + single full, prolific sampled
    Large,   // ≤1000 — standouts only, the rest as counts
    Extreme, // >1000 — statistics, drill required
}

fn scale_of(n: usize) -> Scale {
    match n {
        0..=40 => Scale::Small,
        41..=150 => Scale::Medium,
        151..=1000 => Scale::Large,
        _ => Scale::Extreme,
    }
}

/// Total original repos a dev has in the seed project's stack languages —
/// the depth of stack commitment used to rank stack matches.
fn stack_depth(p: &UserProfile) -> usize {
    p.stack_lang_counts.iter().map(|(_, n)| n).sum()
}

/// " · 142 followers" reach suffix [item 6], empty when not enriched.
fn reach_tag(p: &UserProfile) -> String {
    match p.followers {
        Some(f) => format!(" · {f} followers"),
        None => String::new(),
    }
}

/// " · contributes: a/b, c/d" suffix [item 7], empty when none.
fn contrib_tag(p: &UserProfile) -> String {
    if p.contributes_to.is_empty() {
        String::new()
    } else {
        format!(
            " · contributes: {}",
            p.contributes_to.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        )
    }
}

/// Lowercase single-word cohort handle used in `cohort:<x>` drills.
fn cohort_key(a: Archetype) -> &'static str {
    match a {
        Archetype::Established => "established",
        Archetype::SingleProject => "single",
        Archetype::Prolific => "prolific",
        Archetype::Casual => "casual",
        Archetype::Dormant => "dormant",
        Archetype::Consumer => "consumers",
    }
}

// ---------------------------------------------------------------------------
// Selection: filter → rank → take (and the fan-out gate)
// ---------------------------------------------------------------------------

/// Conjunctive (AND) filters on the metric axes. All set predicates must
/// pass. Absolute thresholds where the agent has intuition (keywords,
/// stars, dates) + percentile thresholds on the normalized axes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Filters {
    pub min_keywords: Option<usize>,      // relatedness gate (distinct kw hits)
    pub min_stars: Option<u64>,           // popularity gate (best repo stars)
    pub active_since: Option<String>,     // recency gate (YYYY-MM-DD)
    pub adopters_only: bool,              // decisive: actual users
    pub stack_only: bool,                 // architectural peers
    pub min_relatedness_pct: Option<f64>, // percentile gate, 0–1
    pub min_effort_pct: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum RankBy {
    Relatedness,
    Popularity,
    Effort,
    Recency,
}

impl RankBy {
    fn value(self, p: &UserProfile) -> f64 {
        match self {
            RankBy::Relatedness => p.scores.relatedness,
            RankBy::Popularity => p.scores.popularity,
            RankBy::Effort => p.scores.effort,
            RankBy::Recency => p.scores.recency,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            RankBy::Relatedness => "relatedness",
            RankBy::Popularity => "popularity",
            RankBy::Effort => "effort",
            RankBy::Recency => "recency",
        }
    }
    pub fn parse(s: &str) -> Option<RankBy> {
        Some(match s.to_lowercase().as_str() {
            "relatedness" | "related" | "relevance" => RankBy::Relatedness,
            "popularity" | "reach" | "popular" => RankBy::Popularity,
            "effort" | "substance" => RankBy::Effort,
            "recency" | "active" | "recent" => RankBy::Recency,
            _ => return None,
        })
    }
}

/// A complete selection: the filter predicate set, the ranking axis, and a
/// human label. Drives both a focused overview view and the fan-out gate.
#[derive(Debug, Clone)]
pub struct Selection {
    pub filters: Filters,
    pub rank: RankBy,
    pub label: String,
    pub take: usize,
}

fn passes(p: &UserProfile, f: &Filters) -> bool {
    if let Some(k) = f.min_keywords {
        if p.hit_terms.len() < k {
            return false;
        }
    }
    if let Some(s) = f.min_stars {
        if p.max_stars < s {
            return false;
        }
    }
    if let Some(d) = &f.active_since {
        if p.last_active.as_str() < d.as_str() {
            return false;
        }
    }
    if f.adopters_only && !p.adopter {
        return false;
    }
    if f.stack_only && !p.stack_match {
        return false;
    }
    if let Some(pct) = f.min_relatedness_pct {
        if p.scores.relatedness < pct {
            return false;
        }
    }
    if let Some(pct) = f.min_effort_pct {
        if p.scores.effort < pct {
            return false;
        }
    }
    true
}

/// Filter → rank → take. The selection function, reused for the overview
/// view and the fan-out gate (expand only these top-K).
pub fn select<'a>(profiles: &'a [UserProfile], sel: &Selection) -> Vec<&'a UserProfile> {
    let mut v: Vec<&UserProfile> = profiles.iter().filter(|p| passes(p, &sel.filters)).collect();
    v.sort_by(|a, b| sel.rank.value(b).total_cmp(&sel.rank.value(a)));
    v.truncate(sel.take.max(1));
    v
}

/// Named preset = a bundled (filters, rank) for a common goal. Keeps the
/// pipeline usable cold; raw filters are the power layer.
pub fn preset(name: &str, take: usize) -> Option<Selection> {
    let recent = "2025-01-01".to_string();
    Some(match name.to_lowercase().as_str() {
        // relevant + active, ranked by reach — who to email
        "outreach" => Selection {
            filters: Filters { min_keywords: Some(2), active_since: Some(recent), ..Default::default() },
            rank: RankBy::Popularity,
            label: "OUTREACH (relevant + active, by reach)".into(),
            take,
        },
        // architectural peers, ranked by substance
        "peers" => Selection {
            filters: Filters { stack_only: true, active_since: Some(recent), ..Default::default() },
            rank: RankBy::Effort,
            label: "PEERS (build in your stack, by effort)".into(),
            take,
        },
        // biggest audience, any domain — coding legends
        "legends" => Selection {
            filters: Filters::default(),
            rank: RankBy::Popularity,
            label: "LEGENDS (biggest reach, any domain)".into(),
            take,
        },
        // anything on-domain, by popularity — competitive intel
        "intel" => Selection {
            filters: Filters { min_keywords: Some(1), ..Default::default() },
            rank: RankBy::Popularity,
            label: "INTEL (on-domain, by popularity)".into(),
            take,
        },
        // people who already depend on you
        "adopters" => Selection {
            filters: Filters { adopters_only: true, ..Default::default() },
            rank: RankBy::Popularity,
            label: "ADOPTERS (actual users)".into(),
            take,
        },
        _ => return None,
    })
}

/// One selection-view line: the score vector inline (legible ranking) +
/// flags + the representative repo.
fn selection_line(p: &UserProfile) -> String {
    let s = &p.scores;
    let mut flags = String::new();
    if p.adopter {
        flags.push_str(" ✅adopter");
    }
    if is_legend(p) {
        flags.push_str(" 🏆legend");
    }
    if p.stack_match {
        flags.push_str(" ⚙stack");
    }
    let best = p
        .relevant
        .first()
        .or(p.flagship.as_ref())
        .or_else(|| p.repos.iter().filter(|r| !r.fork).max_by_key(|r| r.stars));
    let bestline = best
        .map(|r| {
            format!(
                "{} {}★ \"{}\"",
                r.name,
                r.stars,
                r.desc.as_deref().map(|d| trunc(d, 50)).unwrap_or_default()
            )
        })
        .unwrap_or_default();
    format!(
        "{} [rel {:.0} pop {:.0} eff {:.0} rec {:.0}]{}{} · {}",
        p.login,
        s.relatedness * 100.0,
        s.popularity * 100.0,
        s.effort * 100.0,
        s.recency * 100.0,
        reach_tag(p),
        flags,
        bestline
    )
}

/// Render a focused selection view (filter → rank → take), with explicit
/// empty-result guidance rather than a silent empty section.
pub fn render_selection(profiles: &[UserProfile], sel: &Selection) -> String {
    let chosen = select(profiles, sel);
    if chosen.is_empty() {
        let matched = profiles.iter().filter(|p| passes(p, &sel.filters)).count();
        return format!(
            "\n▶ {} — 0 of {} people passed the filter. Loosen it (lower min_keywords, earlier active_since, drop adopters_only/stack_only).\n",
            sel.label, profiles.len()
        ) + &format!("  (matched filter: {matched})\n");
    }
    let mut out = format!(
        "\n▶ {} (top {}) — scores are 0–100 percentile ranks *within this set*, so they're relative, not comparable across calls:\n",
        sel.label, sel.take
    );
    for p in &chosen {
        out.push_str(&format!("  • {}\n", selection_line(p)));
    }
    out
}

/// Build the compact overview — the agent-facing digest. Scale-adaptive:
/// inventory first (the cohort counts), then detail whose depth shrinks as
/// the stargazer count grows.
pub fn build_overview(
    repo: &str,
    profiles: &[UserProfile],
    meta: &ScreenMeta,
    cfg: &ScreenConfig,
    selection: Option<&Selection>,
) -> String {
    let total = profiles.len();
    let scale = scale_of(total);
    let mut out = String::new();
    let scale_note = match scale {
        Scale::Small => "full detail",
        Scale::Medium => "standouts inline, rest sampled",
        Scale::Large => "standouts only, rest by count",
        Scale::Extreme => "statistics — drill cohorts",
    };
    let noun = if meta.noun.is_empty() { "people" } else { meta.noun.as_str() };
    let scope = if meta.partial && meta.total_stargazers > 0 {
        format!("{total} of {} {noun}", meta.total_stargazers)
    } else {
        format!("{total} {noun}")
    };
    out.push_str(&format!(
        "{repo} — {scope} · {} REST requests · 0 READMEs  [scale: {scale:?} → {scale_note}]\n",
        meta.requests
    ));
    out.push_str(
        "(Leads are description+language signals — descriptions often overstate; drill to confirm.)\n",
    );
    // [item 1] Report auto-derived config so the agent can refine it.
    if let Some(kw) = &meta.derived_keywords {
        out.push_str(&format!("(auto-keywords from repo: {})\n", kw.join(",")));
    }
    if let Some(st) = &meta.derived_stack {
        out.push_str(&format!("(auto-stack from repo: {})\n", st.join("+")));
    }
    if let Some(reason) = &meta.partial_reason {
        out.push_str(&format!("(partial: {reason})\n"));
    }

    // [item 2] Adopters first — they already depend on you. The single
    // highest-value signal: not "might be interested", but "is a user".
    let adopters: Vec<&UserProfile> = profiles.iter().filter(|p| p.adopter).collect();
    if !adopters.is_empty() {
        out.push_str(&format!("\n✅ ADOPTERS — already depend on you ({}):\n", adopters.len()));
        for p in &adopters {
            out.push_str(&format!(
                "  • {}{} — {}\n",
                p.login,
                reach_tag(p),
                p.adoption_evidence.as_deref().unwrap_or("(dependency declared)")
            ));
        }
    } else if meta.noun != "users" {
        // Only meaningful for a repo seed (there's a package to depend on).
        out.push_str(
            "\n✅ ADOPTERS: none found — no screened stargazer's repo declares the package as a dependency.\n",
        );
    }

    // Group by archetype, sorted by traction within each.
    let mut buckets: std::collections::BTreeMap<u8, (Archetype, Vec<&UserProfile>)> =
        std::collections::BTreeMap::new();
    for p in profiles {
        buckets
            .entry(p.archetype.rank())
            .or_insert_with(|| (p.archetype, Vec::new()))
            .1
            .push(p);
    }
    for (_, (_, members)) in buckets.iter_mut() {
        members.sort_by(|a, b| b.max_stars.cmp(&a.max_stars).then(b.total_stars.cmp(&a.total_stars)));
    }

    // Focused selection (filter → rank → take) when requested; otherwise
    // the full multi-lens browse. Cohorts render in both modes.
    if let Some(sel) = selection {
        out.push_str(&render_selection(profiles, sel));
    }

    if selection.is_none() {
    // ── Most relevant devs first (the answer to "who matters") ──
    // The screening goal is relevance, not traction, so this leads — and
    // it cross-cuts cohorts, surfacing on-domain repos that the star-ranked
    // cohort lines bury.
    let mut hits: Vec<&UserProfile> = profiles.iter().filter(|p| p.strong_hit).collect();
    // Rank by distinct-keyword breadth (the visible `Nkw` prefix), then
    // traction — once a repo clears the ≥2-keyword on-domain bar, stars are
    // a credibility signal, so a 40★ builder outranks a 0★ name-match at the
    // same breadth. Score only breaks remaining ties.
    hits.sort_by(|a, b| {
        b.hit_terms
            .len()
            .cmp(&a.hit_terms.len())
            .then(b.relevant[0].stars.cmp(&a.relevant[0].stars))
            .then(b.hit_score.cmp(&a.hit_score))
    });
    if !cfg.relevance_keywords.is_empty() {
        let cap = match scale {
            Scale::Small | Scale::Medium => 12,
            Scale::Large => 8,
            Scale::Extreme => 5,
        };
        out.push_str(&format!(
            "\n★ MOST RELEVANT — on-domain repo per dev (≥2 keyword hits), ranked by `Nkw` then traction · keys=[{}] ({} devs):\n",
            cfg.relevance_keywords.join(","),
            hits.len()
        ));
        for p in hits.iter().take(cap) {
            out.push_str(&format!("  • {}\n", relevance_line(p)));
        }
        if hits.len() > cap {
            out.push_str(&format!("  …+{} more\n", hits.len() - cap));
        }

        // Weak single-keyword matches: kept as a one-line footnote so they
        // are available without competing with the real leads.
        let weak: Vec<&UserProfile> = profiles
            .iter()
            .filter(|p| !p.strong_hit && !p.relevant.is_empty())
            .collect();
        if !weak.is_empty() {
            let listed: Vec<String> = weak
                .iter()
                .take(10)
                .map(|p| format!("{}/{}({})", p.login, p.relevant[0].name, p.hit_terms.join("/")))
                .collect();
            out.push_str(&format!(
                "  ~ weak 1-keyword matches ({}): {}{}\n",
                weak.len(),
                listed.join(", "),
                if weak.len() > 10 { ", …" } else { "" }
            ));
        }
    }

    // ── Popularity / reach lens — coding legends, regardless of domain ──
    let mut notable: Vec<&UserProfile> = profiles
        .iter()
        .filter(|p| p.max_stars >= 50 || p.followers.map(|f| f >= 200).unwrap_or(false))
        .collect();
    notable.sort_by(|a, b| {
        b.followers
            .unwrap_or(0)
            .cmp(&a.followers.unwrap_or(0))
            .then(b.max_stars.cmp(&a.max_stars))
            .then(b.total_stars.cmp(&a.total_stars))
    });
    if !notable.is_empty() {
        out.push_str("\n🏆 NOTABLE — biggest reach/traction (popularity lens):\n");
        for p in notable.iter().take(6) {
            let legend = if is_legend(p) { " ⟵ LEGEND" } else { "" };
            let top = p
                .repos
                .iter()
                .filter(|r| !r.fork)
                .max_by_key(|r| r.stars)
                .map(|r| format!("{} {}★", r.name, r.stars))
                .unwrap_or_default();
            out.push_str(&format!(
                "  • {}{} — {}★ total · top: {}{}\n",
                p.login,
                reach_tag(p),
                p.total_stars,
                top,
                legend
            ));
        }
    }

    // ── Quality lens — best-kept serious projects (maintenance + stars) ──
    let mut byq: Vec<&UserProfile> = profiles.iter().filter(|p| quality_score(p) >= 3.5).collect();
    byq.sort_by(|a, b| quality_score(b).total_cmp(&quality_score(a)));
    if !byq.is_empty() {
        out.push_str("\n✦ QUALITY — best-kept projects (maintained + topic-tagged + active):\n");
        for p in byq.iter().take(5) {
            let best = p
                .repos
                .iter()
                .filter(|r| !r.fork)
                .max_by(|a, b| repo_quality(a).total_cmp(&repo_quality(b)));
            if let Some(r) = best {
                let topics = if r.topics.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", r.topics.iter().take(3).cloned().collect::<Vec<_>>().join(","))
                };
                out.push_str(&format!(
                    "  • {} — {} {}★ ({}) active {}{}\n",
                    p.login,
                    r.name,
                    r.stars,
                    r.lang.as_deref().unwrap_or("—"),
                    r.pushed,
                    topics
                ));
            }
        }
    }

    // ── Stack match: shares the seed project's languages (PyO3 signal) ──
    // Catches relevant devs whose repo names don't contain the keywords.
    if !cfg.stack_languages.is_empty() {
        let already: std::collections::HashSet<&str> =
            hits.iter().take(20).map(|p| p.login.as_str()).collect();
        let mut stack: Vec<&UserProfile> = profiles
            .iter()
            .filter(|p| p.stack_match && !already.contains(p.login.as_str()))
            .collect();
        // Rank by depth of stack commitment (total in-stack repos): a serial
        // Rust+Python builder — the kglite/maturin pattern — outranks an
        // incidental one. This is the signal that catches architectural
        // peers keywords structurally cannot (descriptions don't name the
        // toolchain). We show the *counts*, not a sample repo: the most-
        // starred in-stack repo is often off-domain, so the verifiable
        // signal is "builds N things in your exact stack" — then drill.
        stack.sort_by(|a, b| {
            stack_depth(b)
                .cmp(&stack_depth(a))
                .then(b.total_stars.cmp(&a.total_stars))
        });
        if !stack.is_empty() {
            out.push_str(&format!(
                "\n⚙ STACK MATCH — builds in your stack ({}), ranked by depth · drill to confirm ({} devs):\n",
                cfg.stack_languages.join("+"),
                stack.len()
            ));
            for p in stack.iter().take(8) {
                let breakdown = p
                    .stack_lang_counts
                    .iter()
                    .map(|(l, n)| format!("{n} {l}"))
                    .collect::<Vec<_>>()
                    .join(" + ");
                // [item 4] co-location: repos combining all stack langs (PyO3).
                let coloc = match p.colocated_repos {
                    Some(n) if n > 0 => format!(" · {n} combine both (PyO3-style)"),
                    Some(_) => " · none combine both".to_string(),
                    None => String::new(),
                };
                out.push_str(&format!(
                    "  • {} — {} ({} in-stack of {} repos){}{}{}\n",
                    p.login,
                    breakdown,
                    stack_depth(p),
                    p.original_count,
                    coloc,
                    reach_tag(p),
                    contrib_tag(p),
                ));
            }
            if stack.len() > 8 {
                out.push_str(&format!("  …+{} more\n", stack.len() - 8));
            }
        }
    }
    } // end multi-lens browse (selection.is_none())

    // Devs already surfaced as leads above — skip them in the cohort detail
    // so it shows "the rest of the audience", not a re-list of the leads.
    let shown: std::collections::HashSet<&str> = profiles
        .iter()
        .filter(|p| p.strong_hit || p.stack_match)
        .map(|p| p.login.as_str())
        .collect();

    // ── Inventory: cohort counts (the audience shape) ──
    out.push_str("\nCOHORTS (drill 'cohort:<key>' for the full list):\n");
    for (_, (arch, members)) in &buckets {
        out.push_str(&format!(
            "  {:<20} {:>3}  {:<18} {}\n",
            arch.label(),
            members.len(),
            format!("cohort:{}", cohort_key(*arch)),
            cohort_blurb(*arch)
        ));
    }

    // ── Per-cohort detail, depth by scale, leads excluded ──
    for (_, (arch, members)) in &buckets {
        let n = members.len();
        let show = inline_quota(*arch, scale, n);
        if show == 0 {
            continue;
        }
        // Only members not already surfaced as leads above.
        let rest: Vec<&&UserProfile> = members.iter().filter(|p| !shown.contains(p.login.as_str())).collect();
        let lead_count = n - rest.len();
        if rest.is_empty() {
            continue;
        }
        out.push_str(&format!("\n▍{} ({}):\n", arch.label(), n));
        for p in rest.iter().take(show) {
            out.push_str(&format!("  • {}\n", user_overview_line(p)));
        }
        let mut tail = String::new();
        if rest.len() > show {
            tail.push_str(&format!("+{} more", rest.len() - show));
        }
        if lead_count > 0 {
            if !tail.is_empty() {
                tail.push_str(", ");
            }
            tail.push_str(&format!("{} shown as leads above", lead_count));
        }
        if !tail.is_empty() {
            out.push_str(&format!("  …{} — drill 'cohort:{}'\n", tail, cohort_key(*arch)));
        }
    }

    out.push_str(
        "\nDRILL: 'user:<login>' → portfolio · 'user:<login>/repo:<name>' → repo · '…/readme' → README gist · 'cohort:<name>' → full list\n",
    );
    out
}

/// Relevance line: on-domain repo, its language, a description (or topics
/// when the description is empty — the highest-signal leads often have no
/// description), and the keywords that matched.
fn relevance_line(p: &UserProfile) -> String {
    let r = &p.relevant[0];
    let lang = r.lang.as_deref().unwrap_or("—");
    let about = match r.desc.as_deref() {
        Some(d) => format!("\"{}\"", trunc(d, 85)),
        None if !r.topics.is_empty() => format!(
            "topics:[{}]",
            r.topics.iter().take(5).cloned().collect::<Vec<_>>().join(",")
        ),
        None => "(no description)".into(),
    };
    format!(
        "{}kw  {}/{} {}★ [{}] active {} — {} · matched: {}{}{}",
        p.hit_terms.len(),
        p.login,
        r.name,
        r.stars,
        lang,
        r.pushed,
        about,
        p.hit_terms.join(","),
        reach_tag(p),
        contrib_tag(p),
    )
}

fn cohort_blurb(a: Archetype) -> &'static str {
    match a {
        Archetype::Established => "high-traction repos",
        Archetype::SingleProject => "one standout repo",
        Archetype::Prolific => "many repos, low traction",
        Archetype::Casual => "a few modest repos",
        Archetype::Dormant => "no recent public pushes",
        Archetype::Consumer => "mostly forks / no original work",
    }
}

/// How many members of a cohort to show inline, by disclosure scale.
fn inline_quota(arch: Archetype, scale: Scale, n: usize) -> usize {
    let q = match (arch, scale) {
        // Established + single-project: the standouts — keep inline longest.
        (Archetype::Established | Archetype::SingleProject, Scale::Small) => n,
        (Archetype::Established | Archetype::SingleProject, Scale::Medium) => 8,
        (Archetype::Established | Archetype::SingleProject, Scale::Large) => 5,
        (Archetype::Established | Archetype::SingleProject, Scale::Extreme) => 0,
        // Prolific: sample a few, collapse the tail.
        (Archetype::Prolific, Scale::Small) => 6,
        (Archetype::Prolific, Scale::Medium) => 4,
        (Archetype::Prolific, _) => 0,
        // Casual: only at small scale.
        (Archetype::Casual, Scale::Small) => 3,
        (Archetype::Casual, _) => 0,
        // Dormant + consumers: always collapsed to the inventory count.
        (Archetype::Dormant | Archetype::Consumer, _) => 0,
    };
    q.min(n)
}

/// Resolve a `cohort:<key>` handle back to its archetype.
pub fn archetype_from_key(key: &str) -> Option<Archetype> {
    Some(match key {
        "established" => Archetype::Established,
        "single" => Archetype::SingleProject,
        "prolific" => Archetype::Prolific,
        "casual" => Archetype::Casual,
        "dormant" => Archetype::Dormant,
        "consumers" => Archetype::Consumer,
        _ => return None,
    })
}

/// Members of a cohort, rendered in full (the `cohort:<name>` drill).
pub fn render_cohort(arch: Archetype, profiles: &[UserProfile]) -> String {
    let mut members: Vec<&UserProfile> = profiles.iter().filter(|p| p.archetype == arch).collect();
    members.sort_by(|a, b| b.max_stars.cmp(&a.max_stars).then(b.total_stars.cmp(&a.total_stars)));
    let mut out = format!("{} ({}) — {}\n\n", arch.label(), members.len(), cohort_blurb(arch));
    for p in &members {
        out.push_str(&format!("  • {}\n", user_overview_line(p)));
    }
    out
}

/// Render a single user's full portfolio (drill-down level 1).
pub fn render_user(p: &UserProfile) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} — {} · {} original repos ({} forks) · {}★ total · active {}\n  languages: {}\n\n",
        p.login,
        p.archetype.label(),
        p.original_count,
        p.fork_count,
        p.total_stars,
        p.last_active,
        if p.top_langs.is_empty() { "—".into() } else { p.top_langs.join(", ") },
    ));
    let mut originals: Vec<&RepoLite> = p.repos.iter().filter(|r| !r.fork).collect();
    originals.sort_by(|a, b| b.stars.cmp(&a.stars).then(b.pushed.cmp(&a.pushed)));
    for r in originals.iter().take(30) {
        let arch = if r.archived { " [archived]" } else { "" };
        out.push_str(&format!("  • {}{}  (pushed {})\n", repo_line(r), arch, r.pushed));
    }
    if originals.len() > 30 {
        out.push_str(&format!("  …+{} more original repos\n", originals.len() - 30));
    }
    out
}

/// Render a single repo profile (drill-down level 2). The cheap version —
/// everything here is already in the cache, no new request.
pub fn render_repo(login: &str, r: &RepoLite) -> String {
    let topics = if r.topics.is_empty() {
        "—".into()
    } else {
        r.topics.join(", ")
    };
    format!(
        "{login}/{name}\n  {stars}★ · {forks} forks · {lang} · pushed {pushed} · created {created}{arch}\n  topics: {topics}\n  {desc}\n\n  (drill '…/repo:{name}/readme' for the README headline — 1 REST request)\n",
        name = r.name,
        stars = r.stars,
        forks = r.forks,
        lang = r.lang.as_deref().unwrap_or("—"),
        pushed = r.pushed,
        created = r.created,
        arch = if r.archived { " · ARCHIVED" } else { "" },
        desc = r.desc.as_deref().unwrap_or("(no description)"),
    )
}

/// Fetch a repo's README and compact it to a short headline gist. This is
/// the only drill that costs a request, and it's shortlist-only by design.
pub fn fetch_readme(repo: &str) -> Result<String, String> {
    let meta = github::gh_get(&format!("repos/{repo}/readme"))?;
    let url = meta
        .get("download_url")
        .and_then(Value::as_str)
        .ok_or("no README found")?;
    let body = ureq::get(url)
        .set("User-Agent", "mcp-methods")
        .call()
        .map_err(|e| format!("README fetch error: {e}"))?
        .into_string()
        .map_err(|e| format!("README decode error: {e}"))?;
    Ok(compact_readme(&body))
}

/// Strip badges / HTML / boilerplate and keep the first lines of real prose.
fn compact_readme(md: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut chars = 0usize;
    for line in md.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // Drop badge lines, HTML, comments, raw image refs.
        if t.starts_with("<!--") || t.starts_with('<') || t.starts_with("![") || t.contains("shields.io") || t.contains("badge") {
            continue;
        }
        kept.push(line.to_string());
        chars += line.len();
        if chars > 1200 || kept.len() >= 25 {
            kept.push("… (README truncated)".into());
            break;
        }
    }
    kept.join("\n")
}

// ---------------------------------------------------------------------------
// Seed-config derivation [item 1] + shortlist enrichment [items 2/4/6/7]
// ---------------------------------------------------------------------------

/// What to screen: a repo (screen its stargazers) or an explicit set of
/// users (screen them directly). The people-set differs; everything
/// downstream — projection, classification, enrichment, scoring,
/// selection — is identical.
#[derive(Debug, Clone)]
pub enum Seed {
    Repo(String),
    Users(Vec<String>),
}

impl Seed {
    /// Cache key + header label.
    pub fn key(&self) -> String {
        match self {
            Seed::Repo(r) => r.clone(),
            Seed::Users(u) => {
                let mut v = u.clone();
                v.sort();
                format!("users:{}", v.join(","))
            }
        }
    }
    /// Auto-detect from a free-form target: `owner/repo` → repo, else a
    /// comma-separated user list.
    pub fn detect(target: &str) -> Seed {
        let t = target.trim();
        if t.contains('/') && !t.contains(',') {
            Seed::Repo(t.to_string())
        } else {
            Seed::Users(
                t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            )
        }
    }
}

/// Run metadata returned alongside the profiles — what was auto-derived,
/// how much was screened, and whether the result is partial. Drives the
/// overview header and the scale/cost story.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScreenMeta {
    pub derived_keywords: Option<Vec<String>>,
    pub derived_stack: Option<Vec<String>>,
    pub total_stargazers: usize,
    pub screened: usize,
    pub partial: bool,
    pub partial_reason: Option<String>,
    pub requests: usize,
    /// "stargazers" or "users" — the noun for the header.
    #[serde(default)]
    pub noun: String,
}

/// Words too generic to be useful relevance keywords.
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "with", "is", "it", "its",
    "via", "using", "used", "based", "written", "powered", "support", "library", "lib", "tool",
    "tools", "framework", "simple", "lightweight", "fast", "high", "performance", "easy", "small",
    "modern", "minimal", "your", "you", "this", "that", "from", "by", "as", "at", "be", "are",
];

/// Auto-derive relevance keywords + stack from the seed repo itself, so a
/// caller can `screen_stargazers(repo)` with no hand-tuned config. Topics
/// and significant description words become keywords; the top languages
/// (by bytes) become the stack. Returns (keywords, stack); empty on error.
pub fn derive_config(repo: &str) -> (Vec<String>, Vec<String>) {
    let meta = match github::gh_get(&format!("repos/{repo}")) {
        Ok(m) => m,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let mut kw: Vec<String> = Vec::new();
    // Topics first — curated, high-signal.
    if let Some(topics) = meta.get("topics").and_then(Value::as_array) {
        for t in topics {
            if let Some(s) = t.as_str() {
                for tok in tokenize(s) {
                    if tok.len() >= 3 && !STOPWORDS.contains(&tok.as_str()) && !kw.contains(&tok) {
                        kw.push(tok);
                    }
                }
            }
        }
    }
    // Then significant description words.
    if let Some(desc) = meta.get("description").and_then(Value::as_str) {
        for tok in tokenize(desc) {
            if tok.len() >= 4 && !STOPWORDS.contains(&tok.as_str()) && !kw.contains(&tok) {
                kw.push(tok);
            }
        }
    }
    kw.truncate(14);

    // Stack: top languages by bytes.
    let stack = match github::gh_get(&format!("repos/{repo}/languages")) {
        Ok(langs) => {
            let mut v: Vec<(String, u64)> = langs
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, n)| n.as_u64().map(|b| (k.clone(), b)))
                        .collect()
                })
                .unwrap_or_default();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            v.into_iter().take(2).map(|(l, _)| l).collect()
        }
        Err(_) => Vec::new(),
    };
    (kw, stack)
}

/// [item 6] Follower count for one user — reach signal for outreach.
pub fn fetch_user_reach(login: &str) -> Option<u64> {
    let u = github::gh_get(&format!("users/{login}")).ok()?;
    u.get("followers").and_then(Value::as_u64)
}

/// [item 7] Repos a user contributes to but does not own, from their recent
/// public events — surfaces relevance the owned-repo list can't see.
pub fn fetch_contributions(login: &str) -> Vec<String> {
    let events = match github::gh_get(&format!("users/{login}/events/public?per_page=100")) {
        Ok(Value::Array(a)) => a,
        _ => return Vec::new(),
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let login_lc = login.to_lowercase();
    for ev in &events {
        let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(
            kind,
            "PushEvent" | "PullRequestEvent" | "IssuesEvent" | "CommitCommentEvent"
        ) {
            continue;
        }
        if let Some(full) = ev.get("repo").and_then(|r| r.get("name")).and_then(Value::as_str) {
            let owner = full.split('/').next().unwrap_or("");
            if owner.to_lowercase() != login_lc && seen.insert(full.to_string()) {
                out.push(full.to_string());
            }
        }
    }
    out.truncate(6);
    out
}

/// [item 4] Count a dev's original repos that combine *all* stack languages
/// in one repo (the true PyO3/maturin co-location signal). Probes
/// `/languages` on stack-language repos, bounded to keep it cheap.
pub fn probe_colocation(p: &UserProfile, stack: &[String], max_probes: usize) -> usize {
    let in_stack = |l: &str| stack.iter().any(|s| s.eq_ignore_ascii_case(l));
    let mut probed = 0;
    let mut colocated = 0;
    for r in p.repos.iter().filter(|r| !r.fork) {
        if probed >= max_probes {
            break;
        }
        // Only probe repos whose primary language is in-stack (likeliest
        // to also contain the other stack language).
        if !r.lang.as_deref().map(in_stack).unwrap_or(false) {
            continue;
        }
        probed += 1;
        if let Ok(langs) = github::gh_get(&format!("repos/{}/{}/languages", p.login, r.name)) {
            if let Some(obj) = langs.as_object() {
                let present: std::collections::HashSet<String> =
                    obj.keys().map(|k| k.to_lowercase()).collect();
                if stack.iter().all(|s| present.contains(&s.to_lowercase())) {
                    colocated += 1;
                }
            }
        }
    }
    colocated
}

/// [item 2] Find which of `logins` actually depend on the seed package.
/// Code-searches dependency manifests for the package name, intersects the
/// owners with the stargazer set, and verifies a bounded (whole-word) match
/// against the real manifest line to reject substring collisions
/// (e.g. `pkglite` ⊃ `kglite`). Returns login → evidence line.
pub fn find_adopters(
    pkg: &str,
    logins: &std::collections::HashSet<String>,
) -> std::collections::HashMap<String, String> {
    let mut found = std::collections::HashMap::new();
    if pkg.is_empty() {
        return found;
    }
    let logins_lc: std::collections::HashSet<String> =
        logins.iter().map(|l| l.to_lowercase()).collect();
    let manifests = ["Cargo.toml", "pyproject.toml", "requirements.txt", "package.json"];
    for mf in manifests {
        let q = format!("{pkg}+filename:{mf}");
        let results = match github::gh_get(&format!("search/code?q={q}&per_page=30")) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let items = match results.get("items").and_then(Value::as_array) {
            Some(a) => a,
            None => continue,
        };
        for it in items {
            let full = it
                .get("repository")
                .and_then(|r| r.get("full_name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let path = it.get("path").and_then(Value::as_str).unwrap_or("");
            let owner = full.split('/').next().unwrap_or("");
            if owner.is_empty() || !logins_lc.contains(&owner.to_lowercase()) {
                continue;
            }
            if found.contains_key(&owner.to_lowercase()) {
                continue;
            }
            // Verify a bounded match against the actual manifest line.
            if let Some(line) = verify_dependency(full, path, pkg) {
                found.insert(owner.to_lowercase(), format!("{full}/{path}: {line}"));
            }
        }
    }
    found
}

/// Fetch a manifest and return the line that declares `pkg` as a bounded
/// token (rejecting substring collisions), or None.
fn verify_dependency(full_name: &str, path: &str, pkg: &str) -> Option<String> {
    let url = format!("https://raw.githubusercontent.com/{full_name}/HEAD/{path}");
    let body = ureq::get(&url)
        .set("User-Agent", "mcp-methods")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let pkg_lc = pkg.to_lowercase();
    for line in body.lines() {
        let l = line.to_lowercase();
        if let Some(idx) = l.find(&pkg_lc) {
            let before = l[..idx].chars().next_back();
            let after = l[idx + pkg_lc.len()..].chars().next();
            let boundary = |c: Option<char>| {
                c.map(|c| !(c.is_alphanumeric() || c == '_' || c == '-')).unwrap_or(true)
            };
            if boundary(before) && boundary(after) {
                return Some(line.trim().chars().take(80).collect());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Orchestration (fetch)
// ---------------------------------------------------------------------------

/// Fetch a repo's stargazer logins, most-recent first (owner excluded).
/// Uncapped — the caller samples [item 9].
pub fn fetch_stargazer_logins(repo: &str) -> Result<Vec<String>, String> {
    if let Some(err) = crate::git_refs::validate_repo(repo) {
        return Err(err);
    }
    // STARRED_AT ordering isn't available on REST stargazers without a
    // preview media type; the default order is ascending by starred date,
    // so the *last* pages are the most recent — reverse to get most-recent
    // first for sampling.
    let endpoint = format!("repos/{repo}/stargazers?per_page=100");
    let pages = github::gh_get_paginated(&endpoint)?;
    let owner = repo.split('/').next().unwrap_or("").to_lowercase();
    let mut logins: Vec<String> = pages
        .iter()
        .filter_map(|u| u.get("login").and_then(Value::as_str).map(String::from))
        .filter(|l| l.to_lowercase() != owner)
        .collect();
    logins.reverse();
    Ok(logins)
}

/// Fetch + project one user's owned repos. Returns (repos, capped).
pub fn fetch_portfolio(login: &str, cfg: &ScreenConfig) -> Result<(Vec<RepoLite>, bool), String> {
    let endpoint = format!("users/{login}/repos?sort=pushed&per_page=100");
    let raw = github::gh_get_paginated(&endpoint)?;
    let capped = raw.len() >= cfg.max_repos_per_user;
    let repos: Vec<RepoLite> = raw
        .iter()
        .take(cfg.max_repos_per_user)
        .map(project_repo)
        .collect();
    Ok((repos, capped))
}

/// Full screen: derive config if needed [item 1], sample stargazers [item 9],
/// fetch portfolios, classify, then enrich the shortlist [items 2/4/6/7].
/// Returns the profiles, run metadata, and the *effective* config (so a
/// cached re-rank can reuse the derived keywords/stack).
pub fn run_screen(
    seed: &Seed,
    cfg_in: &ScreenConfig,
) -> Result<(Vec<UserProfile>, ScreenMeta, ScreenConfig), String> {
    let mut cfg = cfg_in.clone();
    let mut meta = ScreenMeta::default();

    // The seed package (for adoption) and the people-set differ by seed type;
    // everything downstream is identical.
    let (pkg, all): (String, Vec<String>) = match seed {
        Seed::Repo(repo) => {
            meta.noun = "stargazers".into();
            // [item 1] Auto-derive missing config from the seed repo itself.
            if cfg.relevance_keywords.is_empty() || cfg.stack_languages.is_empty() {
                let (kw, st) = derive_config(repo);
                meta.requests += 2;
                if cfg.relevance_keywords.is_empty() && !kw.is_empty() {
                    cfg.relevance_keywords = kw.clone();
                    meta.derived_keywords = Some(kw);
                }
                if cfg.stack_languages.is_empty() && !st.is_empty() {
                    cfg.stack_languages = st.clone();
                    meta.derived_stack = Some(st);
                }
            }
            let pkg = repo.rsplit('/').next().unwrap_or("").to_string();
            // [item 9] Sample: most-recent N.
            let all = fetch_stargazer_logins(repo)?;
            meta.requests += all.len() / 100 + 1;
            (pkg, all)
        }
        Seed::Users(users) => {
            meta.noun = "users".into();
            // Explicit user set: no seed repo, so no auto-derive and no
            // adoption package. Relatedness needs `keywords` to be useful.
            (String::new(), users.clone())
        }
    };
    meta.total_stargazers = all.len();
    let logins: Vec<String> = match cfg.max_stargazers {
        Some(c) => all.into_iter().take(c).collect(),
        None => all,
    };

    // Portfolios + classify, with graceful rate-limit handling [item 9].
    let mut profiles = Vec::with_capacity(logins.len());
    for login in &logins {
        match fetch_portfolio(login, &cfg) {
            Ok((repos, capped)) => profiles.push(profile_user(login, repos, capped, &cfg)),
            Err(e) if e.to_lowercase().contains("rate limit") => {
                meta.partial = true;
                meta.partial_reason = Some(format!(
                    "GitHub rate limit hit after {} of {} stargazers — retry later or set max_stargazers",
                    profiles.len(),
                    logins.len()
                ));
                break;
            }
            Err(_) => profiles.push(profile_user(login, Vec::new(), false, &cfg)),
        }
    }
    meta.screened = profiles.len();
    meta.requests += profiles.len();
    if !meta.partial && meta.screened < meta.total_stargazers {
        meta.partial = true;
        meta.partial_reason = Some(format!(
            "screened {} most-recent of {} stargazers (max_stargazers cap)",
            meta.screened, meta.total_stargazers
        ));
    }

    enrich(&pkg, &mut profiles, &cfg, &mut meta);
    normalize_scores(&mut profiles);
    Ok((profiles, meta, cfg))
}

/// Bounded shortlist enrichment: adoption [item 2] across all screened,
/// then reach [item 6] + contributions [item 7] + co-location [item 4] for
/// the highest-priority candidates. Keeps the per-call cost bounded so the
/// bulk pass stays cheap.
fn enrich(pkg: &str, profiles: &mut [UserProfile], cfg: &ScreenConfig, meta: &mut ScreenMeta) {
    // [item 2] Adoption — who actually depends on the seed package. Skipped
    // for a Users seed (no package).
    let logins: std::collections::HashSet<String> =
        profiles.iter().map(|p| p.login.clone()).collect();
    let adopters = if pkg.is_empty() {
        std::collections::HashMap::new()
    } else {
        meta.requests += 4;
        find_adopters(pkg, &logins)
    };
    for p in profiles.iter_mut() {
        if let Some(ev) = adopters.get(&p.login.to_lowercase()) {
            p.adopter = true;
            p.adoption_evidence = Some(ev.clone());
        }
    }

    // Shortlist = adopters ∪ keyword leads ∪ stack matches, priority-ordered,
    // bounded so enrichment cost stays modest.
    let mut idx: Vec<usize> = (0..profiles.len())
        .filter(|&i| profiles[i].adopter || profiles[i].strong_hit || profiles[i].stack_match)
        .collect();
    idx.sort_by(|&a, &b| {
        let key = |p: &UserProfile| (p.adopter, p.strong_hit, stack_depth(p), p.total_stars);
        let (ka, kb) = (key(&profiles[a]), key(&profiles[b]));
        kb.cmp(&ka)
    });
    idx.truncate(15);

    // Legend candidates: the highest-traction stargazers overall (by total
    // stars) — may be off-domain, but we want follower counts to answer
    // "is one of my stargazers a coding legend?".
    let mut legend: Vec<usize> = (0..profiles.len()).collect();
    legend.sort_by(|&a, &b| profiles[b].total_stars.cmp(&profiles[a].total_stars));
    legend.truncate(8);

    // Reach (followers) for shortlist ∪ legend candidates.
    let mut reach_idx = idx.clone();
    for l in legend {
        if !reach_idx.contains(&l) {
            reach_idx.push(l);
        }
    }
    reach_idx.truncate(20);
    for &i in &reach_idx {
        if profiles[i].followers.is_none() {
            if let Some(f) = fetch_user_reach(&profiles[i].login) {
                profiles[i].followers = Some(f);
            }
            meta.requests += 1;
        }
    }

    // [item 7] Contributions for the domain shortlist.
    for &i in &idx {
        let c = fetch_contributions(&profiles[i].login);
        meta.requests += 1;
        if !c.is_empty() {
            profiles[i].contributes_to = c;
        }
    }

    // [item 4] Co-location for the top stack matches by depth (the ranking
    // shown in the overview), independent of the lead ordering.
    if !cfg.stack_languages.is_empty() {
        let mut stack_idx: Vec<usize> =
            (0..profiles.len()).filter(|&i| profiles[i].stack_match).collect();
        stack_idx.sort_by(|&a, &b| stack_depth(&profiles[b]).cmp(&stack_depth(&profiles[a])));
        for &i in stack_idx.iter().take(5) {
            let n = probe_colocation(&profiles[i], &cfg.stack_languages, 6);
            profiles[i].colocated_repos = Some(n);
            meta.requests += 6;
        }
    }
}

/// Per-repo "quality" proxy from cheap signals. Maintenance dominates so
/// this lens stays *distinct* from raw popularity: stars only validate (and
/// are capped, so a mega-hit can't buy a quality slot), while recency and
/// curation drive the score and dormancy is penalised — a tidy, active,
/// topic-tagged repo outranks an abandoned star-magnet.
fn repo_quality(r: &RepoLite) -> f64 {
    let mut q = 0.0;
    if r.desc.is_some() {
        q += 1.0;
    }
    if !r.topics.is_empty() {
        q += 1.0;
    }
    // Recency is the strongest maintenance signal.
    q += if r.pushed.as_str() >= "2026-01" {
        2.0
    } else if r.pushed.as_str() >= "2025-01" {
        1.0
    } else if r.pushed.as_str() < "2023-01" {
        -2.0
    } else {
        0.0
    };
    if r.archived {
        q -= 2.0;
    }
    // Stars validate but can't dominate — capped at ~ln(20).
    q += ((r.stars + 1) as f64).ln().min(3.0);
    q
}

/// Best repo-quality across a dev's original repos.
fn quality_score(p: &UserProfile) -> f64 {
    p.repos
        .iter()
        .filter(|r| !r.fork)
        .map(repo_quality)
        .fold(0.0_f64, f64::max)
}

/// A "coding legend": large audience or a genuinely popular project.
fn is_legend(p: &UserProfile) -> bool {
    p.followers.map(|f| f >= 1000).unwrap_or(false) || p.max_stars >= 1000 || p.total_stars >= 2500
}

// ---------------------------------------------------------------------------
// Drill-down + in-memory session store (the MCP tool surface)
// ---------------------------------------------------------------------------

/// Resolve a drill `element_id` against an already-screened profile set.
/// Mirrors `github_issues`' `element_id` convention:
///   `cohort:<key>` · `user:<login>` · `user:<login>/repo:<name>` ·
///   `user:<login>/repo:<name>/readme` (the only drill that costs a request).
pub fn drill(profiles: &[UserProfile], element_id: &str) -> String {
    if let Some(key) = element_id.strip_prefix("cohort:") {
        return match archetype_from_key(key) {
            Some(a) => render_cohort(a, profiles),
            None => format!(
                "unknown cohort '{key}'. Try: established, single, prolific, casual, dormant, consumers"
            ),
        };
    }
    let rest = element_id.strip_prefix("user:").unwrap_or(element_id);
    let mut parts = rest.splitn(2, "/repo:");
    let login = parts.next().unwrap_or("");
    let prof = match profiles.iter().find(|p| p.login.eq_ignore_ascii_case(login)) {
        Some(p) => p,
        None => return format!("no such stargazer in this screen: '{login}'"),
    };
    match parts.next() {
        None => render_user(prof),
        Some(repo_part) => {
            let (rname, want_readme) = match repo_part.strip_suffix("/readme") {
                Some(n) => (n, true),
                None => (repo_part, false),
            };
            let r = match prof.repos.iter().find(|r| r.name.eq_ignore_ascii_case(rname)) {
                Some(r) => r,
                None => return format!("{login} has no repo '{rname}' in this screen"),
            };
            if want_readme {
                let full = format!("{login}/{rname}");
                match fetch_readme(&full) {
                    Ok(gist) => format!("README — {full}\n\n{gist}"),
                    Err(e) => e,
                }
            } else {
                render_repo(login, r)
            }
        }
    }
}

/// One cached screen: the enriched profiles, run metadata, and the
/// *effective* config (with any auto-derived keywords/stack) so a re-rank
/// can reuse them.
#[derive(Clone, Serialize, Deserialize)]
pub struct CachedScreen {
    pub profiles: Vec<UserProfile>,
    pub meta: ScreenMeta,
    pub cfg: ScreenConfig,
}

/// In-memory store of screened profile sets, keyed by seed repo. Lives for
/// the server's lifetime — the stargazer-screen analogue of `ElementCache`.
#[derive(Default)]
pub struct ScreenStore {
    store: std::collections::HashMap<String, CachedScreen>,
}

impl ScreenStore {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Carry the (keyword-independent) enrichment fields from a cached profile
/// onto a freshly re-classified one, so a free re-rank keeps the expensive
/// follower/adoption/co-location/contribution data.
fn carry_enrichment(fresh: &mut UserProfile, cached: &UserProfile) {
    fresh.followers = cached.followers;
    fresh.adopter = cached.adopter;
    fresh.adoption_evidence = cached.adoption_evidence.clone();
    fresh.colocated_repos = cached.colocated_repos;
    fresh.contributes_to = cached.contributes_to.clone();
}

/// One-call entry point for the `screen_stargazers` MCP tool. Without
/// `element_id`: build (or reuse) the screen and return the overview —
/// re-classified against the current keywords/stack so the agent can
/// re-key a cached fetch for free. With `element_id`: drill the cached set.
pub fn screen_dispatch(
    store: &std::sync::Mutex<ScreenStore>,
    seed: &Seed,
    cfg: &ScreenConfig,
    selection: Option<&Selection>,
    element_id: Option<&str>,
    refresh: bool,
) -> String {
    let key = seed.key();
    // Drill path — pure cache hit, no network (README excepted).
    if let Some(eid) = element_id {
        let guard = store.lock().unwrap();
        return match guard.store.get(&key) {
            Some(c) => drill(&c.profiles, eid),
            None => format!(
                "No screen cached for {key}. Call screen(target=\"{key}\") first (no element_id) to build it."
            ),
        };
    }

    // Overview path — reuse the cached fetch unless refreshing, re-classifying
    // with the live cfg (free re-rank) while carrying enrichment over.
    if !refresh {
        let guard = store.lock().unwrap();
        if let Some(c) = guard.store.get(&key) {
            // Incoming cfg overrides cached, else keep cached (auto-derived).
            let eff = ScreenConfig {
                max_repos_per_user: cfg.max_repos_per_user,
                max_stargazers: cfg.max_stargazers.or(c.cfg.max_stargazers),
                relevance_keywords: if cfg.relevance_keywords.is_empty() {
                    c.cfg.relevance_keywords.clone()
                } else {
                    cfg.relevance_keywords.clone()
                },
                stack_languages: if cfg.stack_languages.is_empty() {
                    c.cfg.stack_languages.clone()
                } else {
                    cfg.stack_languages.clone()
                },
            };
            let mut reclassified: Vec<UserProfile> = c
                .profiles
                .iter()
                .map(|u| {
                    let mut p = profile_user(&u.login, u.repos.clone(), u.capped, &eff);
                    carry_enrichment(&mut p, u);
                    p
                })
                .collect();
            normalize_scores(&mut reclassified);
            return build_overview(&key, &reclassified, &c.meta, &eff, selection);
        }
    }

    match run_screen(seed, cfg) {
        Ok((profiles, meta, eff)) => {
            let out = build_overview(&key, &profiles, &meta, &eff, selection);
            store
                .lock()
                .unwrap()
                .store
                .insert(key, CachedScreen { profiles, meta, cfg: eff });
            out
        }
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repo(name: &str, fork: bool, stars: u64, lang: &str, desc: &str, pushed: &str) -> Value {
        json!({
            "name": name, "fork": fork, "archived": false,
            "language": lang, "stargazers_count": stars, "forks_count": 0,
            "pushed_at": format!("{pushed}T00:00:00Z"), "created_at": format!("{pushed}T00:00:00Z"),
            "topics": [], "description": desc,
        })
    }

    fn profiles_for(repos: Vec<Value>) -> UserProfile {
        let cfg = ScreenConfig { relevance_keywords: vec!["graph".into()], ..Default::default() };
        let lite: Vec<RepoLite> = repos.iter().map(project_repo).collect();
        profile_user("tester", lite, false, &cfg)
    }

    #[test]
    fn single_project_dev_detected() {
        let p = profiles_for(vec![
            repo("flagship", false, 179, "Lua", "graph preview plugin", "2025-05-29"),
            repo("misc1", false, 1, "Python", "small thing", "2025-04-01"),
            repo("misc2", false, 0, "Lua", "another", "2025-03-01"),
        ]);
        assert_eq!(p.archetype, Archetype::SingleProject);
        assert_eq!(p.flagship.as_ref().unwrap().name, "flagship");
        assert_eq!(p.relevant.len(), 1); // matched "graph"
    }

    #[test]
    fn prolific_builder_detected() {
        let mut repos = Vec::new();
        for i in 0..20 {
            repos.push(repo(&format!("r{i}"), false, 0, "Rust", "experiment", "2026-05-01"));
        }
        let p = profiles_for(repos);
        assert_eq!(p.archetype, Archetype::Prolific);
        assert_eq!(p.top_langs, vec!["Rust".to_string()]);
    }

    #[test]
    fn consumer_detected() {
        let p = profiles_for(vec![
            repo("fork1", true, 0, "JS", "someone elses", "2025-01-01"),
            repo("fork2", true, 0, "JS", "another fork", "2025-01-01"),
        ]);
        assert_eq!(p.archetype, Archetype::Consumer);
    }

    #[test]
    fn projection_drops_empty_desc() {
        let r = project_repo(&repo("x", false, 0, "Rust", "", "2025-01-01"));
        assert!(r.desc.is_none());
    }

    fn two_profiles() -> Vec<UserProfile> {
        let cfg = ScreenConfig { relevance_keywords: vec!["graph".into()], ..Default::default() };
        let flagship = profile_user(
            "solo",
            vec![project_repo(&repo("flag", false, 99, "Rust", "graph engine", "2025-05-01"))],
            false,
            &cfg,
        );
        let mut repos = Vec::new();
        for i in 0..8 {
            repos.push(project_repo(&repo(&format!("r{i}"), false, 0, "Go", "x", "2026-01-01")));
        }
        let prolific = profile_user("builder", repos, false, &cfg);
        vec![flagship, prolific]
    }

    #[test]
    fn drill_user_and_repo() {
        let p = two_profiles();
        assert!(drill(&p, "user:solo").contains("Single-project"));
        assert!(drill(&p, "user:solo/repo:flag").contains("99★"));
        assert!(drill(&p, "user:nobody").contains("no such stargazer"));
        assert!(drill(&p, "user:solo/repo:ghost").contains("no repo"));
    }

    #[test]
    fn drill_cohort() {
        let p = two_profiles();
        let out = drill(&p, "cohort:prolific");
        assert!(out.contains("Prolific builders"));
        assert!(out.contains("builder"));
        assert!(drill(&p, "cohort:bogus").contains("unknown cohort"));
    }

    #[test]
    fn dispatch_caches_then_drills_without_refetch() {
        // No network: pre-seed the store, then a drill must hit the cache.
        let store = std::sync::Mutex::new(ScreenStore::new());
        store.lock().unwrap().store.insert(
            "a/b".into(),
            CachedScreen { profiles: two_profiles(), meta: ScreenMeta::default(), cfg: ScreenConfig::default() },
        );
        let cfg = ScreenConfig::default();
        let out = screen_dispatch(&store, &Seed::Repo("a/b".into()), &cfg, None, Some("user:solo"), false);
        assert!(out.contains("Single-project"));
        // Missing repo → friendly "build it first".
        let miss = screen_dispatch(&store, &Seed::Repo("x/y".into()), &cfg, None, Some("user:solo"), false);
        assert!(miss.contains("No screen cached"));
    }
}
