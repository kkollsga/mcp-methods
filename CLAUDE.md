# mcp-methods

## Mail interface

Three projects coordinate by dropping markdown files into each other's
`inbox/` folders. We (mcp-methods) correspond with two counterparts:

| Party | Their inbox (where we drop messages TO them) |
|---|---|
| **kglite** | `/Volumes/EksternalHome/Koding/Rust/KGLite/inbox/unread/` |
| **mcp-servers** | `/Volumes/EksternalHome/Koding/MCP servers/inbox/unread/` (note the space in the path) |

Our own inbox (messages FROM them land here):
`/Volumes/EksternalHome/Koding/Rust/mcp-methods/inbox/unread/`

### Workflow

- **Reading**: messages start in `inbox/unread/`. After reading, move
  them to `inbox/read/`. Don't delete — the archive is the record.
- **Writing**: drop the file directly into the recipient's
  `inbox/unread/` folder. They'll move it to their `read/` when they
  process it. We don't keep an outbox; the file is the only copy.

### Naming convention

`YYYY-MM-DD-from-<sender>-<short-topic>.md` — kebab-case topic,
sender slug matches the party name (`mcp-methods`, `kglite`,
`mcp-servers`).

### Frontmatter

Every message opens with a YAML block:

```markdown
---
date: 2026-05-18
from: mcp-methods
re: <recipient>/inbox/read/<prior-message>.md   # or "" if new thread
status: <one-paragraph TL;DR — what this message does, what it asks
        for, what action (if any) the recipient should take>
---
```

The `status:` block is the agent-readable summary; downstream agents
sort and prioritise inbox work from it without opening every file.

### Three-party relationship

- **mcp-methods** (here): the Rust framework. Ships `mcp-methods` on
  crates.io (pure library) and `mcp-methods` on PyPI (pyo3 bindings +
  bundled `mcp-server` CLI).
- **kglite**: downstream consumer that builds on top of the framework.
  Treats us as the vendor. Paying-customer relationship — we don't
  volunteer maintenance-burden concessions; they raise needs, we
  decide on shape.
- **mcp-servers**: operator running kglite deployments
  (legal/o&g/code). One hop further downstream — most coordination
  with them goes via kglite, but we ship release notes directly when a
  framework cut affects their deployment.
