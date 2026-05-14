# Agent CLI / Skills Pattern — Strategic Analysis

**Status:** Research notes. No implementation decisions yet.
**Date:** 2026-05-14
**Audience:** Internal — for thinking through whether mcp-methods should pivot or extend toward the agent-CLI+skills pattern.

---

## TL;DR

The "shift away from MCP" that's been building since late 2025 is real and structural, not hype. Anthropic launched **Agent Skills** on Oct 16, 2025, then released the format as an **open standard** on Dec 18, 2025. Within weeks, ~40 agent products adopted the same `SKILL.md` format — Claude Code, OpenAI Codex CLI, Gemini CLI, GitHub Copilot, VS Code, Cursor, JetBrains Junie, JetBrains Goose, Letta, OpenHands, Mistral Vibe, Kiro, Spring AI, Snowflake, Databricks, Roo Code, Laravel Boost, and more.

**Skills don't replace MCP.** They sit at a different layer of the stack:

- **MCP** = tools / external connectivity ("the hands")
- **Skills** = procedural knowledge / workflow ("the brain")

The two are complementary in principle. In practice, however, MCP servers are *expensive in context* (a typical 5-server setup with 58 tools consumes ~55k tokens before any conversation starts), while a skill costs ~100 tokens at discovery and only loads its full body when the agent needs it. Operators with limited context budgets *do* drop MCP servers in favor of skills when the skill can encode the same workflow, even though strictly the two are different categories.

**Strategic decision (locked):** mcp-methods will add a **second train** alongside the existing MCP train — new workspace members in `crates/` plus a curated `skills/` library, all in the same repo. The existing MCP crates (`mcp-methods`, `mcp-methods-py`, `mcp-server`) are **frozen** in their current behavior: kglite's foundation is untouched, the pip wheel keeps shipping the same surface, the MCP framework keeps doing what it does. The new train is purely additive — it adds CLIs and skill files that consume the same primitives via the existing `mcp-methods` library crate.

---

## 1. What the pattern is

### 1.1 Definitions

An **Agent Skill** is a directory with at minimum a `SKILL.md` file:

```text
my-skill/
├── SKILL.md          # required — frontmatter (name + description) + instructions
├── scripts/          # optional — executable code (Python, Bash, JS, etc.)
├── references/       # optional — documentation, schemas, API specs
├── assets/           # optional — templates, examples
└── ...               # any additional files
```

The `SKILL.md` file uses YAML frontmatter for metadata and Markdown for the instructions the agent reads:

```yaml
---
name: pdf-processing
description: Extract text and tables from PDF files, fill forms, merge documents. Use when working with PDF files or when the user mentions PDFs, forms, or document extraction.
---

# PDF Processing

## Quick start

Use pdfplumber to extract text from PDFs:

```python
import pdfplumber

with pdfplumber.open("document.pdf") as pdf:
    text = pdf.pages[0].extract_text()
```

For advanced form filling, see [FORMS.md](FORMS.md).
```

### 1.2 Progressive disclosure (the core design principle)

Skills load in three stages:

| Level | When loaded | Token cost | Content |
|---|---|---|---|
| **1. Metadata** | Always (at startup, in system prompt) | ~100 tokens / skill | `name` + `description` from YAML frontmatter |
| **2. Instructions** | When skill is triggered (by agent decision or user `/skill-name`) | Under 5k tokens | Body of `SKILL.md` |
| **3. Resources & scripts** | As needed, on demand | Effectively unlimited | Bundled files; scripts execute via bash, code never enters context |

> "Progressive disclosure is the core design principle that makes Agent Skills flexible and scalable."
> — Anthropic engineering blog ([source](https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills))

### 1.3 Activation modes

Skills can be triggered two ways:

- **Implicit (model-invoked):** the agent matches the user's request against installed skills' descriptions and loads the relevant one automatically.
- **Explicit (user-invoked):** the user types `/skill-name` (Claude Code's slash-command UX) or the equivalent in other clients.

Authors control which modes apply per-skill:
- `disable-model-invocation: true` → user-only (for skills with side effects: `/deploy`, `/commit`, `/send-slack-message`)
- `user-invocable: false` → model-only (for background-knowledge skills users shouldn't manually invoke)

### 1.4 Where skills live (filesystem scopes)

Per Claude Code's implementation ([source](https://code.claude.com/docs/en/skills)):

| Scope | Path | Applies to |
|---|---|---|
| Enterprise | Managed settings | All org users |
| Personal | `~/.claude/skills/<name>/SKILL.md` | All your projects |
| Project | `.claude/skills/<name>/SKILL.md` | This project only |
| Plugin | `<plugin>/skills/<name>/SKILL.md` | Where plugin is enabled |

Conflict resolution: enterprise > personal > project; plugin skills use a `plugin-name:skill-name` namespace so they don't collide.

Each agent product has its own scope conventions but the `SKILL.md` files are portable across products — Claude Code, Codex CLI, Cursor, Gemini CLI etc. all read the same shape.

---

## 2. The SKILL.md anatomy

### 2.1 Required fields

```yaml
---
name: skill-name              # ≤64 chars, lowercase + numbers + hyphens, no "claude" or "anthropic"
description: What this skill does and when to use it    # ≤1024 chars
---
```

That's the entire required surface. Everything else is optional.

### 2.2 Claude Code extensions (not in the open standard)

Claude Code extends the format with additional frontmatter fields ([source](https://code.claude.com/docs/en/skills)):

| Field | Purpose |
|---|---|
| `when_to_use` | Additional trigger context, appended to description (counts toward the 1,536-char cap) |
| `argument-hint` | Autocomplete hint (e.g. `[issue-number]`) |
| `arguments` | Named positional arguments for `$name` substitution |
| `disable-model-invocation` | If true, only user can invoke |
| `user-invocable` | If false, only model can invoke |
| `allowed-tools` | Pre-approved tools for this skill (e.g. `Bash(git add *) Bash(git commit *)`) |
| `model` | Override model for this skill's turn |
| `effort` | Override effort level (low/medium/high/xhigh/max) |
| `context: fork` | Run skill in an isolated subagent context |
| `agent` | Which subagent type to use with `context: fork` |
| `hooks` | Hooks scoped to this skill's lifecycle |
| `paths` | Glob patterns limiting when skill is activated |
| `shell` | bash (default) or powershell for inline shell commands |

These are Claude Code-specific. Other clients may implement subsets or alternatives. The cross-vendor open standard is `name + description + markdown body`.

### 2.3 Dynamic context injection (Claude Code feature)

`SKILL.md` can include shell commands that **run before the skill content reaches the model**:

```markdown
---
description: Summarize uncommitted changes and flag risks
---

## Current changes

!`git diff HEAD`

## Instructions

Summarize the changes above in two or three bullets, then list any risks...
```

The `` !`git diff HEAD` `` line is preprocessed: Claude Code runs the command, replaces the line with its output, and only then sends the rendered skill to the model. The agent receives the actual diff in its prompt, not the command.

This pattern is powerful — it lets a skill capture **live context** (the current working tree, the active branch, the PR diff) without burning agent reasoning cycles on tool calls.

Multi-line variant uses fenced code blocks:

````markdown
## Environment
```!
node --version
npm --version
git status --short
```
````

### 2.4 Script bundles

Skills can include executable scripts in `scripts/`:

```text
my-skill/
├── SKILL.md
├── scripts/
│   └── validate.sh
└── references/
    └── api-schema.json
```

The `SKILL.md` references these and the agent invokes them via its bash tool. The script *output* enters context; the script *code* does not. This is the key efficiency win — a 500-line Python script that produces a 5-line summary costs only the 5 lines in agent context.

Scripts can be in any language the runtime supports (Python, Bash, JS, etc.). The path is referenced via `${CLAUDE_SKILL_DIR}` so the script resolves regardless of which scope the skill is installed at.

---

## 3. The driving trend — agentic CLI era

### 3.1 Why CLIs

The narrative is captured by Simon Willison ([source](https://simonwillison.net/2025/Oct/16/claude-skills/)):

> "you can grab a skills folder right now, point Codex CLI or Gemini CLI at it and say 'read pdf/SKILL.md and then create me a PDF describing this project' and it will work, despite those tools and models having no baked in knowledge of the skills system."

The point: **skills work without protocol negotiation**. The agent just reads the markdown and follows the instructions. CLI tools are the canonical runtime because:

1. They expose a uniform interface (stdin/stdout, exit codes)
2. They're version-controlled alongside source code
3. They're cross-platform (a `bash` invocation works on macOS/Linux/Windows-WSL)
4. They compose (`tool-a | tool-b | tool-c`)
5. The output IS the protocol — no need to define a schema

And the broader 2025 trend ([source](https://www.kdnuggets.com/top-5-agentic-coding-cli-tools)):

> "2025 was the start of the Agentic Era, a new wave of software development, moving beyond simple IDE chatbots and onto agentic CLIs. CLI-based options like Claude Code and OpenAI's Codex have gained significant traction among terminal-first developers."

### 3.2 The context-bloat driver

Per the comparison work that's emerged ([source](https://www.morphllm.com/claude-code-skills-mcp-plugins)):

> "A typical five-server [MCP] setup with 58 tools uses approximately 55,000 tokens before any conversation starts. However, Anthropic's Tool Search feature reduces this by 85% through on-demand tool discovery."

Even with Tool Search optimizations, MCP servers have a startup tax. Skills don't:

> "Skills are procedural knowledge (30-50 tokens each, loaded on-demand) while MCP servers are external tool connections (can use 50k+ tokens)."

A skill is ~100 tokens at discovery (name + description); the body only loads when triggered. You can install dozens of skills without measurable context impact.

### 3.3 Simon Willison's "Cambrian explosion" prediction

> "I expect we'll see a Cambrian explosion in Skills which will make this year's MCP rush look pedestrian by comparison."
> — Simon Willison, 2025-10-16

His three core arguments:

1. **Token efficiency:** "Each skill occupies only a few dozen extra tokens, with full details loaded only when needed."
2. **Simplicity of implementation:** "Markdown files with optional scripts — no protocol specification required. They feel a lot closer to the spirit of LLMs — throw in some text and let the model figure it out."
3. **Universal compatibility:** "You can point Codex CLI or Gemini CLI at skills folders regardless of their native integration."

His prediction has held: the ecosystem adoption (~40 products in 6 months) outpaced MCP's first-six-months adoption substantially.

### 3.4 Ecosystem adoption

From the [agentskills.io client showcase](https://agentskills.io/clients), as of May 2026:

**CLI agents:** Claude Code, OpenAI Codex CLI, Gemini CLI, Goose (Block), pi, Mistral Vibe, OpenCode, Autohand Code CLI

**IDE / desktop agents:** Claude (claude.ai), Cursor, VS Code (GitHub Copilot), JetBrains Junie, Amp, Roo Code, Kiro, Workshop, Piebald, Trae (ByteDance), Emdash, Firebender, OpenHands, Mux (Coder)

**Platform / enterprise:** Databricks Genie Code, Snowflake Cortex Code, Letta, Factory, Spring AI, Ona, Agentman, Command Code, Qodo, Laravel Boost, Google AI Edge Gallery, nanobot, fast-agent, VT Code

The standard is genuinely cross-vendor. A `SKILL.md` written for Claude Code reads identically in Codex CLI or Gemini CLI without modification.

---

## 4. Skills vs MCP — the honest comparison

### 4.1 The "tools vs knowledge" framing

Most thoughtful comparisons land on the same model ([source](https://systemprompt.io/guides/claude-skills-vs-agents-vs-mcp)):

> "MCP provides the tools and data access (The 'Hands'), while Skills provide the procedural knowledge and workflow (The 'Brain')"

And:

> "Use MCP when you need Claude to access external systems (GitHub, databases, APIs, browsers). Use Skills when you need Claude to know how to do something."

This framing is useful but slightly underplays the overlap. In practice, a well-designed skill can *encode* the same workflow that an MCP server provides, by directing the agent to use existing bash tools rather than a dedicated MCP server. Example: an MCP server that wraps `git` operations can be replaced by a `git-workflow` skill that tells the agent "use `git diff HEAD`, then `git add -p` interactively, then write a commit message in the format..." The skill costs ~100 tokens of discovery overhead; the MCP server costs thousands.

### 4.2 What MCP wins at

From the critical perspective ([source](https://medium.com/@alonisser/mcp-is-dead-or-mcp-vs-skills-revisited-daaa51b9a519)):

> "the protocol has auth, especially OAuth baked into it, which means we don't need to support 'always fresh tokens'…we let it use the user's own OAuth to a service."

MCP's real strengths:

1. **OAuth / per-user auth.** Skills have no built-in way to handle authentication beyond bash environment variables (or embedded secrets, which is dangerous). MCP servers can broker user-specific OAuth flows.
2. **Autonomous tool discovery at scale.** Skills depend on the model selecting the right one from descriptions; MCP's tool routing is protocol-level and deterministic. For agents with many similar capabilities, MCP's structure may be more reliable.
3. **Stateful connections.** MCP servers can hold connection state (database connections, websocket subscriptions, cached resources). Skills are stateless invocations.
4. **Non-developer environments.** Web-based Claude.ai users can't easily install or invoke skills; MCP server connections are first-class on those surfaces.

### 4.3 What skills win at

1. **Context efficiency.** ~100 tokens vs ~10k+.
2. **Implementation simplicity.** A markdown file vs a protocol-conformant server.
3. **Cross-vendor portability.** ~40 agent products vs MCP's narrower (though still meaningful) reach.
4. **Operator authoring.** A team lead writes a `SKILL.md` for "how we do code review" in 20 minutes; building an MCP server for the same is a weekend project.
5. **Version control alignment.** Skills live in `.claude/skills/` in the repo, get reviewed in PRs, tracked in git history. MCP servers are external processes with their own deployment.

### 4.4 The convergence thesis

The "MCP vs Skills" Medium piece's bottom line:

> "Skills provide immediate clarity through documentation, but this approach doesn't scale… MCP's autonomous discovery may ultimately prove superior at scale, despite current inefficiencies."

The realistic prediction: skills displace MCP for **operator-authored workflow / methodology / convention** content; MCP retains the **external-system integration with auth** niche. Most non-trivial setups use both.

For mcp-methods specifically, the question is which side our primitives belong on. The next section explores this.

---

## 5. CLI integration patterns — how skills use existing tooling

### 5.1 The canonical pattern

```yaml
---
name: codebase-grep
description: Search the codebase with ripgrep, with smart defaults for code search (ignore-case, type filters, context lines).
---

# Codebase grep

For most searches:
\```bash
rg --pretty --smart-case --context 2 "<pattern>" .
\```

For type-restricted searches:
\```bash
rg --type rs --context 2 "<pattern>" .          # Rust only
rg --type py --context 2 "<pattern>" .          # Python only
\```

For finding function definitions:
\```bash
rg --type rs --context 0 "fn \w+" .             # Rust functions
rg --type py --context 0 "def \w+" .            # Python functions
\```

When the result is too large (>200 matches), narrow the search with
`--type` or a more specific pattern before re-running.
```

The skill doesn't *contain* ripgrep. It teaches the agent how to use the ripgrep that's already installed via the system's bash. The skill is documentation + workflow; the CLI is the work.

### 5.2 The bundled-script pattern

For deterministic operations, skills bundle executable scripts:

```text
github-pr-fetcher/
├── SKILL.md
└── scripts/
    └── fetch_compact.py    # fetches PR + compacts the body, prints to stdout
```

```yaml
---
name: github-pr-fetcher
description: Fetch a GitHub PR with smart compaction (drops bot comments, collapses code blocks >50 lines).
---

# GitHub PR fetcher

Fetch and compact a PR:
\```bash
python3 ${CLAUDE_SKILL_DIR}/scripts/fetch_compact.py <owner/repo> <pr-number>
\```

The script reads GITHUB_TOKEN from env and produces a compacted text view.
```

Key efficiency point: the 200-line Python script never enters the agent's context. Only the script's *output* is loaded. This makes scripts substantially more efficient than asking the agent to call multiple low-level tools in sequence.

### 5.3 The dynamic-injection pattern (Claude Code-specific)

```yaml
---
description: Summarize recent commits with file-impact analysis
---

## Last 10 commits

!`git log --oneline -10`

## Files changed since main

!`git diff main --stat`

## Instructions

Looking at the commits + the cumulative diff stat above, summarize what
this branch has been working on...
```

The skill's `!`<command>`` lines run BEFORE the skill text reaches the model. The model sees:

```
## Last 10 commits

abc1234 add CSV exporter
def5678 fix race condition in pool
...
```

…not the bash command. This is a big efficiency win for workflows that always start from the same "what's the current state?" snapshot — the snapshot is pre-fetched once, not lazily through tool calls.

### 5.4 What this means for tool-publishing projects

If you're publishing primitives that an agent might use (ripgrep wrappers, github fetchers, parsers, formatters, validators…), the skill pattern argues for:

1. **Ship the CLIs.** Don't wrap them in a protocol. Just provide good binaries with sensible flags.
2. **Ship companion skills.** Each major workflow gets a `SKILL.md` that teaches the agent how to use the CLI effectively, with worked examples and "when to use" triggers.
3. **Optimize for output-as-context.** CLI output is what the agent sees. Make it scannable, structured (where useful), and concise.
4. **Compose, don't bundle.** A skill that uses `your-cli` + `jq` + `git` is more flexible than one that bundles all the logic.

This is structurally different from publishing an MCP server. The deployment shape is "drop the binary on PATH + drop the skill folder in `.claude/skills/`." No daemon, no JSON-RPC, no protocol handshake.

---

## 6. The implementation shape (locked: in-workspace train)

### 6.1 What we currently have that's useful in either world

mcp-methods's primitives are agnostic to the surface protocol:

| Primitive | What it does | Re-usable in the new train? |
|---|---|---|
| `ripgrep_files`, `ripgrep`, `ripgrep_lines` | In-process ripgrep with smart defaults | Yes — wraps cleanly as CLI |
| `github_discussions`, `git_api` | GitHub fetcher with smart compaction | Yes — CLI |
| `ElementCache` | Drill-down cache for collapsed elements | Yes — CLI (output JSON, skill teaches drilling) |
| `read_file` | Safe file reading with path-traversal protection | Yes — CLI |
| `list_dir` | Tree-formatted directory listing with annotation | Yes — CLI |
| `compact_text`, `collapse_code_blocks` | Text compaction | Yes — CLI (text in, compacted text out) |
| `html_to_text` | HTML → markdown converter | Yes — CLI |
| `McpServer` framework | rmcp-backed boot sequence | MCP-only — stays in the existing train |
| `Manifest` YAML schema | Server configuration | MCP-only — stays in the existing train |
| `Workspace` (clone-and-track) | GitHub repo workspace management | Yes — could ship as CLI (`mcp-workspace`) |

The primitives translate to CLI. The MCP framework stays where it is.

### 6.2 The chosen shape — second train alongside the MCP train

**Constraint:** The current MCP train (`mcp-methods`, `mcp-methods-py`, `mcp-server`) is frozen in its current behavior. kglite's library dependency, the pip wheel, the bundled `mcp-server` binary — all unchanged. Any work in the new train is purely additive.

**Workspace layout:**

```text
mcp-methods/                            # same repo, same workspace
├── Cargo.toml                          # workspace root
├── crates/
│   ├── mcp-methods/                    # UNTOUCHED — kglite's library dep
│   ├── mcp-methods-py/                 # UNTOUCHED — pyo3 bindings
│   ├── mcp-server/                     # UNTOUCHED — MCP CLI binary
│   ├── mcp-rg/             (NEW)       # ripgrep CLI w/ smart defaults
│   ├── mcp-github/         (NEW)       # GitHub fetcher w/ compaction
│   ├── mcp-compact/        (NEW)       # text/html compaction CLI
│   ├── mcp-list/           (NEW)       # list_dir CLI
│   └── …                               # additional CLIs as needed
├── skills/                 (NEW)       # curated SKILL.md library
│   ├── codebase-navigation/
│   │   └── SKILL.md
│   ├── github-research/
│   │   └── SKILL.md
│   └── …
├── docs/                                # extend Sphinx site with skills/CLI docs
├── examples/                            # current downstream-binary example stays
└── CHANGELOG.md                         # unified release log
```

Every new CLI is a workspace member. Each depends on the existing `mcp-methods` library crate for its underlying logic — `ripgrep_files()`, `github_discussions()`, `compact_text()` etc. all stay where they are. The CLI is a thin `clap` wrapper around the library function, producing operator-friendly stdout.

**The skills/ directory** is plain markdown. Operators clone (or symlink) skills into their `~/.claude/skills/` or project `.claude/skills/`. No build step; the SKILL.md files are the deliverable.

**Each CLI publishes as its own crates.io entry** (`mcp-rg`, `mcp-github`, etc.) — same pattern as ripgrep itself (which is `ripgrep` on crates.io but `rg` as the binary). The version line is coordinated with the rest of the workspace.

### 6.3 Why in-workspace beats a separate repo

1. **Shared primitives, single source of truth.** All CLIs depend on `crates/mcp-methods/`. One implementation, one set of tests, one CHANGELOG entry per behavior change. A separate repo would force either duplicating the primitives or vendoring them.

2. **Zero touch to the MCP train.** New crates are purely additive workspace members. `cargo build -p mcp-methods` produces the exact same artifact whether `crates/mcp-rg/` exists or not. kglite's `mcp-methods = "0.3"` pin resolves identically. The existing CI gate (`cargo tree -p mcp-methods -e all | grep pyo3` returns empty) keeps passing.

3. **Release infrastructure already exists.** Multi-crate workspace, fmt/clippy precommit, PyPI wheel build, crates.io publish flow — all set up. New members slot into the existing pipeline.

4. **Future divergence is reversible.** If the new train eventually outgrows the `mcp-methods` identity, we can extract it to a separate repo later (`git filter-repo` preserves history; crates.io entries can be renamed via deprecate-and-republish). Starting in-workspace doesn't lock us in.

5. **Maintenance overhead doesn't double.** A separate repo means separate `.git`, separate GitHub repo, separate CI, separate CHANGELOG, separate CONTRIBUTING.md, separate README, separate docs site. For a one-maintainer project, that's a significant tax for no benefit while the trains are this closely coupled.

### 6.4 What stays MCP-specific

The new train doesn't touch:

- `crates/mcp-methods/` library code — kglite depends on this verbatim
- `crates/mcp-methods-py/` Python bindings — pip wheel surface unchanged
- `crates/mcp-server/` binary — operators using `pip install mcp-methods` keep getting the same `mcp-server` CLI on PATH
- The MCP YAML manifest schema (`workspace.applies_to`, `tools[].bundled:`, trust gates, etc.)
- The `Manifest::to_json()` JSON-shape contract
- The `Workspace` clone-and-track type
- The `McpServer` framework

The new CLIs are separate binaries on crates.io. They share the library crate but produce different artifacts. An operator who only wants the MCP path installs `mcp-methods` from pip and gets exactly what they got before. An operator who wants the skills path installs the new CLIs from cargo and drops skill folders in `~/.claude/skills/`.

### 6.5 Where the new train has an unfair advantage

We have months of operator feedback shaping the primitives:

- `ripgrep` defaults (file type, match_limit, smart_case — tuned for code-search)
- `ElementCache` drill-down (no other ripgrep-equivalent has this)
- `compact_discussion` with bot-filtering + maintainer-highlighting + collapse-by-size heuristics — substantially better than naive "fetch issue body" wrappers
- `Workspace` with atomic swap, GitHub clone-tracking, watch mode, `.env` walk-up

A skill that says "use `gh issue view`" gets a verbose, agent-context-hostile output. A skill that says "use `mcp-github fetch-compact <repo> <issue>`" gets our compaction. The value-add over commodity CLIs is real.

### 6.6 Where the new train risks being indistinct

The skill pattern doesn't *need* a framework. A skill is just a markdown file. Anyone can write one. If we publish `mcp-rg` and `mcp-github`, we compete with:

- The user's existing `rg` (faster install, more familiar)
- `gh` CLI (canonical for GitHub access)
- The community skills repositories on GitHub

Our differentiation has to come from quality-of-defaults + value-add features (`ElementCache`, smart compaction, tested-against-production heuristics). That's a real bar but the primitives ARE genuinely better than the commodity alternatives — we've spent months tuning them against actual agent-driven usage.

---

## 7. Risks and tradeoffs (given the locked choice)

### 7.1 Maintenance burden — the biggest risk

The new train adds workspace members + a skills library to maintain. Concretely:

- Each new CLI is a new crates.io entry with its own version + publish coordination
- Each new CLI needs per-platform binaries (or at least a `cargo install` story)
- Each skill in `skills/` needs to stay in sync with the underlying CLI defaults and primitive behavior
- Documentation expands (Sphinx site needs sections for the new CLIs + skill authoring guide)
- CHANGELOG entries cover both trains
- Release cadence has to handle both — a primitive change can affect both the MCP server AND a CLI

Mitigation: keep the new train **small**. Resist publishing a CLI for every primitive. Start with 2-3 high-value CLIs (`mcp-github`, `mcp-rg` with ElementCache integration, maybe `mcp-compact`) and 3-4 skills. If the train grows past ~5 CLIs and ~6 skills, that's a signal to consider Option C (separate repo) for the future.

### 7.2 Identity dilution

mcp-methods's README, README in crates.io, docs site landing page — all currently say "MCP utility methods." Adding a CLI/skills train means the project does two structurally-different things. New readers may be confused about which path is for them.

Mitigation: update README + docs to explicitly call out "Two trains, one library":
- Train 1 (MCP) — for operators who want a stdio MCP server, or downstream binaries like kglite
- Train 2 (CLI + Skills) — for operators using Claude Code / Codex CLI / Cursor / etc. who want efficient CLIs invoked from skill files

Both trains share the same underlying `crates/mcp-methods/` primitives.

### 7.3 The "MCP servers retain auth" lock-in

For any workflow that needs OAuth or per-user authentication, the CLI/skills shape doesn't cleanly cover MCP's territory. CLIs can read `GITHUB_TOKEN` from env, but OAuth flows, per-user token refresh, multi-tenant auth — these are MCP's domain.

Our current GitHub primitives use a single `GITHUB_TOKEN` from `.env`, which is fine for personal-machine use. The CLI train inherits that exact same auth model — no regression. If we ever target hosted/multi-tenant deployments, the MCP train retains the auth edge.

### 7.4 Skills as a moving target

The open standard was finalized Dec 2025; it's barely 5 months old at time of writing. Vendor-specific extensions (Claude Code's `disable-model-invocation`, dynamic injection, subagent context, hooks) are evolving. Building heavily against one vendor's extension set risks rework.

Mitigation: stick to the cross-vendor open standard (`name + description + markdown body + scripts/`) for the skills library. Use Claude Code-specific features (e.g. `!`<command>`` dynamic injection) only when the value is high enough to justify the lock-in, and document the dependency clearly in each skill.

### 7.5 Risks NOT in play (closed off by the locked decision)

- **No regression for kglite.** The MCP train is frozen. kglite's `mcp-methods = "0.3"` pin keeps resolving to library code that hasn't changed.
- **No project rename / crates.io entry churn.** New CLIs get new names; the existing `mcp-methods` crate stays the same name with the same purpose.
- **No deprecation timeline.** The MCP train doesn't have an end-of-life. Both trains run indefinitely.
- **No multi-repo coordination.** Everything stays in one workspace, one CI, one release log.

---

## 8. Open questions for implementation planning

Now-relevant questions (since the workspace shape is locked):

1. **Which 2-3 CLIs ship first?** The high-value candidates: `mcp-github` (compaction + ElementCache uniquely ours), `mcp-rg` (ripgrep wrapper with code-search defaults), `mcp-compact` (text/html compaction). The lowest-value: `mcp-list` (operators have `ls`/`tree`/`find`), `mcp-read-file` (operators have `cat`/`head`/`sed`). The Workspace clone-and-track might justify `mcp-workspace` if/when an operator asks for it outside the MCP context.

2. **Which 3-4 skills ship first?** Probable candidates:
   - `codebase-navigation` — composes `mcp-rg` + `list_dir` (or system equivalents) for code exploration workflows
   - `github-research` — composes `mcp-github` + `gh` CLI for issue/PR/diff fetching with compaction
   - `text-compaction` — wraps `mcp-compact` for the "I have 500k tokens of context, help me trim" use case
   - One more domain-specific (TBD based on what operators actually request)

3. **Crate naming.** Lean: `mcp-rg`, `mcp-github`, `mcp-compact` (the "mcp" prefix is a historical artifact; rebrand only if the train outgrows the identity). Alternatives considered: `mcp-methods-rg` (verbose), `kollsga-rg` (personal-brand), `agent-rg` (generic). The `mcp-*` prefix is fine.

4. **How are skills distributed?** Three options:
   - **Git clone + symlink:** operators clone the repo, symlink `skills/codebase-navigation/` into `~/.claude/skills/`. Simplest, no packaging.
   - **CLI installer:** `cargo install mcp-skills-installer` that copies skills to the right location. More effort, more polished.
   - **Plugin manifest:** Claude Code supports plugins; we could ship as a plugin. Cleanest but Claude-Code-specific.

   For starting: git clone + symlink. Add an installer if/when operators ask.

5. **Does kglite want CLI/skills equivalents of their patterns?** A `kglite-cypher` CLI + `cypher-query` skill could reduce their Python wrapper maintenance. Worth asking when they're past 0.9.30.

6. **What's the right interaction model between CLIs and skills?** Two patterns:
   - **CLI ships with default behavior; skill teaches advanced usage.** `mcp-github fetch-pr 123` just works; the skill teaches `mcp-github fetch-pr 123 --compact --drill-into cb_1` for advanced cases.
   - **CLI is minimal; skill encodes the recommended invocation.** `mcp-github fetch-pr` requires the skill to know what flags to pass for compaction.

   First pattern (good defaults + skill for advanced) is operator-friendlier. Lean that direction.

7. **Skills version-pinning vs CLI version-pinning.** If a skill says "use `mcp-github fetch-pr <number>`" and the CLI changes its argument shape in a later version, the skill breaks. How do we coordinate?
   - Each skill declares a minimum CLI version in its description ("Requires `mcp-github` ≥ 0.4.0")
   - Or: every release of the skills library is paired with a tested CLI version pin
   - Or: CLIs treat their arg shape as a public API contract and follow semver
   - Probably some combination. Worth thinking about before we ship.

---

## 9. Implementation phasing (no commitments — just sequencing)

Given the locked Option B shape, the natural phasing:

**Phase 0 — Experiment (~1 day, zero commitment)**
Hand-write 2 skills *for personal use* in `~/.claude/skills/`. Skills use commodity tools (ripgrep, gh, python) — not yet our CLIs. Goal: validate the pattern fits actual workflows before publishing.

**Phase 1 — First CLI + companion skill (~3-5 days)**
Pick the highest-value CLI (likely `mcp-github` because of ElementCache + smart compaction — genuinely novel). Build the crate, publish to crates.io, write the companion `github-research` skill. One CLI + one skill, end-to-end. Update README + docs.

**Phase 2 — Expand to 2-3 CLIs + skills (~1 week)**
Add `mcp-rg` and `mcp-compact`. Write `codebase-navigation` and `text-compaction` skills. Document the train in the Sphinx site as a parallel path to the MCP train.

**Phase 3 — Operator feedback loop**
Use the new CLIs + skills personally in mcp-servers operator deployment. Note what's missing, what's awkward, what's surprisingly useful. Iterate.

**Phase 4 — Decide on expansion vs hold**
At this point, we'll know whether the train is growing organically (operators asking for more CLIs/skills) or hitting the "we have enough" plateau. If growing, plan Phase 5 (more CLIs/skills + possibly a plugin distribution mechanism). If plateauing, stabilize what's there and shift focus back to MCP work.

No timeline commitments. Phases 0-1 are cheap; Phase 2 is medium effort; Phase 3+ depend on signal.

---

## 10. Bottom line

The shift is real. Skills are not hype — they have ~40 agent products adopting them, a coherent open standard, an order-of-magnitude better context profile than MCP, and a credible "this might be bigger than MCP" prediction from Simon Willison.

But the shift is also more nuanced than "MCP is dead." Skills are knowledge; MCP is tools. The two coexist; neither fully replaces the other.

**Decision (locked):** mcp-methods adds a second train (CLIs + skills) alongside the existing MCP train. New workspace members in `crates/`, new `skills/` directory, all in the same repo. The MCP train is **frozen in current behavior** — kglite's foundation untouched, the pip wheel unchanged, the MCP server binary unchanged.

The strategic bet: the primitives we've spent months tuning (`ElementCache`, smart GitHub compaction, ripgrep with code-search defaults, the Workspace pattern) are valuable in both worlds. The MCP train serves operators who want a stdio MCP server. The new train serves operators using Claude Code / Codex CLI / Cursor / etc. who want efficient CLIs invoked from skill files. Both consume the same underlying library; both improve when the library improves.

The risk: maintenance overhead. Mitigation: keep the new train small (3-4 CLIs, 3-4 skills) and grow only when operator demand justifies it.

The win condition: the new train serves at least one real audience (the mcp-servers operator's own daily use, if nothing else) without ever requiring a change to the MCP train. The two trains live in parallel, share primitives, and serve different distribution shapes.

---

## Sources

### Anthropic primary
- [Equipping agents for the real world with Agent Skills](https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills) — Anthropic engineering blog, the canonical reference
- [Agent Skills overview](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/overview) — Claude API docs (spec, lifecycle, security)
- [Extend Claude Code with skills](https://code.claude.com/docs/en/skills) — Claude Code docs (file scopes, frontmatter, dynamic injection, lifecycle)
- [anthropics/skills (GitHub)](https://github.com/anthropics/skills) — public skills repo (PDF, DOCX, XLSX, PPTX + community examples)

### Open standard
- [Agent Skills overview (agentskills.io)](https://agentskills.io/home) — open standard documentation
- [Agent Skills client showcase](https://agentskills.io/clients) — list of ~40 implementing products

### Commentary and analysis
- [Claude Skills are awesome, maybe a bigger deal than MCP](https://simonwillison.net/2025/Oct/16/claude-skills/) — Simon Willison's "Cambrian explosion" prediction (Oct 16, 2025)
- [MCP is dead or MCP vs Skills — revisited](https://medium.com/@alonisser/mcp-is-dead-or-mcp-vs-skills-revisited-daaa51b9a519) — Alon Nisser's critical perspective (auth, scale)
- [Agent Skills Explained: How SKILL.md Files Work and Why They're Everywhere](https://www.firecrawl.dev/blog/agent-skills) — Firecrawl blog on CLI integration patterns
- [Claude Code Skills vs MCP vs Plugins: Complete Guide 2026](https://www.morphllm.com/claude-code-skills-mcp-plugins) — Morph LLM comparison (token cost data)
- [Compare Skills vs Subagents vs MCP Servers in Claude](https://systemprompt.io/guides/claude-skills-vs-agents-vs-mcp) — SystemPrompt guide ("tools vs knowledge" framing)

### Ecosystem narrative
- [Top 5 Agentic Coding CLI Tools](https://www.kdnuggets.com/top-5-agentic-coding-cli-tools) — KDnuggets on the agentic-CLI trend
- [AI Coding Tools in 2025: Welcome to the Agentic CLI Era](https://thenewstack.io/ai-coding-tools-in-2025-welcome-to-the-agentic-cli-era/) — The New Stack (paywall/incomplete fetch, referenced for the headline thesis)

### Vendor-specific implementations
- [OpenAI Codex Skills](https://developers.openai.com/codex/skills) — Codex CLI skills support
- [Gemini CLI Skills](https://geminicli.com/docs/cli/skills/) — Google's implementation
- [Cursor Skills](https://cursor.com/docs/context/skills) — Cursor IDE's implementation
- [VS Code Agent Skills](https://code.visualstudio.com/docs/copilot/customization/agent-skills) — VS Code / GitHub Copilot's implementation
