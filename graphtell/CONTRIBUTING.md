# Contributing

Contributions to GraphTell are welcome. This document focuses on "how to change things so they
don't get sent back": architecture constraints, the FKB workflow, rule discipline, tests and
verification.

For architecture and pipeline background see [`README.md`](./README.md); for FKB authoring details
see [`docs/fkb-authoring.md`](./docs/fkb-authoring.md); for the current support list see
[`SUPPORTED.md`](./SUPPORTED.md).

---

## 1. Development environment

```bash
cargo build                                    # build the backend
cargo test                                     # run all Rust tests
cargo run -p gt-app -- validate                 # validate the built-in FKB
cargo run -p gt-app -- create --name X --path /repo   # build a graph
```

The backend starts an in-process HTTP service; the desktop app (Tauri) and the web app share the
same `/api` contract. The frontend lives in `ui/` (React + TS + antd, layered top-down).

---

## 2. Architecture constraints (read before changing code)

The repo is a **ports-and-adapters** layering, and dependencies always point inward to the core
`gt-domain`:

- **`gt-domain` must not depend on any concrete technology** (it may not `use` any adapter or
  framework). All IO is injected backwards through traits in `port`.
- **Prefer declarative means for a new language / framework / node kind -- don't jump straight to
  editing the engine.** Roughly 90% of FKB needs zero engine changes.
- The few cases that genuinely need engine changes (see `docs/fkb-authoring.md` §9): needing a
  value/matching capability `ValueSource` doesn't support yet, or a node / edge kind needing dedicated
  rendering. (Introducing a new edge kind itself needs no code -- just declare
  `semantic_edge_kinds` / `bridge_edge_kinds` in the FKB.)
- **Backward-compatible schema changes**: when adding fields to structs like `CallSiteFact` /
  `ValueSource`, always add `#[serde(default)]` and supply `None` / defaults at every construction
  site (search `CallSiteFact {` / `CallRecord {` to confirm none are missed, otherwise it won't
  compile).

---

## 3. Adding support for a new framework / language

### New framework (most common, pure YAML)
1. Read §1–§6 of [`docs/fkb-authoring.md`](./docs/fkb-authoring.md).
2. Add a YAML under `fkb/<subdir>/`: `detectors` (recognition conditions) + `rules` (extraction
   rules) + any needed `semantic_kinds` / `semantic_edge_kinds`.
3. **Key discipline**:
   - Reuse existing node / edge kinds; don't invent new vocabulary (a new kind must also be declared
     in the YAML, otherwise the collapsed view hides it).
   - Out-of-process mediators (Cache / ConfigKey / Event / Queue / Topic) must declare `side`
     (`backend` / `frontend`), otherwise same-named frontend and backend nodes get wrongly merged.
   - Use `arg` + `require_literal` to avoid turning variable names into identities and producing
     junk nodes; fall back with `value_fallback` when a value can't be taken.
4. Get `graphtell validate` passing (mind the `⚠` convention warnings -- more valuable than errors).
5. Build a graph from a real sample and **visually confirm the graph is drawn correctly**.

### New language
1. Implement `gt_domain::port::LanguageParser`, translating the tree-sitter syntax tree into the
   language-agnostic `SyntaxFacts` (reference: `src/java` is the "second language", `src/python` the
   "third" -- the latter additionally demonstrates decorator modeling, `owner_class` backfill for
   module-level functions, and other dynamic-language issues).
2. Register it in `DefaultParserRegistry` and add extensions in `scanner::language_of_extension`.
3. Write the framework YAML for that language under `fkb/` (semantic extraction goes entirely
   through FKB; the core knows no framework).

> **Dump the syntax tree first; don't write a parser from memory.** Add a temporary test printing
> `root_node().to_sexp()`, and confirm the node types and **field names** before you start.
> Two real bugs were caught this way: `typed_parameter` has **no** `name` field in
> tree-sitter-python (all parameter types were lost), and `from x import y` treated the
> `module_name` node itself as an import item (fabricating `fastapi.fastapi` out of nothing).
> Neither errors -- they silently produce wrong facts, which you cannot figure out by reading code.

> Reference examples: `fkb/java/spring-boot.yaml` + the end-to-end test
> `crates/gt-pipeline/tests/java_spring_features.rs` (synthetic sample -- verifies cache / event /
> queue / topic / schedule all landing without an external project); for Python see
> `fkb/python/fastapi.yaml` + `tests/python_fastapi_features.rs`.

---

## 4. Compliance rules: measure before writing

Writing a rule is cheap; **verifying it doesn't produce noise is expensive**. Measure every
candidate rule against the sample library (5 ThinkPHP + 3 Spring Boot projects already built) before
deciding to ship it. There are only two criteria:

- hits **must not be 0** (silent failure -- more dangerous than false positives, because it doesn't
  surface as an error);
- hits **must not flood** (noise).

Candidates already rejected by measurement and kept on record so they aren't re-discussed (private
method never called, config key with no reader, cache written but never read, table written but not
read, god method, queue delivery with no consumer, GET contract name containing create/edit,
external callback without signature verification …) are listed in the README under "what makes a new
rule shippable".

Rules **declare their scope up front** via `applies_to.languages` / `applies_to.frameworks`, avoiding
"PHP-only semantics reporting every node as a violation in a Java project". The engine also derives
dependencies from predicates automatically (edges / annotations / capabilities) and disables a rule
when those facts are absent from the graph (`rules_unavailable`) -- no hand-written `requires`
needed.

---

## 5. Tests and verification

- **Engine / rule unit tests**: `cargo test -p gt-pipeline -p gt-domain -p gt-adapter-parser`.
- **End-to-end build tests**: `crates/gt-pipeline/tests/sample_project_pipeline.rs` (PHP sample),
  `crates/gt-pipeline/tests/java_spring_features.rs` (synthetic Java sample),
  `tests/python_fastapi_features.rs` / `tests/python_flask_features.rs` (synthetic Python samples),
  `tests/node_real_samples.rs` (synthetic NestJS / Express + real samples),
  `tests/unsupported_language.rs` (visibility of languages with no parser).
- **When adding a Java semantic feature**: prefer adding the corresponding annotation / call to the
  synthetic sample in `java_spring_features.rs` and asserting nodes and edges (the cheapest way to
  verify "FKB really lands the framework semantics in the graph").
- **FKB changes**: `graphtell validate` must be fully green; changing FKB does **not** trigger a
  rebuild, but some test must cover the rule you changed.
- **Parser / engine changes**: require rebuilding the graph and running the corresponding e2e tests.

---

## 6. Commits and PRs

- Describe clearly "what changed, why, and how it was verified".
- If you touched `gt-domain` data structures, confirm every construction site got its default (§2).
- If you added framework support, update the matrix in `SUPPORTED.md`; if you changed FKB authoring
  capabilities (e.g. a new `ValueSource`), update `docs/fkb-authoring.md` too.
- Documentation is written in English, consistent with the existing `README.md` / `docs/`.

You don't need to know Rust to contribute FKB / rules / perspective declarations (all YAML); for
engine changes, feel free to open an issue to discuss the approach first.
