# Writing Effective Skills

A companion to [Authoring Skills](authoring-skills.md). That page covers the mechanics (frontmatter fields, where files live, lint output). This page covers the *craft*: what makes a skill that agents actually trigger and use well. The patterns here are distilled from reading Anthropic's published skills at [github.com/anthropics/skills](https://github.com/anthropics/skills) plus our own five bundled defaults.

## The description is your discovery mechanism

Agents see two things about a skill before they decide to fetch its body:

1. **Name** — the lookup key. Short, lowercase, snake_case.
2. **Description** — everything else. The agent reads this from `prompts/list` and decides "is this relevant to what the user just asked?"

So the description is *not* a tagline. It is the entire trigger. If the description is too short or too generic, the agent will fail to load the skill in cases where it would have been useful — what Anthropic's docs call **undertriggering**.

### Anatomy of a good description

Anthropic's published descriptions are 25–140 words and consistently follow this shape:

```
<Capability — what the skill enables, action-led.>
TRIGGER when <specific contexts, phrases, file types, user requests>.
ALSO TRIGGER when <secondary contexts the agent might miss>.
SKIP for <related-but-distinct cases that belong to other tools/skills>.
```

The TRIGGER / SKIP language is borrowed from Anthropic's own `claude-api` skill, which is the most pushy of their published descriptions. You don't need the literal keywords if the same information is conveyed in prose — but you do need:

- **Concrete triggers**. Not "when relevant" but "when the user mentions a `.pdf` file" or "when code imports `anthropic`".
- **Negative cases**. What this skill is *not* for. Anthropic's `docx` skill explicitly says "Do NOT use for PDFs, spreadsheets, Google Docs."
- **A bit of pushiness**. Anthropic's skill-creator explicitly notes: "Claude has a tendency to undertrigger skills — to combat this, please make the skill descriptions a little bit pushy."

### Example: before and after

Before (our pre-0.3.35 style, ~15 words):
```yaml
description: Methodology for the `grep` source-search tool — regex syntax, where to scope, how to interpret results.
```

After (~130 words, with explicit TRIGGER / SKIP):
```yaml
description: "Search code using regex patterns across the configured source roots, with file-glob scoping and result caps. TRIGGER when the user wants to find a symbol or string across files (function/class/variable name, error message, log line), locate call sites textually, hunt for patterns by file type, or sweep a codebase for occurrences of a name. ALSO TRIGGER when grep would be the natural shell tool — surfacing it here keeps the agent inside the framework's source sandbox. SKIP when the codebase has a knowledge graph and the question is structural (\"where is X defined?\", \"what calls Y?\") — graph queries are exact, grep is fuzzy. SKIP for single-file reads (use read_source) and directory layout questions (use list_source)."
```

The five bundled skills in `crates/mcp-methods/src/server/bundled_skills/` all follow this shape. Use them as templates.

## Body anatomy

A skill body has a recurring structure that's worth following because agents (and humans skimming the file) can navigate it quickly:

```markdown
# <Skill name> methodology

## Overview

<2–3 sentence framing. What the tool is, when it's right, what comes
before and after it in the typical workflow.>

## Quick Reference

| Task | Approach |
|---|---|
| <common task A>          | <one-line pattern> |
| <common task B>          | <one-line pattern> |
| <pitfall A>              | <what to do instead> |

## <Major topic 1>

<concrete prose, code blocks, examples>

## <Major topic 2>

...

## Common Pitfalls

❌ <anti-pattern, framed as a specific behavior to avoid>

❌ <anti-pattern>

✅ <positive guidance, often a heuristic>

✅ <positive guidance>

## When <skill> is the wrong tool

- **<scenario>** — use <other tool> because <reason>.
- **<scenario>** — use <other tool> because <reason>.
```

Two of these sections do most of the work:

### Quick Reference

A small table near the top mapping common tasks to one-line patterns. Anthropic's `pdf` and `docx` skills both lead with one. Why it matters: an agent that finds the skill via description, then opens the body, can absorb the Quick Reference in two seconds and decide "yes, my task is one of these rows" without reading further. The table is also the easiest way to *constrain* the body — if you can't fit a task into a one-line pattern, the skill is probably trying to cover too much.

### Common Pitfalls

A list of `❌` / `✅` pairs framed as specific behaviors, not generic advice. Anthropic's `webapp-testing` skill has the cleanest version:

```
❌ Don't inspect the DOM before waiting for networkidle on dynamic apps
✅ Do wait for page.wait_for_load_state('networkidle') before inspection
```

This format works because each pitfall is concrete enough that the agent can pattern-match: "am I about to do the ❌ thing? then do ✅ instead." Generic warnings ("be careful with regex") don't get matched against; specific ones ("re-grepping with slightly different patterns when you've already found the file → switch to read_source") do.

## Decision trees for multi-mode tools

If your tool has multiple modes or dispatches based on argument shape, include an ASCII decision tree. Anthropic's `webapp-testing` skill has one for static-vs-dynamic webapps; our `github_issues` skill has one for FETCH / SEARCH / LIST / drill-down.

```
What do you want?

1. A specific issue/PR you already have a number for?
   └── FETCH: github_issues(number=N).

2. To search by topic / keyword?
   └── SEARCH: github_issues(query="...").

3. To browse recent open items?
   └── LIST: github_issues() — no args.

4. You FETCHed something and it had [cb_N] / [patch_N] markers?
   └── Drill: github_issues(number=N, element_id="cb_1").
```

These read well to agents because each branch is a complete, unambiguous instruction. Avoid generic "it depends" trees — every leaf should be actionable.

## Tone and voice

Anthropic's writing guide for their skill-creator skill puts it concisely:

> Try to explain to the model why things are important in lieu of heavy-handed musty MUSTs.

In practice:

- **Imperative + reasoning.** "Be specific. `fn foo` finds far less noise than `foo`." Not just "be specific."
- **Conversational where appropriate.** Asides like "if this looks unfamiliar, that's expected" are fine.
- **`**CRITICAL:**` markers only when a pitfall has actually bitten people.** Don't overuse them. If everything is critical, nothing is.
- **Avoid ALL-CAPS MUST/NEVER as primary mode.** They read as nagging and signal that you don't trust the reader.
- **Second person OK, first person ("we") fine in framing.** Avoid third person.
- **Don't apologize for the tool.** "This is a primitive tool but…" undercuts the agent's confidence in using it.

## Size targets

| Size | When | Examples |
|---|---|---|
| **< 100 lines** | Single-purpose tools with one main mode | webapp-testing (95) |
| **100–300 lines** | Most tool skills | our 5 bundled (90–180), mcp-builder (236) |
| **300–500 lines** | Multi-faceted skills with several distinct workflows | claude-api top-level (324), pdf (314) |
| **500+ lines** | Only with progressive disclosure | docx (590) + references/, skill-creator (485) + references/ |

Anthropic's hard guidance: **keep SKILL.md under 500 lines for optimal performance**. If you're approaching that, split into a top-level SKILL.md that points at `reference/<subtopic>.md` files. We don't currently support the reference-file pattern in our `.skills/` layout (single SKILL.md only), so 500 lines is a hard ceiling, not a soft one — for now, target 300 or below.

## What our framework adds

Anthropic's frontmatter has two required fields (`name`, `description`) and an optional `license`. Ours adds framework-specific extensions:

| Field | Purpose | Anthropic equivalent |
|---|---|---|
| `applies_to:` | Semver gating per binary | none (skills aren't version-tied) |
| `references_tools:` | Drives the auto-inject pass on tool descriptions | none |
| `references_arguments:` | Lint surface for argument-name correctness | none |
| `auto_inject_hint:` | Whether tool descriptions get a `prompts/get` pointer | none |

These exist because our skills are *bundled with the binary* and tied to the registered tool catalogue — they're not standalone artifacts the way Anthropic's are. Use them when:

- **`applies_to`** — your skill assumes a specific version of `mcp-methods` or a downstream binary.
- **`references_tools`** — your skill name matches the tool's name; this lets the framework auto-append a pointer.
- **`auto_inject_hint: false`** — your skill is cross-cutting (e.g. "test-driven-development") and doesn't have a single matching tool.

## A note on iteration

Both the Anthropic skill-creator guide and our experience say the same thing: **build evals first, write skills second**. The shape:

1. Pick three realistic user prompts the skill should help with.
2. Run them against an agent *without* the skill — record what goes wrong.
3. Write the minimal skill that fixes those failures.
4. Re-run; iterate.

Anthropic's skill-creator skill automates this loop with their `evals/evals.json` shape and a benchmarking viewer. We don't have that machinery in `mcp-methods` (yet). The Python pyo3 surface (`SkillRegistry.from_manifest`) makes it cheap to wire your own eval harness around the resolved-set — feed each prompt to a test agent twice (with and without `register_skills_as_prompts`), compare outputs.

## See also

- [Authoring Skills](authoring-skills.md) — file format, where files live, the lint surface
- [Three-Layer Composition](../explanation/three-layer-composition.md) — design rationale for project / domain-pack / bundled
- [Anthropic's published skills](https://github.com/anthropics/skills) — the canonical examples to model on. We particularly recommend reading `webapp-testing/SKILL.md` (short, focused) and `mcp-builder/SKILL.md` (medium-length, structurally close to ours)
- [Anthropic's skill best-practices doc](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/best-practices) — official guidance, including the "build evals first" recipe
