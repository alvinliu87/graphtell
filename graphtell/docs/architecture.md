# GraphTell

> This file is the **technical documentation** (architecture / pipeline / FKB / deployment /
> measured data / extension).
> For the product overview, online demo and downloads see **[the repo-root README](../../README.md)**.

An analysis platform that turns any codebase into a **graph**: `tree-sitter` parses syntactic nodes,
then the **Framework Knowledge Base (FKB)** synthesizes semantic nodes, yielding a graph you can
query, annotate, and analyze for blast radius and dead code.

- Backend: Rust (**ports-and-adapters** layering, core has zero IO dependencies), SQLite persistence
- Frontend: React + TypeScript + Ant Design (**top-down** layering)
- Desktop resident app: Tauri (the backend starts an HTTP service **in-process**; the desktop and web
  apps share the same `/api` contract)
- Goal: cover all mainstream stacks via tree-sitter -- currently landed **PHP** (ThinkPHP 6 / CRMEB /
  Laravel / Uni-app backend contracts), **Java** (Spring Boot), **JavaScript/TypeScript** (Uni-app
  frontend / NestJS·Express backend / TypeORM entity mapping) and **Python** (FastAPI / Flask /
  Celery / SQLAlchemy). The full support matrix and known boundaries are in
  [`SUPPORTED.md`](../SUPPORTED.md).

Once the graph is built it can answer two more questions:

| Capability | Input | Output |
| --- | --- | --- |
| **Compliance check** ([below](#compliance-check-running-rules-on-the-graph)) | rules declared in `rules/*.yaml` | violation list (`path:line`, jumpable) |
| **Prompt augmentation** ([below](#prompt-augmentation-querying-the-graph-by-prompt)) | a prompt | which code to look at + a context pack you can paste into an LLM |

---

## Quick start

```bash
# 1) build the backend
cargo build

# 2) create a project and build the graph automatically (P0 → P7)
./target/debug/graphtell create --name CRMEB --path /path/to/CRMEB-master

# 3) start the HTTP service (shared by web / Tauri frontends)
./target/debug/graphtell serve --port 5177

# 4) frontend
cd ui && npm install && npm run dev      # http://localhost:5173

# 5) desktop app (needs a system WebView: Windows WebView2 / macOS WKWebView / Linux webkit2gtk)
cd ui && npm run tauri dev
```

Other commands:

```bash
./target/debug/graphtell list
./target/debug/graphtell run    --project 1
./target/debug/graphtell stats  --project 1
./target/debug/graphtell export --project 1   # export the graph (nodes / edges / file paths) as JSON, for the static demo / external analysis
./target/debug/graphtell delete --project 1

# compliance check
./target/debug/graphtell rules
./target/debug/graphtell check  --project 1                 # full check and write to the DB
./target/debug/graphtell check  --project 1 --rule hot-table  # run a single rule
./target/debug/graphtell check  --project 1 --dry-run --json  # preview, print the full report

# prompt augmentation
./target/debug/graphtell recall --project 1 --query "store_order 订单表"
./target/debug/graphtell recall --project 1 --query "优惠券相关代码" --markdown  # print the LLM context pack
```

---

## Architecture

### Backend: ports-and-adapters layering

```
                ┌─────────────────────────────────────────────┐
   inbound      │  gt-adapter-http (axum)   src-tauri (Tauri) │
   adapters     └───────────────┬─────────────────────────────┘
                                │ use-case calls
                ┌───────────────▼─────────────────────────────┐
   application  │  gt-application：ProjectService /            │
                │  PipelineService / GraphQueryService         │
                └───────────────┬─────────────────────────────┘
                                │ domain services
                ┌───────────────▼─────────────────────────────┐
   pipeline     │  gt-pipeline：P0 Ingest → P2 CfAst → P3      │
                │  Prepare → P4 AnnotatePre → P5 Synthesize    │
                │  → P6 AnnotatePost → P7 Resolve              │
                └───────────────┬─────────────────────────────┘
                                │ ports (traits)
                ┌───────────────▼─────────────────────────────┐
   domain       │  gt-domain：entities / value objects / port traits
                └─────────────────────────────────────────────┘

   outbound     gt-adapter-parser(tree-sitter)  gt-adapter-fkb(YAML)
   adapters     gt-adapter-sqlite(SQLite)       gt-adapter-fs(scanning)
```

**Dependencies always point inward**: `gt-domain` depends on no concrete technology; all IO is
injected backwards through the traits defined in `port` (`Persistence` / `FileSystem` / `FileScanner`
/ `ParserRegistry` / `KnowledgeProvider` / `PipelineObserver` / `Clock`).

Two deliberate design choices:

- **Persistence is split into 6 fine-grained traits** (`ProjectReader` / `ProjectWriter` / `GraphSink`
  / `GraphQuery` / `SymbolTableReader` / `DiagnosticSink`), then composed into `Persistence` via a
  blanket impl -- an implementer only cares about the part it uses, and the SQLite / in-memory
  implementations are interchangeable.
- **`NodeKind` / `EdgeKind` / `Phase` are open strings + constant shorthands** -- a new language,
  framework or node kind needs no core change (see "Extending to a new language / framework" at the
  end).

### Frontend: layering and dependency direction

```
ui/src
├── app/        app entry: theme, routes, global styles
├── pages/      ProjectsPage / GraphPage / ExplorerPage / DiagnosticsPage
├── widgets/    AppShell / ProjectTable / PipelineProgress / GraphCanvas
├── features/   create-project / delete-project / run-pipeline / node-detail
├── entities/   project / graph / pipeline (model + api + hooks)
└── shared/     api(HTTP) / lib(format, useAsync) / ui(PageHeader, StatCard)
```

Dependencies may only go **top-down**: `pages → widgets → features → entities → shared`.

---

## Pipeline

| Phase | What it does | Output |
| --- | --- | --- |
| **P0 Ingest** | recognizes sub-projects (`composer.json` / `package.json` / `pom.xml` …), excludes `vendor`, `node_modules`, `target`, static assets, build outputs | sub-projects + files to analyze |
| **P2 CfAst** | language-agnostically lands `SyntaxFacts` as nodes | `Class` / `Interface` / `Trait` / `Enum` / `Method` / `Function` / `Property` / `Const` / `Namespace` / **`CallSite`**; `imports` table (short name → FQN); `by_name` index; extends / implements / trait edges |
| **P3 Prepare** | recognizes frameworks per FKB, resolves `AppRoot`, loads authoritative sources | `app_root`, container bindings, event table, `schema`, `config_keys`, `i18n`, `facade_map`, `route_list`, `nginx` |
| **P4 AnnotatePre** | selectors act on **source** | Taint (`source` / `sanitizer` / `sink`), `listener`, and other tags |
| **P5 Synthesize** | **idempotently synthesizes** semantic nodes by `identity` | `Table` / `HttpContract` / `ConfigKey` / `I18nKey` / `Event` / `Queue` / `Cache` / `Topic` + edges like `HandledBy` |
| **P6 AnnotatePost** | selectors act on **graph nodes** | `pii.phone`, `data.criticality`, `config.storage`, `auth.public`, `entrypoint.login`, `i18n.missing_locale`; registers `by_alias` |
| **P7 Resolve** | funnel resolution (L1 literal → L2 registry → L3 alias → L4 convention → L6 intersect with the full set) + **fixpoint iteration** | dynamic edges: `ResolvesTo` / `Triggers` / `HandledBy` |

The phase **order cannot be reversed**: P5 queries P3's authoritative symbol table; P6 queries P5's
aggregated result (fan_in is only accurate after aggregation); P7 queries the aliases P6 registered.

---

## Key design

### 1. `identity` idempotent merging

Three different rules (`Db::name('store_order')`, a Model's `$table`, a class-name convention) merge
into **the same** `Table` node as long as they compute the same `identity`:

```yaml
identity:
  kind: Fqn
  value: { arg: 0 }
  normalize: [ { strip_prefix: [] }, singularize, { strip_prefix: [] } ]
```

`strip_prefix: []` (an empty list) means "use the table prefix detected for the current project",
rather than hardcoding a specific prefix. The prefix is read automatically at P3 by FKB's `db_prefix`
root_rule from framework config (e.g. ThinkPHP's `connections.mysql.prefix` in `config/database.php`,
supporting the `env('KEY', 'default')` default), or comes from project config
`ProjectConfig.table_prefixes`; the generic layer `ProjectConfig::default()` bakes in no
project-specific prefix.

Normalization makes `store_order` / `eb_store_order` / `store_orders` converge onto one node;
otherwise fan_in would become 67+66+67 instead of 200, and blast-radius analysis plus dead-table
detection would all be distorted.

### 2. The contract bridge: `HttpContract`

Backend `Route::post('apple_login', 'Login/appleLogin')` and frontend
`uni.request({url:'/api/apple_login', method:'POST'})` normalize to **the same identity**
`POST /apple_login` → merged into one node. So "dead endpoint (backend only)" and "ghost call
(frontend only)" become detectable automatically.

### 3. Everything through FKB, nothing hardcoded

Each YAML under `fkb/` declares: how to recognize the framework, how to resolve `AppRoot`, which
authoritative tables to load, which rules run at which phase. Adding a framework = adding one YAML.
The core knows nothing about ThinkPHP, CRMEB or Uni-app.

For example, `AppRoot` resolution (`autoload.psr-4` in `composer.json`):

```json
{"value": "app", "confidence": 1.0,
 "source": ".../crmeb/composer.json autoload.psr-4 (map_dir=app/)",
 "fallback_used": false}
```

#### The core's default must be **empty**, never one stack's

"Nothing hardcoded" is only true if the **fallback** knows no language either. A stack-specific constant is
bad; a stack-specific *default* is worse, because it fires exactly where nobody is looking (no parser wired,
no knowledge declared) and returns plausible-looking output instead of an honest empty result.

The rule: **knowledge that differs per stack has no neutral default — when it is missing, the step is
skipped.** Where it has landed so far:

| Where | The wrong default | Now |
|---|---|---|
| `NamespacePolicy` | `Default` = PHP (`\` + `::` + PHP builtin types) | empty = "notation unknown"; `ns_separator` is `Option<char>` |
| `LanguageParser::member_separator()` | defaulted to `"::"` | **required** — a parser that forgets it fails to compile |
| primitive / builtin types | `PHP_BUILTIN_TYPES` lived in `gt-domain::port` | each parser declares its own list inside its own crate |
| P11 Sign vocabulary | PHP function names + a `language == php` gate | FKB `sign_check`; a stack declaring none is not judged |
| built-in loader ids | a `php_migration_schema` arm in `run_builtin` | language-agnostic id, dispatched to the tech-stack adapter |
| P7 name resolution | `trim_start_matches('\\')` / `split("::")` | separators from `lang_policy_for_sub(sub)` |

Two corollaries that have already cost real bugs:

* **"Unknown" must be visible, not silently substituted.** `runner.rs` used to fall back to "whichever
  parser registered first" (alphabetically `java`), which is what kept a Python project shipping only
  `requirements.txt` — sub-project language `unknown` — working *by accident*. The fix was the cause (the
  marker list now covers `requirements.txt` / `setup.py` / `Pipfile`), not restoring a lucky default.
* **A test that passes because of a default passes for the wrong reason.** When `NamespacePolicy`'s default
  stopped being PHP, a unit test broke: it exercised PHP spelling (`think\facade\Db`) without ever stating
  the language. The fix was to inject the policy in the test, not to restore the default.

Where such knowledge belongs: **notation and parser facts → the language adapter** (`gt-adapter-parser`,
`gt-adapter-php`); **vocabulary and conventions → FKB** (the framework file, or the unconditional language
layer `fkb/<lang>/common.yaml` with `apply_without_detection: true`).

### 4. Diagnostics are a first-class product

`UnresolvedLink` (a route pointing at a non-existent handler → runtime 500),
`AnnotateTargetMissing`, `AliasTargetMissing` (target is in vendor, which is expected) and
`IdentityUnresolved` all land in the diagnostics table and are shown in the UI.

---

## Interaction model: from "understanding" to "acting"

### 1. Two-level filter + single-object chain

Two fixed levels at the top: **level 1 picks the perspective, level 2 picks the object**. After
selecting, only **this one object**'s chain subgraph is rendered; other objects' chain edges are
**not drawn at all** (not dimmed). Honesty about the deliberately omitted parts is kept by three
things:

* **Count hint** -- `74 edges drawn; another 96 belonging to other objects' chains deliberately omitted`
* **Unresolved accounting** -- the panel lists `UnresolvedLink` / `AnnotateTargetMissing` etc.
* **Switchable** -- the level-2 list can switch to another object at any time

Perspectives are declared in `views/perspectives.yaml`, in two categories with completely different
semantics:

| | Object perspective | Aggregate perspective |
| --- | --- | --- |
| Examples | route / table / Schedule / Event / Queue / Cache / Topic | Domain / DeployUnit / Platform |
| Semantics | **single chain**: center + concentric rings | **aggregate overview**: cluster boxes / matrix, not a single chain |
| Level-2 filter | yes | no |

### 2. Layout is decided by the algorithm; no node may drift freely

| View mode | Layout | Edge style |
| --- | --- | --- |
| Object entry subgraph (default) | **radial / concentric rings** (ring = hop count) | straight |
| Layered call chain | **layered** (top-down) | 90° orthogonal polyline |
| Path mode (forensics) | **linear Spine** (longest chain as the main axis) | straight |
| Aggregate overview | **compound clusters** (big box + counts) | straight |
| Multi-end comparison | **matrix** (two dimensions, cell color blocks) | none |
| Table relations / ER | **ER orthogonal** | 90° |

The same input always yields the same output -- reproducible, screenshot-comparable, unit-testable.

### 3. Click to switch perspective (only for nodes that have one)

| Node | Click behavior |
| --- | --- |
| `HttpContract` / `Table` / `Schedule` / `Event` / `Queue` / `Cache` / `Topic` / `Middleware` | switch to the corresponding **object perspective**, level 2 synced to that node |
| `Domain` / `DeployUnit` / `Platform` | switch to the **aggregate perspective** (box + count / matrix) |
| `ConfigKey` / `KeyPattern` / `Component` / `SecretLocation` | **don't switch** the top filter, only open the right-side Inspector |

Supporting constraints: hover only highlights, click switches, right-click and the detail icon don't
switch, the breadcrumb can go back, the old center is kept as a neighbor marked `from`, and the
level-2 list highlights in sync.

The middleware perspective deliberately uses only `depth: 1`: one auth middleware often guards
hundreds of endpoints (CRMEB's `AllowOriginMiddleware` guards 280), and expanding one more hop
would pull in every table / config touched by the endpoints it guards -- that's not "who this
middleware guards", it's a whole graph. To see what resources a given endpoint later touches, click
that endpoint to switch to the route perspective.

### 4. The URL is the scene

`?p=route&n=58548&d=2&m=radial&i=123` is the complete scene: refresh, forward/back and shared links
all restore it. When the URL goes stale (node id invalid, perspective gone), `reconcileViewState`
corrects it and falls back to defaults -- never silently showing an unrelated graph.

### 5. Jumping: how the graph proves its own trustworthiness

* **Node → definition locations**: a synthesized node (`Table:user`) necessarily comes from multiple
  co-occurrences (SQL CREATE TABLE + the Model's `$table` + each call site), so it gets a
  **multi-location list** rather than a fabricated single location. The `user` table has 14
  locations in this sample.
* **Edge → evidence chain**: a solid line is a single hop; a dashed line expands every CallSite
  location passed through, letting the user verify personally -- that's what makes "a dashed line is
  a hypothesis to verify" concrete.
* **Unresolved** → jump to the broken-link point itself and label the reason.

Interaction is strictly separated from "click to switch perspective": **click = navigation; jumping
goes through right-click / the hover icon / Inspector's Open in IDE**.

Technically it uses `vscode://file:line`, `jetbrains://`, `cursor://`, and **must ship "copy
path:line" as a fallback** (covering CI / web / no IDE / remote containers). Location uses the
**path + symbol + line triple** to guard against line-number drift. `SecretLocation` jumps only to
the key-name location and never displays the value -- an analysis tool must not become a leak source.

## Compliance check: running rules on the graph

Rules are declared in `rules/*.yaml` and loaded by `gt-adapter-rules`; **the core knows no concrete
rule** -- adding a rule means adding one YAML.

```yaml
- id: http-contract-without-handler
  title: HTTP contract has no handler
  severity: error
  category: contract
  applies_to: { kinds: [HttpContract] }
  when:
    - no_outgoing: HandledBy     # contract --HandledBy--> handler, an **out-edge**
  message: "Contract {name} resolved to no handler; calling it will fail at runtime"
```

**Violations land directly as `Diagnostic`s** (code prefix `rule:`). Diagnostics are already a
first-class product (with `severity` / `location` / `payload`), so the rule engine **needs no new
storage and introduces no new data structures**. Available predicates:

| Category | Predicates |
| --- | --- |
| Topology | `no_incoming` / `has_incoming` / `no_outgoing` / `has_outgoing` / `fan_in_gte` / `fan_in_lte` / `fan_out_gte` |
| Annotation | `has_annotation` / `no_annotation` / `no_capability` (`Authentication` / `RateLimiting`) |
| Property | `property_is` / `property_missing` |
| Name | `name_contains` / `name_starts_with` / `fqn_contains` / `identity_contains` / `text_contains` |
| Combination | `all_of` / `any_of` / `not` |

Rules are loaded per **applicable environment** by directory; the core knows no concrete rule --
adding one means adding one YAML:

| Directory | Applies to | Content |
| --- | --- | --- |
| `rules/global/` | language-agnostic | rules depending only on **graph topology** (fan-in/out, semantic edges): the contract bridge (`http-contract-without-handler` / `frontend-calls-missing-backend` / `backend-endpoint-never-called`), hot tables (`hot-table`), config-read hotspots (`config-read-hotspot`), dead tables (`dead-table`), write-only / read-only tables (`write-only-table` / `read-only-table`), high-fan-out methods (`hotspot-method`), the cross-language runtime smell **external call inside a loop** (`ext-call-in-loop`, consumed from the P12 annotation) |
| `rules/php/` | PHP projects only | criteria depend on edges / annotations only PHP FKB produces: raw SQL execution points (`raw-sql-sink`), PII tables (`pii-table-needs-review` / `pii-table-hot`), never-triggered events / queues (`orphan-event` / `orphan-queue`), **per-row DB read / write inside a loop** (`n1-query-in-loop` / `n1-write-in-loop`, below), **external call inside a loop** (`ext-call-in-loop`, repeated per stack as `js-` / `python-` / `java-ext-call-in-loop`), **multiple writes without a transaction** (`multi-write-without-tx`), **signature-verification quality** (`sign-compare-loose` / `sign-weak-hash`, below) |
| `rules/js/` | projects with a frontend sub-project | dead code on the frontend event bus (`eventbus-emitted-without-listener` / `eventbus-listened-without-emitter` / `eventbus-orphan`) -- `EventBus` and `Emits` / `ListensTo` are frontend semantics and don't exist in a backend-only project |

30 rules ship built-in (of which `write-endpoint-without-auth` is `enabled: false` by default because
the capability channel isn't produced yet, so its criterion is always true). Supporting more
languages later only means adding rules for that stack under `rules/<lang>/` and declaring
`languages`; neither the core nor the `global` layer changes.

#### Two new graph facts used by N+1 (per-row DB read / write inside a loop)

A loop is a control-flow concept that `CallSite` alone cannot express: it records only "who called
whom", not "how many times". So this rule depends on two new facts, both on
**CallSite nodes**:

| Fact | Produced by | Notes |
| --- | --- | --- |
| `properties.in_loop` | P2 CfAst (value from the PHP parser) | whether the call site sits in the **body** of a `for` / `foreach` / `while` / `do-while`. A call in the loop **condition** is evaluated once and doesn't count |
| `db-query` / `db-write` annotation | P7 Resolve | the **call-site-level projection** when FKB `db_verbs` judges this call site read / write and it really lands a `ReadsDb` / `WritesDb` edge |

Attaching to the call site rather than the method is because a method-level `ReadsDb`, once
propagated along the call chain by P8, would cover every upstream caller -- that only yields "this
method's call chain read the DB", unable to distinguish "queried N times inside a loop" from "queried
once outside it"; the noise would share a root with the rejected "god method" rule.

Known boundary: `db-query` depends on FKB's `db_verbs`. Both `thinkphp` and `laravel` declare it, so
N+1 runs on both kinds of PHP project (Laravel's Eloquent / Query Builder verb list is under
`db_verbs` in `fkb/php/laravel.yaml`). The Java side isn't done (`mapper.xxx()` would need Java
`db_verbs` + loop recognition in the Java parser).

#### What the signature rules (`sign-compare-loose` / `sign-weak-hash`) do and don't judge

**They judge**: how the signature is compared after being computed -- `$sign == $calc` /
`$this->CreatedSign($params) != $params['sign']`. PHP's `==` / `!=` are loose comparisons (`0e…`
digests compare equal) and not constant-time; the correct form is `hash_equals()`. And whether the
signature uses `md5` / `sha1` (`info` level: WeChat V2 / older Alipay / some shipping gateways
officially require MD5, so reporting it as a "vulnerability" would be a false positive).

**They don't judge**: whether the callback **verifies the signature at all**. That criterion must
cross procedures into the SDK internals, and PHP excludes `vendor` while Java doesn't scan Maven
dependencies -- EasyWeChat / yansongda-pay / official SDK `verify()` simply isn't in the graph, so
any "no verification call on the path" criterion holds for every callback (100% false positives).

The graph facts needed are likewise parser additions: a comparison expression isn't a call site, so
`==` was invisible on the graph; hence
[`SignCompareFact`](../crates/gt-domain/src/model/syntax.rs) was added (collecting only `==` / `!=`
where at least one side looks like a signature value), and P11 `phase::sign` then applies the
`weak_sign_compare` / `weak_sign_hash` annotations.

The noise gate lives in the parser: **in e-commerce code `sign` is overwhelmingly "check-in"**
(`$sign_mode` / `$sign_last_date` / `$sign_total_days` / `$points_sign_enabled`). Of 32 "== comparisons
containing sign" measured, 24 were check-ins, so neither side of the comparison may be a string
literal, and check-in words like `sign_type` / `sign_mode` are excluded.

#### Runtime smells: `ext-call-in-loop` / `multi-write-without-tx`

These two are the natural extension of N+1 under the "loop / batch" theme; both **report only
established facts, never judge vulnerabilities** -- the fix is left to people and context.

- **`ext-call-in-loop` (external call inside a loop)**: one network round-trip costs an order of
  magnitude more than one DB query, so putting it in a loop (`curl_exec` / `Http::get` /
  `GuzzleHttp\Client::request` / `Mail::send` …) multiplies interface latency serially by N. The
  criterion fully reuses N+1's `in_loop` fact, only swapping the verb list for `external_calls`
  (declared per stack in `fkb/{php,js,python,java}/common.yaml`; `fkb/php/guzzle.yaml` adds the Guzzle
  form). P12 `phase::external` applies the `ext-call-in-loop` annotation — and does so with **no language
  gate**, so the rule consuming it is declared once per stack (`rules/{php,js,python,java}/runtime.yaml`,
  each with its own id, the `n1-query` precedent). It cannot live in `rules/global/`: a rule declaring no
  `languages` must depend only on graph topology, never on an annotation a given stack may not produce.
- **`multi-write-without-tx` (multiple writes without a transaction)**: one method writes directly to
  ≥2 distinct tables (deduped `WritesDb` edge targets, counting only **direct** edges landed by P7,
  not indirect ones propagated by P8), and the method carries no transaction marker (`transaction` /
  `startTrans` / `commit` …). Failure at any intermediate step leaves partially-applied dirty data.
  P13 `phase::tx` applies the `multi-write-without-tx` annotation (on the **method node**, since it is
  a method-level boundary problem).

| Rule | Why "table count" instead of "write-verb count" | Why it may under-report |
| --- | --- | --- |
| `multi-write-without-tx` | "≥2 write verbs at call sites" would count two `if/else` branches writing the same table (`CartLogic::add`'s `update` / `insert`) as two writes -- those are mutually exclusive branches with no partial success. Switching to "≥2 tables" makes such false positives disappear (likeshop dropped from 105 to 20) | the transaction may open in an outer caller (cross-procedure), invisible on the graph → the text says "no transaction boundary identified", not "no transaction" |

Measured (12 samples): `ext-call-in-loop` hits 0 in most projects (remote calls inside loops really
are rare in mature e-commerce code; bagisto has just 1), while `multi-write-without-tx` has 14–65 hits
on likeshop / beikeshop / shopxo / bagisto / CRMEB, mostly at entries that genuinely need a
transaction such as refunds / stock deduction / withdrawals (`OrderGoodsLogic::decStock`,
`WithdrawLogic::confirm`).

### What makes a new rule "shippable": measure before writing

Writing a rule is cheap; **verifying it doesn't produce noise is expensive**. Every candidate is
measured against the sample library (8 projects already built: 5 ThinkPHP + 3 Spring Boot) before
deciding to ship. There are only two criteria: hits **must not be 0** (silent failure) and **must not
flood** (noise).

The following candidates were **rejected** exactly this way (kept on record so they aren't
re-discussed later):

| Candidate | Measurement | Conclusion |
| --- | --- | --- |
| Private method never called | extracted 8 samples to verify: `$this->resultError()` is called 4 times in source yet has no `Calls` edge -- method-level `Calls` resolution coverage is insufficient (~44% of calls only resolve to class level) | **Rejected**. All "never called" rules are false-positive-dominated under current graph coverage |
| Config key with no reader | all 250 backend config keys **do** have readers; the only 110 hits were frontend locale keys mis-recognized as `ConfigKey` | **Rejected** (0 hits or pure noise). Reworked into the positive `config-read-hotspot` |
| Cache key written but never read | sampling `comGoodsId` / `diyVersionNav`: `getStorageSync` clearly exists in source, yet no `ReadsCache` edge on the graph | **Rejected** (gap in read-side resolution) |
| Event bus emitted without listener / listened without emitter | 12 hits sampled and verified, ~70% are genuinely dead code | **Shipped**, level `info`, with text stating both possibilities (following the honest style of `frontend-calls-missing-backend`) |
| Table written but never read | hits are log / audit tables like `system_event` / `wechat_message` | **Rejected** ("write-only" is normal design for log tables) |
| Table written but no model mapping (`MapsTo`) | 100% hits on Java projects (that edge isn't produced on the Java side at all); PHP hits mix in dirty aliased table names like `goods g` | **Rejected** (a graph gap, not a code problem) |
| A method reads/writes more than N tables ("god method") | at threshold 15 on shopxo, 215 methods hit, topped by `Index` / `Add` (one method touching 48 tables looks more like an over-join) | **Rejected** (noise-dominated) |
| Queue delivered but no consumer | hit names `app` / `rule` / `module` are products of dynamic queue names; `product_stock_job` can't be grepped in source | **Rejected** (unverifiable) |
| GET contract whose name contains `create` / `edit` | the hit is `GET /agent/level/create` -- in a ThinkPHP admin this **renders a form page**, so GET is legitimate | **Rejected** (naming heuristics inevitably misfire on admin frameworks) |
| Page with no entry navigation | 0 hits on all 7 projects | **Rejected** (silent failure) |
| External callback without signature verification | the criterion must cross procedures into SDK internals, and PHP excludes `vendor` while Java doesn't scan Maven deps -- the SDK's `verify()` isn't in the graph, so the criterion holds for every callback | **Rejected** (graph gap, 100% false positives). Reworked into judging only **verification quality**: `sign-compare-loose` / `sign-weak-hash` |
| i18n missing locale / high-criticality table / runtime-mutable config | all three can only match by `kind`, with no `subkind` filter, so hits equalled every node (358 / 156 / 248) | **Rejected** (predicates lack `subkind`; a hit means flooding) |

### How a rule knows "where to run": environment gate + criterion validation

The two most common rule failures **don't surface as errors** -- they surface as "0 violations"
(more dangerous than false positives):

1. **Environment mismatch** -- PHP-only event semantics (`Triggers` / `Emits`) simply don't exist in
   a Java project, so putting `orphan-event` on a pure Java project reports every event node as
   "nobody triggers it".
   → rules **declare their scope up front** via `applies_to.languages` / `applies_to.frameworks`
   (e.g. `languages: [php]`); a mismatch skips the rule and counts it into the report's
   `rules_not_applicable`.
2. **Criterion always true** -- inverse predicates are always true when "the evidence doesn't exist":
   `no_annotation: pii` holds for every table when the graph has no `pii` annotation at all.
   → before running, the engine **derives** dependencies from the predicates (edges / annotations /
   capabilities) and confirms those facts really exist in the graph; otherwise it disables the rule
   and counts it into `rules_unavailable`. No hand-written `requires` is needed, and the derived
   result always agrees with `when`.

The report therefore has four states (all show 0 hits, but with completely different natures):

| Field | Meaning |
| --- | --- |
| `rules_run` | actually executed, produced a normal conclusion |
| `rules_not_applicable` | skipped for environment mismatch (**expected behavior**, not a failure) |
| `rules_unavailable` | criterion doesn't hold; running it would always-true false-positive, so better not to run |
| `rules_silent` | ran but 0 hits; confirm whether "the code is genuinely clean" or "the rule is blind" |

### Runs automatically after a build, no manual step

After `create` builds a graph successfully, `PipelineService` **automatically** runs a check and
writes violations into the diagnostics table (prefix `rule:`), so users see conclusions on the
DiagnosticsPage right after building -- no manual `check` needed.

The automatic check deliberately **swallows errors**: a failure in the check engine only records a
`warn`; it must not let "conclusions couldn't be computed" decide that the "graph build" failed (the
graph is a far more valuable asset). Editing a rule YAML also **doesn't trigger a rebuild** --
rerunning a check takes about 1 second, rerunning parsing takes tens of seconds to minutes.

Two deliberate design conventions:

1. **No taint reachability analysis** -- the MVP reports only "confirmed facts" (a sink was
   recognized on the graph; no auth was recognized on a write endpoint), and the text is always
   "not identified / needs confirmation" rather than "a vulnerability exists". The cost and false
   positive rate of path-reachability computation are both too high; half-doing it is worse than not
   doing it.
2. **Some rules double as graph acceptance devices** -- e.g. when "ghost calls" produce a large
   batch, it's usually not that the code is wrong, but that **the frontend baseURL prefix didn't
   participate in identity normalization** (a currently known limitation). So such rules' text states
   both possibilities and the severity is lowered accordingly. Rules don't only catch code's
   mistakes; they also expose the graph's own gaps.

### Rerun semantics

Running the full set clears the whole `rule:` prefix; running only some rules **replaces just those**
(including rules disabled by criteria) -- rerunning A alone won't erase B/C's conclusions.

### Rule parameterization: a rule can have tunables

A rule can't be only "on / off" -- "how much fan-in counts as a hot table" has different answers for
codebases of different sizes. So a rule may declare `params:` and reference them as `$key` in `when` /
`applies_to`:

```yaml
- id: hot-table
  applies_to: { kinds: [Table], limit: "$max_nodes" }
  params:
    - key: min_fan_in
      label: Fan-in threshold
      kind: number
      default: 50
      min: 1
      max: 100000
  when:
    - fan_in_gte: "$min_fan_in"
  message: "Table {name} is a hot table (semantic in-edges ≥ {param:min_fan_in})"
```

Four conventions:

1. **The `$` prefix must be written explicitly.** Nothing is guessed from "whether it looks like a
   number" -- in a string-typed parameter `"50"` could be either a literal or a reference, and
   guessing wrong costs silently wrong conclusions. To use `$` itself as a literal, write `$$x` (an
   isolated `$` also counts as a literal), so `name_starts_with: "$"` matches **names starting with
   `$`** and won't be mistaken by the engine for an empty parameter reference.
2. **Undeclared references error at load time.** Referencing an undeclared parameter degrades to `0` /
   `""`; used on `limit` that means **the candidate set becomes empty** (the rule silently 0-hits),
   exactly the failure mode this project most wants to avoid -- so it must be a load error, not a
   runtime surprise.
3. `{param:key}` in the message renders with the **effective value**. Otherwise after a user raises
   the threshold to 200 the report would still say "≥ 50", reading as if the rule didn't take effect.
4. Built-in rules already got parameters for "threshold-style" criteria: the fan-in threshold of
   `hot-table` / `pii-table-hot`, the fan-in upper bound of `dead-table`, the name filters of the
   three contract rules, and the candidate cap of `raw-sql-sink`.

### Per-project override: same rule set, different criteria per project

`enabled` in YAML is the **global default**; project-level overrides live in the
`project_rule_config` table:

| Field | Semantics |
| --- | --- |
| `enabled` | `NULL` = inherit the global default; `0/1` = project-level override |
| `options` | stores only the parameter keys **that were overridden**; un-overridden ones take the `params` default |

Storing only the diff is crucial: restoring defaults = deleting that row, so the rule keeps evolving
with the YAML instead of freezing some project's old threshold in the DB.

| Interface | Purpose |
| --- | --- |
| `GET /api/projects/{id}/rules/config` | read all overrides for that project |
| `PUT /api/projects/{id}/rules/config` | write a single one (**patch semantics**: omitted fields keep their value, so "changing enabled state" won't wipe a tuned threshold) |
| `POST /api/projects/{id}/rules/config/batch` | batch write (enable / disable a whole category) |
| `DELETE /api/projects/{id}/rules/config/{rule_id}` | restore defaults |

Changing the criteria requires a rerun -- the violations in the DB are conclusions under the
"previous criteria". The UI merges "save" and "rerun" into one action so users don't finish editing
and find the results unchanged.

## Prompt augmentation: querying the graph by prompt

Full-text search answers "which file contains this string"; recall answers "which code does this
topic involve". The latter must rely on the graph.

```
After hitting a seed, it expands outward along Calls / HandledBy / WritesDb / ReadsDb … chain edges,
so results include code whose **name has no keyword but is genuinely related**:

  prompt "store_order 订单表"
    1. Table  store_order                    ← direct hit (hop 0)
    6. Method createOrder                    ← brought in by graph expansion (hop 1, it writes this table)
    7. Method userDaoSelect                  ← brought in by graph expansion (hop 1, it reads this table)
```

Every result states `direct` (direct hit) and `hop` (hops from the seed) -- the user must be able to
see why a result is here, otherwise recall is no different from full-text search.

Scoring = keyword match (exact > prefix > substring) × multi-word bonus × kind weight (semantic nodes
first) + fan-in bonus, with expansion decaying by `0.5^hop`.

Chinese is supported through **structural hint words**: "表" / "接口" / "事件" / "配置" / "队列" /
"定时任务" are recognized as kind boosts for `Table` / `HttpContract` / `Event` / …, and that
conclusion is echoed back to the user explicitly.

The recall encoder has two tiers (decided by compile features, switched automatically at runtime):
the **default** (`model-candle` and `model-ort` are both default features) tries to load local bge-m3
weights for cross-lingual semantic vectors, and when weights are missing it **automatically falls
back** to the local hash encoder (offline, zero dependencies, no LLM calls). That's why a pure-Chinese
prompt like "下单改优惠" can hit English nodes such as `placeOrder` / `applyDiscount`; when it falls
back to the hash encoder, pure-Chinese recall without an identifier is weaker, but still never goes
online. The weights directory is set by the environment variable `GT_BGE_MODEL` (default
`models/bge-m3-safetensors`); run `tools/convert_bge_safetensors.py` to convert HuggingFace's
`pytorch_model.bin` into safetensors to enable it. If you don't want to compile candle / ort, use
`cargo build -p gt-app --no-default-features` to go straight to the hash encoder.

> On the command line this capability is triggered by the `recall` subcommand
> (`graphtell recall --project 1 --query "…"`), which is the CLI-side entry of "prompt augmentation";
> the web / desktop capability of the same name lives on the "prompt augmentation" page.

The output `markdown` field is a context pack you can paste straight into an LLM (seeds + related
code + `path:line` + source snippets + graph relations).

## Measured on the CRMEB sample

Sample: `samples/php-projects/thinkphp/CRMEB` (**v6.0.0**, 3 sub-projects, 2189 source files).
A full build takes about **25 seconds** (time dominated by CfAst, ~18s):

| Phase | Nodes | Edges | Annotations | Time |
| --- | --- | --- | --- | --- |
| Ingest | 0 | 0 | 0 | 146ms |
| CfAst | 93 608 | 91 351 | 0 | 18.1s |
| Prepare | 0 | 0 | 0 | 427ms |
| AnnotatePre | 20 | 19 | 2 287 | 335ms |
| Synthesize | 2 539 | 2 697 | 0 | 2.9s |
| GuardCapability | 0 | 0 | 1 204 | 60ms |
| AnnotatePost | 74 | 106 | 2 592 | 1.18s |
| Resolve | 0 | 21 029 | 1 606 | 1.88s |
| Propagate | 0 | 5 730 | 0 | 102ms |
| Taint | 0 | 0 | 135 | 32ms |
| Cors | 0 | 0 | 3 | 4ms |
| Sign | 0 | 0 | 3 | 5ms |
| External | 0 | 0 | 2 | 4ms |
| Tx | 0 | 0 | 2 | 29ms |
| Guard | 0 | 4 436 | 0 | 47ms |

The built graph has **96 241 nodes / 125 368 edges** in total; main node output:
`Class` 1043, `Method` 6724, `CallSite` 80 263, **`HttpContract` 1603**, **`Table` 156**,
`Function` 1907, `ConfigKey` 278, `Cache` 42, `Queue` 27, `Event` 20, **`Schedule` 17**.

Of these, `Schedule` comes from CRMEB's **project-level** FKB synthesizing `crontab/...` routes.
Annotations cover channels such as `pii.phone` (including `store_order`
identified via the `user_phone` variant column name), `data.criticality`, `config.storage:Database`
and `entrypoint.login`.

> **About samples and release packages**: large third-party projects like CRMEB / Bagisto are **not
> distributed with the repo** (licensing + size); set `GRAPHTELL_SAMPLE_DIR` and supply them yourself
> to reproduce the numbers above (they correspond to **v6.0.0**; other versions differ). The light
> samples shipped in the repo (see `samples/`) are always available and have been auto-generated into
> **a gallery page that renders directly on GitHub** -- see below.
>
> **Sample licensing and distribution**: the repo **distributes only the self-made synthetic
> fixture** `samples/frontend-backend-link` (`.gitignore` excludes the rest via `**/samples/*`, with
> an exception for that fixture); third-party samples exist only locally by default and aren't
> distributed with the repo -- see [`docs/samples-licenses.md`](samples-licenses.md) for sources and
> licenses.

## Example demo (GitHub gallery)

[`tools/gen_demo.sh`](../tools/gen_demo.sh) runs "build graph → rule check → recall example → graph
export" for each sample and generates two directly publishable static artifacts, placed in
**[`docs/demo/`](demo/README.md)**:

- **Interactive site** [`docs/demo/index.html`](demo/index.html): multi-project switching + three
  tabs -- **graph** (a zoomable, draggable node-edge graph colored by kind, click for details),
  **rule check** (violation table, filterable by severity / rule), **prompt augmentation** (recall
  context pack for Chinese queries). Large projects render only one **connected subgraph** and state
  the full scale honestly.
- **Markdown gallery** `docs/demo/README.md`: renders natively on GitHub, good for browsing inside
  the repo.

No model weights needed (recall falls back to hashing). Regenerate locally:
`./tools/gen_demo.sh --build` (the sample tree defaults to the repo-root `samples/`, overridable via
`GRAPHTELL_SAMPLES_DIR`).

Publishing: repo **Settings → Pages → Source "GitHub Actions"** (one-time); afterwards pushing
`master` / `main` auto-publishes via
[`.github/workflows/deploy-demo.yml`](../../.github/workflows/deploy-demo.yml) to
`https://<username>.github.io/<repo>/` -- **no domain purchase needed**. (Gitee doesn't run GitHub
Actions, so it must be deployed manually through its Gitee Pages service.)

> Samples that integration tests depend on are likewise a "soft dependency": when `samples/` is absent
> (e.g. a release package / partial checkout), the related tests skip instead of failing -- consistent
> with the demo script's behavior of skipping missing samples.

---

## MVP status and known limitations

This repo is released in **MVP** form; please be aware of these differences from the full vision:

- **Naming unified as "prompt augmentation"**: the web / desktop menu, this README, MCP tool
  descriptions and the self-contained composition page all call it "prompt augmentation"; internally
  it consists of "code recall (retrieval)" + "prompt composition", and the CLI subcommand is still
  `recall` (`graphtell recall --project 1 --query "…"`).
- **Some entries are hidden in the MVP, but the functionality remains**:
  - **Node browse (Explorer)**: overlaps heavily with "prompt augmentation" (semantic retrieval), and
    the list has a hard `limit: 200` cap with no sorting, so the MVP leaves it out of the sidebar
    menu; visiting `/projects/:id/explorer` directly still works.
  - **Settings page**: it only served "jump to IDE" (local root template / WSL / default IDE), and
    the "jump to IDE" entry is disabled, so the settings page was removed along with it.
- **Encoder default compile features**: `model-candle` and `model-ort` are **both default features**.
  When weights (`GT_BGE_MODEL`, default `models/bge-m3-safetensors`) are missing it automatically
  falls back to the local hash encoder (offline, zero dependencies, no LLM calls). If you don't want
  to compile candle / ort, use `cargo build -p gt-app --no-default-features`.
- **Perspectives (the level-1 options of the two-level filter) are not implemented in the MVP**: the
  `page` / `domain` / `deploy_unit` / `platform` perspectives in `views/perspectives.yaml` are
  commented out and don't take effect; the filter currently works only by node kind and name.

---

## Directory layout

```
crates/
├── gt-domain            domain core (entities + ports)
├── gt-application       use-case orchestration
├── gt-pipeline          P0/P2/P3/P4/P5/P6/P7
├── gt-adapter-fs        file scanning (exclusion rules)
├── gt-adapter-parser    tree-sitter (currently: PHP / Java / JavaScript·TypeScript / Python)
├── gt-adapter-fkb       FKB YAML loading
├── gt-adapter-sqlite    SQLite persistence
├── gt-adapter-http      axum REST API
├── gt-adapter-rules     rule YAML loading (CheckRule)
└── gt-app               composition root + CLI
src-tauri/               Tauri desktop app (separate workspace)
ui/                      React + TS + antd (layered: pages → widgets → … → shared)
fkb/                     preset framework knowledge (framework-level under php/, java/…; project-level under projects/)
rules/                   check rules (compliance check)
views/                   perspective declarations (level-1 options of the two-level filter)
docs/
├── demo/                example demo: interactive site + Markdown gallery (publishable to GitHub Pages)
└── samples-licenses.md  licenses and sources of third-party samples
tools/                   analysis and demo scripts (gen_demo.sh etc.)
scripts/                 release scripts (package-release.sh)
```

> `samples/` (sample codebases) sits at the **repo root** and is **not distributed with the repo**
> (`.gitignore` excludes it via `**/samples/*`, with an exception only for the self-made fixture
> `samples/frontend-backend-link`) -- see "Sample licensing and distribution".

## Deployment (Docker / release package)

The backend `graphtell serve` **serves both the REST API and the built React SPA** on a single port
(default 5177) (`--ui-dir` points at `ui/dist`; same origin, no CORS, no reverse proxy needed).

### Docker (recommended)

```bash
# model-candle / model-ort on by default: recall uses real bge-m3 semantic vectors (auto-falls back to lexical when weights are missing)
docker build -t graphtell:latest .
docker run -d -p 5177:5177 -v $(pwd)/data:/data graphtell:latest
# or one command: docker compose up -d --build
```

Open `http://localhost:5177/` in a browser. For a smaller "lexical-only" image:
`docker build --build-arg GT_FEATURES="--no-default-features" -t graphtell:hash .`

### Release package (no Docker)

`scripts/package-release.sh` builds the frontend + backend release binary and packs them into
`release/graphtell-<version>.tar.gz` (containing the `graphtell` binary, `ui/`, `fkb/`, `rules/`,
`views/` and startup notes):

```bash
./scripts/package-release.sh
# after extracting:
./graphtell --data-dir ./data serve --bind 0.0.0.0 --port 5177 --ui-dir ./ui
```

> The frontend is compiled with `VITE_API_BASE=same-origin` at build time, so the SPA uses relative
> paths and can be deployed to any hostname without rebuilding the image.

## Extending to a new language / framework

* **New language**: implement `gt_domain::port::LanguageParser` (translate the syntax tree into
  `SyntaxFacts`) and register it in `DefaultParserRegistry`; add extensions in
  `scanner::language_of_extension`.
* **New framework**: add a YAML under `fkb/` (detectors / root_rules / loaders / rules / resolvers).
* **New node kind**: just write the new `node:` name in the YAML -- no Rust change.
* **New edge kind**: declare `semantic_edge_kinds` / `bridge_edge_kinds` in the YAML (e.g. Python's
  `DependsOn`) -- no `kinds.rs` change.
* **Capability gaps are never silent**: when files of a language are scanned in but no parser is
  registered, P2 produces a `NoParserForLanguage` diagnostic and skips -- you never get the silent
  failure of "recognized as a sub-project, but the graph is empty".
