# Deviations from the doctrine agent baseline

Local improvements in `AGENTS.md` and `.agents/skills/`, each with the concrete
reason it adds value in this repo. Adaptations (this repo's own commands, paths,
thresholds, counterparts) are not listed. Format and sync duty:
`../doctrine/reference/AGENTS.baseline.md`.

## Mail messages carry a required frontmatter TL;DR

- **Where:** `AGENTS.md` → "Mail interface" (Frontmatter)
- **Baseline says:** inbox notes route to the party who can act; the generic `notify` skill writes them.
- **Local rule:** every message opens with a YAML block (`date`, `from`, `re`, `status`); `status` is a one-paragraph TL;DR naming what is asked and what the recipient should do. The generic `notify` skill is not installed.
- **Why it adds value here:** work arrives as mail (42 archived messages against 2 PRs at the 2026-07-29 decision), and downstream agents sort inbox work from `status:` without opening each file; the generic skill's schema would regress that.
- **Upstream:** candidate
- **Last reviewed:** doctrine 0.2.0

## Read-inbox disposal is status-and-evidence-gated with a 30-day grace

- **Where:** `.agents/skills/read-inbox/SKILL.md` → "1. Reconcile the read archive"
- **Baseline says:** purge an archived message once it has a completed Status record, an archive timestamp older than seven full days, and verified replacements.
- **Local rule:** a record is disposal-eligible only when its Status footer names every durable replacement and those exist; eligible records move to a manifested archive batch, and deletion waits 30 days from that archive timestamp. Unmarked legacy records are never deleted.
- **Why it adds value here:** about 40 messages from May to July 2026 predate `dev-docs/`, so `inbox/read/` is their only record, and threads run for weeks (messages cite a prior message by path). An age-based or seven-day purge would delete the repo's coordination history on first run.
- **Upstream:** not proposed (repo-specific history)
- **Last reviewed:** doctrine 0.2.0

## add-todo asks whether the fix needs a release

- **Where:** `.agents/skills/add-todo/SKILL.md` → "3. Ground it" and "4. Choose the detail home"
- **Baseline says:** pin the fix site, scope the detail, add a lean backlink.
- **Local rule:** an entry on the publish path says in its detail doc whether the fix needs a release (workflow, `docs/` and `Makefile` changes do not); a claim checked and found not to apply is recorded as a negative with no todo line; an item that is itself a gate states how it will be proven to fail; durable design reference goes to the tracked `dev-documentation/`, not `plans/`.
- **Why it adds value here:** any code push to `main` fires an immutable crates.io and PyPI publish, so "does this need a release" decides how a fix may land; recorded negatives stopped re-audits of forwarded findings (roughly half of one forwarded audit already did not apply).
- **Upstream:** candidate
- **Last reviewed:** doctrine 0.2.0

## Review bar: a tool's effort level never licenses preferences

- **Where:** `AGENTS.md` → "Review — report what is broken, not what you would have written" (last paragraph)
- **Baseline says:** a finding names a concrete failing input, state or consequence (R15).
- **Local rule:** the same, plus: a review tool's effort or confidence level buys more speculative bugs, never permission to report preferences.
- **Why it adds value here:** review tools offer effort levels, and without the clause a higher level reads as a mandate for design opinions, which R15 files under planning (an inference about tool behaviour, not a recorded incident).
- **Upstream:** candidate
- **Last reviewed:** doctrine 0.2.0

## dev-docs is bounded by a lint gate, not a cleanup skill

- **Where:** `AGENTS.md` → "Build & test" (the R4 table) and `dev-docs/README.md` → "Declined"
- **Baseline says:** every file accumulation has a bound and an owner; the estate's owner for `dev-docs/` is the `dev-docs-cleanup` skill with purge tiers.
- **Local rule:** `make check-dev-docs` (part of `make lint`) fails above 20 files or a size cap; there is no purge tier or cleanup skill. The remedy at the bound is a decision: prune the backlog (fold fully shipped plans into `plans/shipped.md`) or add the tier that owns the growth.
- **Why it adds value here:** `dev-docs/` is a backlog index and a few plans and nothing generates into it, so a cleanup skill would have nothing to clean; the lint gate is the only owner that cannot be forgotten, and it already fired on 2026-11-26 (21 files).
- **Upstream:** not proposed
- **Last reviewed:** doctrine 0.2.0
