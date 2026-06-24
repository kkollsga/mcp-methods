//! Driver for the unified screening tool. Seeds on a repo (its stargazers)
//! or a user list, enriches + scores, and supports filter→rank→take views.
//!
//! Usage:
//!   screen <owner/repo | user[,user…]> [drill]
//!     [--keywords a,b] [--stack Rust,Python] [--max-stargazers N] [--refresh]
//!     [--preset outreach|peers|legends|intel|adopters] [--rank relatedness|popularity|effort|recency]
//!     [--top N] [--min-keywords N] [--active-since YYYY-MM-DD] [--adopters-only] [--stack-only]
//!   screen <seed> cohort:<key> | user:<login>[/repo:<name>[/readme]]

use mcp_methods::screen::{
    self, build_overview, normalize_scores, preset, run_screen, CachedScreen, Filters, RankBy,
    ScreenConfig, Seed, Selection,
};
use std::fs;

fn cache_path(seed: &Seed) -> String {
    format!(
        ".screen-cache-{}.json",
        seed.key().replace(['/', ',', ':'], "-")
    )
}

fn load_cache(seed: &Seed) -> Option<CachedScreen> {
    serde_json::from_str(&fs::read_to_string(cache_path(seed)).ok()?).ok()
}

fn csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn take(args: &[String], i: &mut usize) -> String {
    *i += 1;
    args.get(*i).cloned().unwrap_or_default()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: screen <owner/repo | user[,user…]> [drill] [flags]");
        std::process::exit(2);
    }
    let seed = Seed::detect(&args[0]);

    let mut drill: Option<String> = None;
    let mut refresh = false;
    let mut cfg = ScreenConfig::default();
    let mut preset_name: Option<String> = None;
    let mut rank: Option<RankBy> = None;
    let mut top = 10usize;
    let mut filters = Filters::default();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--keywords" => {
                cfg.relevance_keywords = csv(&take(&args, &mut i))
                    .into_iter()
                    .map(|s| s.to_lowercase())
                    .collect()
            }
            "--stack" => cfg.stack_languages = csv(&take(&args, &mut i)),
            "--max-stargazers" => cfg.max_stargazers = take(&args, &mut i).parse().ok(),
            "--preset" => preset_name = Some(take(&args, &mut i)),
            "--rank" => rank = RankBy::parse(&take(&args, &mut i)),
            "--top" => top = take(&args, &mut i).parse().unwrap_or(10),
            "--min-keywords" => filters.min_keywords = take(&args, &mut i).parse().ok(),
            "--active-since" => filters.active_since = Some(take(&args, &mut i)),
            "--adopters-only" => filters.adopters_only = true,
            "--stack-only" => filters.stack_only = true,
            "--refresh" => refresh = true,
            other if !other.starts_with("--") => drill = Some(other.to_string()),
            _ => {}
        }
        i += 1;
    }

    // Build the selection (preset, or explicit rank/filters, else none).
    let selection: Option<Selection> = if let Some(name) = &preset_name {
        preset(name, top)
    } else if rank.is_some() || filters_active(&filters) {
        Some(Selection {
            filters,
            rank: rank.unwrap_or(RankBy::Relatedness),
            label: "SELECTION".into(),
            take: top,
        })
    } else {
        None
    };

    if let Some(target) = drill {
        match load_cache(&seed) {
            Some(c) => print!("{}", screen::drill(&c.profiles, &target)),
            None => {
                eprintln!("no cache; run the overview first");
                std::process::exit(1);
            }
        }
        return;
    }

    if !refresh {
        if let Some(c) = load_cache(&seed) {
            eprintln!("(using cached fetch — pass --refresh to refetch)");
            let mut reclassified: Vec<_> = c
                .profiles
                .iter()
                .map(|u| {
                    let mut p = screen::profile_user(&u.login, u.repos.clone(), u.capped, &c.cfg);
                    p.followers = u.followers;
                    p.adopter = u.adopter;
                    p.adoption_evidence = u.adoption_evidence.clone();
                    p.colocated_repos = u.colocated_repos;
                    p.contributes_to = u.contributes_to.clone();
                    p
                })
                .collect();
            normalize_scores(&mut reclassified);
            let out = build_overview(
                &seed.key(),
                &reclassified,
                &c.meta,
                &c.cfg,
                selection.as_ref(),
            );
            print!("{out}");
            eprintln!("\n[overview size: {} bytes]", out.len());
            return;
        }
    }

    eprintln!("screening {} …", seed.key());
    match run_screen(&seed, &cfg) {
        Ok((profiles, meta, eff)) => {
            let out = build_overview(&seed.key(), &profiles, &meta, &eff, selection.as_ref());
            let _ = fs::write(
                cache_path(&seed),
                serde_json::to_string(&CachedScreen {
                    profiles,
                    meta,
                    cfg: eff,
                })
                .unwrap(),
            );
            print!("{out}");
            eprintln!("\n[overview size: {} bytes]", out.len());
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn filters_active(f: &Filters) -> bool {
    f.min_keywords.is_some()
        || f.min_stars.is_some()
        || f.active_since.is_some()
        || f.adopters_only
        || f.stack_only
        || f.min_relatedness_pct.is_some()
        || f.min_effort_pct.is_some()
}
