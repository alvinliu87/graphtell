# Support matrix (SUPPORTED)

The languages / frameworks / semantic features GraphTell currently supports, plus the **known honest
boundaries**.

For the full architecture and pipeline see [`docs/architecture.md`](./docs/architecture.md); for FKB
authoring see [`docs/fkb-authoring.md`](./docs/fkb-authoring.md).

---

## 1. Languages and frameworks

| Language | Framework / form | Semantic extraction | Compliance rules |
| --- | --- | --- | --- |
| **PHP** | ThinkPHP (5.1 / 6.x / 8.x) / CRMEB / Laravel / Uni-app backend contracts / **Symfony (PHP 8 attribute routes, §3.3)** | complete (tables / **ORM relations** / **table foreign keys** / routes / config / i18n / cache / events / queues / signature verification …, §3.2) | complete (including N+1, signature verification, external call in loop, multi-write without tx) |
| **Java** | Spring Boot (Spring Cache / ApplicationEvent / Spring AMQP / Spring Kafka / Spring Scheduling / JPA / MyBatis-Plus) | complete (§2) | reuses `rules/global/` (topology-based rules); framework-specific rules (`orphan-event` / `orphan-queue` …) not yet written for Java |
| **JavaScript / TypeScript** | Uni-app frontend (event bus / local storage / pages / Store) | complete | `rules/js/` (frontend event bus dead code) |
| **JavaScript / TypeScript** | **Node backend**: NestJS (decorator routes / controllers) / Express (member-style routes) / **Koa** / **Fastify** | route contracts (NestJS additionally links `HandledBy` to method nodes, §3.1) | reuses `rules/global/` (topology-based); framework-specific rules not yet written for Node |
| **Python** | FastAPI / Flask / Celery / SQLAlchemy / **Django (ORM: tables / columns / relations / table foreign keys, §3.4)** | routes / dependency injection / config / cache / table mapping / task queues / scheduling / Django model tables (§3) | reuses `rules/global/` (topology-based); framework-specific rules not yet written for Python |

> A new language = implement `gt_domain::port::LanguageParser` to translate the syntax tree into
> `SyntaxFacts` (reference: `gt-adapter-parser/src/java`) and register it in `DefaultParserRegistry`;
> zero core changes.

---

## 2. Java (Spring Boot) semantic features

All semantic nodes come from `fkb/java/spring-boot.yaml` (declarative FKB, **no core changes**).
Annotations are modeled as call sites in the core (`callee` = annotation name); method call arguments
capture literals positionally.

| Semantics | Trigger | Node | Edge | Notes |
| --- | --- | --- | --- | --- |
| **Cache** | `@Cacheable` / `@CachePut` / `@CacheEvict` | `Cache` | `ReadsCache` / `WritesCache` | cache name from annotation arg `arg0` |
| **Event** | `@EventListener(handler)` / `publisher.publishEvent(new X())` | `Event` | `ListensTo` / `Emits` | **type-level merging**: publishers and subscribers of the same event type merge into one node (§5) |
| **Queue** | `@RabbitListener(queues="q")` / `rabbitTemplate.convertAndSend("q", …)` | `Queue` | `ListensTo` / `PublishesTo` | consumer side from the annotation, producer side from the method call argument |
| **Topic** | `@KafkaListener(topics="t")` / `kafkaTemplate.send("t", …)` | `Topic` | `ListensTo` / `PublishesTo` | producer matched by receiver variable-name convention (§5) |
| **Schedule** | `@Scheduled` | `Schedule` | `Triggers` | the scheduler triggers this method (out-edge) |
| **HttpContract** | `@RequestMapping` / `@GetMapping` / `@PostMapping` … | `HttpContract` | `HandledBy` | contract `METHOD /path` normalized, bridgeable with frontend `uni.request` contracts |
| **Table** | `@TableName("x")` / `@Table(name="x")` / MyBatis `mapper/*.xml` | `Table` | `MapsTo` / `ReadsDb` / `WritesDb` | MyBatis-Plus and JPA table names; XML statements land edges by read / write |
| **ConfigKey** | `@Value("${app.name}")` | `ConfigKey` | `ReadsConfig` | strips `${` `}` to get the config key |

End-to-end self-check: `crates/gt-pipeline/tests/java_spring_features.rs` (synthetic sample, no
external project needed).

---

## 3. Python (FastAPI / Flask / Celery / SQLAlchemy) semantic features

Python is the **third language landed**: only one `LanguageParser` was implemented and registered;
all semantics are declared by `fkb/python/*.yaml`, with **zero core changes**. The key modeling is
**decorator ≡ call site** (same mechanism as Java annotations), so `@app.get("/x")` is hit directly
by a `kind: call` selector -- which is why a framework like Flask (the Nth framework of the same
language) costs almost nothing (just one more YAML).

| Semantics | Trigger | Node | Edge | Notes |
| --- | --- | --- | --- | --- |
| **HttpContract** (FastAPI / Flask 2.x) | `@app.get/post/put/delete/patch("/x")` | `HttpContract` | `HandledBy` | method derived from the decorator name |
| **HttpContract** (Flask) | `@app.route("/x")` / `@bp.route` | `HttpContract` | `HandledBy` | decodes the first item when `methods=["POST"]` is written; otherwise lands `GET` per Flask's default semantics |
| **Dependency injection** | `def h(db=Depends(get_db))` | no node created (both ends are existing function nodes) | `DependsOn` | the dependency is written in a **parameter default**; `DependsOn` is declared by this FKB via `bridge_edge_kinds` |
| **ConfigKey** | `os.environ.get("K")` / `os.getenv("K")` | `ConfigKey` | `ReadsConfig` | |
| **Cache** | `cache/redis::{get,set}` | `Cache` | `ReadsCache` / `WritesCache` | |
| **Table** (SQLAlchemy) | class attribute `__tablename__ = "users"` | `Table` | `MapsTo` | uses a P6 **graph-node selector** + `HasProperty` (not a call site) |
| **Queue** (Celery) | `@celery_app.task` / `@shared_task` with `.delay()` / `.apply_async()` | `Queue` | `ListensTo` / `PublishesTo` | producer and consumer **merge into one node** by task name |
| **Schedule** (Celery beat) | `add_periodic_task(30.0, …)` | `Schedule` | `Triggers` | |

The parser side also added several **generic** capabilities (not Python-specific): keyword arguments
can be taken by name, list / tuple literals can be accessed keyed by index, calls inside parameter
defaults are recorded as call sites, and literal assignments in a class body register as `Property`.

End-to-end self-checks (all synthetic samples, no external projects):
`crates/gt-pipeline/tests/python_fastapi_features.rs`,
`crates/gt-pipeline/tests/python_flask_features.rs`.

### 3.4 Django (Python) semantic features

Django is the largest framework in the Python ecosystem, and this file adds its **ORM modeling**
(tables / columns / relations / table-level foreign keys) using **the same edges** as Node (TypeORM)
and PHP (Laravel / ThinkPHP) (`References` / `ForeignKey` / `HasColumn` / `MapsTo`), with `ForeignKey`
directly reusing the generic `Project` action. It is almost YAML-only -- the Python parser gained just
one neutral capability: capturing "a `name = Call(...)` field declaration inside a class body" (same
mechanism as JS field decorators and Java field annotations).

| Semantics | Trigger | Node | Edge | Notes |
| --- | --- | --- | --- | --- |
| **Table** | `class Post(models.Model)` | `Table` | `MapsTo` | table name from the class short name (`snake_plural` + `short_name` + `singularize`: `Post` → `post`); Django's real table name is `app_label_modelname`, and `app_label` isn't available here, so the pluralized class short name is used as a fallback |
| **Column** | `name = models.CharField(max_length=200)` | `Column` | `HasColumn` (model class → column) | identity is `model-class.field` (`app.models.Post.title`); the model class already `MapsTo` → `table`, so "table → column" is two hops: table ←MapsTo— model —HasColumn→ column; only column field types are collected, while `ForeignKey` / `OneToOneField` / `ManyToManyField` count as relations |
| **Model relation** | `author = models.ForeignKey(User, on_delete=...)` | — (both ends are existing model-class nodes) | `References` (declaring side → target model) | target is the class name of the first positional argument (the parser resolves it to an FQN via import or module prefix and stores it in `entity`); only the foreign-key holder is built |
| **Table foreign key** | projected from the `References` above | — (both ends are existing table nodes) | `ForeignKey` (table → table) | P6 uses `Project`: walk each `References`, and each end walks one hop along `MapsTo` |
| **HTTP route** | `path("users/", user_list)` (`urls.py`) | `HttpContract` | `HandledBy` (contract → view) | see "Routes" below |

**Routes (`path()` / `re_path()` / `url()`)**: Django writes URLs and views in `urls.py` and **doesn't
encode the HTTP method** (one view handles all methods), so the contract method uses the wildcard `ANY`
(`is_wildcard_http_method` treats `ANY` as matching any frontend call method) -- avoiding both
mislabeling Django endpoints with a concrete verb and the read/write heuristic's misjudgment of
"unknown method → read". The path comes from the route literal (normalized to a leading `/`); the view
(`arg1`) is resolved by the parser into a full FQN stored in `entity`, then linked to the view node via
`HandledBy` (functions aren't in the short-name index, so a full FQN is required for `find_by_name` to
hit -- the same mechanism as FastAPI using `owner_class` FQN).

**Prerequisites (parser side, language-neutral extensions)**:
`gt-adapter-parser/src/python/mod.rs`
1. Turn `name = SomeCallable(...)` inside a class body into a call site, with the owner precise to
   "class.field" (`owner_class` = model class, `owner_member` = field name), so FKB can take
   `owner_class.owner_member` as the column identity; the relation field's first positional argument
   (a class-name identifier) is resolved to an FQN stored in `entity` for `References` to link directly
   to the target model class. This extension **doesn't know Django** -- it's just the neutral syntax
   shape "bare identifier = call inside a class body", so other "field-style ORMs" beyond SQLAlchemy
   benefit directly.
2. Resolve the **view argument** of `path()` / `re_path()` / `url()` calls into a full FQN stored in
   `entity` (same reason: function nodes aren't in the short-name index, so a full FQN is required to
   link).
3. **Relative imports**: `from .views import user_list` is restored to `myapp.views.user_list`
   against the current module's parent package (leading dot count aligned to parent-package depth);
   absolute imports are unaffected.

**Honest boundaries**:
- **The Django idiom is `from django.db import models` then `models.CharField`**: if a project instead
  does `from django.db.models import CharField` and writes `CharField(...)` directly, the current
  callee won't match (bare field names are too generic to loosen without hurting ordinary variables).
- **String-referenced relations aren't resolved**: in `tags = models.ManyToManyField("Tag")` the
  argument is a string and `Tag` may not be in the same compilation unit; the parser doesn't resolve
  class names inside strings, so it skips per `require_class` and builds no dangling edge (same
  handling as PHP's `morphTo()`). Supporting string foreign keys needs extra parsing.
- **`class Meta: db_table` isn't read**: the explicitly specified real table name isn't taken;
  the table name falls back to the pluralized class short name. If a project uses `db_table` heavily,
  a capability to read `Meta` attributes must be added.
- **All three route-view forms are supported**: ① bare name -- `from .views import user_list` then
  `path("x/", user_list)`; ② module attribute -- `from . import views` then
  `path("x/", views.user_list)` (`resolve_symbol` now recursively restores the module prefix →
  `myapp.views.user_list`); ③ CBV -- `path("x/", ArticleListView.as_view())` (only the `as_view()`
  Django idiom is recognized, taking the called object's class name). All three link to the view node
  via `HandledBy`.
- **`path("api/", include("api.urls"))` style "prefix + include"**: this is treated as one endpoint
  contract (with no handler edge) -- known noise, since the real endpoints mounted by `include` live in
  its child `urls.py`.

End-to-end self-check (synthetic project, no external sample):
`crates/gt-pipeline/tests/django_features.rs` (covering table / column / relation / foreign key /
route).

---

## 3.1 Node backend (NestJS / Express) semantic features

The Node backend needs **no new parser**: `JsFrontendParser` already models TS **class / method**
decorators as call sites (proven by the `nestjs_decorators_become_call_sites` unit test in
`crates/gt-adapter-parser/src/js/mod.rs`; `callee` carries an `@` prefix to distinguish it from an
ordinary `obj.get()` member call) -- the same mechanism as FastAPI's `@app.get`. Express's
`app.get('/x')` is an ordinary member call (receiver `app`, method `get`), the same shape as FastAPI's
verb shortcuts. So routes / dependency injection / entity mapping are all declared purely by
`fkb/js/*.yaml`.

**Table-level foreign keys** additionally need one **generic** capability: the new `Project` action --
projecting a class of edges onto another layer (walk each `along` out-edge of the matching node, have
each end walk its own edge-kind chain to a landing point, and build an edge between the two landing
points). "Class → the table it maps to" is essentially **one hop along `MapsTo`**, while `ValueSource`
only knows names and can't reach "the far end of an edge"; and one entity may have several
`@ManyToOne`, so projection must be one-to-many (`Link` takes only one name per end and would silently
drop edges). Which edges to walk is still declared by FKB; the core still knows nothing about TypeORM.

**Field-level semantics** (`@Column`) need two more **generic** capabilities (not Node-specific; other
languages share them): ① the parser attaches **field decorators** to the field FQN (`Class.field`) --
attaching only class / method decorators would drop `@Column` entirely; ② at P2,
when a node can't be found precisely by the call site's `owner_fqn`, it **falls back to the owning
class** (falling all the way back to the file node would make `@Column`'s `HasCallSite` / semantic
edges originate from `File`).

| Framework | Trigger | Node | Edge | Notes |
| --- | --- | --- | --- | --- |
| **NestJS** route | `@Get('user')` / `@Post('users')` … | `HttpContract` | `HandledBy` (pointing **out** of the contract, to the `Class.method` node) | method from the decorator name (minus `@`); path from the argument |
| **NestJS** controller | `@Controller()` | `Controller` | — | purely structural display node |
| **NestJS** dependency injection | `constructor(private svc: UserService)` | — (the provider is a class node) | `DependsOn` (`UserController --DependsOn--> UserService`) | constructor parameters **with an access modifier** = provider; the parser records it as an `@Inject` call site and FKB builds the edge |
| **TypeORM** entity | `@Entity('user')` | `Table` | `MapsTo` (entity class → table) | table name from the argument; argument-less `@Entity()` and object-form `@Entity({name})` are skipped |
| **TypeORM** column | `@Column()` / `@PrimaryGeneratedColumn()` / `@CreateDateColumn()` … | `Column` | `HasColumn` (entity class → column) | column identity is `entity-class.field` (`UserEntity.username`), so same-named columns (`id`) count separately per entity; `Column` / `HasColumn` are declared by this FKB via `semantic_kinds` / `semantic_edge_kinds` |
| **TypeORM** relation | `@ManyToOne` / `@ManyToMany` / `@OneToOne` | — (both ends are existing entity-class nodes) | `References` (referencing side → referenced entity) | target entity taken from the **field type annotation** (an arrow-function argument `type => X` yields no literal); only the **foreign-key holder** is built, since `@OneToMany` is the reverse declaration and would duplicate |
| **TypeORM** table foreign key | projected from the `References` above | — (both ends are existing table nodes) | `ForeignKey` (table → table) | P6 uses the `Project` action: walk each `References`, both ends walk one hop along `MapsTo` to land on a table. **One-to-many** -- an entity with N `@ManyToOne` produces N edges |
| **Express** route | `app.get('/login')` / `router.post(...)` | `HttpContract` | — | method from the member name (get/post…); path from arg0 |
| **Koa** route | `router.get('/users')` / `admin.post(...)` | `HttpContract` | — | same as above; the receiver must be a Router instance (**not `app`** -- Koa's app is only for `use()`). `router.all(...)` normalizes to `ANY` |
| **Fastify** route | `fastify.get('/users')` / `server.put(...)` | `HttpContract` | — | same as above; receivers `fastify` / `app` / `server`. The `fastify.route({method,url})` schema form isn't supported yet |

Koa / Fastify need **zero parser changes** (same shape as Express: `receiver.method(path, handler)` is
an ordinary member call), each just adding one FKB. The three have different receivers, so they're
written separately: Express uses `app` / `router` / `api`, Koa uses `router` (not `app`), Fastify uses
`fastify` / `app` / `server`.
End-to-end self-check: `crates/gt-pipeline/tests/node_koa_fastify_features.rs`.

> The **HandledBy direction** differs between the two route styles, which is easy to trip over: NestJS
> uses `link.direction: to_target` (the edge points **out** of `HttpContract` to the method node), so
> what's queried is "the contract's **out-edge**", not "the method's out-edge".

**Verification samples**: besides the synthetic self-checks, real open-source projects were downloaded
for end-to-end verification (git-ignored, not committed):
`lujakob/nestjs-realworld-example-app` (NestJS + TypeORM), `sahat/hackathon-starter` (Express).
`crates/gt-pipeline/tests/node_real_samples.rs` runs real build assertions when the samples exist and
skips when they're missing.

**Honest boundaries**:
- **NestJS controller prefixes aren't concatenated**: the `@Controller('user')` prefix isn't merged
  with the `@Get('user')` method path; only the method decorator's argument path is taken (cross-decorator
  merging like `n1-query-in-loop` would need extra FKB capability).
- **Express doesn't link `HandledBy`**: Express routes and handlers are **decoupled** (the handler
  lives in another file); a module-level route's owner is the file path with no matching node, so to
  avoid dangling edges Express only lands contract nodes.
- **Dependency injection only recognizes "constructor params with a modifier"**:
  `constructor(private svc: UserService)` counts, `constructor(plain: Foo)` doesn't; when the provider
  type can't be resolved to a class node (generics / interfaces / not imported) it's silently skipped.
- **TypeORM only recognizes string-argument `@Entity('user')`**: object form `@Entity({ name: 'user' })`
  and argument-less `@Entity()` don't yield a table name yet (no literal → skip; better missing than
  guessed) -- such entities' `Column`s still attach to the entity class, but there is **no**
  corresponding `Table` (in the real sample, `Comment` is exactly this shape: 2 column nodes, no table
  node).
- **Only "column" decorators are collected**: `@Column` / `@PrimaryGeneratedColumn` /
  `@CreateDateColumn` / `@UpdateDateColumn` / `@DeleteDateColumn` count as columns; relation decorators
  (`@ManyToOne` …) aren't columns and are modeled separately via `References` edges. The column name is
  taken from the field name (TypeORM default); explicit renaming via `@Column({ name: 'x' })` isn't
  extracted yet.
- **Only the foreign-key holder gets a relation**: `@ManyToOne` / `@ManyToMany` / `@OneToOne` build
  `References`; `@OneToMany` is the reverse declaration describing the same relation as its paired
  `@ManyToOne`, so building it would be a redundant reverse edge -- hence skipped, meaning a
  **one-way `@OneToMany` (with no paired ManyToOne) builds no edge**. `@JoinColumn` / `@JoinTable` only
  mark JOIN shape and build no edge of their own.
- **Table foreign keys only cover entities that have a table**: foreign keys are projected from
  `References`, so both ends must reach a `Table` along `MapsTo`. An entity with argument-less
  `@Entity()` has no table node, so its foreign keys don't enter the graph (in the real sample
  `Comment → article` is missing exactly for this reason; `article → user` and `user → article` are
  produced normally).
- **Route receivers are narrowed by variable-name convention** (as with Python): `app` / `router` /
  `api` etc.; bare `get/post` is too generic.

---

## 3.2 PHP (ThinkPHP / Laravel) ORM semantic features

**Table mapping** on the PHP side has existed for a while (more mature than the Node side: it also has
`db_verbs` backing the `ReadsDb` / `WritesDb` read / write classification). This round adds **model
relations** and **table-level foreign keys** -- using the same edges as the Node side (`References` /
`ForeignKey`), with `ForeignKey` directly reusing the `Project` action introduced in §3.1.

| Semantics | Trigger | Node | Edge | Notes |
| --- | --- | --- | --- | --- |
| **Table** (existing) | `Db::name('x')` (TP6) / `DB::table('x')` (Laravel) / model convention `extends Model` | `Table` | `MapsTo` (+ P7's `ReadsDb` / `WritesDb`) | written per framework (table-name sources differ) |
| **Model relation** | `$this->hasMany(Post::class)` / `belongsTo` / `belongsToMany` / `morphXxx` … | — (both ends are existing model-class nodes) | `References` (declaring side → target model) | target from arg0's class constant; **built on both sides**, see below |
| **Table foreign key** | projected from the `References` above | — (both ends are existing table nodes) | `ForeignKey` (table → table) | P6 uses `Project`: walk each `References`, both ends walk one hop along `MapsTo` |
| **Column** | ① `CREATE TABLE` in SQL install scripts; ② Laravel migration `database/migrations/*.php` | `Column` | `HasColumn` (table → column) | both loaders write the authoritative `schema` symbol table (**union**, loader order irrelevant), and the core materializes it into graph nodes at the end of P6 |

**Why both sides here but only the holder on the Node side**: TypeORM's `@ManyToOne` / `@OneToMany` are
usually written **in pairs**, so building only the holder avoids reverse redundancy; Laravel / ThinkPHP
**often declare only one side** (most commonly `User hasMany Post`, with no reverse `belongsTo`), so
building one side would miss edges broadly -- better redundant than missing.

Following the precedent of `db_verbs`, the rules are **written per framework**
(`fkb/php/laravel.yaml` and `fkb/php/thinkphp.yaml`) rather than factored into `php-common.yaml` --
relation methods are a **strong ORM assumption**, while `php-common` declares
`apply_without_detection: true` and **applies unconditionally to all PHP projects**.

### Where columns come from: precipitate the authoritative schema, don't chew on migrations

PHP ORM models **usually don't declare fields** (fields live in migrations / table structure), so
there's no field-level declaration source like TypeORM's `@Column`. More critically: **Laravel
migration's `$table->string('email')` is written inside a closure**, so the call site's owner is the
closure rather than the model class, and pure FKB can't tell which table it belongs to -- that path
doesn't work.

Hence **precipitate the authoritative schema**: columns from the `schema` symbol table (parsed from
`CREATE TABLE` in SQL install scripts at P3, plus table-name call sites found in source) are
materialized by the core into `Table --HasColumn--> Column` at the end of P6. This is a **language /
framework-agnostic** capability -- it only recognizes "a `Table` node + the schema having its columns",
so any project with SQL install scripts automatically gets field-level graph nodes. Column identity
carries table-name scope (`user.email`), so same-named columns (`id` / `created`) count separately per
table.

**Laravel's columns come from the migration loader** (`php_migration_schema`, scanning `Schema::create`
/ `Schema::table` in `database/migrations/*.php`). Both loaders write the same `schema` table and take
a **union** internally -- otherwise the later one would overwrite the earlier one wholesale (loader
order isn't guaranteed).

Parsing only recognizes a **whitelist of column-declaration methods** (`$table->string('email')`); it
must not just "take the first string argument": modifiers and non-column declarations such as
`->comment('说明')` / `->after('col')` / `->dropColumn('x')` also carry string arguments and would be
mistaken for column names. Declarations without arguments get a column name per Laravel convention:
`id()` → `id`, `timestamps()` → `created_at` / `updated_at`, `softDeletes()` → `deleted_at`.

### Columns deliberately **don't enter the collapsed view**

Columns are on the order of "table count × column count" (dozens of tables × a dozen columns = several
hundred nodes). If `Column` counted as a semantic node, the collapsed view would be blown out, and it
would eat the `MAX_NODES = 400` budget for reachable semantic nodes, squeezing out the real tables /
contracts. Hence:

* `Column` is **not** in `NodeKind::SYNTHESIZED` -- the collapsed view (filtered by `is_semantic()`)
  doesn't draw columns; nodes are still built and `HasColumn` edges still linked, so blast radius can
  still drill to field level.
* **Opening a table still shows its columns**: `NodeView.columns` brings columns out as **node
  attributes** (bare column names), so "expand a table to see its fields" is unaffected -- columns just
  don't take a slot in the collapsed view. Two column paths on the view side: PHP is
  `Table --HasColumn--> Column`; TypeORM is
  `Table <--MapsTo-- entity class --HasColumn--> Column` (`@Column` attaches to the entity class, so
  it's one hop around).
* `HasColumn` is classified as a **bridge edge** rather than a semantic edge, so it is **not counted**
  in "semantic in-edges / out-edges N" (it's a composition relation, not a resource dependency;
  counting it as semantic would make a 15-column table's out-edge count jump by 15 and distort the
  metric), but it stays in `is_chain_edge` so it remains **traversable**.
* `ForeignKey` (table → table) has semantic nodes at both ends and is a genuine resource dependency →
  it is a semantic edge, counts toward fan, and is drawn.

One mechanism detail you must know: **FKB's `semantic_kinds` / `semantic_edge_kinds` are registered
globally** (`register_semantic_kinds` registers unconditionally for every FKB in `load_dir`; language
filtering only happens when selecting rules). So "make columns visible for only one framework" is
impossible -- any FKB declaring `Column` as semantic makes columns semantic in **all projects**
(including PHP). Distinguishing by language requires first changing the loading logic to register per
language.

**Honest boundaries**:
- **Migrations only recognize Laravel's `database/migrations/*.php`**: ThinkPHP's column source is
  still the SQL install script (it has no equivalent migration directory convention). Forms outside
  `$table->xxx()` (e.g. `taggable_id` / `taggable_type` produced by `$table->morphs('taggable')`) are
  added to the whitelist; uncovered dialects are skipped (better missing than guessed).
- **Table names are singularized**: table node names go through `singularize` (`user`), while DDL /
  migrations often write the plural (`users`); precipitation retries the plural form as a fallback, but
  the `ColumnsMatch` predicate does **not** have that fallback, so PII tagging can still miss plural
  table names.
- **`morphTo()` builds no edge**: it has no class-constant argument (the polymorphic target is only
  determined at runtime), so with no target it's skipped and no dangling edge is built.

End-to-end self-check (synthetic project, no external sample):
`crates/gt-pipeline/tests/php_orm_features.rs`.

### Middleware: only route-level mounting, not global

| Semantics | Trigger | Node | Edge | Notes |
| --- | --- | --- | --- | --- |
| **Middleware** | `Route::group(fn){...}->middleware(X::class[, true])` / a single route carrying `->middleware(...)` | `Middleware` (**promoted** from `Class`, not newly created) | `PassesThrough` | P3 extracts mounting → P5.5 capability annotation → P14 builds the edge + promotes; the route perspective's "conclusions" panel has an extra "passes through middleware" row |

**Promotion rather than synthesis**: a middleware's identity is the class's FQN, and P2 already created
a node for that `Class`; synthesizing another would mean two entities for the same code (split fan-in,
two sets of jump locations). So P14 uses `GraphDelta::kind_patches` to **change only the kind, keeping
a single node**.

**Why the edge is named "passes through" rather than "guarded by"**: middleware includes guards that
reject requests (`AuthToken` / `Blocker` / `throttle`) as well as bypasses that only add response
headers or log (`AllowOrigin` / `AdminLog`). Calling them all "guards" would **over-claim** for the
latter -- the same discipline as not writing `MapsTo` as `ReadsDb` (static ownership ≠ action; passing
through ≠ guarding). "Does this endpoint need auth" is answered by the `Capability: Authentication`
annotation, not by the edge name.

**Why only route / route-group-level mounting is collected**: global middleware
(`app/middleware.php`, `Kernel::$middleware`) holds for every endpoint -- it's an **environment
constant, not information**; drawing it would just repeat the same tautology on every graph (exactly
the same lesson as `write-endpoint-without-auth` being disabled: treating a global fact as an
endpoint-level fact judged 1529 of 1603 contracts as public).

Measured on CRMEB (pipeline rerun): `route_list` keys and contract names matched **1265 / 1265** →
**4436** `PassesThrough` edges, **10** classes promoted to `Middleware`, and `auth.public` dropped from
1529 to 894 (endpoints wrongly judged "public" got corrected).
Combination distribution: 12 kinds, the largest at **63%** -- the accurate statement is that
discriminative power lies **not between individual endpoints but between apps** (admin / user / support
/ public); within one app it really is the same middleware set.

**Laravel side** (`fkb/php/laravel.yaml` gained two loaders, `php_routes` / `php_middleware_aliases`):

| Project | Measurement |
| --- | --- |
| bagisto | `route_list` **0 → 207** rows (previously Laravel projects had no route table at all, so the "route table registers handler" conclusion never showed); 24 contracts with guards |
| aimeos | 16 contracts with guards (`auth:sanctum` / `guest` …) |

Two things resolved: ① the modifier-first form `Route::middleware('auth')->group(fn)` -- the chain root
now takes `group`; ② the array form `->middleware(['auth','throttle:60'])` -- each item counts as one
mount.

Known boundaries (stated honestly, not papered over for these forms):
- **Alias → class** depends on `$routeMiddleware` in `app/Http/Kernel.php`. Of the 3 samples only aimeos
  has `Http/Kernel.php`, and it does **not** declare `$routeMiddleware` (it uses Laravel's built-in
  aliases), so aliases mostly can't be restored to class names: guard names still show (`web` / `guest`
  / `throttle:5,1`), but they **don't link to class nodes**. Bare short names (`NoCacheMiddleware`) fall
  back through `resolve_short_name` and can link (4 edges measured on bagisto); ambiguous short names
  are always rejected.
- Laravel's built-in middleware classes (`auth` / `guest` / `throttle`) live in `vendor`, already
  excluded at P0 -- even a successful alias restore may find no node to link. That's a **boundary of
  the graph**, not a defect of the restore logic.

### 3.3 Symfony (PHP) route semantic features

Symfony is one of the largest frameworks in the PHP ecosystem, and this round adds its **PHP 8
attribute routes** (`#[Route]` / `#[Get]` / `#[Post]` …) → `HttpContract` + `HandledBy`, using the
**same semantics** as Spring (annotations), FastAPI (decorators) and Django (`path()`).

On the parser side (`collect_method` in `gt-adapter-parser/src/php/mod.rs`) each route attribute is
**synthesized into a call site** (PHP 8 attribute nodes reach the parser as `attribute_list`):

| Form | Contract method | Notes |
| --- | --- | --- |
| `#[Route('/x', methods: ['GET'])]` | `GET` | reads the named argument `methods` array |
| `#[Route('/x', methods: ['GET','POST'])]` | `GET` + `POST` | split into **two** contracts (one per `(method, path)`) |
| `#[Route('/x')]` (no methods) | `ANY` | Symfony doesn't restrict the method → wildcard |
| `#[Get('/x')]` / `#[Post('/x')]` … | `GET` / `POST` | shortcut attributes imply the method |

The synthesized call site's `callee_text` carries an **`attr.` prefix** (e.g. `attr.Route`). This is
required: FKB's `callee` pattern isn't a regex -- a single colon is interpreted as `receiver:method`,
and a bare name matches **by method name**, so `attr:Route` wouldn't match (receiver=`attr`,
method=`Route`), while a bare `Get` would wrongly hit calls like `$cache->Get()`. `entity` points at
the controller method itself, and FKB uses
`to: { entity: true, resolve: class_const }` to link `HandledBy`.

End-to-end self-check (synthetic project): `crates/gt-pipeline/tests/symfony_route_features.rs`.

**Honest boundaries**:
- **Only method-level attributes are recognized**: a class-level `#[Route]` prefix
  (`#[Route('/api')] class X`) isn't handled (no prefix concatenation), so such routes don't become
  contracts.
- **Only these 8 attribute names are recognized** (`Route` / `Get` / `Post` / `Put` / `Delete` /
  `Patch` / `Options` / `Head`); other attributes (e.g. `#[ORM\Entity]`, `#[IsGranted]`) are ignored.
- **`use ... Route as MyRoute` aliases aren't restored**, so `#[MyRoute('/x')]` isn't recognized.
- Placeholders in route paths are kept verbatim (`/api/users/{id}`) and aren't alias-merged with that
  path's template variables.

---

## 4. Compliance check (rules)

Rules are loaded per applicable environment by directory; the core knows no concrete rule:

| Directory | Applies to | Content |
| --- | --- | --- |
| `rules/global/` | language-agnostic (topology-only) | contract bridge (`http-contract-without-handler` / `frontend-calls-missing-backend` / `backend-endpoint-never-called`), hot tables (`hot-table`), config-read hotspots (`config-read-hotspot`), dead tables (`dead-table`), write-only / read-only tables, high-fan-out methods (`hotspot-method`) |
| `rules/php/` | PHP projects only | raw SQL execution points, PII tables, never-triggered events / queues, per-row DB read/write in a loop (N+1), external call in loop, multi-write without tx, signature verification quality |
| `rules/java/` | Java projects only | per-row DB read/write in a loop (N+1) -- the criterion is isomorphic to the PHP version, see §5 |
| `rules/js/` | projects with a frontend sub-project | frontend event bus dead code |

About 30 rules ship built-in. Rules **declare their scope up front** via `applies_to.languages` /
`applies_to.frameworks`; an environment mismatch skips them (`rules_not_applicable`), and if the facts
a criterion depends on aren't in the graph the rule is disabled (`rules_unavailable`) -- avoiding the
"0 hits" silent failure that's more dangerous than false positives. See the README's "How a rule knows
where to run".

---

## 5. Known boundaries (stated honestly)

These are **real gaps in the current graph**, not bugs; keep them in mind when writing rules and
reading graphs:

- **Java N+1 (per-row DB read/write in a loop): done** (`rules/java/n1-query.yaml`). Three rings were
  closed: P7's read / write classification requires the **receiver type to have a `MapsTo` edge**,
  while Spring FKB only builds `MapsTo → Table` for **entity classes** annotated with `@Table` /
  `@TableName`; JPA Repository / MyBatis Mapper interfaces
  (`UserRepository extends JpaRepository<User, Long>`) don't have that edge, so `userRepository.save()`
  can't land a `WritesDb`. The three steps:

  1. **The Java parser captures `extends` generic arguments** -- **done**.
     `interface UserRepository extends JpaRepository<User, Long>` extracts `User`, recording it
     as a `generic.JpaRepository` synthesized call site (`entity` = entity name). This has to work
     around a structural quirk: an interface's `extends` is an **`extends_interfaces` node without a
     field name** in tree-sitter-java, so `child_by_field_name` cannot fetch it, and without that
     "interface extends interface" inheritance would be lost entirely.
  2. **Add rules chaining Repository / Mapper → entity → table** -- **done**. `spring-boot.yaml` writes
     the `Link` (DAO → entity's `References`) + `Project` (project along the entity's `MapsTo` into
     DAO → table), verified by `crates/gt-pipeline/tests/java_db_verbs.rs`. Two pitfalls:
     * `Action::Link` does `ev.string(src)` then `find_by_name`; the **top-level `resolve` doesn't
       participate** and must be written on each `ValueSource`.
     * The generic argument is a **bare name** (entity and DAO are in the same package, no import),
       while the short-name index only collects imports, so the parser must complete it into an FQN
       using the DAO's package, otherwise `find_by_name` misses and no edge is produced.

  3. **Field declaration types completed to an FQN using the same package** -- also a key ring.
     `private UserRepository repo;` is a **short name** in source, while `mapped_tables` looks up
     `MapsTo` by FQN, so a short name finds nothing ⇒ `repo.save()` still lands no action edge. The
     parser now completes it using the enclosing class's package (`com.demo.UserRepository`).

  End-to-end self-check (synthetic project): `crates/gt-pipeline/tests/java_db_verbs.rs`, covering
  "entity → table", "DAO → table (MapsTo)", "`findById` → ReadsDb / `save` → WritesDb".

  **Honest boundaries**:
  * **Only same-package bare type names are recognized**: if a field type comes from a **cross-package
    import** (`import com.other.User;`), completing it against the current package yields a wrong FQN,
    so such calls land no action edge (better missing than guessed).
  * **JPA derived queries aren't in the verb table**: names like `findByEmail` / `countByStatus` aren't
    fixed and can't be enumerated, so they land no `ReadsDb`.
  * **Only the first DAO generic argument is taken**: `JpaRepository<User, Long>` takes `User` (the
    entity), matching convention.

    Note: `Synthesize` can't be used to bypass this -- `Synthesize` means "create a node then link",
    while what's needed here is to link to an **already existing** Table node; the table name
    `eb_user` can't be derived from the entity name `User` (that value comes from `@TableName`), and
    creating another would duplicate the table. The MyBatis **XML** mapper path (`mybatis::select` and
    other pseudo call sites) already emits `ReadsDb` / `WritesDb` directly and isn't subject to this
    limitation.
- **Kafka producer matching relies on receiver variable-name conventions**: `kafkaTemplate` /
  `kafkaProducer` / `producer` (three common field names) plus `KafkaTemplate.sendDefault` (an
  exclusive method name as fallback). If a project uses another field name (e.g. `kt`), add the
  variable name to FKB's `callee` alternatives. The bare method name `send` can't be used directly (it
  would misfire on `sendError` / `email.send` …).
- **Fallback for event type-level merging**: with `publishEvent(var)` (the argument is a variable
  rather than `new X()`), or when the handler parameter type can't be resolved, identity falls back to
  `owner_member` (the send / receive method name) -- then no merging happens, but the `ListensTo` /
  `Emits` edges aren't lost. `entity` takes the bare type name (generics stripped).
- **The message producer's destination depends on positional capture of "method-call argument
  literals"**: `convertAndSend` also comes from `RedisTemplate` / `JmsTemplate`, unified into `Queue`;
  distinguishing them would require refining by receiver umbrella name.
- **The syntax layer has no types**: `rabbitTemplate` / `kafkaTemplate` are variable names and can't be
  restored to classes, so producer matching uses variable-name conventions rather than types (a symbol
  table would be needed for robustness).
- **Python routes / cache are narrowed by receiver variable-name conventions**: `app` / `router` /
  `api_router` / `cache` / `redis` …. Python's `get` / `set` are too generic; bare method names would
  misfire on things like `requests.get("/api")` as routes. If a project uses another variable name
  (e.g. `v1_router`), add it to FKB's `callee` alternatives (the same trade-off as Java's
  `kafkaTemplate::send`).
- **Flask's `methods=` is only decoded at arg1**: written further back it falls back to `GET`;
  multi-method routes (`["GET","POST"]`) take only the first.
- **Celery task merging uses `short_name` normalization**: same-named tasks in **different modules
  merge into one node** (merging wins over distinguishing).
- **Celery beat's `Triggers` points at the registration function**, not the triggered task -- the latter
  is an argument and not a literal, so it can't currently be a link target (better no link than a
  fabricated edge).
- **Nested `Depends` isn't expanded**: if `get_db` itself has another `Depends(...)`, it isn't
  recursed.
- **Languages without a parser warn, but the graph is still empty**: Go / Rust / Kotlin / C# / Ruby are
  recognized as sub-projects and their files are scanned, but with no parser -- P2 produces a
  `NoParserForLanguage` diagnostic and skips. **This removes the silent failure, but not the capability
  gap**: real support still needs a `LanguageParser` for each.
- **No vectors, no LLM calls**: recall for pure Chinese with no identifier doesn't work (see the
  README's "prompt augmentation").

---

## 6. How to extend / verify

- Add a framework = add a YAML under `fkb/` (detectors / root_rules / loaders / rules / resolvers) and
  run `graphtell validate`.
- Add a language = implement `LanguageParser` (reference: `gt-adapter-parser/src/java` as the "second
  language", `src/python` as the "third"; the latter additionally demonstrates decorator modeling,
  `owner_class` backfill for module-level functions, and other dynamic-language issues).
- If you add a language but **don't write a parser yet**, be aware: its files are still scanned and
  sub-projects still recognized, and P2 reports `NoParserForLanguage`. Don't treat a marker in
  `MarkerProvider` (default table: `gt-adapter-techstack`) as "supported".
- Add a library whose facts live in files that are **not source code** (MyBatis mapper XML today) = implement
  `ResourceAdapter` in `gt-adapter-resource`, and declare the detectors for the same knowledge id in
  `fkb/<lang>/<lib>.yaml`. The kernel runs such an adapter only for sub-projects where P3 recognised that id
  and applies the returned pseudo call sites itself, so `gt-pipeline` needs no change.
- Add a compliance rule = add a YAML under `rules/<env>/`, then measure hits against the sample library
  (5 ThinkPHP + 3 Spring Boot projects already built): it must be neither 0 (silent failure) nor
  flooding (noise) before you decide to ship it.
- Changing FKB doesn't trigger a rebuild; changing the parser / engine does.
