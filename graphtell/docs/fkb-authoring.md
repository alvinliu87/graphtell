# FKB authoring guide (Framework Knowledge Base)

FKB is GraphTell's "framework knowledge" layer: **one YAML describes how a framework is recognized
and how semantic nodes and edges are extracted from source**. The core **knows no concrete
framework** -- adding support for a framework / language only means adding a YAML, with no Rust
changes.

This guide is for you if you want GraphTell to support a new framework (whether you hand-write it
or generate it with AI).

---

## 1. How it gets loaded (understand the mechanism, trip over less)

- **Directory auto-discovery**: on startup it recursively scans `fkb/<any-subdir>/*.yaml`; drop in a
  new file and it loads automatically -- **no registry manifest needed**.
- **Semantic kinds auto-registered**: `semantic_kinds` declared in FKB are registered into the core
  at load time (§5.2); a new node kind needs no core change.
- **Pluggable, without touching the core repo**: point `--fkb-dir <dir>` or the environment variable
  `GRAPHTELL_FKB_DIR` at your own FKB directory and the Engine uses it wholesale. Others can ship
  their own rule set against your Engine without opening a PR at all.
- **Graceful skip for broken files**: a malformed YAML won't crash the program; it warns and skips.

> Validation: after writing an FKB, run `graphtell validate` (§8) before building a graph -- it saves
> a lot of "wrote it, nothing happened" silent failures.

---

## 2. What an FKB looks like (top-level fields)

```yaml
id: my-framework          # required, globally unique; an external dir with the same id overrides built-in
display_name: My Framework
language: php             # open string: php / java / javascript / … decides which sub-projects it applies to
version_hint: ">=2.0"     # display only

detectors: [...]          # how to "recognize" this framework (takes effect only on a hit, unless apply_without_detection)
root_rules: [...]         # how to resolve the framework root (e.g. the app dir)
loaders: [...]            # P3 loads authoritative symbol tables (schema / config_keys / i18n / facade_map …)
resolvers: [...]          # P7 dynamic resolution declarations (container make / event / facade / handler …)

rules: [...]              # ★ core: this framework's extraction rules
semantic_kinds: []        # "new semantic node kinds" introduced by this FKB (must be declared if not on the core list, §5.2)

scope: framework          # framework (generic) | project (project-specific, loaded only when that project is recognized)
apply_without_detection: false  # true = applies to every sub-project of that language — rules, loaders and the
                                # three merged lists alike (for a "language-generic layer", no framework assumptions)
exclude_rules: [...]     # what must not be scanned; resolved in P0 from the project's own config (§2.4)

# Framework-specific conventions (those with no framework assumptions go in the language-generic layer; strong assumptions stay here):
handler: {...}            # config for the `method_ref` resolver: how a STRING literal (e.g. a route handler 'admin.Login/login') resolves back to "class + method". Generic — not route-only; any rule may use `resolve: method_ref` on a string-literal arg.
db_verbs: {read: [...], write: [...]}   # model CRUD verbs -> read / write classification
magic_delegation: {...}   # @method annotation + __call forwarding to some property
external_calls: [...]     # callees that make network requests (for "external call inside a loop" detection)
tx_calls: [...]           # transaction boundary markers
entry_methods: [...]      # candidate consumer entry method names (handle/fire/doJob/__invoke/run …)
```

**Core principle**: `rules` is the point; `detectors` decides "who this knowledge applies to";
`semantic_kinds` decides "whether the node you create is visible".

### 2.1 Detectors: four kinds of evidence

A detector answers "does this knowledge apply to this sub-project". It is a **gate**, and the gate must stay
narrow: the rules of every FKB that passes the gate are matched against every call site, so a gate left open
costs `O(call sites × rules)` and — worse — can interpret code with knowledge that does not belong to it.

| Kind | Evidence | Use for |
|---|---|---|
| `manifest_dependency` | a dependency is declared in `composer.json` / `package.json` | the framework itself; the package a project installs **directly** |
| `file_exists` | a marker file or directory exists | frameworks with a characteristic entry (`artisan`, `think`) |
| `lock_dependency` | a dependency is in the **lock file** (`composer.lock` / `package-lock.json`), i.e. in the resolved closure | a package reached **transitively** |
| `import_exists` | the source **imports** this FQN (`use GuzzleHttp\Client;`) | a library — see below |
| `call_exists` | a call site matches this callee pattern | a library with no import (inline `\GuzzleHttp\Client::request()`) |

**`provides`** covers what no detector can reach. A framework is a bundle, and it is the bundle the manifest
names: `laravel/framework` pulls in `illuminate/database`, `illuminate/cache` … none of which appear in the
app's own `composer.json`, and which the lock file only lists *because* the framework asked for them. The
framework declares the bundle once:

```yaml
# laravel.yaml
provides: [illuminate-database]
```

Recognising `laravel` then recognises `illuminate-database` too, transitively and cycle-safely. Directly
detected knowledge always ranks **above** provided knowledge regardless of confidence, so a framework's own
declaration of a single-valued field (`db_verbs`, `method_ref`, all taken by `find_map`) still wins over a
component's — which is why `db_verbs` stays in `laravel.yaml` and not in `illuminate-database.yaml`.

#### The three merged lists follow detection

`external_calls`, `tx_calls` and `middleware_capabilities` are not per-framework settings — every FKB that
applies contributes to one shared list. **"Applies" now means detected**: a recognised framework or project,
or a knowledge base flagged `apply_without_detection`.

So where you put an entry decides its reach:

* genuinely language-wide (`curl_exec`, `transaction`, `commit`) → the unconditional layer, e.g.
  `php/common.yaml`. These are never detected (they declare no detectors) and would vanish from every
  project if they lived anywhere else;
* specific to one library or component (`GuzzleHttp\Client::*`, `DB::transaction`) → that library's own file,
  where its detectors gate it.

Before this followed detection, the lists were merged from every FKB of the language, which is why entries
that belong to one library had drifted into the language-common file.

Prefer **code evidence** (`import_exists` / `call_exists`) for libraries, for two reasons:

1. **A manifest says what was installed, not what was used**, and it is blind to anything pulled in
   transitively — a Laravel app's own `composer.json` lists `laravel/framework` and nothing else, so every
   `illuminate/*` component its code uses is invisible to `manifest_dependency`.
2. **It is alias-proof.** `use GuzzleHttp\Client as G;` records the same FQN, so renaming the local alias
   changes nothing. Detection reads the per-file import tables, so it also survives a short-name collision
   (`GuzzleHttp\Client` vs some project `Client`) — the global short-name index would not.

Declare both for a library when the language allows more than one spelling; either one matching is enough:

```yaml
detectors:
  - kind: import_exists
    symbol: "GuzzleHttp\\Client"      # exact FQN, or a `*` suffix for a namespace prefix ("GuzzleHttp\\*")
    confidence: 0.95
  - kind: call_exists
    callee: "GuzzleHttp\\Client::*"   # same grammar as a rule's `selector.callee`
    confidence: 0.9
```

See `fkb/php/guzzle.yaml` for a worked example. Note what it does **not** do: rather than switching on
`apply_without_detection` to skip detection entirely, it lets the code decide — an unconditional FKB applies
its rules to every project of that language, which is only safe for knowledge that is genuinely universal
(see `php/common.yaml`).

### 2.2 `method_ref` vs `class_const` (resolving references to code)

Two `resolve:` strategies turn a rule argument into a real graph node. Pick by the argument's **shape**:

| Strategy | Argument shape | Example | Existence-gated? |
|---|---|---|---|
| `class_const` | a real code reference (`X::class`, a variable of known type) | `Event::listen('x', Listener::class)` | yes (`require_class`) |
| `method_ref` | a **string literal** denoting `class::method` / a function | `Route::get('/x', 'admin.Login/login')` | yes |

`method_ref` is **generic, not route-only**: any rule whose argument is a string that names a callable may use
`resolve: method_ref`. It reads the per-framework `method_ref` field (`method_separators` / `hierarchy_separators` /
`controller_layer_depth` / `app_anchor_dir`) to split the string, then keeps **only** the candidate
that **actually exists on the graph** (`find_by_name`) — so a miss yields no edge and never synthesizes a ghost node.

**No controller directory name is ever assumed.** The FKB does not hard-code `controller`, `Http/Controllers`, or any
such convention. Instead, at prepare time `root_namespaces` is derived from the tech-stack manifest (`composer.json`'s
`autoload.psr-4` for PHP, its equivalent elsewhere), and the
resolver matches the handler against the **real class FQNs on the graph** under those namespaces:

* If the handler is already fully-qualified (`App\Http\Controllers\UserController`, `app\admin\controller\Login`), it
  resolves by exact match — the directory name is irrelevant.
* If it is a short name (`admin.Login/login`), the resolver infers the app module (from the route file path, see
  `app_anchor_dir`) and looks up the class whose FQN sits exactly `controller_layer_depth` namespace segments below that
  module and whose trailing segments equal the (hierarchy-expanded) controller name. Only the **depth** is configured,
  never the name — so `controller` / `Http\Controllers` / anything the user chose all work, and ambiguous matches are
  rejected rather than connected to the wrong node.

`controller_layer_depth` is a structural fact (ThinkPHP = 1, Laravel's `App\Http\Controllers` = 2), not a name; it
defaults to 1 and may be omitted for the common case.

> Do **not** point `method_ref` at a `X::class` argument — those are real references and belong to `class_const`.
> `method_ref` is only ever fed string literals by the rules that invoke it.

#### Non-route example

`method_ref` is **edge-agnostic** — any rule may use it on a string-literal argument, not just routes. A queue
that is enqueued by string (instead of `Job::class`) is the same shape as a route handler:

```yaml
# Queue a job by string FQN or short name: `Queue::push('app\job\SendMail')` / `Queue::push('SendMail')`
- id: queue-consumer        # lands on the graph as `myfw-queue-consumer`, see §2.3
  selector: { kind: call, callee: "Queue::push" }
  binding:
    - Link:
        kind: HandledBy
        direction: to_target
        to: { arg: 0, resolve: method_ref }   # FQN -> exact match (L1); short name -> import/template fallback
```

Because the resolver only ever keeps a candidate that **exists on the graph**, a typo'd or dynamic job string simply
produces no edge — never a ghost node. (When a framework enqueues by `X::class` instead, use `class_const`, not `method_ref`.)

---

### 2.3 Naming: the file, the `id`, and the item ids

#### The file name and `id` carry the framework name — never a version number

Version is **not a dimension of the mechanism**: no detector can express a version range (§2.1 — a
`manifest_dependency` names a package, not `^6.0`), and `version_hint` is display-only. What a knowledge
base actually describes is an **API-shape family**, and shape differences between releases are absorbed
**as data inside one file** — parallel selectors, dual loaders, fallbacks:

```yaml
# fkb/php/thinkphp.yaml covers 5.1 / 6.x / 8.x in one file:
#   * event registry: 5.x `app/tags.php` (flat) vs 6.x+ `app/event.php` (nested under `listen`)
#     -> two `config_entry` rules, each keyed to its own file name
#   * event trigger: 5.x `Hook::listen` vs 6.x+ `event()` / `Event::trigger` -> two resolvers
#   * app root: 5.x `application/` vs 6.x+ `app/` -> one `root_rules` entry with fallbacks
version_hint: "5.1 / 6.x / 8.x"
```

**Split a file when the shape forks, not when the version number changes.** A detector is a gate: every
rule of a knowledge base that passes is matched against every call site, so the gate must stay narrow
(§2.1). Splitting by version duplicates one shape's rules across N files whose gates all overlap;
splitting by shape keeps the gates mutually exclusive — give each new file its own evidence (a marker
file, a manifest entry, a code import) and name it after the **shape family**, e.g. `thinkphp.yaml` +
`thinkphp-legacy.yaml`, never `thinkphp5.yaml` / `thinkphp6.yaml`.

#### Item ids are local names

`rules` / `loaders` / `root_rules` / `resolvers` ids are written **without** the framework prefix; the
loader namespaces them at load time as `<id>-<local id>`:

```yaml
id: thinkphp
rules:
  - id: pii        # lands on the graph as `thinkphp-pii`
```

Why the namespace exists at all:

1. **Ids are provenance.** A rule id is written into the graph — a node's `sources`, an edge's
   `evidence.rule`, an annotation's `evidence.hook` — and into diagnostics (`rule <id>: …`). Bare names
   would be ambiguous once several knowledge bases have touched the same node.
2. **Ids are the dedup key.** Rules are collected from every applicable knowledge base (framework +
   language-common + project) and `dedup_rules` keeps the **first** id it sees, so two knowledge bases
   sharing an id means one of them is **silently dropped** — decided by concatenation order. That is a
   real failure mode, not a hypothetical one: one product's knowledge split across two files that both
   declare `id: crmeb` means the project-level one is shadowed whole — its rules (including crontab
   routes -> `Schedule`) never run. Hence **one product, one knowledge file, one gate**
   (`fkb/projects/crmeb.yaml`).

Why it is added at load time rather than typed by hand: repeating it on forty rules is exactly how an
author ends up forgetting it (the same argument that moved `side` to the top level, §5.1).

Two consequences worth knowing:

* **It is idempotent.** An id that already starts with `<id>-` is left alone — that is both the escape
  hatch for a knowledge base that wants a different prefix and the reason a not-yet-migrated file keeps
  working.
* **`graphtell validate` reports collisions**: a qualified id declared by two files is a warning, because
  the second declaration never runs.

### 2.4 `exclude_rules`: keeping generated files out of the scan

Which directories are *not* source code is framework knowledge — ThinkPHP writes its cache under
`runtime/`, Laravel compiles Blade into `storage/framework/`, uni-app generates `unpackage/`. Those names
are only conventions, though: a project can move `public/` to `web/`, point ThinkPHP's runtime elsewhere,
or rename Composer's `vendor-dir`. So the knowledge base declares **rules**, and the directories are read
from the project instead of being hard-coded:

```yaml
root_rules:
  - id: app-root
    key: app_root                # resolved from composer.json's autoload.psr-4
    source: { kind: manifest_json, manifest: composer.json, pointer: autoload.psr-4, pick: shallowest_dir }
    confidence: 1.0

exclude_rules:
  - id: runtime-dir
    glob: "{app_root}/runtime/**"   # <- rendered from the resolved app_root, not a literal name
    fallbacks: ["runtime/**"]
  - id: public-dir
    glob: "{value}/**"
    source: { kind: manifest_json, manifest: composer.json, pointer: extra.public-dir, pick: first_dir }
    fallbacks: ["public/**"]
```

* **Placeholders**: `{value}` is this rule's own `source`; `{<key>}` is the value of the `root_rules`
  entry whose `key` is `<key>`. Both use the single-value `source` kinds as `root_rules`
  (`manifest_json` / `directory_exists` / `manifest`), because resolution happens in **P0, before the
  scan** — nothing has been parsed yet, so only manifests and directory probes are available.
  `root_rules` additionally offers `manifest_entries`, which yields a **list** and therefore cannot fill
  a placeholder.
* **Collections (`manifest_entries`)**: enumerate every child of one key instead of reading a single
  pointer — for the families a project may repeat, whose member **names** are user-chosen. The canonical
  case is database connections: `config/database.php` may declare `mysql` / `mysql_read` / `order`, or
  only `pgsql`, and the driver key is `type` on ThinkPHP but `driver` on Laravel. Which key means what is
  framework knowledge, so it is declared here:

  ```yaml
  root_rules:
    - id: db-connections
      key: db_connections          # fact value = one record per connection
      source:
        kind: manifest_entries
        manifest: config/database.php
        root: connections          # enumerate children of `connections`
        default_from: default      # which pointer names the default entry
        fields:
          - { name: name,   from: key }             # the entry's own name
          - { name: driver, pointer: "{key}.type" } # `driver` on Laravel
          - { name: prefix, pointer: "{key}.prefix" }
  ```

  `{key}` in a field pointer is replaced by the entry name. A field that cannot be resolved is
  **omitted** rather than guessed.
* **`fallbacks` are guesses**: each is kept only when the directory it points at really exists. A stale
  convention therefore cannot delete a directory that merely shares its name — the failure mode of an
  exclusion is always "scanned something useless", never "dropped real source code".
* **A rule that resolves to nothing** is reported as an `ExcludeRuleUnresolved` diagnostic. A rule that
  would resolve to the sub-project root itself (e.g. `{app_root}/**` with `app_root = "."`) is **refused**
  outright.
* Globs are relative to the **sub-project root**; `/**` means "the whole subtree".

The price of resolving this early: framework detection in P0 is the manifest-only subset of P3's
(§2.1) — `import_exists` / `call_exists` need parsed code. A framework recognisable only from code
contributes its excludes one build later, and only after P3 has seen it.

### 2.5 `loaders`: built-in loader ids are language-agnostic

A P3 loader fills one authoritative symbol table (`schema` / `config_keys` / `i18n` / `facade_map` /
`middleware_aliases` …). Its source can be a built-in parser:

```yaml
loaders:
  - id: migration-schema
    table: schema                 # which symbol table it writes
    from:
      kind: builtin
      name: migration_schema      # <-- language-agnostic id
      params:                     # framework-specific paths / spellings live HERE
        paths: ["database/migrations"]
        extensions: ["php"]
```

**Rule: the built-in `name` never carries a language or ecosystem prefix** — write `migration_schema`,
not `php_migration_schema`. The id says **what** is produced; **how** it is parsed belongs to the
tech-stack adapter.

Why:

* The kernel owns only a handful of neutral ids (`db_schema`, `config_keys`, `declared_middleware`,
  `routes`, `nginx_config` — see `prepare.rs::run_builtin`). **Every other id is delegated to the
  tech-stack adapter matched by `sub.language`** (`other => run_adapter_loader`). That single arm is
  what keeps the kernel language-free: adding a stack never means touching `run_builtin`.
* A name one stack claims is therefore reusable by another: `migration_schema` means "DB migrations ->
  `schema` rows". PHP's adapter parses Laravel / ThinkPHP `Schema::create`; a Java or Node adapter
  reuses the same name and parses Flyway / Liquibase / TypeORM. No second id, no kernel branch.
* The language-specific part is `params`, not `name`. That is why the same loader is declared once per
  framework file (see the header comment of `fkb/php/common.yaml`): the paths and spellings differ, the
  id does not.

Consequences worth knowing:

* **A typo loads nothing, and says so only in the log.** An id no stack's adapter recognises is a
  logged no-op (`Ok(vec![])`), not an error — it never falls back to another language's parser. So a
  renamed id fails as "the symbol table is quietly empty", which surfaces far downstream (e.g. zero
  `Column` nodes). Always pin a loader with an end-to-end test, and check the negative case (a bogus
  id must make that test fail) before trusting it.
* **There is exactly one spelling — no aliases.** `php_db_schema` / `php_config_keys` /
  `php_migration_schema` were deprecated and have since been **removed**; the neutral id is the only id
  the kernel or any adapter recognises. Consequence: renaming a loader id in the engine is a breaking
  change for every knowledge file, so grep the whole `fkb/` tree first (and keep an end-to-end test on
  the loader — a stale name is only a logged no-op).
* If a loader's parsing is genuinely language-neutral (SQL install scripts, nginx config), it belongs
  to the kernel — with a neutral id and everything it needs declared in `params`.

---

## 3. A rule = phase + selector + action

```yaml
- id: my-rule
  phase: Synthesize        # pipeline phase (see §3.1)
  selector:                # what it acts on
    kind: call
    callee: "Cache::get|*Cache::*"   # see §3.2
  binding:                 # what to do on a hit (multiple actions allowed)
    - Synthesize: {...}     # create a semantic node (most common)
    # - Link: {...}        # just create an edge
    # - Project: {...}     # project a class of edges onto another layer (§3.4)
    # - Annotate: {...}     # add an annotation
  confidence: 0.9
```

### 3.1 Phase

| Value | Meaning |
|---|---|
| `Synthesize` | **P5 synthesizes non-code semantic nodes** (Cache / Event / Table / HttpContract …). The vast majority of extraction rules use this. |
| `AnnotatePre` | P4 annotates by source selector |
| `AnnotatePost` | P6 annotates / registers aliases on the aggregated result |

(The others -- `Ingest` / `CfAst` / `Prepare` / `Alias` -- are used by the core and the loading
pipeline, and generally don't appear in hand-written rules.)

### 3.2 Selector

The most common is `call`:

```yaml
selector:
  kind: call
  callee: "Cache::get|Cache::has|*Cache::get"   # `|` = or; `*` = wildcard (matched by the language separator, e.g. `think\facade\Cache`)
  where: []                                      # optional narrowing predicates (see the Predicate enum in source)
```

Other selectors: `inheritance` (extends / implements), `config_entry` (config item), `declaration`
(syntax declaration), `node` (an existing graph node), `dynamic` (P7 dynamic-resolution call).

### 3.3 Action (binding)

- `Synthesize`: materialize a semantic node (§4).
- `Link`: create just one edge (`kind` + two `ValueSource`s `from`/`to` + `resolve`).
- `Project`: project **a class of edges onto another layer** (§3.4).
- `Annotate`: add an annotation (used by compliance / asset marking).

### 3.4 The Project action: projecting a class of edges onto another layer

`Link` can only take **one name** per end; it can't reach "the far end of an edge". When what you
want is "A relates to B, and A and B each map to A' and B', so move the relation to between A' and
B'", use `Project`:

```yaml
- Project:
    kind: ForeignKey      # the produced edge kind
    along: References     # walk each **out-edge of this kind** of the matching node (one-to-many)
    from: [MapsTo]        # from the edge's **source**, walk this kind-chain to a landing point (empty = the source itself)
    to:   [MapsTo]        # from the edge's **target**, walk this kind-chain to a landing point (empty = the target itself)
    confidence: 0.7
```

Key points:

- **One-to-many**: a node produces as many edges as it has `along` edges. So "one entity has several
  `@ManyToOne`" won't collapse to only the first one (which `Link` would silently drop).
- **Skip the edge if the landing point is unreachable**: when an entity has no corresponding `Table`,
  its foreign key doesn't enter the graph -- better missing than guessed.
- Usually paired with `phase: AnnotatePost` + `selector: { kind: node }`: projection needs the edges
  built by P5 to be in place.
- Which edge to walk is declared entirely by FKB; the core still knows no framework.

---

## 4. The Synthesize action: how to create a semantic node

```yaml
- Synthesize:
    node: Cache            # node kind (open string). ★ The modern way writes the concrete kind directly; don't write `node: ExternalSystem, subtype: Cache` (old style, still supported)
    # subtype: Cache       # old mechanism: if set, it is "promoted to kind" (node ignored). New rules should use the direct form above
    identity:
      kind: Named          # Fqn | Named | ContractId (see §4.1)
      value: { arg: 0, require_literal: true }   # take argument 0 and it must be a literal
      value_fallback: { literal: "Cache" }        # fallback when it can't be taken
    fields:
      - name: key
        value: { arg: 0, require_literal: true }
      - name: side          # ★★★ see §5.1: out-of-process mediator / asset nodes must carry side
        value: { literal: "backend" }
    link:
      kind: ReadsCache      # edge kind (see §5.4)
      direction: incoming   # incoming (caller → this node) | outgoing | to_target
    confidence: 0.85
```

### 4.1 identity (decides "which calls merge into one node")

- `Named`: named (`{ value: { arg: 0 } }` takes an argument as the name, e.g. a cache key, an event
  name).
- `Fqn`: fully-qualified name (e.g. `Table:store_order`).
- `ContractId`: HTTP contract `METHOD /path` (for `HttpContract`, with `method` / `path` sources).

> **Idempotent merging**: three different rules that compute the same `identity` merge into one node.
> That's why "backend `Cache::get('token')`" and "frontend `uni.setStorageSync('token')`" are split
> into two nodes by `side` (below).

### 4.2 ValueSource (taking values -- the core of FKB's expressiveness)

Common sources (see `ValueSource` in `gt-domain/src/model/fkb.rs`):

`arg` (nth argument) · `element` (argument is an array, take an index) · `field` (take a field of an
object literal) · `property` (class property) · `method_name` (the called method's name) ·
`owner_class` (the class producing the call) · `owner_member` (the method / field producing the call)
· `receiver_class` (the called receiver class, resolved through import aliases) · `entity` (the
**primary domain type** the call site is about, filled by the parser per call kind -- e.g. the first
parameter type of `@EventListener`, or `X` in `publishEvent(new X())`; returns None when it can't be
taken, leaving it to `value_fallback`) · `literal` (a literal) · `require_literal` (accept literals
only, reject variables) · `require_class` (the resolution must be a real class, else None overall) ·
`transform` (snake_plural / lower / …) · `normalize` (normalization chain).

> A typical use of `entity` is **type-level merging**: Spring's `@EventListener` and `publishEvent`
> both use the event type (rather than the send / receive method name) as identity, so publishers and
> subscribers of the same event type merge into one `Event` node (see the `spring-event-*` rules in
> `fkb/java/spring-boot.yaml` + the end-to-end test `tests/java_spring_features.rs`).

> **Taking keyword arguments / list elements** (introduced on the Python side, mechanism is
> language-general): the Python parser captures keyword arguments as `[("queue", value)]` and list /
> tuple literals as `[("0", value), ("1", value)]` **keyed by index**. Hence:
> * `{ arg: 1, field: "queue" }` -- take a keyword argument by name (e.g. Celery's
>   `apply_async(queue=…)`);
> * `{ source: { arg: 1, field: "methods" }, field: "0" }` -- take the first list element (e.g.
>   Flask's `methods=["POST"]`).
>
> Note `element` only works on a **top-level `arg`**; nested access must use `field` by name, which is
> why lists are keyed by index.

> **`short_name` normalization**: take the last segment of a dotted / namespaced path
> (`app.tasks.send_email` → `send_email`). Used when "an entity's fully-qualified name and its short
> name must merge into one node" -- e.g. a Celery task's registration side has only the short name
> while the dispatch side resolved to the fully-qualified name via `import`. The difference from
> `strip_namespace`: it additionally splits on `.` (`strip_namespace` deliberately doesn't split `.`,
> otherwise it would strip Java auto-routed package names too).

---

## 5. Conventions (the easiest place to trip -- please read)

### 5.1 `side`: splitting frontend and backend (most important)

Declare it **once per knowledge base**, at the top level:

```yaml
id: thinkphp
language: php
side: backend            # frontend | backend | external — a closed set, validated at load time
```

The loader then fills it in for every `Synthesize` action in that file which does not state one
explicitly, so no rule can forget it. The engine injects that value into the node `identity`'s scope
(see `with_scope` in `engine.rs`), which decides **which nodes merge**: without it, frontend
`uni.setStorageSync('token')` and backend `Cache::get('token')` share a name and merge into **the
same** Cache node, and the graph becomes a mess. It is also what the "cache perspective / event
perspective" split relies on (`side: backend` / `side: frontend` in `views/perspectives.yaml`).

Details:

* **Which rules inherit it**: those whose synthesised node kind already carries a `side` somewhere in
  the shipped knowledge (`Cache` / `ConfigKey` / `Event` / `EventBus` / `HttpContract` / `I18nKey` /
  `Page` / `Queue` / `Store` / `Table` — the list lives in `loader.rs`). Widening it to every kind would
  silently re-key nodes that were never party-aware, because the scope takes part in the merge key.
* **A per-rule `fields: [ { name: side, ... } ]` still wins** — an escape hatch for knowledge that
  genuinely crosses parties.
* **Cross-language knowledge (`language: "*"`, e.g. `universal/common.yaml`) must NOT declare one**:
  the same file is loaded for every party, so a single value would be a lie — state it per rule there,
  if at all.
* **Why this replaced per-rule literals**: repetition is exactly why `python/celery.yaml` and
  `java/spring-boot.yaml` (Event / Queue) + `java/spring-cache.yaml` (Cache) ended up declaring none at all — their Cache / Event / Queue nodes then had
  no party evidence and vanished from every side-filtered perspective. The carried-over lesson is that
  `side` is one value per *particle* (per sub-project role), not one per rule; the whole point of the
  closed vocabulary is that a PHP producer and a Python consumer stay on the same `backend` side and
  therefore merge onto the same Queue node.

**`side` is one claim, not the whole truth — never select on it.** Two facts follow from that:

1. Several parties can write the **same** synthesised node (that is the point of a contract bridge:
   the backend `Route::post('/api/delete')` and the frontend `axios.post('/api/delete')` both exist).
   So `side` is accumulated into a sorted de-duplicated set **`sides`**, and the scalar `side` becomes
   a derived label: one party ⇒ that party, several ⇒ `bridge`. Which means `side` **never** tells you
   "is this party involved" — after the merge it reads `bridge` for both.
2. Therefore: whenever you need "does X really touch this node", select on the **evidence**, not on the
   property — e.g. `frontend.called` is judged by `where: [ has_incoming: CallsHttp ]`, not by
   `side = frontend` (same for the compliance rule `frontend-calls-missing-backend`).

`identity.scope` deliberately keeps using the **rule-declared** literal (that is what decides "are
these the same thing"), so nothing about merging changes; `sides` is read-only derived data and must
**never** be folded into `IdentityKey::scope` — doing so would make the merge key depend on how many
parties happen to have touched the node.

### 5.2 `semantic_kinds`: introducing a new node kind

**Built-in semantic node kinds** (the collapsed view shows only these by default):
`Table` · `HttpContract` · `ConfigKey` · `I18nKey` · `Event` · `Queue` · `Cache` · `Topic` ·
`Schedule` · `Page` · `EventBus` · `EventHandler`.

If your FKB uses a **new kind not on that list** (e.g. `Store`), you **must** declare it at the top
level:

```yaml
semantic_kinds: [Store]
```

Otherwise the node isn't treated as semantic and the collapsed view hides it (it looks like "wrote
it, nothing happened"). Reference: `fkb/js/uni-app.yaml` declares
`semantic_kinds: [Store, Page, EventBus]` (`Store` isn't built-in, so it must be declared).

### 5.3 Getting a node into a "perspective"

For a node to show up in a perspective, its `kind` (and `side`) must be registered in
`views/perspectives.yaml`:

- Built-in kinds already have matching perspectives (cache / local_storage / event / event_bus /
  queue / topic / route / table …).
- If you use a **new kind**, add a perspective to `perspectives.yaml` (declaring `node_kind` +
  optional `side`) and a mapping in `node_views`; otherwise clicking that node only opens the
  Inspector instead of switching perspective.

### 5.4 Edge kinds: new edge kinds can also be "FKB-only, zero code" (isomorphic to nodes)

**Built-in semantic edges**: `Triggers` · `PublishesTo` · `ReadsDb` · `WritesDb` · `MapsTo` ·
`ReadsConfig` · `ResolvesTo` · `WritesCache` · `Mutates` · `NavigatesTo` · `Emits` · `ListensTo` ·
`ReadsCache`.
**Built-in bridge edges**: `HandledBy` · `CallsHttp`.

In most cases **reuse existing edge kinds**; don't invent new vocabulary. But if you genuinely need a
brand-new edge kind, you now **don't need to change `kinds.rs`** either -- just declare it at the FKB
top level and it is registered into the registry at load time:

```yaml
semantic_edge_kinds: [SendsWebhook]   # new semantic edge: counted / drawn as a semantic edge
bridge_edge_kinds:   [MyBridge]       # new bridge edge: connects a semantic node ↔ a syntactic node
```

After declaring, `is_semantic` / `is_bridge` / `is_chain_edge` treat it as a first-class citizen
automatically (isomorphic to how `semantic_kinds` handles nodes), so rendering and the "N in-edges"
count are correct.
> Note: the engine's **dedicated rendering** for **brand-new** edge kinds (e.g. the bridge-edge
> layout of `HandledBy` / `CallsHttp`) is still a built-in special case; a new edge kind falls back to
> default rendering. This matches "nodes display without code changes" -- classification is zero-code,
> only dedicated styling needs a small change.

`graphtell validate` understands this too: an edge kind is not reported as "unregistered" as long as
it is in the core list **or** in any FKB's `semantic_edge_kinds` / `bridge_edge_kinds`.

---

## 6. Complete examples

### 6.1 Backend cache (excerpt from `fkb/php/common.yaml`) -- shows `side: backend`

```yaml
- id: cache-predis-read     # namespaced to `php-common-cache-predis-read` at load time (§2.3)
  phase: Synthesize
  selector:
    kind: call
    callee: "Cache::get|Cache::has|*Cache::get|*Cache::remember"
  binding:
    - Synthesize:
        node: Cache
        identity:
          kind: Named
          value: { arg: 0, require_literal: true }
          value_fallback: { literal: "Cache" }
        fields:
          - name: key
            value: { arg: 0, require_literal: true }
          - name: side
            value: { literal: "backend" }
        link: { kind: ReadsCache, direction: incoming }
        confidence: 0.85
```

### 6.2 Frontend event bus (`fkb/js/uni-app.yaml`) -- shows `semantic_kinds` + `side: frontend`

```yaml
id: frontend-js
language: javascript
semantic_kinds: [Store, Page, EventBus]   # Store isn't built-in, so it must be declared
rules:
  - id: emit                 # namespaced to `frontend-js-emit` at load time (§2.3)
    phase: Synthesize
    selector:
      kind: call
      callee: "uni.$emit|uni.$on|\\$eventHub.$emit"
    binding:
      - Synthesize:
          node: EventBus
          identity:
            kind: Named
            value: { arg: 0, require_literal: true }
          fields:
            - name: side
              value: { literal: "frontend" }
          link:
            kind: Emits
            direction: outgoing
```

### 6.3 Java HTTP contract (`fkb/java/spring-boot.yaml`) -- shows `ContractId` identity

```yaml
- id: mapping-http-contract  # namespaced to `spring-boot-mapping-http-contract` at load time (§2.3)
  phase: Synthesize
  selector:
    kind: call
    callee: "RequestMapping|GetMapping|PostMapping|PutMapping|DeleteMapping"
  binding:
    - Synthesize:
        node: HttpContract
        identity:
          kind: ContractId
          method: { method_name: true }                 # GET/POST…
          path:   { arg: 0, require_literal: true }     # "/api/login"
        link: { kind: HandledBy, direction: incoming }
```

---

## 7. Generating FKB in bulk with AI (recommended)

FKB is declarative YAML with fixed matching semantics, which suits an LLM distilling from framework
docs / source:

1. Give the LLM **this guide** + **2–3 reference samples** (`fkb/php/common.yaml`,
   `fkb/js/uni-app.yaml`, `fkb/java/spring-boot.yaml`) + the target framework's docs / source.
2. Have it produce one FKB YAML (key points: use existing node / edge kinds, remember `side`, and
   remember `semantic_kinds` for new kinds).
3. Run `graphtell validate --fkb-dir <your-dir>` for syntax + convention checks.
4. Run `graphtell create --path <sample-repo>` against real code and **look at whether the graph is
   drawn correctly**.

> ⚠ AI will produce FKB that "looks right but actually mis- or under-matches" -- step 4 must be done;
> never trust the generation alone.

---

## 8. The `validate` subcommand

```
graphtell validate            # validate the built-in FKB directory
graphtell validate --fkb-dir ./my-fkbs   # validate your own directory
```

Reports per file:

- `✓` parsed: prints `id` / language / rule count / of which synthesized nodes / `semantic_kinds`.
- `✗` parse failed: prints the concrete error (field-level).
- `⚠` convention warnings (most valuable):
  - a rule produces a node kind that is neither on the core list nor in `semantic_kinds` -- the
    collapsed view will hide it;
  - an edge kind is neither in `kinds.rs`'s `SEMANTIC` / `BRIDGE` list nor declared in any FKB's
    `semantic_edge_kinds` / `bridge_edge_kinds` -- it won't render as a semantic / bridge edge
    (§5.4: declaring it in the FKB is enough; no `kinds.rs` change).

Finally it summarizes "N passed / M failed", exiting non-zero on failure.

---

## 9. When you need to change the engine (instead of just writing FKB)

In these cases **FKB alone isn't enough**, and Rust must be touched:

1. The engine doesn't yet support a **matching / value-taking capability** you need (e.g. some new
   parameter extraction, a new resolution strategy) -- extend `ValueSource` / `ResolveStrategy` /
   selectors.
2. A node or edge kind needs **dedicated rendering** (e.g. the `Event|Queue|Topic` branches hardcoded
   in `view_service.rs`, or the bridge-edge layout of `HandledBy` / `CallsHttp`). Declaring a new edge
   kind itself needs no code -- see §5.4.

> Note: a **brand-new edge kind** needs no line in `SEMANTIC` / `BRIDGE` in `kinds.rs` -- declare
> `semantic_edge_kinds` / `bridge_edge_kinds` at the FKB top level instead (§5.4). Only *dedicated rendering* for such a kind still needs an engine change.

Most real frameworks (Spring / Laravel / Django / Rails / Express …) map onto the **existing**
Cache/Event/Queue/HttpContract/Table/ConfigKey kinds, so about 90% of FKB can be done with **zero
engine changes**.
