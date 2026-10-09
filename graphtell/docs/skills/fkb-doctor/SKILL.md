---
name: fkb-doctor
description: >-
  Guide the user's LLM to check whether GraphTell's loaded FKB actually "sees" their codebase, and
  to author project-specific FKB (scope: project) that fills the gaps — without the user writing YAML
  by hand. Use when: the user asks why GraphTell misses their calls/endpoints/config; wants to improve
  recall for their repo; asks to "补全/完善/补充 FKB"; or wants an LLM to generate the missing
  knowledge base for their project. Triggers on phrases like "图里看不到我的代码", "召回不到 XX",
  "帮我补 FKB", "为什么认不出我的框架", "generate FKB for my project".
---

# fkb-doctor — close GraphTell's FKB coverage gaps with the LLM

> **Install**: this file ships in `docs/skills/` because `.codebuddy/` is gitignored. To activate it in an
> IDE that discovers CodeBuddy skills, copy this directory to `.codebuddy/skills/fkb-doctor/` (so the file
> lands at `.codebuddy/skills/fkb-doctor/SKILL.md`).

GraphTell builds a code graph from call sites, but only the edges a **rule fired on** become
"visible" to recall. `graphtell coverage` quantifies that: it reports, per sub-project, how many call
sites became a semantic edge and which callees are still invisible. This skill turns that report into
a **project-scoped FKB** authored by the LLM, validated against the engine, and verified to actually
shrink the gap by rebuilding the graph.

> Core rule: **only ever generate `scope: project` FKB** (a new file in `fkb/projects/` or a private
> `--fkb-dir`). Never edit the builtin framework FKB under `fkb/<lang>/`. Project FKB is auto-loaded,
> scope-limited, and safe to throw away.

## Tools you have

- `graphtell list` — list built projects: `#<id> <name> <status> <root>`. Pick the target id.
- `graphtell coverage --project <ID> --json` — the gap report (machine-readable; feed it to yourself).
- `graphtell coverage --project <ID>` — same, human-readable (for the user).
- `GET /api/projects/{ID}/coverage` — the same report over HTTP (for the Web UI / MCP; same JSON shape
  as `coverage --json`).
- `graphtell --fkb-dir <DIR> validate` (or `GRAPHTELL_FKB_DIR=<DIR> graphtell validate`) — lint a
  draft FKB dir (syntax + conventions, no DB, no graph build). **Always validate before presenting.**
- `graphtell create --name <n> --path <root>` — build/rebuild the graph for a project root. The next
  rebuild picks up any `fkb/projects/*.yaml` (or whatever `--fkb-dir` resolves to), so re-running
  `coverage` after a rebuild measures whether your draft helped.

## The coverage report shape (what "covered" means)

`coverage --json` returns:

```jsonc
{
  "project_id": 1,
  "totals": { "total_calls": 82440, "covered_calls": 46746, "coverage_ratio": 0.567,
              "sub_projects": 3, "sub_projects_with_gaps": 1 },
  "sub_projects": [
    {
      "sub_project_id": 12, "name": "frontend:admin", "language": "javascript", "role": "frontend:admin",
      "frameworks": ["frontend-js"],
      "total_calls": 16569, "covered_calls": 4274, "coverage_ratio": 0.26,
      "flags": ["low_coverage"],                 // language_unknown | no_framework | low_coverage
      "uncovered_samples": [                      // ≤25 distinct invisible callees, with location
        { "callee": "request", "file": "template/admin/src/api/agent.js", "line": 55 }
      ]
    }
  ]
}
```

Interpretation (this is how the engine computes it — don't second-guess it):
- A call site counts as **covered** when its *enclosing method* produced at least one semantic/bridge
  edge (ReadsCache, CallsHttp, ReadsDb, ReadsConfig, HandledBy, …). The semantic edge points at the
  method, not the call site node, so "covered" is a method-level proxy, not per-call precision.
- `flags`:
  - `language_unknown` — the sub-project's language wasn't recognized → **all** language-gated rules
    for it silently no-op. Highest priority to fix (usually a missing marker / `scope: project`
    `detectors`).
  - `no_framework` — no FKB claimed this stack (empty `frameworks`). Often the real gap.
  - `low_coverage` — **advisory only.** `<30%` covered **and** ≥20 call sites. It is *not* counted in
    `sub_projects_with_gaps`, because most call sites in real code are utility calls that should
    never be captured, so a low ratio alone does not prove missing knowledge.
- `sub_projects_with_gaps` counts only the **unambiguous** knowledge gaps (`language_unknown` /
  `no_framework`). Trust this number over the raw ratio.
- `uncovered_samples` are concrete, real callees + `file:line`. **Most of them are noise** — treat
  them as candidates to triage, not a to-do list (see pitfalls below).

## Workflow

### 1. Pick the project
Run `graphtell list`. If the user's repo isn't there yet, `graphtell create --name <n> --path <root>`
first. Confirm the `<ID>` with the user if ambiguous.

### 2. Get the gaps
Run `graphtell coverage --project <ID> --json`. Read `totals` and every sub-project with non-empty
`flags`. For each flagged sub-project, collect its `language`, `frameworks`, and `uncovered_samples`.

### 3. Gather context (read these, do NOT guess)
- `docs/fkb-authoring.md` — the authoritative authoring guide (selectors, captures, `link`,
  `semantic_kinds`, conventions, the "AI-generated FKB looks right but misses" warning).
- `fkb/projects/sample_project.yaml` — the project-FKB template (use its `scope: project` +
  `detectors` shape verbatim).
- `fkb/<language>/common.yaml` and any `fkb/<language>/<framework>.yaml` for the detected frameworks
  — copy their *style* (selector patterns, node synthesis, `link` kinds) as references.
- The actual source at each `uncovered_samples[].file:line` — you must ground rules in real code, not
  invented signatures.

### 4. Draft a project FKB
Create `fkb/projects/<slug>.yaml` (or stage it in a private dir you pass via `--fkb-dir`). **Mirror the
exact schema of `fkb/projects/sample_project.yaml`** — the engine is strict (`deny_unknown_fields`).
The shape is `detectors` as a **sequence** of `{kind, path, confidence}`, and each rule uses
`selector:` + `binding:` (not `when`/`actions`). A minimal correct example:

```yaml
scope: project
display_name: <slug>
language: php                       # the sub-project's language from coverage
version_hint: "..."                 # optional free-text
detectors:
  - kind: file_exists
    path: composer.json             # php; or package.json / pom.xml / requirements.txt …
    confidence: 0.8
rules:
  - id: <slug>-cache-read
    phase: Synthesize
    selector:
      kind: call
      callee: "Cache::get"          # from uncovered_samples; keep it concrete
    binding:
      - Synthesize:
          node: Cache               # synthesized node kind
          identity: { kind: Named, value: { arg: 0 } }
          link: { kind: ReadsCache, direction: incoming }   # call-site -> Cache
          confidence: 0.9
```

Rules of thumb:
- Start from the `uncovered_samples` callees — those are provably invisible today.
- Reuse existing node/edge kinds from the builtin FKB of that language; if you must introduce a new
  kind, declare it where the engine expects (builtin files declare `semantic_kinds` at file level for
  `scope: builtin`; for `scope: project` reuse builtin kinds or follow `docs/fkb-authoring.md` —
  `validate` will flag unknown kinds / undeclared kinds).
- Keep `detectors` tight (a concrete marker file + confidence) so the file only loads for the right
  project. `language:` at the top also scopes it.
- One file, `scope: project`. No edits to `fkb/<lang>/`.

### 5. Validate, then converge
1. `graphtell --fkb-dir <draft_parent> validate` — fix every error/warning (unknown fields,
   undeclared node/edge kinds, unregistered edge kind, id conflicts). **Do not skip.**
2. Rebuild so the graph includes the draft: `graphtell run --project <ID>` re-runs the pipeline on
   the existing project and picks up any newly loaded FKB (`fkb/projects/*.yaml` auto-loads, or
   whatever `--fkb-dir` resolves to). (First-time build uses `graphtell create --name <n> --path <root>`,
   which also rebuilds if the name already exists.)
3. Re-run `graphtell coverage --project <ID> --json`. Compare `coverage_ratio` / `flags` /
   `uncovered_samples` before vs after. **Only present the draft if the gap shrank**; otherwise read
   the new samples and iterate (this is what turns "looks right" into "measured right").
4. Repeat until the user is satisfied or remaining gaps are out of scope.

### 6. Hand off for human approval
Show the user a diff of the new `fkb/projects/<slug>.yaml` and a short before/after of the coverage
numbers. **Do not write the file into the repo without explicit approval** — present it, let them
confirm, then write. Mention any sub-projects you deliberately left (e.g. `language_unknown` that
needs a real marker file you can't infer).

## Worked example (verified end-to-end)

Target: project **#15 express-hackathon**. `mongoose` was a real dependency and **no FKB file mentioned it**
(`grep -rin "mongoose" fkb/` → empty), so its data model was completely invisible.

> Status: this gap has since been **promoted to a builtin framework file** (`fkb/js/mongoose.yaml`), so rerunning
> the loop on the same project finds no gap. Keep it as the reference template of what a good round looks like.

1. **Gap** — `coverage --project 15 --json`: 7652 calls, 75 covered, `0.98%`, `low_coverage`. Ranking the
   invisible callees by frequency (the capped `uncovered_samples` alone is not enough) showed ~247
   mongoose-ish calls, including `mongoose.model`, `user.save`, `mongoose.connect`.
2. **Ground truth** — `models/Session.js:33` → `mongoose.model('Session', sessionSchema)`;
   `models/User.js:285` → `mongoose.model('User', userSchema)`.
3. **Rule** (project FKB, validated with `✓ 1 rule(s), 1 synthesis rule(s)`):

```yaml
id: hackathon-mongoose
scope: project
language: javascript
detectors:
  - kind: file_exists
    path: package.json
    confidence: 0.5
rules:
  - id: mongoose-model-table
    phase: Synthesize
    selector:
      kind: call
      callee: "mongoose::{model}|mongoose.model"
    binding:
      - Synthesize:
          node: Table
          identity: { kind: Named, value: { arg: 0, require_literal: true } }
          link: { kind: MapsTo, direction: incoming }   # "a model maps to a table" — semantic
          confidence: 0.9
```

4. **Rebuild** — `graphtell --fkb-dir <builtin-fkb + this file> run --project 15`
   (merge the draft into a *copy* of `fkb/` in /tmp so the builtin KB is still loaded — pointing
   `--fkb-dir` at a dir holding only the draft would drop all 41 builtin files).
5. **Result** — coverage `0.98% → 1.93%` (+73 covered) and the two real collections entered the graph:

```
models/Session.js --MapsTo--> Session
models/User.js    --MapsTo--> User
```

`+73` for 2 call sites is the owner-method proxy at work: once `models/User.js` / `models/Session.js`
carry a semantic edge, every call site they own counts as covered. Rebuilding with the plain builtin FKB
restored the previous state exactly, so the loop is safe to iterate.

## Notes / pitfalls
- `coverage` is a *method-level proxy*: one rule firing in a method marks all its calls "covered". So
  trust the **uncovered** samples (genuinely invisible) more than the exact ratio.
- Frontend JS almost always shows `low_coverage`, and it is usually **not** a gap: utility calls
  (`this.$emit`, `Array.map`, `Math.min`, `JSON.stringify`, `deepCopy`, …) are legitimately
  non-semantic. Triage `uncovered_samples` and keep only *framework / SDK / project-private wrapper*
  calls that should become a semantic edge (HttpContract/CallsHttp/ReadsCache/…).
- **Real case study — do not "fix" a correctly-rejected call.** In CRMEB, `request` appeared in
  `uncovered_samples` at `template/admin/src/api/agent.js:55`. Reading the source showed that call is
  `stairListApi(url, params) { return request({ url: url, … }) }` — the URL is a **variable**, so the
  builtin `http-contract` rule rejected it on purpose (`require_literal: true`) to avoid inventing a
  junk `GET /<dynamic-url>` contract. The sibling calls in the same file *with literal URLs* did
  produce contracts (`GET /agent/index`, `PUT /agent/spread`, …: 2578 HttpContracts from 955 `request`
  call sites). The lesson: **always open the file before writing a rule** — an "invisible" callee is
  often invisible *by design*, and forcing it through produces exactly the wrong-match failure mode
  the authoring guide warns about.
- If `language_unknown`, fix detection first (a `detectors.file_exists` + correct `scope: project`),
  because language-gated rules can't fire until the language is known.
