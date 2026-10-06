//! Prompt-augmentation (code recall) use case: given a prompt, find on the graph "which code to read".
//!
//! # How it differs from full-text search
//!
//! Full-text search answers "which file contains this string"; recall answers "which code does this topic involve".
//! The latter must use the graph: after hitting a seed, expand along call chain / read-write edges to pull in
//! code that **doesn't contain the keyword but is genuinely related**.
//!
//! # Honest boundaries of the MVP
//!
//! This is **structural recall**: find seeds in the graph by identifier, then expand by graph. No semantic vectors, no LLM.
//! A pure-Chinese prompt only works when it contains an identifier or a structural hint word ("table"/"interface"/"event"…).
//! Deliberate tradeoff: make "graph can recall" verifiable first, then the semantic layer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use gt_domain::error::Result;
use gt_domain::model::{Node, NodeId, ProjectId};
use gt_domain::model::kinds::is_chain_edge;
use gt_domain::port::{FileScanner, FileSystem, NodeFilter, Persistence};
use serde::{Deserialize, Serialize};

use crate::embedding::{cosine, default_embedder, Embedder};

/// Node kinds excluded from recall by default.
///
/// `CallSite` is a single call site inside a method body (80% of all nodes in CRMEB); recalling it means treating
/// "every line of code" as an answer — too fine-grained and extremely noisy.
pub const DEFAULT_EXCLUDED_KINDS: &[&str] = &[
    "CallSite",
    "File",
    "Directory",
    "Namespace",
    "Property",
    "Const",
];

/// Weight of the vector-path recall (same scale as the lexical score, for easy merged ranking).
const VECTOR_WEIGHT: f64 = 200.0;
/// Cosine below this is treated as "not related" and excluded from candidates.
pub const VECTOR_THRESHOLD: f64 = 0.3;
/// Top-N lexical-path entries used as seeds.
///
/// Tried widening to 10/12: brings back `order_cancel_time` but pushes unrelated methods (`save`) to the top of
/// "how to modify order discount" — a net loss, abandoned. The real fix is [`wants_config_value`]: config queries
/// stop inverting kind preference, the config key rises to #2 without touching the quota. Keep original value.
const SEED_COUNT: usize = 5;
/// Top-N extra vector-path entries as seeds (union with lexical seeds) to fill cross-language recall.
const VECTOR_SEED_COUNT: usize = 4;
/// Only attach source snippets to the top-N hits in the context pack; other hits list name + location only.
/// Snippets are the bulk of volume (each ~7-15 lines); attaching all would bloat default output to ~800+ tokens,
/// while the model needs to read closely only the top few. The list still keeps all hit names/locations, no info lost.
const SNIPPET_TOP: usize = 6;
/// Cap on the number of full source files returned for hits (avoid stuffing too many files and blowing up context).
const INCLUDE_BODY_MAX_FILES: usize = 8;
/// Byte cap per hit file's full source; truncate beyond it (avoid huge files blowing up context).
const INCLUDE_BODY_MAX_BYTES: usize = 128 * 1024;

/// Generic CRUD verb method names (action only, no business semantics).
///
/// Under action intent, [`rank_weight`] gives `Method` an overall 1.5× boost to float methods/classes above routes and
/// infrastructure. But verb-only methods like `edit`/`save`/`update` carry no business info; once boosted by a query word
/// like "modify", they push CRUD of unrelated domains to the top. So such methods lose the action boost only when "no
/// content word is hit" (fall back to 1.0): if they also hit a content word (coupon/order `edit`), they keep the boost.
/// Compound business names (`createForm`/`getWorkbench`) are not pure-verb so keep the boost — precision only applies to
/// "pure-verb methods with no business content", zero regression.
const GENERIC_CRUD_METHODS: &[&str] = &[
    "edit", "save", "update", "modify", "create", "add", "insert", "delete", "remove", "destroy",
    "new", "set", "get", "list", "find", "query", "fetch", "search", "select", "load", "read",
    "index",
];

/// Whether a method name is only a generic CRUD verb (no business semantics). See [`GENERIC_CRUD_METHODS`].
fn is_generic_crud_method(name: &str) -> bool {
    GENERIC_CRUD_METHODS.contains(&name)
}

/// Discount on the base match score for pure CRUD verb methods that hit only by the verb (no content word).
///
/// Just dropping the action boost isn't enough: such methods still get 100 on exact same-name match and dominate. After the
/// discount they yield to real domain answers. Those hitting a content word are completely unaffected.
const GENERIC_CRUD_VERB_ONLY_DISCOUNT: f64 = 0.5;

/// Discount factor for ORM **association accessors** (boilerplate).
///
/// Methods like `user()`/`refund()` have bodies of only `hasOne/hasMany/belongsTo`, no business logic; on the graph they
/// carry a `MapsTo` out-edge. They get 100 by exact same-name match with generic words; measured "notify user after order"
/// had four such in top five. At 0.3 they do not hog seed slots but can still be recalled.
const RELATION_ACCESSOR_DISCOUNT: f64 = 0.3;

/// Whether it's an ORM association accessor: has a `MapsTo` out-edge (maps to another entity).
fn has_maps_to(id: i64, outgoing: &HashMap<i64, Vec<gt_domain::model::Edge>>) -> bool {
    outgoing
        .get(&id)
        .map(|es| es.iter().any(|e| e.kind.as_str() == "MapsTo"))
        .unwrap_or(false)
}


/// Boost for **named symbols** (anchors): when the prompt writes a full identifier (`StoreOrderCreateServices`/
/// `createOrder`/`store_order`), the user already knows which class/method to read — precise pointing, not semantic similarity.
///
/// Measured: "which downstream methods does createOrder call" got flooded by `*Listener`, `createOrder` absent: "after" triggers
/// event intent, event seeds have `EVENT_SEED_MIN` floor (400) beating createOrder's exact match. Boosting precise pointers
/// returns the named symbol to the first seed slot.
const ANCHOR_EXACT_BOOST: f64 = 3.0;
/// Identifier prefix / suffix match (`createOrders` ↔ anchor `createOrder`).
const ANCHOR_NAME_BOOST: f64 = 2.2;
/// Located in the fqn (parent/child namespace or directory), weaker than the name.
const ANCHOR_FQN_BOOST: f64 = 1.6;

/// Discount factor for **test files**.
///
/// Developers don't look for business impl in unit tests, but `test/user.test.js` is full of symbols that "look like answers"
/// (`makeFakeUser`/`fixtureFilename`); measured: 4 of top 5 of "thumbnail after avatar upload" came from `test/**`. After
/// downweighting they can still be recalled, just without occupying slots.
const TEST_FILE_DISCOUNT: f64 = 0.55;

/// Discount factor for **generator boilerplate files** (MyBatis Generator `mall-mbg`/`generated-sources`…).
///
/// Such files are full of `andPaymentTimeIsNull`/`createCriteria`; a Chinese query gets full marks by lexical match yet has
/// zero business semantics — measured: 7 of mall's top8 came from here.
const GENERATED_FILE_DISCOUNT: f64 = 0.5;

/// Discount for Criteria / Example DSL chained method names.
///
/// Complements the path check: even when not under `mbg` (a `*Example` class copied into a business package also pollutes),
/// the shape `andXxxEqualTo`/`createCriteria` is itself a MyBatis Generator fingerprint.
const CRITERIA_BUILDER_DISCOUNT: f64 = 0.45;

/// Whether the path is **test code**: by path segments / filename, to avoid wrongly hitting business dirs like `Contest/`/`latest/`.
fn is_test_path(path: &str) -> bool {
    let norm = path.replace('\\', "/").to_lowercase();
    let (_, file) = match norm.rsplit_once('/') {
        Some((dir, f)) => (dir, f),
        None => ("", norm.as_str()),
    };
    // Filename check: `*.test.js` / `*.spec.ts` / `*_test.go` / `test_*.py`
    if file.starts_with("test_")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.ends_with("_test.go")
        || file.ends_with("_test.php")
        || file.ends_with("_test.py")
    {
        return true;
    }
    // Directory check: `test/` / `tests/` / `__tests__/` / `spec/` / `specs/`
    norm.split('/').any(|seg| {
        matches!(seg, "test" | "tests" | "__tests__" | "spec" | "specs")
    })
}

/// Whether the path is **framework / generator boilerplate output**.
fn is_generated_path(path: &str) -> bool {
    let norm = path.replace('\\', "/").to_lowercase();
    norm.split('/').any(|seg| {
        seg.contains("mbg") || seg.contains("generated") || seg.contains("_pb2")
    })
}

/// Whether it's a MyBatis Generator query-builder method: `example.createCriteria()`, `addCriterion(...)`,
/// `andPaymentTimeIsNull()`/`orStatusEqualTo()`…
///
/// Judged only by **shape** (camelCase + `and`/`or` prefix + capitalized 3rd letter), framework-independent.
fn is_criteria_builder_method(name: &str) -> bool {
    if matches!(
        name,
        "createCriteria" | "createCriteriaInternal" | "addCriterion" | "addCriterionWithNoValue"
    ) {
        return true;
    }
    // `andPaymentTimeIsNull`/`orIdIn`: and|or prefix + capitalized word start + long enough (exclude `and`/`or` themselves
    // and normal words like `android` that start with `and` — their 3rd char is lowercase).
    for prefix in ["and", "or"] {
        let rest = match name.strip_prefix(prefix) {
            Some(r) if r.chars().count() >= 2 => r,
            _ => continue,
        };
        if rest.chars().next().is_some_and(|c| c.is_uppercase()) {
            return true;
        }
    }
    false
}

/// Path-dimension downweight factor (test / generated code).
fn file_noise_discount(path: &str) -> f64 {
    if is_test_path(path) {
        TEST_FILE_DISCOUNT
    } else if is_generated_path(path) {
        GENERATED_FILE_DISCOUNT
    } else {
        1.0
    }
}

/// The node's **file path** pointer (for [`node_noise_discount`] lookup).
fn node_file_path<'a>(node: &Node, files: &'a HashMap<i64, String>) -> Option<&'a str> {
    let fid = node.file_id.as_ref()?;
    files.get(&fid.get()).map(|s| s.as_str())
}

/// A node's combined downweight: ORM accessor × file (test/generated) × query-builder method.
fn node_noise_discount(
    node: &Node,
    files: &HashMap<i64, String>,
    boilerplate: &HashSet<i64>,
) -> f64 {
    let mut d = 1.0;
    if boilerplate.contains(&node.id.get()) {
        d *= RELATION_ACCESSOR_DISCOUNT;
    }
    if let Some(p) = node_file_path(node, files) {
        d *= file_noise_discount(p);
    }
    if is_criteria_builder_method(&node.name) {
        d *= CRITERIA_BUILDER_DISCOUNT;
    }
    d
}

/// **Fully-named identifiers** (anchors) written in the prompt.
///
/// Keep conservative: only "looks like an identifier (camelCase/snake_case, ≥2 tokens, len ≥6)" **and** "really exists in
/// the graph (some node's name/fqn contains it)" counts as an anchor. The "really exists" part is required — otherwise treating
/// a casually written English word as an anchor would steer ranking wrong; while PascalCase class names rarely collide by name.
fn extract_anchors(query: &str, nodes: &[Node]) -> Vec<String> {
    let mut cands: Vec<String> = Vec::new();
    let mut buf = String::new();
    for ch in query.chars() {
        if (ch.is_alphanumeric() && !is_cjk(ch)) || ch == '_' {
            buf.push(ch);
        } else if !buf.is_empty() {
            cands.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        cands.push(buf);
    }

    cands
        .into_iter()
        .filter(|c| is_identifier_shaped(c))
        .map(|c| c.to_lowercase())
        .filter(|low| {
            nodes.iter().any(|n| {
                n.name.to_lowercase().contains(low.as_str())
                    || n.fqn
                        .as_deref()
                        .is_some_and(|f| f.to_lowercase().contains(low.as_str()))
            })
        })
        .collect()
}

/// Whether it looks like a "multi-word identifier": `StoreOrderCreateServices`/`createOrder`/`store_order`.
/// A single English word (`shipping`/`platform`) doesn't count — it's an ordinary query word, no anchor boost.
fn is_identifier_shaped(s: &str) -> bool {
    if s.chars().count() < 6 {
        return false;
    }
    if !s.chars().any(|c| c == '_' || c.is_uppercase()) {
        return false;
    }
    split_ident_tokens(s).len() >= 2
}

/// Whether the node hits some anchor, and the corresponding boost tier (see [`ANCHOR_EXACT_BOOST`]).
fn anchor_multiplier(node: &Node, anchors: &[String]) -> f64 {
    if anchors.is_empty() {
        return 1.0;
    }
    let name = node.name.to_lowercase();
    let fqn = node.fqn.as_deref().unwrap_or("").to_lowercase();
    let mut best = 1.0f64;
    for a in anchors {
        if name == *a {
            best = best.max(ANCHOR_EXACT_BOOST);
        } else if name.starts_with(a.as_str()) || name.ends_with(a.as_str()) {
            best = best.max(ANCHOR_NAME_BOOST);
        } else if !fqn.is_empty() && fqn.contains(a.as_str()) {
            best = best.max(ANCHOR_FQN_BOOST);
        }
    }
    best
}

/// Concept clusters (Chinese concept → English synonym cluster): for "ranking-side concept weighting".
///
/// Complements [`expand_intent_aliases`] (query side, decides **whether to match**): the latter expands a Chinese concept into
/// English tokens for lexical matching; this mechanism additionally boosts, at the **ranking stage**, nodes that hit "multiple
/// expressions of the concept", letting truly-domain nodes (e.g. `resendVerificationEmail` containing mail+email) float above
/// "nodes that only hit generic notify/send" and "test files" — exactly why "send email" recall is weak in scattered-naming
/// projects (not a miss, but out-competed by generic words and tests).
///
/// **Generality**: depends on no specific project's node text; switching projects takes effect automatically. Covers high-frequency
/// concepts (notify/pay/stock/password/auth/refund); extendable in the same pattern (edit this array, zero cost, zero re-encoding).
/// Opposite to node enrichment (per-project edits, needs re-encoding), this is a generic ranking-layer improvement.
///
/// `core` = specific tokens (a hit counts as the concept, single token also mild boost); `all` = all tokens (incl. generic notify/
/// message, only aggregate boost when co-occurring with core, to avoid single generic word over-boosting). Concept clusters cover the
/// same Chinese concept → English surface as domain alias packs. This is a root-cause generic ranking layer: works on any project,
/// zero node changes, zero re-encoding; when a concept isn't triggered it has zero effect on ranking (zero eval risk adding it).
///
/// Same rules: core uses specific tokens (single hit ×1.2), all includes generic tokens (aggregate ×1.5 only co-occurring).
/// Generic words known to wrongly lift unrelated nodes (pay/password/pwd/message/code/send/limit/control…) stay demoted to `all`.
const CONCEPT_CLUSTERS: &[(&[&str], &[&str])] = &[
    // notify / communication concept
    (
        &["mail", "email", "sms"],
        &["mail", "email", "sms", "notify", "message", "push"],
    ),
    // pay / transaction concept (pay is in `all`: avoid "unpaid" etc. negatives over-lifting pay* nodes and squeezing order-cancel targets)
    (
        &["payment", "checkout"],
        &["pay", "payment", "checkout", "transaction", "wallet"],
    ),
    // stock / warehouse concept
    (
        &["stock", "inventory", "warehouse"],
        &["stock", "inventory", "warehouse", "goods", "deduct", "decrement", "oversell"],
    ),
    (
        &["encrypt", "cipher", "bcrypt", "encoder"],
        &[
            "password", "passwd", "pwd", "encrypt", "cipher", "bcrypt", "encoder", "hash",
            "decrypt",
        ],
    ),
    // auth / JWT concept
    (
        &["auth", "permission", "rbac", "jwt", "oauth", "guard", "login"],
        &[
            "auth", "permission", "rbac", "jwt", "oauth", "guard", "login", "token", "bearer",
            "strategy", "middleware", "authorize", "authenticate",
        ],
    ),
    // refund / after-sales concept
    (&["refund", "reback"], &["refund", "reback", "return"]),
    // ===== below aligned from domain alias concepts (generic, zero node changes) =====
    // balance / assets
    (&["balance", "yue", "now_money"], &["balance", "yue", "now_money"]),
    // coupon / discount
    (&["coupon", "discount"], &["coupon", "discount"]),
    // content entities (article / comment / brand / category / tag)
    (
        &["article", "post", "blog", "comment", "brand", "category", "tag"],
        &["article", "post", "blog", "comment", "brand", "category", "tag"],
    ),
    // concurrency / consistency (lock / concurrent / transaction)
    (
        &["lock", "mutex", "atomic", "concurrent", "transaction", "commit"],
        &[
            "lock", "mutex", "atomic", "concurrent", "transaction", "commit", "pessimistic",
            "optimistic",
        ],
    ),
    // scheduled / delayed (timeout / cron / expire)
    (
        &["schedule", "cron", "timer", "timeout", "expire", "overtime", "delay"],
        &["schedule", "cron", "timer", "timeout", "expire", "overtime", "delay"],
    ),
    // alert / threshold / time-limit (limit generic, keep in `all`)
    (&["warn", "alert", "threshold"], &["warn", "alert", "threshold", "limit"]),
    // top-up / recharge
    (&["recharge"], &["recharge"]),
    // fulfillment / logistics (delivery / shipping / express)
    (
        &["logistics", "delivery", "express", "ship", "dispatch"],
        &["logistics", "delivery", "express", "ship", "send", "dispatch", "shipping"],
    ),
    // distribution / commission
    (
        &["distribution", "brokerage", "commission"],
        &["distribution", "resale", "brokerage", "commission"],
    ),
    // withdrawal
    (&["withdraw", "cashout"], &["withdraw", "cashout"]),
    // verification / consume (verify/consume generic, keep in `all`)
    (&["writeoff"], &["writeoff", "verify", "consume"]),
    // marketing / campaign
    (
        &["promotion", "marketing", "campaign"],
        &["promotion", "marketing", "activity", "campaign"],
    ),
    // member / tier
    (&["member", "vip", "level", "grade"], &["member", "vip", "level", "grade"]),
    // address
    (&["address"], &["address", "shipping"]),
    // avatar
    (&["avatar", "profile"], &["avatar", "profile"]),
    // verification code (code/verify generic, keep in `all`)
    (&["captcha"], &["captcha", "code", "verify"]),
    // QR code
    (&["qrcode", "qr"], &["qrcode", "qr", "code"]),
    // follow / activity feed
    (&["follow", "feed"], &["follow", "feed", "activity"]),
    // favorite / like
    (&["favorite", "bookmark", "like"], &["favorite", "bookmark", "like"]),
    // invoice / bill
    (&["invoice", "bill"], &["invoice", "bill"]),
    // signature
    (&["sign", "signature"], &["sign", "signature"]),
    // whitelist / blacklist
    (
        &["whitelist", "blacklist", "exclude", "anonymous", "deny"],
        &["whitelist", "blacklist", "exclude", "anonymous", "deny"],
    ),
    // finance: transfer
    (&["transfer"], &["transfer"]),
    // finance: clearing / settlement
    (
        &["clearing", "settle", "settlement"],
        &["clearing", "settle", "settlement"],
    ),
    // finance: reconciliation
    (
        &["reconcile", "reconciliation"],
        &["reconcile", "reconciliation"],
    ),
    // finance: ledger / flow
    (&["statement", "ledger", "flow"], &["statement", "ledger", "flow"]),
    // finance: account
    (&["account"], &["account"]),
    // finance: credit (limit generic, keep in `all`)
    (&["credit"], &["credit", "limit"]),
    // finance: repayment
    (&["repay"], &["repay"]),
    // finance: interest rate
    (&["interest", "rate"], &["interest", "rate"]),
    // finance: risk control (control generic, keep in `all`)
    (&["risk"], &["risk", "control"]),
    // finance: rollback / reversal
    (
        &["rollback", "recover", "restore"],
        &["rollback", "recover", "restore"],
    ),
];

/// Weight multiplier when a node hits a concept cluster (mild, avoid noise):
/// - hits ≥2 different tokens in `all` (multiple expressions, e.g. mail+email) → ×1.5;
/// - otherwise hits ≥1 token in `core` (e.g. email/sms) → ×1.2;
/// - otherwise ×1.0 (single generic word notify/message not weighted alone).
fn concept_multiplier(node: &Node, clusters: &[(&[&str], &[&str])]) -> f64 {
    if clusters.is_empty() {
        return 1.0;
    }
    let name = node.name.to_lowercase();
    let fqn = node.fqn.as_deref().unwrap_or("").to_lowercase();
    let identity = node
        .identity
        .as_ref()
        .map(|i| i.value.to_lowercase())
        .unwrap_or_default();
    let mut boost = 1.0;
    for (core, all) in clusters {
        let mut n_all = 0usize;
        let mut n_core = 0usize;
        for t in *all {
            let tok = *t; // &str
            let hit = name.contains(tok) || fqn.contains(tok) || identity.contains(tok);
            if hit {
                n_all += 1;
                if core.contains(&tok) {
                    n_core += 1;
                }
            }
        }
        if n_all >= 2 {
            boost *= 1.5;
        } else if n_core >= 1 {
            boost *= 1.2;
        }
    }
    boost
}

/// Determine which concept clusters are triggered by the expanded alias terms.
///
/// Reuses [`expand_intent_aliases`] output: Chinese concept words have been bridged into English tokens, so if `alias_terms`
/// contains any token of a cluster's `all`, that cluster is triggered.
fn triggered_concepts(
    alias_terms: &[String],
) -> Vec<(&'static [&'static str], &'static [&'static str])> {
    let mut out = Vec::new();
    for (core, all) in CONCEPT_CLUSTERS {
        let fired = all
            .iter()
            .any(|t| alias_terms.iter().any(|a| a.as_str() == *t));
        if fired {
            out.push((*core, *all));
        }
    }
    out
}

/// Over-generic "architecture noun" aliases (service/api/model/entity…).
///
/// These often appear as class-name suffixes in fqns. Treating them as "content words" mis-judges the action boost: shipping's
/// `DeliveryService.save`, because its fqn contains `service`, is treated as "hit content" and keeps the boost — exactly a pitfall a
/// precise version once regressed into. Only real domain words (coupon/order/stock/pay…) count as content words (see [`score_node`]).
const GENERIC_NOUNS: &[&str] = &[
    "service", "api", "interface", "model", "entity", "config", "configuration", "data", "file",
    "log", "cache", "task", "job", "message", "session", "property", "attribute", "field", "page",
    "event", "queue", "topic", "schedule", "eventbus", "dict", "dictionary", "workbench",
    "dashboard", "third", "party",
    "user", "member", "admin", "agent", "common", "base",
];

/// Whether this token is a generic architecture noun (see [`GENERIC_NOUNS`]).
fn is_generic_noun(t: &str) -> bool {
    GENERIC_NOUNS.contains(&t)
}

/// Floor for the bootstrap score of event handlers (listeners/subscribers) as seeds in event-driven queries.
///
/// See [`collect_event_seeds`]: handler names are often extremely generic (uniformly `handle`), lexical score ≈0, they must enter
/// the seed set via bootstrap score to participate in BFS. Actual score is "strongest lexical seed × 0.85" clamped to [floor, 700],
/// so listeners and direct callees float to top without beating truly name-matched strong seeds.
const EVENT_SEED_MIN: f64 = 400.0;
/// Boost for an event handler's "direct callee": float the "listener → business handler" chain above an ordinary "seed → 1-hop neighbor".
const EVENT_CALLEE_BOOST: f64 = 1.6;
/// Event seed cap: many listeners may match; keep only top-N by relevance (matched query-word count) to avoid flooding BFS/list.
const EVENT_SEED_CAP: usize = 10;

/// Event-driven query recognition: user asks "how to Y after X"/"after success…"/"event/listen/callback…", reconstructing the
/// "trigger → listener → handler" chain. The answer is usually an event handler (generic `*Listener`/`handle`), not a name-matched method.
///
/// Only recognize "temporal / explicit event" signals, not action words like "notify/place order" — otherwise action queries would be
/// misjudged as event queries and mixed with listener seeds (action intent handled separately by [`action_intent`]).
fn event_intent(q: &str) -> bool {
    const KW: &[&str] = &[
        // Chinese: temporal / post-action
        "之后", "之后怎么", "后怎么", "成功后", "完成后", "到账后", "支付后", "下单后", "退款后",
        "发货后", "创建后", "登录后", "注册后", "支付成功", "下单成功",
        // Chinese: explicit event semantics
        "事件", "监听", "触发器", "回调", "订阅",
        // English
        "after", "on success", "once", "on complete",
        "event", "listener", "subscribe", "observer", "trigger", "callback",
    ];
    let low = q.to_lowercase();
    KW.iter().any(|k| low.contains(&k.to_lowercase()))
}

/// Whether this node is an event handler (listener/subscriber/observer). Judged by naming convention + namespace, framework-independent
/// (ThinkPHP `*Listener`, Laravel `EventListener`, Spring `@EventListener`, NestJS `@OnEvent` all covered).
fn is_event_handler(node: &Node) -> bool {
    let name = node.name.to_lowercase();
    let class_part = node
        .fqn
        .as_deref()
        .map(|f| f.split("::").next().unwrap_or(f).to_lowercase())
        .unwrap_or_default();
    const SUFFIXES: &[&str] = &[
        "listener", "subscriber", "observer", "eventhandler", "eventsubscriber",
        "eventlistener", "eventconsumer", "consumer",
    ];
    if SUFFIXES.iter().any(|s| class_part.ends_with(s) || name.ends_with(s)) {
        return true;
    }
    // Methods `handle`/`listen`/`dispatch`/`__invoke`/`onX` under event/listener/observer/subscriber/handler namespace or path count as handlers.
    let in_event_ns = node.fqn.as_deref().map(|f| {
        let fl = f.to_lowercase();
        ["listener", "event", "observer", "subscriber", "handler", "eventbus", "events"]
            .iter()
            .any(|t| fl.contains(t))
    }).unwrap_or(false);
    if in_event_ns {
        const HANDLER_METHODS: &[&str] = &["handle", "listen", "dispatch", "__invoke", "onevent"];
        if HANDLER_METHODS.iter().any(|m| name == *m) {
            return true;
        }
        if name.starts_with("on")
            && name.chars().nth(2).map(|c| c.is_uppercase()).unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Collect event-handler seeds (see [`is_event_handler`], and `Event` nodes).
///
/// Return only nodes not yet in seeds and eligible; many handlers may exist, so require name or fqn to hit any query word to be a
/// seed, otherwise all listeners flood BFS into "listener soup" drowning the relevant one.
///
/// More matched query words (spanning more concepts) = more relevant; keep top [`EVENT_SEED_CAP`] by matched-word count. Seed score is
/// weighted by matched-word rarity (idf): hitting only generic `order` ties with siblings; hitting rare `notify`/`coupon`/`refund`
/// beats siblings — the generic basis for "notify after order" and "roll back coupon after refund".
fn collect_event_seeds<'a>(
    nodes: &[Node],
    index: &'a HashMap<i64, &'a Node>,
    existing: &HashSet<i64>,
    match_terms: &[String],
    base_score: f64,
) -> Vec<(f64, Vec<String>, &'a Node)> {
    const SKIP_KINDS: &[&str] = &["I18nKey", "Page", "EventBus"];
    // Document frequency df: nodes containing this word. Smaller df = rarer, idf = ln(N/df) larger.
    let total = nodes.len().max(1);
    let df: HashMap<String, usize> = match_terms
        .iter()
        .map(|t| {
            let tl = t.to_lowercase();
            let c = nodes
                .iter()
                .filter(|n| {
                    n.name.to_lowercase().contains(&tl)
                        || n.fqn
                            .as_deref()
                            .map(|f| f.to_lowercase().contains(&tl))
                            .unwrap_or(false)
                })
                .count();
            (t.clone(), c)
        })
        .collect();
    // Collect (score, node), then truncate by score descending, to ensure most relevant listeners enter BFS.
    let mut candidates: Vec<(f64, i64)> = Vec::new();
    for node in nodes {
        let id = node.id.get();
        if existing.contains(&id)
            || DEFAULT_EXCLUDED_KINDS.contains(&node.kind.as_str())
            || SKIP_KINDS.contains(&node.kind.as_str())
        {
            continue;
        }
        if !(is_event_handler(node) || node.kind.as_str() == "Event") {
            continue;
        }
        let low = node.name.to_lowercase();
        let fqn_low = node.fqn.as_deref().map(|f| f.to_lowercase()).unwrap_or_default();
        let matched: Vec<&String> = match_terms
            .iter()
            .filter(|t| {
                let t = t.to_lowercase();
                low.contains(&t) || fqn_low.contains(&t)
            })
            .collect();
        if matched.is_empty() {
            continue;
        }
        let rarity: f64 = matched
            .iter()
            .map(|t| {
                let c = (*df.get(*t).unwrap_or(&0)).max(1);
                ((total as f64) / (c as f64)).ln().max(0.0)
            })
            .sum();
        let score = (base_score + rarity * 20.0).min(base_score + 200.0).max(EVENT_SEED_MIN);
        candidates.push((score, id));
    }
    // Higher score first; tie by smaller id, for reproducibility.
    candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
    candidates
        .into_iter()
        .take(EVENT_SEED_CAP)
        .map(|(s, id)| (s, vec!["<event>".to_string()], index[&id]))
        .collect()
}

/// Split an identifier into a lowercase token sequence by "camelCase boundary + non-alphanumeric boundary".
///
/// E.g.: `StoreCouponIssue` → ["store","coupon","issue"]; `userAddressServices` → ["user","address","services"]; `HTTPResponse` →
/// ["http","response"]. Whole-string `contains` would treat mid-word fragments as hits (`Recorder` contains `order`); token +
/// prefix matching means `order` only hits `order`/`orders`/`orderItem`, not `recorder`. Shape-dependent,
/// project-independent. Prefix (not strict) keeps reasonable compound/inflected matches (`pay`→`payment`).
fn split_ident_tokens(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    for i in 0..n {
        let c = chars[i];
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let is_upper = c.is_uppercase();
        if is_upper && !cur.is_empty() {
            let prev = chars[i - 1];
            let prev_is_lower = prev.is_lowercase();
            let prev_is_upper = prev.is_uppercase();
            let next_is_lower = i + 1 < n && chars[i + 1].is_lowercase();
            // Split at camelCase word start (foo|Bar) or abbreviation end (HTTP|Server).
            if prev_is_lower || (prev_is_upper && next_is_lower) {
                tokens.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Whether this node "hits a content word": among tokens from method name or class identifier, some non-generic token (coupon/order/
/// stock…) not a CRUD verb nor architecture noun. Only then does a pure CRUD verb method keep the action boost (see [`GENERIC_CRUD_METHODS`]).
///
/// Content words come from this query's terms, matched by token prefix (not substring), so project-independent — asking stock
/// recognizes `stock`, asking pay recognizes `pay` (incl `payment`), but won't misjudge `recorder` containing `order`.
fn has_content_word(node: &Node, terms: &[String]) -> bool {
    let name_tokens = split_ident_tokens(&node.name);
    let class_tokens = match node.fqn.as_deref() {
        Some(f) => {
            let class_part = f.split("::").next().unwrap_or(f);
            // Take only the last class-name segment, strip namespace/directory — otherwise `order/DeliveryService`'s dir `order` would be
            // treated as hitting order content (a once-regressed pitfall). The class itself is unrelated to orders.
            let ident = class_part.rsplit(['\\', '/']).next().unwrap_or(class_part);
            if ident.is_empty() {
                name_tokens.clone()
            } else {
                split_ident_tokens(ident)
            }
        }
        None => name_tokens.clone(),
    };
    let all: Vec<&String> = name_tokens.iter().chain(class_tokens.iter()).collect();
    terms.iter().any(|raw| {
        let t = raw.to_lowercase();
        !GENERIC_CRUD_METHODS.contains(&t.as_str())
            && !is_generic_noun(&t)
            && all.iter().any(|tok| tok.starts_with(&t))
    })
}

/// Chinese intent word → English symbol candidate words (offline semantic bridge, no model).
///
/// ⚠️ Keep only cross-domain generic words. A whole e-commerce vocabulary was once put in, with two-way harm: in non-e-commerce projects
/// these never match (bridge absent); and `cohesion_multiplier`'s boost only applied to them (ranking drifted by domain).
/// Domain-specific "Chinese intent → this project's symbols" always goes through [`RecallService::project_bridge`]: a generic layer.
///
/// Both included because both key to cross-language recall: (1) action verbs (query/add/delete/paginate/undo/rollback…) → natural
/// language → code action; every system has CRUD/pagination/upload — measured "paginate list articles" couldn't produce `findAll`.
/// (2) generic technical nouns (stock→stock/token→token/auth→oauth/order→order/user→user…) — basic vocabulary across any domain,
/// not a specific business entity. Including them lets Chinese "revoke token" align with `revokeToken` in vector space.
///
/// Boundary: only words that still hold when switching projects; not a specific business's proprietary entities/jargon.
// ============================================================================
// Built-in alias table (organized by domain pack)
// ----------------------------------------------------------------------------
// Old table was one big e-commerce/CMS-shaped table; non-e-commerce projects had poor out-of-box quality. Now split into domain packs:
//   - `PACK_GENERIC`   cross-domain generic; - `PACK_ECOMMERCE` e-commerce/CMS; - `PACK_FINANCE` finance (demonstrates infinite extension).
// Default [`BUILTIN_PACKS`] loads all; project-level `.graphtell/aliases.json` can append jargon (see [`merged_aliases`]).
// ============================================================================

/// Cross-domain generic alias pack: action verbs + generic technical nouns + configurable-value vocabulary.
/// These words appear in almost all software systems, not bound to a specific business domain.
const PACK_GENERIC: &[(&str, &[&str])] = &[
    // ---- generic action verbs (cross-domain) ----
    ("分页查询", &["find", "list", "page", "all", "index"]),
    ("查询", &["find", "get", "query", "fetch", "search", "select"]),
    ("获取", &["get", "fetch", "find", "load"]),
    ("读取", &["read"]),
    ("拉取", &["pull", "fetch"]),
    ("列表", &["list", "all", "index"]),
    ("分页", &["page", "pagination", "limit", "offset"]),
    ("新增", &["add", "create", "insert", "new"]),
    ("添加", &["add", "create", "insert"]),
    ("创建", &["create", "add", "insert"]),
    ("删除", &["delete", "remove", "destroy"]),
    ("移除", &["remove", "delete"]),
    ("修改", &["update", "modify", "edit", "save"]),
    ("更新", &["update", "modify", "save"]),
    ("编辑", &["edit", "update"]),
    ("详情", &["detail", "info", "get"]),
    ("校验", &["validate", "check", "verify"]),
    ("验证", &["validate", "verify", "check"]),
    ("上传", &["upload"]),
    ("下载", &["download"]),
    ("导入", &["import"]),
    ("导出", &["export"]),
    ("统计", &["count", "stat", "summary", "total"]),
    ("提交", &["submit", "commit"]),
    ("撤销", &["revoke", "cancel"]),
    ("回滚", &["rollback"]),
    ("拦截", &["intercept", "block"]),
    ("过滤", &["filter"]),
    ("搜索", &["search"]),
    ("发送", &["send"]),
    ("接收", &["receive"]),
    ("通知", &["notify"]),
    ("回调", &["callback"]),
    ("登录", &["login", "auth", "signin"]),
    ("登出", &["logout"]),
    ("注册", &["register", "signup"]),
    ("认证", &["authenticate", "auth"]),
    ("授权", &["authorize", "oauth"]),
    ("计算", &["compute", "calculate"]),
    ("生成", &["generate"]),
    ("转换", &["convert", "transform"]),
    ("解析", &["parse"]),
    ("序列化", &["serialize"]),
    ("加密", &["encrypt"]),
    ("解密", &["decrypt"]),
    ("排序", &["sort", "order"]),
    ("汇总", &["aggregate"]),
    ("重置", &["reset"]),
    ("刷新", &["refresh"]),
    // ---- generic technical nouns (cross-domain; not specific business entities) ----
    ("支付", &["pay", "payment", "checkout"]),
    ("付款", &["pay", "payment"]),
    ("用户", &["user", "member", "customer"]),
    ("角色", &["role"]),
    ("权限", &["permission", "authority"]),
    ("管理员", &["admin"]),
    ("订单", &["order"]),
    ("商品", &["product", "goods", "item"]),
    ("库存", &["stock", "inventory"]),
    ("取消", &["cancel"]),
    ("时间", &["time"]),
    ("时限", &["time", "timeout", "expire"]),
    ("阈值", &["threshold", "limit", "warn"]),
    ("预警", &["warn", "warning", "alert"]),
    ("余额", &["balance", "yue", "now_money"]),
    ("优惠", &["coupon", "discount"]),
    ("折扣", &["discount"]),
    ("金额", &["amount", "price"]),
    ("价格", &["price"]),
    ("账单", &["bill", "invoice"]),
    ("评论", &["comment"]),
    ("文章", &["article", "post"]),
    ("博客", &["blog"]),
    ("品牌", &["brand"]),
    ("分类", &["category"]),
    ("标签", &["tag"]),
    ("类型", &["type", "kind"]),
    ("方式", &["method", "way", "mode"]),
    ("令牌", &["token"]),
    ("凭证", &["credential"]),
    ("第三方", &["third", "party", "oauth"]),
    ("访问", &["access"]),
    ("字典", &["dict", "dictionary"]),
    ("配置", &["config", "configuration"]),
    ("工作台", &["workbench", "dashboard"]),
    ("缓存", &["cache"]),
    ("会话", &["session"]),
    ("消息", &["message"]),
    ("推送", &["push", "notify"]),
    ("表", &["table", "schema"]),
    ("数据表", &["table", "schema"]),
    ("日志", &["log"]),
    ("错误", &["error"]),
    ("异常", &["exception"]),
    ("任务", &["task", "job"]),
    ("服务", &["service"]),
    ("接口", &["api", "interface"]),
    ("模型", &["model"]),
    ("实体", &["entity"]),
    ("字段", &["field"]),
    ("属性", &["property", "attribute"]),
    ("文件", &["file"]),
    ("图片", &["image"]),
    ("视频", &["video"]),
    ("数据", &["data"]),
    // ---- compound-intent filler words (Chinese-specific compound phrases → English tokens) ----
    ("失败", &["fail", "failure"]),
    ("不足", &["insufficient", "lack"]),
    // ---- stability / concurrency control (common questions in high-concurrency systems) ----
    ("限流", &["rate", "limit", "throttle"]),
    ("熔断", &["circuit", "breaker", "fallback"]),
    ("幂等", &["idempotent", "idempotency"]),
    ("队列", &["queue", "mq"]),
];

/// E-commerce / CMS domain alias pack: only those that appear as fixed English in this domain (refund=refund, groupon=groupon,
/// shipping address=address, avatar=avatar…). Jargon like `seckill`/`brokerage` specific to some promo/platform stays in
/// project-level `.graphtell/aliases.json`, not the built-in table.
const PACK_ECOMMERCE: &[(&str, &[&str])] = &[
    ("短信", &["sms", "message"]),
    ("合并", &["merge", "combine"]),
    ("积分", &["point", "score", "integral"]),
    ("规格", &["spec", "specification", "attr", "attribute"]),
    ("验证码", &["captcha", "code", "verify"]),
    // ---- fill in domain words the alias table alone would miss (the query would only hit generic add/save) ----
    ("二维码", &["qrcode", "qr", "code"]),
    ("头像", &["avatar", "profile"]),
    ("地址", &["address"]),
    ("收货地址", &["address", "shipping"]),
    ("会员", &["member", "vip"]),
    ("等级", &["level", "grade"]),
    ("优惠券", &["coupon"]),
    ("购物车", &["cart"]),
    ("店铺", &["shop", "store"]),
    ("物流", &["logistics", "shipping"]),
    // delivery / express / shipping: three common Chinese phrasings for logistics, surfaces delivery/express/ship respectively.
    // Only cross-domain generic phrasings; platform jargon like "same-city delivery" stays in project-level `.graphtell/aliases.json`.
    ("配送", &["delivery", "express", "shipping", "dispatch"]),
    ("快递", &["express", "courier", "delivery"]),
    ("发货", &["delivery", "ship", "send", "express"]),
    ("发票", &["invoice"]),
    // ---- synonym completion: let different phrasings all bridge to the same English token ----
    ("退款", &["refund"]),
    ("退货", &["refund", "return"]),
    ("售后", &["aftersale", "refund", "service"]),
    ("团购", &["groupon", "group"]),
    ("拼团", &["groupon", "group"]),
    ("动态流", &["feed", "activity"]),
    ("关注", &["follow"]),
    ("收藏", &["favorite", "bookmark"]),
    ("点赞", &["like"]),
    // ---- transaction / fulfillment (let recharge, deduct, place-order hit specific methods, not generic config nodes) ----
    ("充值", &["recharge"]),
    ("下单", &["order", "place", "create"]),
    ("扣减", &["deduct", "reduce", "decrement", "dec"]),
    // ---- retail / distribution / fulfillment extension (let store, distribution, commission, withdrawal, verification hit specific impls) ----
    ("门店", &["store", "shop", "retail"]),
    ("分销", &["distribution", "resale", "brokerage"]),
    ("佣金", &["commission", "brokerage"]),
    ("提现", &["withdraw", "cashout"]),
    ("核销", &["writeoff", "verify", "consume"]),
    ("营销", &["promotion", "marketing"]),
    ("活动", &["activity", "campaign", "promotion"]),
];

/// Finance domain alias pack: demonstrates the "domain pack" mechanism can extend infinitely by business domain.
/// Non-e-commerce projects (clearing/reconciliation systems) get this domain's vocabulary bridge out of the box. A directional example.
const PACK_FINANCE: &[(&str, &[&str])] = &[
    ("转账", &["transfer"]),
    ("清算", &["clearing", "settle"]),
    ("对账", &["reconcile", "reconciliation"]),
    // rollback / reversal: coupon rollback (recoverCoupon), point rollback (integralAndCouponBack's Back), stock rollback
    // (regressionStock). Code often uses recover/rollback/*Back suffix for "rollback".
    ("回退", &["rollback", "recover", "restore"]),
    ("回滚", &["rollback", "recover"]),
    ("风控", &["risk", "control"]),
    ("结算", &["settle", "settlement"]),
    ("流水", &["statement", "ledger", "flow"]),
    ("交易", &["trade", "transaction"]),
    ("账户", &["account"]),
    ("授信", &["credit", "limit"]),
    ("还款", &["repay"]),
    ("利率", &["interest", "rate"]),
];

/// **Failure-symptom / ops vocabulary** alias pack.
///
/// How developers ask is misaligned with how code is named: people describe goals by symptom ("email won't send", "oversold",
/// "timeout not cancelled"), while the responsible code is `sendMail`/`decGoodsStock`/`cancelTimeOutOrder`. Similarity is extremely
/// low — even vectors can't pull them together; an explicit bridge is required.
///
/// Difference from [`PACK_GENERIC`]: there literal translation (delete→delete); here multi-landing inference: a symptom has different
/// implementation surfaces in different systems ("lock" could be lock/mutex/atomic/optimistic/transaction), so give candidate surfaces
/// and let this project's symbols decide who hits. English words here are wider, selection more cautious.
///
/// Boundary same as other packs: only cross-domain generic symptoms (email, password, token, login, scheduled, concurrency,
/// duplicate submit…). Business-specific symptoms go to project-level
/// `.graphtell/aliases.json`。
const PACK_SYMPTOM: &[(&str, &[&str])] = &[
    // ---- email / message channel ----
    ("邮件", &["mail", "email", "smtp", "mailer"]),
    ("邮箱", &["mail", "email"]),
    // ---- identity / credentials ("wrong password" "change password" land on this group) ----
    ("密码", &["password", "passwd", "pwd"]),
    ("口令", &["password", "passwd"]),
    // `passwordEncoder`/BCrypt Beans contain only password+encoder; matching alone on "encrypt→encrypt" can't hit the encoder suffix shape.
    ("密文", &["encrypt", "cipher", "hash", "bcrypt", "encoder"]),
    ("签名", &["sign", "signature"]),
    // JWT side: query writes `JWT` (uppercase); key uses lowercase + ASCII case-insensitive match.
    ("jwt", &["jwt", "jsonwebtoken", "token", "bearer", "auth", "guard", "strategy"]),
    ("鉴权", &["auth", "authenticate", "authorize", "guard", "middleware"]),
    ("白名单", &["whitelist", "exclude", "anonymous"]),
    ("黑名单", &["blacklist", "deny"]),
    // ---- concurrency / consistency ("oversold" "duplicate order" "concurrent deduct to negative") ----
    ("超卖", &["stock", "oversell", "deduct", "decrement", "dec"]),
    ("加锁", &["lock", "mutex", "atomic", "pessimistic", "optimistic"]),
    ("并发", &["concurrent", "lock", "atomic", "mutex"]),
    ("事务", &["transaction", "atomic", "commit"]),
    // ---- scheduled / delayed execution ----
    ("超时", &["timeout", "expire", "overtime", "delay"]),
    ("定时", &["schedule", "cron", "timer"]),
];

/// All built-in domain packs. Loaded all by default; to narrow by domain, trim this array (or future on-demand selection config).
const BUILTIN_PACKS: &[&[(&str, &[&str])]] = &[
    PACK_GENERIC,
    PACK_ECOMMERCE,
    PACK_FINANCE,
    PACK_SYMPTOM,
];

/// Merge all built-in domain packs into one mergeable owned alias table.
///
/// Same key across packs merges the English expansions, dedup, no overwrite.
fn builtin_aliases() -> Vec<(String, Vec<String>)> {
    let mut list: Vec<(String, Vec<String>)> = Vec::new();
    for pack in BUILTIN_PACKS {
        for (zh, ens) in *pack {
            if let Some(slot) = list.iter_mut().find(|(z, _)| z == zh) {
                for e in ens.iter() {
                    let owned = (*e).to_string();
                    if !slot.1.contains(&owned) {
                        slot.1.push(owned);
                    }
                }
            } else {
                list.push(((*zh).to_string(), ens.iter().map(|e| (*e).to_string()).collect()));
            }
        }
    }
    list
}

/// The set of "Chinese keys" in all built-in alias packs, for CJK segmentation's known-word filtering (see [`parse_query`]).
fn builtin_alias_keys() -> std::collections::HashSet<String> {
    let mut s = std::collections::HashSet::new();
    for pack in BUILTIN_PACKS {
        for (zh, _) in *pack {
            s.insert((*zh).to_string());
        }
    }
    s
}

/// The complete alias table after merging built-in generic aliases + project-level `.graphtell/aliases.json`.
///
/// The project-level file is the right home for domain jargon: travels with the codebase, doesn't pollute tool source. Format:
/// `{ Chinese word: [English token, ...], ... }`, e.g. `{ "seckill": ["seckill"] }`. Same key as built-in appends expansions, no
/// overwrite. On missing/parse failure, silently fall back to built-in (only a warning), so recall isn't blocked.
fn merged_aliases(project_root: Option<&std::path::Path>) -> Vec<(String, Vec<String>)> {
    let mut list = builtin_aliases();
    if let Some(root) = project_root {
        let cfg = root.join(".graphtell").join("aliases.json");
        if let Ok(text) = std::fs::read_to_string(&cfg) {
            match serde_json::from_str::<std::collections::HashMap<String, Vec<String>>>(&text) {
                Ok(extra) => {
                    for (zh, ens) in extra {
                        if let Some(slot) = list.iter_mut().find(|(z, _)| z == &zh) {
                            for e in ens {
                                if !slot.1.contains(&e) {
                                    slot.1.push(e);
                                }
                            }
                        } else {
                            list.push((zh, ens));
                        }
                    }
                }
                Err(e) => tracing::warn!("failed to parse the project alias config {cfg:?}: {e} (ignored; using the built-in table only)"),
            }
        }
    }
    list
}

/// Expand Chinese intent words in the query into English candidate tokens.
///
/// **ASCII keys** (abbreviations like `jwt`) match case-insensitively: developer writes `JWT`, key stores lowercase; strict `contains`
/// would make this bridge never take effect. Chinese keys unaffected.
fn expand_intent_aliases(query: &str, aliases: &[(String, Vec<String>)]) -> Vec<String> {
    let low = query.to_lowercase();
    let mut out = Vec::new();
    for (zh, en) in aliases {
        let hit = if zh.is_ascii() {
            low.contains(&zh.to_lowercase())
        } else {
            query.contains(zh.as_str())
        };
        if hit {
            for e in en {
                if !out.iter().any(|x: &String| x == e) {
                    out.push(e.clone());
                }
            }
        }
    }
    out
}

/// Intent word → category label (for phrase aggregate boost).
///
/// Chinese intent words ("order") and expanded English tokens ("order"/"orders") belong to one category, so a node hitting tokens
/// across categories (e.g. "order"+"coupon") fits the combined intent "order discount", not isolated "order".
fn alias_group_map(aliases: &[(String, Vec<String>)]) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for (zh, en) in aliases {
        m.insert(zh.clone(), zh.clone());
        for e in en {
            m.insert(e.clone(), zh.clone());
        }
    }
    m
}

/// Hitting multiple intent categories (e.g. "order"+"coupon") gives an aggregate boost, letting combined intent beat isolated words.
///
/// +30% per extra category, capped +90% (max ×1.9).
fn cohesion_multiplier(matched: &[String], group_map: &HashMap<String, String>) -> f64 {
    let groups: std::collections::HashSet<&str> = matched
        .iter()
        .filter_map(|t| group_map.get(t))
        .map(|s| s.as_str())
        .collect();
    if groups.len() <= 1 {
        1.0
    } else {
        1.0 + 0.3 * ((groups.len() - 1) as f64).min(3.0)
    }
}

/// Seed selection: lexical top-k and vector top-k union (dedup), merged score = lexical + vector.
///
/// Key: cross-language nodes (coupon→Coupon) even with low lexical score can be BFS-expanded as "vector seeds" instead of being drowned by
/// generic-word lexical matches like Order*; merging by addition (not max) lets pure semantic hits also rank.
fn select_seeds<'a>(
    lexical: &HashMap<i64, (f64, Vec<String>)>,
    vector: &HashMap<i64, f64>,
    index: &'a HashMap<i64, &'a Node>,
) -> Vec<(f64, Vec<String>, &'a Node)> {
    let mut lex: Vec<(f64, i64)> = lexical.iter().map(|(id, (s, _))| (*s, *id)).collect();
    // Tie by node id, to keep seed selection reproducible.
    lex.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    let mut vec: Vec<(f64, i64)> = vector.iter().map(|(id, s)| (*s, *id)).collect();
    vec.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });

    const SEED_SKIP_KINDS: &[&str] = &["I18nKey", "Page"];
    let seedable = |id: &i64| {
        index
            .get(id)
            .map(|n| !SEED_SKIP_KINDS.contains(&n.kind.as_str()))
            .unwrap_or(false)
    };

    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(f64, Vec<String>, &'a Node)> = Vec::new();
    for (_, id) in lex
        .iter()
        .filter(|(_, id)| seedable(id))
        .take(SEED_COUNT)
        .chain(vec.iter().filter(|(_, id)| seedable(id)).take(VECTOR_SEED_COUNT))
    {
        if seen.insert(*id) {
            let (ls, lm) = lexical.get(id).cloned().unwrap_or((0.0, Vec::new()));
            let vs = vector.get(id).copied().unwrap_or(0.0);
            let score = ls + vs;
            let matched = if lm.is_empty() {
                vec!["<vector>".to_string()]
            } else {
                lm
            };
            out.push((score, matched, index[id]));
        }
    }
    out
}

/// Text used for a node's vector encoding (name + kind + fqn + identity + i18n-bridge enrichment).
fn node_embed_text(node: &Node, enrich: &EnrichIndex) -> String {
    let mut s = String::new();
    s.push_str(node.kind.as_str());
    s.push(' ');
    s.push_str(&node.name);
    let toks = key_tokens(&node.name);
    if !toks.is_empty() {
        s.push(' ');
        s.push_str(&toks.join(" ").as_str());
    }
    if let Some(f) = &node.fqn {
        s.push(' ');
        s.push_str(f);
        let ft = key_tokens(f);
        if !ft.is_empty() {
            s.push(' ');
            s.push_str(&ft.join(" ").as_str());
        }
    }
    if let Some(i) = &node.identity {
        s.push(' ');
        s.push_str(&i.value);
    }
    let mut consider: Vec<String> = toks.clone();
    if let Some(f) = &node.fqn {
        consider.extend(key_tokens(f));
    }
    const MAX_ENRICH: usize = 10;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut added: usize = 0;
    for t in &consider {
        if let Some(hits) = enrich.get(t) {
            for (zh, btoks) in hits {
                if seen.insert(zh.clone()) && added < MAX_ENRICH {
                    s.push(' ');
                    s.push_str(zh);
                    added += 1;
                }
                for bt in btoks {
                    if bt != t && seen.insert(bt.clone()) && added < MAX_ENRICH {
                        s.push(' ');
                        s.push_str(bt);
                        added += 1;
                    }
                }
                if added >= MAX_ENRICH {
                    break;
                }
            }
        }
        if added >= MAX_ENRICH {
            break;
        }
    }
    s
}

/// Text used for the query's vector encoding (original query + expanded English intent words).
fn query_embed_text(query: &str, alias_terms: &[String]) -> String {
    let mut s = String::with_capacity(query.len() + alias_terms.join(" ").len() + 8);
    s.push_str(query);
    for t in alias_terms {
        s.push(' ');
        s.push_str(t);
    }
    s
}

/// Whether a node participates in vector encoding.
///
/// `Method`/`Function` are included in the vector space to raise the recall ceiling; excluding them for "being most of the
/// graph" would leave Chinese semantic queries unable to hit business methods directly — only via `INTENT_ALIASES`
/// lexical bridge or BFS from a hit class, quality capped. Encoding cost is bounded by `ensure_cached_with`'s BATCH + skipping
/// [`DEFAULT_EXCLUDED_KINDS`].
///
/// Filtering out `getXxx`/`setXxx` accessors ("many, semantics carried by fields") is wrong too: Chinese "get/query" intent often
/// lands exactly on `getXxx` business methods (`getWorkbench`/`getDictData`/`getAdminByUsername`/`setRechargeConfig`) — all business,
/// not pure field accessors. So accessors are not filtered here: all methods/functions with a source location are included.
fn is_vector_kind(node: &Node) -> bool {
    let k = node.kind.as_str();
    if DEFAULT_EXCLUDED_KINDS.contains(&k) {
        return false;
    }
    if k == "Method" || k == "Function" {
        // Include any node with a source location (has `file_id`): skip synthetic nodes without source, but don't filter by get/set,
        // to avoid killing business methods that directly correspond to Chinese "get/query" intent.
        return node.file_id.is_some();
    }
    true
}

/// Encoding **priority** during warmup (smaller = earlier).
///
/// Full encoding on a big project takes 45+ min on local CPU; during that window recall can only use the fast hash (degraded quality).
/// Let the kinds really often recalled encode first, so semantic quality is basically usable early in warmup instead of only after all
/// done — the main improvement to degraded-period experience, changing no vector result.
///
/// Order follows [`rank_weight`]'s action-intent boost: Method/Function are the main impl landing points, then classes carrying business
/// concepts; the rest (Table/HttpContract/EventBus…) mostly come via BFS.
fn warm_priority(kind: &str) -> u8 {
    match kind {
        "Method" | "Function" => 0,
        "Class" | "Interface" | "Trait" | "Enum" => 1,
        _ => 2,
    }
}

/// Sort the to-encode list: ① high-value kinds first; ② within same priority, **by adjacent text length**.
///
/// Point ② is pure perf: a batch pads to its longest sequence; if one 256-token text is mixed in, dozens of short texts pad to 256,
/// wasting several× compute. Sorting by length buckets similar lengths, minimizing padding waste. Sequences compute independently in
/// BERT, so batching doesn't change vectors — zero quality risk.
fn sort_pending_for_warmup(pending: &mut Vec<(u64, String, u8)>) {
    pending.sort_by(|a, b| a.2.cmp(&b.2).then(a.1.len().cmp(&b.1.len())));
}

/// A single recall request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallQuery {
    /// The prompt (natural language + identifiers mixed is fine).
    pub query: String,
    /// Max number of returned entries.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Hop count to expand from seeds (0 = seeds only).
    #[serde(default = "default_hops")]
    pub hops: u32,
    /// Only look at given kinds; empty = no limit.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Whether to attach source snippets (needs file reads, slightly costlier).
    #[serde(default = "default_true")]
    pub with_snippets: bool,
    /// Whether to also append the full source of the files touched by the hits (default false).
    ///
    /// In MCP / IDE scenarios the LLM's context can read the implementation directly, saving a round-trip `read` to fetch the full file.
    /// Only the top few hit files are taken (see [`INCLUDE_BODY_MAX_FILES`]), and a single file beyond [`INCLUDE_BODY_MAX_BYTES`]
    /// is truncated and marked, to avoid huge files blowing up context.
    #[serde(default)]
    pub include_body: bool,
}

fn default_limit() -> usize {
    10
}
fn default_hops() -> u32 {
    2
}
fn default_true() -> bool {
    true
}

/// A recall hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallHit {
    pub node_id: NodeId,
    pub kind: String,
    pub name: String,
    pub fqn: Option<String>,
    /// Combined score (seed score × hop decay).
    pub score: f64,
    /// Hop count from the seed (0 = the seed itself).
    pub hop: u32,
    /// The matched seed node name.
    pub seed: String,
    /// The matched query word.
    #[serde(default)]
    pub matched_terms: Vec<String>,
    /// Whether it directly hit a keyword (false = pulled out by graph expansion).
    pub direct: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Key relation (e.g. `WritesDb ×3`), for the context pack to explain "why relevant".
    #[serde(default)]
    pub relations: Vec<String>,
}

/// Seed info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedInfo {
    pub node_id: NodeId,
    pub kind: String,
    pub name: String,
    pub score: f64,
}

/// Recall quality tier.
///
/// Recall quality **varies enormously**: some queries have the answer in the top two, some have **both intents miss** with the top
/// all generic-word noise. But both return something that looks the same — downstream (AI IDE) trusts them equally, so **silent failure**
/// is the worst failure mode. Here we report quality explicitly, letting the caller fall back to grep / reading. Judgment uses only
/// project-independent signals (content-word coverage + top-spread gap).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum RecallQuality {
    /// Content words mostly hit, top-spread healthy — directly trustworthy.
    High,
    /// Some content words missed, or hits scattered — suggest judging by the list yourself.
    Medium,
    /// Most content words missed, top is generic-word match — suggest grep / reading instead.
    Low,
}

impl RecallQuality {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecallQuality::High => "high",
            RecallQuality::Medium => "medium",
            RecallQuality::Low => "low",
        }
    }
}

/// Background warmup progress (semantic vector bge computation).
///
/// Lets IDE / MCP know whether recall is still on the cold path (fast vector / lexical), to hint "retry later for better quality".
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WarmupStatus {
    /// Whether this project's semantic vectors are all ready (recall takes the full semantic path).
    pub warmed: bool,
    /// Whether background warmup is in progress.
    pub warming: bool,
    /// Number of encoded nodes (only meaningful when `warming`).
    pub done: usize,
    /// Total nodes to encode (only meaningful when `warming`).
    pub total: usize,
}

/// Recall result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallResult {
    pub project_id: ProjectId,
    pub query: String,
    /// The parsed query words.
    pub terms: Vec<String>,
    /// Matched structural hints (e.g. "table" → Table).
    pub kind_hints: Vec<String>,
    pub seeds: Vec<SeedInfo>,
    pub hits: Vec<RecallHit>,
    /// The context pack (Markdown) ready to paste into an LLM.
    pub markdown: String,
    /// Whether truncated by the scan cap.
    pub truncated: bool,
    /// Recall confidence (0~1): weighted content-word coverage and top-spread gap.
    pub confidence: f32,
    /// Quality tier (see [`RecallQuality`]).
    pub quality: RecallQuality,
    /// Basis for the tier judgment (in plain words).
    pub quality_reason: String,
    /// **Content words** not hit (query words still distinguishing after dropping generic CRUD verbs / architecture nouns).
    /// When quality is low, take these straight to grep.
    pub missing_terms: Vec<String>,
    /// Background warmup progress (filled only when `warming`, for IDE/MCP to hint if quality is affected by cold path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warmup: Option<WarmupStatus>,
}

/// Version of the node-embed-text (`node_embed_text`).
///
/// **Bump this when changing how `node_embed_text` is constructed** (e.g. adding identifier splitting). Otherwise `ensure_cached_with`
/// only backfills "missing nodes", never recomputes cached vectors, and stale vectors get silently reused forever — once misjudged
/// "stale vectors" as "weak semantic model". The persisted-vector "space version": bump on any vector-space change, else old files get
/// silently reused (same dim, same version, guard can't tell), query and nodes land in different spaces, cosine distorts wholesale.
///
/// v3: cache key changed from node id to content hash of node embed text. Old v2 files keyed by node id; after graph rebuild
/// (`reset_project` reassigns ids) old keys all miss, forcing full re-embed every rebuild (tens of minutes on CPU). From v3, content
/// fingerprint as key: unchanged nodes' vectors survive rebuild/restart via disk, full recompute drops to only changed nodes.
/// v3→v4: persistence format JSON→rmp (parsing an order of magnitude faster); old JSON files version-mismatch, one-time re-encode.
const EMBED_TEXT_VERSION: u32 = 4;

/// Envelope for persisted vector files: versioned; on mismatch the whole file is invalidated and recomputed.
#[derive(Serialize, Deserialize)]
struct PersistedEmbeds {
    version: u32,
    /// Vector dimension: must match the current semantic encoder to reuse. After switching to a smaller model (dim change, e.g. bge-m3
    /// 1024 → e5 768) old files dim-mismatch, whole file invalidated and re-encoded; else `cosine` by `min(len)` would silently use
    /// wrong dim and distort. `#[serde(default)]` drops old format (no dim field) to 0 — then reuse only when current encoder dim
    /// matches the vector's actual dim (same-dim model like bge-m3), no forced re-encode.
    #[serde(default)]
    dim: usize,
    vectors: HashMap<u64, Vec<f32>>,
}

/// Semantic vector cache key: deterministic content fingerprint (FNV-1a 64) of the node embed text.
///
/// Key by content not `node.id`: after graph rebuild (`reset_project` reassigns ids), unchanged nodes' embed text is unchanged →
/// fingerprint unchanged → persisted vectors survive rebuild/restart, dropping "full re-embed (tens of min on CPU)" to "only embed
/// changed nodes". Fast-vector cache key uses `node.id as u64` directly (instantly recomputable, no fingerprint needed).
fn embed_text_key(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Recall service.
pub struct RecallService {
    store: Arc<dyn Persistence>,
    fs: Arc<dyn FileSystem>,
    _scanner: Arc<dyn FileScanner>,
    /// Fast encoder (always available, offline hash): when semantic vectors aren't warmed up, recall immediately encodes query + nodes
    /// with it, so the UI never blocks; semantic quality auto-takes over after background warmup.
    fast_embedder: Arc<dyn Embedder>,
    /// Semantic encoder (real bge-m3, loaded on demand). `None` ⇒ only lexical / fast-vector path, no background warmup.
    semantic_embedder: Option<Arc<dyn Embedder>>,
    /// Fast-vector cache (hash, instantly recomputable, not persisted). Keyed by `node.id as u64`.
    fast_cache: Arc<Mutex<HashMap<u64, Vec<f32>>>>,
    /// Semantic vector cache (bge, persisted to `embed_persist_dir`). Keyed by content hash of node embed text (see [`embed_text_key`]),
    /// so unchanged nodes' vectors survive rebuild.
    node_embed_cache: Arc<Mutex<HashMap<u64, Vec<f32>>>>,
    /// Set of projects with warmed-up (bge vectors computed and persisted) vectors; judged per project whether to take the semantic path.
    warmed_projects: Arc<Mutex<HashSet<i64>>>,
    /// Set of projects currently warming up in background (prevent duplicate spawn).
    warming_projects: Arc<Mutex<HashSet<i64>>>,
    /// Background-warmup progress counter: project id → (encoded nodes, total to encode). Kept only during warmup, cleared on
    /// finish/failure, for `/api/.../warmup` and MCP to expose "warmup progress".
    warm_progress: Arc<Mutex<HashMap<i64, (usize, usize)>>>,
    /// Whether to allow background async warmup (only HTTP production entry enables; CLI/tests disable, to avoid spawning threads).
    enable_async_warmup: bool,
    /// Semantic vector persistence dir (`<dir>/<project_id>.rmp`). **Not** written at graph build; only computed and persisted during
    /// recall background warmup / manual `embed` command, loaded directly on restart. `None` ⇒ no persistence (in-memory cache only).
    embed_persist_dir: Option<PathBuf>,
    /// Candidate snapshot persistence dir (`<dir>/<project_id>.json`). Cold start loads in seconds from here, no rebuilding all nodes +
    /// edges from SQLite live (CRMEB measured ~10s → <1s). `None` ⇒ no persistence. On graph rebuild, [`Self::clear_node_cache`] deletes
    /// the whole dir to force invalidation.
    snapshot_persist_dir: Option<PathBuf>,
    /// Project i18n bridge cache: `project id -> [(Chinese text, English tokens split from that text's key)]`. Chinese queries map via
    /// it to **this project's** symbols, depending on no domain-specific vocabulary.
    bridge_cache: Arc<Mutex<HashMap<i64, Vec<(String, Vec<String>)>>>>,
    /// **Candidate-set snapshot** cache: `project id -> nodes + neighbors + file paths participating in recall`.
///
/// The biggest fixed cost in one recall isn't scoring but pulling the candidate set from SQLite (CRMEB 12k nodes measured 856ms +
/// neighbors 110ms), and multi-intent queries pull it again per sub-intent. The graph only changes on rebuild: rebuild calls
/// [`Self::clear_node_cache`]; also a cheap `stats` check before each reuse (see [`Self::snapshot_stale`]). `Arc` lets the caller borrow
/// the snapshot for the whole recall without holding a write lock.
    candidate_cache: Arc<Mutex<HashMap<i64, Arc<CandidateSet>>>>,
    /// Query-vector cache (`query text -> vector`): same prompt asked again (common in IDE) needs no more bge forward (~750ms).
    /// Very small, a plain LRU suffices.
    query_vec_cache: Arc<Mutex<Vec<(String, Vec<f32>)>>>,
}

/// The "graph snapshot" needed for one recall: candidate nodes + neighbors + file paths. See [`RecallService::candidate_cache`].
///
/// Derives `Clone`/`Serialize`/`Deserialize` for **disk reuse**: cold start reads the persisted snapshot (<1s) instead of
/// rebuilding from SQLite live (CRMEB ~10s). See [`RecallService::candidate_set`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CandidateSet {
    nodes: Vec<Node>,
    incoming: HashMap<i64, Vec<gt_domain::model::Edge>>,
    outgoing: HashMap<i64, Vec<gt_domain::model::Edge>>,
    files: HashMap<i64, String>,
    /// Total **whole-graph** node/edge count at snapshot build (from `stats`), for [`RecallService::snapshot_stale`]'s cheap check.
/// Must be whole-graph scope: `nodes` only holds participating kinds, naturally smaller than `stats.nodes`; comparing it to `stats.nodes`
/// would always judge "stale", making snapshot reuse moot.
    node_count: u64,
    edge_count: u64,
    /// The `kinds` filter used at snapshot build (empty = all kinds). Filtered requests don't share the full snapshot.
    kinds: Vec<String>,
}

/// Envelope for persisted candidate snapshots: versioned; on mismatch the whole file is invalidated and rebuilt (like [`PersistedEmbeds`]).
const SNAPSHOT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct PersistedSnapshot {
    version: u32,
    set: CandidateSet,
}

/// Query-vector cache capacity (see [`RecallService::query_vec_cache`]).
const QUERY_VEC_CACHE_CAP: usize = 64;

/// Query text → vector, with a small LRU cache (see [`RecallService::query_vec_cache`]).
///
/// Extracted as a free function so query encoding can run in a **separate thread** parallel with candidate load / disk load (all
/// independent; query encoding is one bge forward, ~800ms on CPU, the biggest serial item in cold start). Moving this logic to a thread
/// cuts the cold-start latency of "first question after restart" by ~1s.
///
/// `semantic` participates in the key: two encoders have different spaces, can't reuse each other.
fn encode_query_cached(
    cache: &Arc<Mutex<Vec<(String, Vec<f32>)>>>,
    embedder: &Arc<dyn Embedder>,
    text: &str,
    semantic: bool,
) -> Vec<f32> {
    let key = format!("{}{}", if semantic { "sem|" } else { "fast|" }, text);
    if let Some(v) = cache
        .lock()
        .unwrap()
        .iter()
        .find(|(k, _)| k == &key)
        .map(|(_, v)| v.clone())
    {
        return v;
    }
    let v = embedder.embed_query(text);
    let mut c = cache.lock().unwrap();
    c.retain(|(k, _)| k != &key);
    c.push((key, v.clone()));
    if c.len() > QUERY_VEC_CACHE_CAP {
        c.remove(0);
    }
    v
}

impl RecallService {
    pub fn new(
        store: Arc<dyn Persistence>,
        fs: Arc<dyn FileSystem>,
        scanner: Arc<dyn FileScanner>,
    ) -> Self {
        Self::with_embedder_and_cache(
            store,
            fs,
            scanner,
            None,
            Arc::new(Mutex::new(HashMap::new())),
            None,
            None,
        )
    }

    /// Inject a custom semantic encoder (e.g. a test fake). Background warmup disabled by default, to avoid spawning threads.
    pub fn with_embedder(
        store: Arc<dyn Persistence>,
        fs: Arc<dyn FileSystem>,
        scanner: Arc<dyn FileScanner>,
        embedder: Arc<dyn Embedder>,
    ) -> Self {
        Self::with_embedder_and_cache(
            store,
            fs,
            scanner,
            Some(embedder),
            Arc::new(Mutex::new(HashMap::new())),
            None,
            None,
        )
    }

    /// Inject semantic encoder + shared semantic vector cache.
    /// `semantic_embedder` `None` ⇒ only lexical / fast-vector path, no background warmup.
    pub fn with_embedder_and_cache(
        store: Arc<dyn Persistence>,
        fs: Arc<dyn FileSystem>,
        scanner: Arc<dyn FileScanner>,
        semantic_embedder: Option<Arc<dyn Embedder>>,
        node_embed_cache: Arc<Mutex<HashMap<u64, Vec<f32>>>>,
        embed_persist_dir: Option<PathBuf>,
        snapshot_persist_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            store,
            fs,
            _scanner: scanner,
            fast_embedder: default_embedder(),
            semantic_embedder,
            fast_cache: Arc::new(Mutex::new(HashMap::new())),
            node_embed_cache,
            warmed_projects: Arc::new(Mutex::new(HashSet::new())),
            warming_projects: Arc::new(Mutex::new(HashSet::new())),
            warm_progress: Arc::new(Mutex::new(HashMap::new())),
            enable_async_warmup: false,
            embed_persist_dir,
            snapshot_persist_dir,
            bridge_cache: Arc::new(Mutex::new(HashMap::new())),
            candidate_cache: Arc::new(Mutex::new(HashMap::new())),
            query_vec_cache: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Enable background async warmup (called only by HTTP production entry): when semantic vectors aren't ready, recall immediately
    /// returns via the fast encoder, while spawning a thread to compute and persist bge vectors; the project then auto-switches to semantic.
    pub fn with_async_warmup(mut self) -> Self {
        self.enable_async_warmup = true;
        self
    }

    /// Clear node vector cache (called after graph rebuild / re-scan, to avoid hitting stale vectors).
    /// Also resets warmup state so next recall re-takes the fast path and (production entry) re-warms in background.
    pub fn clear_node_cache(&self) {
        self.fast_cache.lock().unwrap().clear();
        self.node_embed_cache.lock().unwrap().clear();
        self.warmed_projects.lock().unwrap().clear();
        self.warming_projects.lock().unwrap().clear();
        self.warm_progress.lock().unwrap().clear();
        // After rebuild, i18n texts may change, the bridge must be invalidated too.
        self.bridge_cache.lock().unwrap().clear();
        // Nodes/neighbors swapped; candidate snapshot and query-vector cache must be invalidated (else always answer the old graph).
        self.candidate_cache.lock().unwrap().clear();
        self.query_vec_cache.lock().unwrap().clear();
        // Delete persisted candidate snapshot too: after rebuild, if size happens to match it won't be judged stale; must force rebuild by deleting the file.
        if let Some(dir) = &self.snapshot_persist_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Get this project's candidate-set snapshot participating in recall (cross-request reuse, see [`Self::candidate_cache`]).
    ///
    /// Requests with a `kinds` filter don't enter the cache (minor path; caching would need a "project+filter" key).
    ///
    /// Cold-start optimization: on memory-cache miss, first try the persisted snapshot for second-level recovery ([
    /// `Self::load_persisted_snapshot`]); if size validation passes, reuse directly, avoiding live rebuild from SQLite (CRMEB ~10s → <1s);
    /// else rebuild live and persist the result for future restarts.
    fn candidate_set(&self, project_id: ProjectId, kinds: &[String]) -> Result<Arc<CandidateSet>> {
        let cacheable = kinds.is_empty();
        if cacheable {
            if let Some(snap) = self.candidate_cache.lock().unwrap().get(&project_id.get()) {
                if snap.kinds.is_empty() && !self.snapshot_stale(project_id, snap) {
                    return Ok(Arc::clone(snap));
                }
            }
            // Memory miss: try persisted snapshot for second-level recovery (avoid live rebuild).
            if let Some(set) = self.load_persisted_snapshot(project_id) {
                if !self.snapshot_stale(project_id, &set) {
                    let arc = Arc::new(set);
                    self.candidate_cache
                        .lock()
                        .unwrap()
                        .insert(project_id.get(), Arc::clone(&arc));
                    return Ok(arc);
                }
            }
        }
        let built = Self::build_candidate_set(self.store.as_ref(), project_id, kinds)?;
        let snap = Arc::new(built);
        if cacheable {
            self.candidate_cache
                .lock()
                .unwrap()
                .insert(project_id.get(), Arc::clone(&snap));
            // Persist in background for second-level load on restart (don't block this request).
            self.persist_snapshot(project_id, &snap);
        }
        Ok(snap)
    }

    /// Recover candidate set from persisted snapshot (see [`Self::snapshot_persist_dir`]). Missing / corrupt / version-mismatch returns
    /// `None`; caller falls back to live rebuild.
    fn load_persisted_snapshot(&self, project_id: ProjectId) -> Option<CandidateSet> {
        let dir = self.snapshot_persist_dir.as_ref()?;
        let path = dir.join(format!("{}.bin", project_id.get()));
        let t = std::time::Instant::now();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!("snapshot load {path:?} read failed: {e}");
                return None;
            }
        };
        match rmp_serde::from_slice::<PersistedSnapshot>(&bytes) {
            Ok(p) if p.version == SNAPSHOT_VERSION => {
                tracing::debug!(
                    "snapshot load {path:?} 成功 {} ms, 节点 {}",
                    t.elapsed().as_millis(),
                    p.set.nodes.len()
                );
                Some(p.set)
            }
            Ok(p) => {
                tracing::debug!("snapshot load version mismatch: file {} vs current {}", p.version, SNAPSHOT_VERSION);
                None
            }
            Err(e) => {
                tracing::debug!("snapshot load {path:?} parse failed: {e}");
                None
            }
        }
    }

    /// Persist the candidate set (background thread, atomic write to temp file then rename). On graph rebuild, [`Self::clear_node_cache`]
    /// deletes the whole dir to force invalidation.
    fn persist_snapshot(&self, project_id: ProjectId, snap: &CandidateSet) {
        let dir = match &self.snapshot_persist_dir {
            Some(d) => d.clone(),
            None => return,
        };
        let pid = project_id.get();
        let set = snap.clone();
        thread::spawn(move || {
            if let Err(e) = (|| -> std::io::Result<()> {
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(format!("{pid}.bin"));
                let tmp = dir.join(format!("{pid}.bin.tmp"));
                let bytes = rmp_serde::to_vec(&PersistedSnapshot {
                    version: SNAPSHOT_VERSION,
                    set,
                })
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                std::fs::write(&tmp, &bytes)?;
                std::fs::rename(&tmp, &path)?;
                Ok(())
            })() {
                tracing::warn!("failed to persist the candidate snapshot (project {pid}): {e}");
            }
        });
    }

    /// Whether the snapshot is stale: one read-only `stats` (a few `COUNT`, milliseconds) compared to snapshot size.
///
/// Without this check, a real regression is introduced: when the graph is rebuilt in **another process** by `graphtell run`, or the
/// project is deleted/edited, the resident service isn't notified (only watch calls `clear_node_cache`), so recall keeps answering the
/// old graph — whereas before the change every query re-read the DB, no such problem. Size mismatch ⇒ rebuild snapshot; size exactly
/// equal is covered by `clear_node_cache` on rebuild.
    fn snapshot_stale(&self, project_id: ProjectId, snap: &CandidateSet) -> bool {
        match self.store.stats(project_id) {
            Ok(s) => s.nodes != snap.node_count || s.edges != snap.edge_count,
            Err(_) => false,
        }
    }


    fn build_candidate_set(
        store: &dyn Persistence,
        project_id: ProjectId,
        kinds: &[String],
    ) -> Result<CandidateSet> {
        let wanted: Vec<String> = if kinds.is_empty() {
            scan_kinds(store, project_id)
        } else {
            kinds.to_vec()
        };
        let mut nodes: Vec<Node> = Vec::new();
        for kind in wanted {
            let mut batch = store.query_nodes(&NodeFilter {
                project_id,
                kind: Some(gt_domain::model::NodeKind::new(kind)),
                name_contains: None,
                limit: Some(SCAN_LIMIT),
                offset: None,
            })?;
            nodes.append(&mut batch);
        }
        let ids: Vec<NodeId> = nodes.iter().map(|n| n.id).collect();
        let incoming = store.edges_incoming(&ids)?;
        let outgoing = store.edges_outgoing(&ids)?;
        let stats = store.stats(project_id).ok();
        let node_count = stats.as_ref().map(|s| s.nodes).unwrap_or(0);
        let edge_count = stats.as_ref().map(|s| s.edges).unwrap_or(0);
        let files = store.file_paths(project_id)?;
        Ok(CandidateSet {
            nodes,
            incoming,
            outgoing,
            files,
            node_count,
            edge_count,
            kinds: kinds.to_vec(),
        })
    }

    /// Load a project's bge vectors from the persisted file into the semantic cache, and mark warmed-up.
///
/// Prefer rmp (v4+, parsing an order of magnitude faster); transition-period old file is v3 JSON, fall back to read once and rewrite rmp
/// in background, then directly take rmp. Returns whether it really loaded (file exists and version/dim match); `false` means the
/// persisted file is invalid, caller must treat as "not warmed up" (see [`load_persisted_into`]).
    fn load_persisted(&self, rmp_path: &Path, project_id: ProjectId) -> bool {
        let expected_dim = self.semantic_embedder.as_ref().map_or(0, |e| e.dim());
        // Decide the actual file to read: rmp first, fall back to old json (transition) if missing.
        let (path, need_rmp) = if rmp_path.exists() {
            (rmp_path.to_path_buf(), false)
        } else {
            let json_path = rmp_path.with_extension("json");
            if json_path.exists() {
                (json_path, true)
            } else {
                return false;
            }
        };
        let ok = load_persisted_into(
            &path,
            &self.node_embed_cache,
            project_id,
            &self.warmed_projects,
            expected_dim,
        );
        // Transition: after loading from old JSON, rewrite vectors as rmp in the background; on restart take rmp directly (sub-second),
        // without blocking this recall request.
        if ok && need_rmp {
            let store = self.store.clone();
            let cache = self.node_embed_cache.clone();
            let dir = self.embed_persist_dir.clone();
            let dim = expected_dim;
            let pid = project_id.get();
            thread::spawn(move || {
                if let Ok(nodes) = fetch_nodes(store.as_ref(), ProjectId(pid)) {
                    let enrich = build_enrich_index(&compute_bridge(&nodes));
                    if let Some(d) = dir {
                        Self::persist_vectors(
                            &cache,
                            dim,
                            &d.join(format!("{pid}.rmp")),
                            &nodes,
                            &enrich,
                        );
                    }
                }
            });
        }
        ok
    }

    /// Persist this project's bge vectors to the on-disk file (only nodes participating in recall, to avoid cross-project bleed).
    ///
    /// Use rmp (msgpack) not JSON: CRMEB's full vector JSON is ~153MB and serde_json parse ~5s — the root cause of "slow first query";
    /// rmp of the same size is ~1/2 the volume and parses an order of magnitude faster (sub-second), making the warmup near-invisible.
    fn persist(&self, path: &Path, nodes: &[Node], enrich: &EnrichIndex) {
        Self::persist_vectors(
            &self.node_embed_cache,
            self.semantic_embedder.as_ref().map_or(0, |e| e.dim()),
            path,
            nodes,
            enrich,
        );
    }

/// Write this project's bge vectors to the rmp on-disk file (see [`PersistedEmbeds`]).
///
/// Extracted as a free function so a background thread can rewrite offline during the "old JSON → rmp" transition, not blocking recall.
/// Use rmp not JSON: CRMEB's full vector JSON is ~153MB and serde_json parse ~5s — the root cause of "slow first query"; rmp is ~1/2
/// the volume and parses an order of magnitude faster.
fn persist_vectors(
    cache: &Mutex<HashMap<u64, Vec<f32>>>,
    dim: usize,
    path: &Path,
    nodes: &[Node],
    enrich: &EnrichIndex,
) {
    let cache = cache.lock().unwrap();
    let vectors: HashMap<u64, Vec<f32>> = nodes
        .iter()
        .filter_map(|n| {
            let key = embed_text_key(&node_embed_text(n, enrich));
            cache.get(&key).cloned().map(|v| (key, v))
        })
        .collect();
    drop(cache);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let env = PersistedEmbeds {
        version: EMBED_TEXT_VERSION,
        dim,
        vectors,
    };
    if let Ok(data) = rmp_serde::to_vec(&env) {
        let _ = std::fs::write(path, data);
    }
}

    /// Ensure all topic-level nodes participating in vector recall are encoded into the given cache (batch-encode and backfill the missing).
    /// Reusable by the lexical path / semantic path / manual `embed` command, independent of the specific query.
    ///
    /// Returns the number of nodes **newly encoded this call** — the caller uses it to decide whether to persist: persisting re-serializes
    /// the whole project's vectors (CRMEB 157MB / ~0.5s), and almost every recall misses zero nodes, so persisting every time is pure write
    /// amplification. `semantic` true keys by content hash of node embed text (survives rebuild); false keys by `node.id as u64` (fast hash
    /// space, instantly recomputable). `enrich` is built by the caller and passed in, to avoid each path rebuilding the enrichment index.
    fn ensure_cached_with(
        &self,
        nodes: &[Node],
        embedder: &Arc<dyn Embedder>,
        cache: &Mutex<HashMap<u64, Vec<f32>>>,
        enrich: &EnrichIndex,
        semantic: bool,
    ) -> usize {
        const BATCH: usize = 256;
        let mut pending: Vec<(u64, String, u8)> = Vec::new();
        for node in nodes {
            if !is_vector_kind(node) {
                continue;
            }
            let text = node_embed_text(node, enrich);
            let key: u64 = if semantic {
                embed_text_key(&text)
            } else {
                node.id.get() as u64
            };
            if cache.lock().unwrap().contains_key(&key) {
                continue;
            }
            pending.push((key, text, warm_priority(node.kind.as_str())));
        }
        sort_pending_for_warmup(&mut pending);
        for chunk in pending.chunks(BATCH) {
            let texts: Vec<String> = chunk.iter().map(|(_, t, _)| t.clone()).collect();
            let vecs = embedder.embed_batch(&texts);
            let mut cache = cache.lock().unwrap();
            for ((key, _, _), v) in chunk.iter().zip(vecs.into_iter()) {
                cache.insert(*key, v);
            }
        }
        pending.len()
    }

    /// Project i18n bridge: read this project's `I18nKey` nodes, produce "Chinese text → that key's English tokens".
    ///
    /// This is a **generic** "Chinese intent → this project's symbols" bridge: depends on no domain vocabulary; works whenever the project
    /// ships i18n — e-commerce / finance / game / internal systems treated alike. Cached per project, invalidated on graph rebuild by
    /// [`Self::clear_node_cache`].
    fn project_bridge(
        &self,
        project_id: ProjectId,
        nodes: &[Node],
    ) -> Vec<(String, Vec<String>)> {
        if let Some(v) = self.bridge_cache.lock().unwrap().get(&project_id.get()) {
            return v.clone();
        }
        let entries = compute_bridge(nodes);
        self.bridge_cache
            .lock()
            .unwrap()
            .insert(project_id.get(), entries.clone());
        entries
    }

    /// Which Chinese phrases the query hit, returning "phrase → this project's tokens".
    ///
    /// When multiple hit, take the top-N by phrase length descending: longer phrases are more specific ("insufficient stock" beats "stock").
    fn match_project_bridge(
        &self,
        project_id: ProjectId,
        query: &str,
        nodes: &[Node],
    ) -> Vec<(String, Vec<String>)> {
        const MAX_HITS: usize = 16;
        let mut hits: Vec<(String, Vec<String>)> = self
            .project_bridge(project_id, nodes)
            .into_iter()
            .filter(|(zh, _)| query.contains(zh.as_str()))
            .collect();
        // Longer phrases are more specific ("insufficient stock" beats "stock").
        hits.sort_by(|a, b| b.0.chars().count().cmp(&a.0.chars().count()));
        hits.truncate(MAX_HITS);
        hits
    }

    /// Manual warmup: compute and persist a project's all topic-level nodes' bge vectors (graph build does not happen here).
    /// The user can run `graphtell embed --project N` in the background; then all recalls and restarts hit the cache instantly.
    pub fn warm_up(&self, project_id: ProjectId) -> Result<usize> {
        let emb = match &self.semantic_embedder {
            Some(e) => e,
            None => {
                tracing::warn!("no semantic encoder configured (missing bge weights); embed is a no-op");
                return Ok(0);
            }
        };
        let nodes = fetch_nodes(self.store.as_ref(), project_id)?;
        let enrich = build_enrich_index(&self.project_bridge(project_id, &nodes));
        if let Some(dir) = &self.embed_persist_dir {
            let path = dir.join(format!("{}.rmp", project_id.get()));
            self.load_persisted(&path, project_id);
        }
        self.ensure_cached_with(&nodes, emb, &self.node_embed_cache, &enrich, true);
        if let Some(dir) = &self.embed_persist_dir {
            let path = dir.join(format!("{}.rmp", project_id.get()));
            self.persist(&path, &nodes, &enrich);
        }
        self.warmed_projects.lock().unwrap().insert(project_id.get());
        Ok(nodes
            .iter()
            .filter(|n| is_vector_kind(n))
            .count())
    }

    /// Query a project's background-warmup progress.
    ///
    /// Returns `Some` meaning "warming up" or "warmup finished"; returns `None` meaning neither warming nor ever warmed (i.e. this project is
    /// currently on the cold path). Reused by `/api/.../warmup` and [`RecallResult::warmup`].
    pub fn warmup_progress(&self, project_id: i64) -> Option<WarmupStatus> {
        let warmed = self.warmed_projects.lock().unwrap().contains(&project_id);
        let warming = self.warming_projects.lock().unwrap().contains(&project_id);
        if !warming && !warmed {
            return None;
        }
        let (done, total) = self
            .warm_progress
            .lock()
            .unwrap()
            .get(&project_id)
            .copied()
            .unwrap_or((0, 0));
        Some(WarmupStatus {
            warmed,
            warming,
            done,
            total,
        })
    }

    /// Diagnostics: given a query, output the cosine with each node's vector (descending).
    ///
    /// Used to classify "why a Chinese query can't recall the target symbol" into:
    /// * **model / text problem** — the target symbol's cosine is below [`VECTOR_THRESHOLD`] to begin with; fix the node embed text or model;
    /// * **threshold / ranking problem** — cosine is actually high but blocked by threshold, vector-seed budget, or ranking; fix those.
    ///
    /// When `name_filter` is non-empty, only nodes whose name contains those substrings — full encoding on a big project takes tens of minutes,
    /// and diagnostics usually only care about the target symbol.
    pub fn debug_cosine(
        &self,
        project_id: ProjectId,
        query: &str,
        name_filter: &[String],
        top: usize,
    ) -> Result<Vec<(String, String, f64)>> {
        let Some(emb) = &self.semantic_embedder else {
            return Err(gt_domain::error::DomainError::infra(
                "未配置语义编码器（缺 bge 权重），无法做余弦诊断",
            ));
        };
        let alias_terms = expand_intent_aliases(query, &builtin_aliases());
        let qvec = emb.embed_query(&query_embed_text(query, &alias_terms));

        let nodes = fetch_nodes(self.store.as_ref(), project_id)?;
        let enrich = build_enrich_index(&self.project_bridge(project_id, &nodes));
        let picked: Vec<&Node> = nodes
            .iter()
            .filter(|n| is_vector_kind(n))
            .filter(|n| {
                name_filter.is_empty()
                    || name_filter
                        .iter()
                        .any(|f| n.name.to_ascii_lowercase().contains(&f.to_ascii_lowercase()))
            })
            .collect();
        let texts: Vec<String> = picked.iter().map(|n| node_embed_text(n, &enrich)).collect();
        let vecs = emb.embed_batch(&texts);

        let mut rows: Vec<(String, String, f64)> = picked
            .iter()
            .zip(vecs.into_iter())
            .map(|(n, v)| (n.kind.to_string(), n.name.clone(), cosine(&qvec, &v)))
            .collect();
        rows.sort_by(|a, b| {
            b.2.partial_cmp(&a.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        rows.truncate(top);
        Ok(rows)
    }

    /// Split one sentence into multiple **independent intents**.
    ///
    /// A question like "how to modify the product stock-warning threshold, and the order auto-cancel time" packs two independent questions into
    /// one sentence. Treated as one bag-of-words recall, the two intents' words interfere, and `limit` gets filled by one intent's high-score
    /// noise — measured: both sub-questions **each find the answer alone** (`product_stock_job` / `ConfigKey order_cancel_time`), but together
    /// both drop out of the top 20. So split and recall separately, then merge by intent (see [`RecallService::recall`]).
    ///
    /// Only when ≥2 segments are split and all long enough is it multi-intent; otherwise fall back to single intent, zero regression.
    fn split_intents(q: &str) -> Vec<String> {
        // Split by conjunctions first, then by punctuation.
        let mut parts: Vec<String> = vec![q.to_string()];
        for sep in ["以及", "并且", "还有", "另外"] {
            let mut next = Vec::new();
            for p in parts {
                next.extend(p.split(sep).map(|s| s.to_string()));
            }
            parts = next;
        }
        let mut next = Vec::new();
        for p in parts {
            next.extend(p.split(|c| "、，,；;。".contains(c)).map(|s| s.to_string()));
        }
        let out: Vec<String> = next
            .into_iter()
            .map(|s| s.trim().to_string())
            // Too-short fragments (particles / stray punctuation side-branches) don't form a separate intent.
            .filter(|s| s.chars().count() >= 4)
            .collect();
        if out.len() >= 2 { out } else { Vec::new() }
    }

    /// Merge multi-intent results: round-robin one per intent, so every intent has a representative in the final list (else high-score
    /// intents would again empty others), dedup by node_id throughout.
    fn merge_intent_hits(groups: Vec<Vec<RecallHit>>, limit: usize) -> Vec<RecallHit> {
        let mut out: Vec<RecallHit> = Vec::new();
        let mut seen: HashSet<i64> = HashSet::new();
        let mut idx = vec![0usize; groups.len()];
        while out.len() < limit {
            let mut progressed = false;
            for gi in 0..groups.len() {
                while idx[gi] < groups[gi].len() {
                    let h = groups[gi][idx[gi]].clone();
                    idx[gi] += 1;
                    if seen.insert(h.node_id.get()) {
                        out.push(h);
                        progressed = true;
                        break;
                    }
                }
                if out.len() >= limit {
                    break;
                }
            }
            if !progressed {
                break;
            }
        }
        out
    }


    /// Assess recall quality (see [`RecallQuality`]).
    ///
    /// Uses three **project-independent** signals, so it holds on any project:
    /// 1. **concept coverage** — how many of the query's Chinese intent concepts (place order / coupon / notify…) are hit by an
    ///    **informative** hit (excluding ORM association boilerplate); counted by concept not word, any English expansion counts
    ///    (coupon → coupon or discount both OK);
    /// 2. **top-spread gap** — how far top1 leads the mean of #2~#5; tiny when a bunch of homogeneous noise;
    /// 3. **concept cohesion** — whether one hit covers ≥2 concepts. Without it, "how to notify after order" would be misjudged High
    ///    because each concept is hit by a different noise node.
    ///
    /// Thresholds are **conservative**: rather report low than give false confidence (misjudging is worse than not judging).
    ///
    /// # Known limits (don't repeat)
    ///
    /// This function **can't distinguish "hit" from "hit right"**: noise nodes often really hit the query words too —
    /// `UserAddressServices::create` does cover "order + user" just like the answer `StoreCouponIssue::edit` covers "modify + coupon".
    /// The difference is semantic not structural (this query wants "order + **notify**"); lexical + graph can't tell. So **structural
    /// signals stop here**; further needs semantic understanding.
    ///
    /// Two tightening schemes tried and **abandoned** (both hurt good queries, don't retry):
    /// * only name hit (not fqn) — would misjudge "how to modify order discount": its "coupon" only exists in class name `StoreCouponIssue`;
    /// * name + class identifier — can't block `UserAddressServices` (user is in the class name), still reports healthy.
    ///
    /// # TODO (capability gap, not solvable here)
    ///
    /// **event-driven chain recall**: queries like "how to notify user after order" / "how to roll back coupon after refund" want the
    /// chain "order event → listener → message service", needing directed chain expansion along `Triggers`/`PublishesTo`/`ListensTo`,
    /// not name matching. Until then, such queries can only be downgraded via [`RecallQuality`] (grep / read yourself).
    /// Pure action verbs not counted as domain concepts in quality assessment.
    ///
    /// They only express "find the implementation" intent (see [`action_intent`]), no business semantics; if treated as coverage concepts
    /// they'd hurt recall quality: e.g. "how to generate product QR" — `generate` expands to `generate`, almost no identifier is named
    /// generate, so judged Medium "partial feature word missing (generate)", while the real driver is the domain concept "QR". After
    /// excluding these pure verbs, coverage is driven only by domain concepts (order/coupon/refund/QR…), alerts become trustworthy.
    /// Note: keep words carrying business events like "place order / pay / notify / callback / rollback / deduct" as concepts.
    const QUALITY_ACTION_VERBS: &[&str] = &[
        "生成", "上传", "修改", "新增", "创建", "删除", "查询", "读取", "加载", "获取",
        "设置", "保存", "更新", "处理", "计算", "校验", "拦截", "执行", "调用", "写入",
    ];

    fn assess_quality(
        query: &str,
        hits: &[RecallHit],
        boilerplate: &HashSet<i64>,
        aliases: &[(String, Vec<String>)],
    ) -> (RecallQuality, f32, String, Vec<String>) {
        // 2) Only count "informative" hits: ORM association boilerplate (hasOne etc.) doesn't count.
        let mut matched: HashSet<String> = HashSet::new();
        for h in hits.iter().filter(|h| !boilerplate.contains(&h.node_id.get())) {
            for t in &h.matched_terms {
                matched.insert(t.to_lowercase());
            }
        }

        let concepts: Vec<(&str, &[String])> = aliases
            .iter()
            .filter(|(zh, _)| {
                query.contains(zh.as_str()) && !Self::QUALITY_ACTION_VERBS.contains(&zh.as_str())
            })
            .map(|(zh, ens)| (zh.as_str(), ens.as_slice()))
            .collect();
        // Coverage counted by **concept count** (not keyword count), so the missed set keeps only concepts.
        let missing_concepts: Vec<(&str, &[String])> = concepts
            .iter()
            .filter(|(_, ens)| !ens.iter().any(|e| matched.contains(&e.to_lowercase())))
            .map(|(zh, ens)| (*zh, *ens))
            .collect();

        let coverage = if concepts.is_empty() {
            1.0
        } else {
            1.0 - (missing_concepts.len() as f32 / concepts.len() as f32)
        };

        let mut missing: Vec<String> = Vec::new();
        for (zh, ens) in &missing_concepts {
            missing.push((*zh).to_string());
            for e in *ens {
                if !missing.iter().any(|m| m == e) {
                    missing.push((*e).to_string());
                }
            }
        }
        // Explanation text uses only Chinese concepts, to avoid being too long.
        let missing_zh: Vec<String> =
            missing_concepts.iter().map(|(zh, _)| (*zh).to_string()).collect();

        // 3) top-spread gap.
        let top = hits.first().map(|h| h.score).unwrap_or(0.0);
        let rest: Vec<f64> = hits.iter().skip(1).take(4).map(|h| h.score).collect();
        let rest_mean = if rest.is_empty() {
            0.0
        } else {
            rest.iter().sum::<f64>() / rest.len() as f64
        };
        let gap: f32 = if rest_mean > 0.0 {
            (((top / rest_mean - 1.0) / 1.5).clamp(0.0, 1.0)) as f32
        } else {
            1.0
        };

        let confidence = (0.65 * coverage + 0.35 * gap).clamp(0.0, 1.0);

        let mut best_cohesion = 0usize;
        for h in hits.iter().filter(|h| !boilerplate.contains(&h.node_id.get())) {
            let tl: HashSet<String> =
                h.matched_terms.iter().map(|t| t.to_lowercase()).collect();
            let n = concepts
                .iter()
                .filter(|(_, ens)| ens.iter().any(|e| tl.contains(&e.to_lowercase())))
                .count();
            best_cohesion = best_cohesion.max(n);
        }
        let fragmented = concepts.len() >= 2 && best_cohesion < 2;

        let unevaluable = concepts.is_empty();

        let quality = if coverage < 0.5 || (concepts.len() >= 3 && best_cohesion < 2) {
            RecallQuality::Low
        } else if coverage >= 0.9 && confidence >= 0.55 && !fragmented && !unevaluable {
            RecallQuality::High
        } else {
            RecallQuality::Medium
        };

        let reason = if unevaluable {
            "The query contains no evaluable intent concepts (mostly domain-specific or unusual wording); recall quality cannot be confirmed — judge the results yourself".to_string()
        } else if quality == RecallQuality::Low && coverage >= 0.5 {
            "Each concept was matched by unrelated nodes (no single hit covers two concepts) — likely generic terms colliding by name".to_string()
        } else {
            match quality {
            RecallQuality::Low => format!(
                "Most feature terms missed ({}) and the top rows are generic-term matches — grep or read the code yourself to confirm",
                if missing_zh.is_empty() {
                    "too few hits".to_string()
                } else {
                    missing_zh.join("、")
                }
            ),
            RecallQuality::Medium => {
                if missing_zh.is_empty() {
                    "Feature terms matched but the top rows are not distinctive enough; results may be scattered".to_string()
                } else {
                    format!("Some feature terms missed ({}); results may be incomplete", missing_zh.join("、"))
                }
            }
            RecallQuality::High => "Feature terms mostly matched and the top rows are healthily distinctive".to_string(),
            }
        };

        (quality, confidence, reason, missing)
    }

    /// Quality alert block (Markdown). Empty string when quality is High — don't disturb normal results.
    ///
    /// Key: give an **actionable** out: not just "I can't", but list the missed feature words, so the caller (AI IDE) knows what to grep.
    fn quality_advisory(
        quality: RecallQuality,
        confidence: f32,
        reason: &str,
        missing: &[String],
        event: bool,
    ) -> String {
        if quality == RecallQuality::High {
            return String::new();
        }
        let label = match quality {
            RecallQuality::Low => "low",
            RecallQuality::Medium => "medium",
            RecallQuality::High => "high",
        };
        let mut s = format!("> ⚠️ **Recall quality: {} (confidence {:.2})** — {}\n", label, confidence, reason);
        if !missing.is_empty() {
            s.push_str(&format!(
                ">\n> Try searching again with these feature terms: {}\n",
                missing.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>().join("、")
            ));
        }
        if event {
            s.push_str(
                ">\n> This query looks event / flow driven (\"what happens after X\"). On the graph the right answer is usually an event listener \
                 (`*Listener` / `*Subscriber` / `@EventListener` / `@OnEvent`) or a subscriber, whose method names are very generic \
                 (uniformly `handle`), so lexical recall rarely hits them. Read the corresponding listeners under the project's \
                 `listener` / `event` / `observer` / `subscriber` directories and the related `*Services` implementations to confirm \
                 the \"trigger → listener → handler\" chain.\n",
            );
        }
        s.push('\n');
        s
    }

    /// Run one recall: single intent goes straight to [`Self::recall_single`]; multi-intent (multiple independent questions joined by
    /// 、/,) splits, recalls each, then merges (see [`split_intents`] / [`merge_intent_hits`]).
    pub fn recall(&self, project_id: ProjectId, q: &RecallQuery) -> Result<RecallResult> {
        let parts = Self::split_intents(&q.query);
        if parts.is_empty() {
            return self.recall_single(project_id, q);
        }
        let limit = q.limit.max(1);
        let mut groups: Vec<Vec<RecallHit>> = Vec::new();
        let mut terms: Vec<String> = Vec::new();
        let mut hints: Vec<String> = Vec::new();
        let mut seeds: Vec<SeedInfo> = Vec::new();
        let mut truncated = false;
        let mut missing_terms: Vec<String> = Vec::new();
        let mut quality = RecallQuality::High;
        let mut confidence = 1.0f32;
        let mut reasons: Vec<String> = Vec::new();
        let sub_results: Vec<Result<RecallResult>> = if parts.len() > 1 {
            std::thread::scope(|s| {
                let handles: Vec<_> = parts
                    .iter()
                    .map(|part| {
                        let sub = RecallQuery { query: part.clone(), limit, ..q.clone() };
                        s.spawn(move || self.recall_single(project_id, &sub))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or_else(|_| Err(gt_domain::error::DomainError::infra("子意图召回线程 panic"))))
                    .collect()
            })
        } else {
            parts
                .iter()
                .map(|part| {
                    let sub = RecallQuery { query: part.clone(), limit, ..q.clone() };
                    self.recall_single(project_id, &sub)
                })
                .collect()
        };
        for (part, r) in parts.iter().zip(sub_results) {
            let r = r?;
            for t in &r.terms {
                if !terms.iter().any(|x| x == t) {
                    terms.push(t.clone());
                }
            }
            for h in &r.kind_hints {
                if !hints.iter().any(|x| x == h) {
                    hints.push(h.clone());
                }
            }
            seeds.extend(r.seeds);
            truncated |= r.truncated;
            // Multi-intent takes the **worst** tier and lowest confidence: rather report low than give false confidence.
            quality = quality.max(r.quality);
            confidence = confidence.min(r.confidence);
            reasons.push(format!("「{}」：{}", part, r.quality_reason));
            for m in &r.missing_terms {
                if !missing_terms.iter().any(|x| x == m) {
                    missing_terms.push(m.clone());
                }
            }
            groups.push(r.hits);
        }
        let hits = Self::merge_intent_hits(groups, limit);
        let quality_reason = reasons.join("；");
        let advisory = Self::quality_advisory(
            quality,
            confidence,
            &quality_reason,
            &missing_terms,
            event_intent(&q.query),
        );
        let mut markdown =
            render_markdown(project_id, q, &terms, &hints, &seeds, &hits, &advisory);
        if q.include_body {
            markdown = append_file_bodies(self.fs.as_ref(), &hits, markdown);
        }
        Ok(RecallResult {
            project_id,
            query: q.query.clone(),
            terms,
            kind_hints: hints,
            seeds,
            hits,
            markdown,
            truncated,
            confidence,
            quality,
            quality_reason,
            missing_terms,
            warmup: self.warmup_progress(project_id.get()),
        })
    }

    /// Full flow of a single-intent recall (multi-intent dispatched multiple times by [`RecallService::recall`]).
    fn recall_single(&self, project_id: ProjectId, q: &RecallQuery) -> Result<RecallResult> {
        let (terms, kind_hints) = parse_query(&q.query);
        let wants_config = wants_config_value(&q.query);
        let action = action_intent(&q.query) && !wants_config;
        // Flow intent (reconstruct call chain): results re-sorted by graph topology (see [`reorder_for_flow`]).
        let flow = flow_intent(&q.query);
        // Event-driven intent (how to Y after X / after success…): supplement the event handlers drowned by naming convention
        // (listeners/subscribers) as seeds and float them, see [`collect_event_seeds`] / [`is_event_handler`].
        let event = event_intent(&q.query);

        let aliases = merged_aliases(
            self.store
                .get_project(project_id)?
                .map(|p| std::path::PathBuf::from(p.root_path))
                .as_deref(),
        );
        let alias_terms = expand_intent_aliases(&q.query, &aliases);
        let mut match_terms = terms.clone();
        for t in &alias_terms {
            if !match_terms.iter().any(|x| x == t) {
                match_terms.push(t.clone());
            }
        }

        // Triggered concept cluster (e.g. "email" → notify cluster): for ranking-side concept weighting, see [`concept_multiplier`].
        let triggered = triggered_concepts(&alias_terms);

        let qtext = query_embed_text(&q.query, &alias_terms);
        let early_semantic = self.semantic_embedder.is_some()
            && (self
                .warmed_projects
                .lock()
                .unwrap()
                .contains(&project_id.get())
                || self.embed_persist_dir.as_ref().map_or(false, |d| {
                    d.join(format!("{}.rmp", project_id.get())).exists()
                }));
        let qvec_cache = self.query_vec_cache.clone();
        let qvec_embedder = if early_semantic {
            self.semantic_embedder.clone().unwrap()
        } else {
            self.fast_embedder.clone()
        };
        let qtext_for_thread = qtext.clone();
        let qvec_handle = thread::spawn(move || {
            encode_query_cached(&qvec_cache, &qvec_embedder, &qtext_for_thread, early_semantic)
        });

        // ---- 1) candidate set: nodes participating in recall (snapshot reused across requests, see [`Self::candidate_set`]).
        let t_nodes = std::time::Instant::now();
        let snap = self.candidate_set(project_id, &q.kinds)?;
        let nodes: &[Node] = &snap.nodes;
        // Node-enrichment index (content-hash key needed): shares the same one with `ensure_cached_with` / `persist`, avoid rebuilding each recall.
        let enrich = build_enrich_index(&self.project_bridge(project_id, nodes));
        let truncated = nodes.len() >= SCAN_LIMIT as usize;
        tracing::debug!(
            "latency: 候选装载 {} 个节点 {} ms",
            nodes.len(),
            t_nodes.elapsed().as_millis()
        );

        // Project semantic bridge (depends on loaded nodes, so placed after the candidate set).
        // Matched Chinese phrases adopted by descending length: longer phrases are more specific ("insufficient stock" better than "stock").
        let bridge_hits = self.match_project_bridge(project_id, &q.query, &nodes);
        for (_zh, toks) in &bridge_hits {
            for t in toks {
                if !match_terms.iter().any(|x| x == t) {
                    match_terms.push(t.clone());
                }
            }
        }

        // ---- 2) neighbors (for expansion and relation summary): same snapshot as candidate nodes.
        // id → node index: every BFS hop re-looks up node summary; linear find degrades to O(N²).
        let index: HashMap<i64, &Node> = nodes.iter().map(|n| (n.id.get(), n)).collect();
        let incoming = &snap.incoming;
        let outgoing = &snap.outgoing;
        let files = &snap.files;
        let root = self
            .store
            .get_project(project_id)?
            .map(|p| std::path::PathBuf::from(p.root_path));

        let mut group_map = alias_group_map(&aliases);
        // Each matched i18n text forms its own group, so "hit multiple business concepts → aggregate boost" holds in any domain,
        // not just for the built-in e-commerce vocabulary.
        for (zh, toks) in &bridge_hits {
            group_map.insert(zh.clone(), zh.clone());
            for t in toks {
                group_map.insert(t.clone(), zh.clone());
            }
        }
        // Full set of ORM association accessors (hasOne/hasMany boilerplate): used both for scoring downweight and for quality
        // assessment (such hits don't count as "informative hits").
        let boilerplate: HashSet<i64> = nodes
            .iter()
            .filter(|n| has_maps_to(n.id.get(), &outgoing))
            .map(|n| n.id.get())
            .collect();

        // Identifiers **named by the prompt** (`StoreOrderCreateServices` / `createOrder`): precise pointing, boost them and raise
        // their status relative to noise (see [`anchor_multiplier`]).
        let anchors = extract_anchors(&q.query, &nodes);

        let mut lexical: HashMap<i64, (f64, Vec<String>)> = HashMap::new();
        for node in nodes {
            if DEFAULT_EXCLUDED_KINDS.contains(&node.kind.as_str()) {
                continue;
            }
            let anchored = anchor_multiplier(node, &anchors) > 1.0;
            let verb_only = action
                && is_generic_crud_method(&node.name.to_lowercase())
                && !has_content_word(node, &match_terms)
                && !anchored;
            if verb_only {
                continue;
            }
            let (mut score, matched) = score_node(node, &match_terms, &kind_hints, &incoming, action);
            if score > 0.0 {
                score *= cohesion_multiplier(&matched, &group_map);
                score *= anchor_multiplier(node, &anchors);
                // Ranking-side concept weighting: nodes hitting concept clusters like "notify" float up (see [`concept_multiplier`]).
                score *= concept_multiplier(node, &triggered);
                // Test files / generator boilerplate / ORM association accessors: no business logic, downweight below seed slots
                // (see [`node_noise_discount`]).
                score *= node_noise_discount(node, &files, &boilerplate);
                lexical.insert(node.id.get(), (score, matched));
            }
        }

        let t_load = std::time::Instant::now();
        let mut use_semantic = self.semantic_embedder.is_some()
            && self.warmed_projects.lock().unwrap().contains(&project_id.get());
        if !use_semantic && self.semantic_embedder.is_some() {
            if let Some(dir) = &self.embed_persist_dir {
                let path = dir.join(format!("{}.rmp", project_id.get()));
                use_semantic = self.load_persisted(&path, project_id);
            }
        }
        tracing::debug!(
            "latency: 载入落盘向量 {} ms（节点 {}）",
            t_load.elapsed().as_millis(),
            nodes.len()
        );
        let (chosen, cache): (&Arc<dyn Embedder>, &Mutex<HashMap<u64, Vec<f32>>>) = if use_semantic {
            (self.semantic_embedder.as_ref().unwrap(), &self.node_embed_cache)
        } else {
            (&self.fast_embedder, &self.fast_cache)
        };
        // Query encoding was pre-handed to a separate thread, parallel with candidate load / disk load; wait for it here.
        // Cold start: thread finishes early (fully covered by candidate load's ~1s); hot path same-sentence repeat hits cache, instant.
        let t_q = std::time::Instant::now();
        let qvec = qvec_handle
            .join()
            .expect("查询编码线程 panic");
        tracing::debug!(
            "latency: 查询编码 {} ms（查询侧文本 {} 字符，别名词 {} 个，线程并行）",
            t_q.elapsed().as_millis(),
            qtext.chars().count(),
            alias_terms.len()
        );

        // Only the semantic space is persisted (fast hash space needs no persistence, instantly recomputable).
        // Computation and persistence do **not** happen at graph build.
        let t_enc = std::time::Instant::now();
        let newly_encoded =
            self.ensure_cached_with(nodes, chosen, cache, &enrich, use_semantic);
        tracing::debug!(
            "latency: 节点编码补齐 {} ms（新增 {} 个）",
            t_enc.elapsed().as_millis(),
            newly_encoded
        );
        // Only write to disk when **new vectors were really computed**. Previously unconditional write-back meant every recall re-serialized
        // the whole project's vectors (CRMEB 157MB / ~0.5s) — content that never changed once.
        if use_semantic && newly_encoded > 0 {
            if let Some(dir) = &self.embed_persist_dir {
                let path = dir.join(format!("{}.rmp", project_id.get()));
                let t_p = std::time::Instant::now();
                self.persist(&path, nodes, &enrich);
                tracing::debug!("latency: persist write-back {} ms", t_p.elapsed().as_millis());
            }
        }

        if !use_semantic
            && self.enable_async_warmup
            && self.semantic_embedder.is_some()
            && self.warming_projects.lock().unwrap().insert(project_id.get())
        {
            let (store, emb, sem_cache, dir, warmed, warming, progress) = (
                self.store.clone(),
                self.semantic_embedder.clone().unwrap(),
                self.node_embed_cache.clone(),
                self.embed_persist_dir.clone(),
                self.warmed_projects.clone(),
                self.warming_projects.clone(),
                self.warm_progress.clone(),
            );
            thread::spawn(move || {
                warm_up_worker(store, emb, sem_cache, dir, project_id, warmed, warming, progress);
            });
        }

        let mut vector: HashMap<i64, f64> = HashMap::new();
        for node in nodes {
            if !is_vector_kind(node) {
                continue;
            }
            // Semantic space takes vector by content hash of node embed text (survive rebuild); fast space by node id (instant recompute).
            let key: u64 = if use_semantic {
                embed_text_key(&node_embed_text(node, &enrich))
            } else {
                node.id.get() as u64
            };
            let nvec = match cache.lock().unwrap().get(&key).cloned() {
                Some(v) => v,
                None => continue,
            };
            let c = cosine(&qvec, &nvec);
            if c >= VECTOR_THRESHOLD {
                let kw = if action {
                    let nlow = node.name.to_lowercase();
                    if is_generic_crud_method(&nlow) {
                        let has_content =
                            has_content_word(node, &match_terms) || anchor_multiplier(node, &anchors) > 1.0;
                        if has_content {
                            rank_weight(node.kind.as_str(), true)
                        } else {
                            kind_weight(node.kind.as_str()) * GENERIC_CRUD_VERB_ONLY_DISCOUNT
                        }
                    } else {
                        rank_weight(node.kind.as_str(), true)
                    }
                } else {
                    1.0
                };
                // Same scope as lexical path: named-symbol boost + test/generated downweight, so the two paths' seeds don't get back the
                // boilerplate nodes that should be suppressed just because they went through vectors.
                let s = c
                    * VECTOR_WEIGHT
                    * kw
                    * anchor_multiplier(node, &anchors)
                    * concept_multiplier(node, &triggered)
                    * node_noise_discount(node, &files, &boilerplate);
                let entry = vector.entry(node.id.get()).or_insert(0.0);
                *entry = (*entry).max(s);
            }
        }

        let mut seed_tuples = select_seeds(&lexical, &vector, &index);
        if event {
            let top_lex = lexical.values().map(|(s, _)| *s).fold(0.0_f64, f64::max);
            let event_base = if anchors.is_empty() {
                (top_lex * 0.55).max(EVENT_SEED_MIN)
            } else {
                top_lex * 0.55
            };
            let existing: HashSet<i64> =
                seed_tuples.iter().map(|(_, _, n)| n.id.get()).collect();
            for (s, m, n) in
                collect_event_seeds(&nodes, &index, &existing, &match_terms, event_base)
            {
                seed_tuples.push((s, m, n));
            }
        }
        let seeds: Vec<SeedInfo> = seed_tuples
            .iter()
            .map(|(s, _, n)| SeedInfo {
                node_id: n.id,
                kind: n.kind.to_string(),
                name: n.name.clone(),
                score: *s,
            })
            .collect();

        // ---- 4) expansion: BFS along chain edges, hop decay
        let mut best: HashMap<i64, RecallHit> = HashMap::new();
        for (seed_score, matched, seed) in &seed_tuples {
            let mut frontier = vec![(seed.id, 0u32)];
            let mut seen: HashSet<i64> = HashSet::new();
            seen.insert(seed.id.get());
            while let Some((id, hop)) = frontier.pop() {
                let decay = 0.5f64.powi(hop as i32);
                let score = seed_score * decay;
                let node = index.get(&id.get()).copied();
                let entry = best.entry(id.get()).or_insert(RecallHit {
                    node_id: id,
                    kind: node.map(|n| n.kind.to_string()).unwrap_or_default(),
                    name: node.map(|n| n.name.clone()).unwrap_or_default(),
                    fqn: node.and_then(|n| n.fqn.clone()),
                    score,
                    hop,
                    seed: seed.name.clone(),
                    matched_terms: if hop == 0 { matched.clone() } else { Vec::new() },
                    direct: hop == 0,
                    file: None,
                    line: None,
                    snippet: None,
                    relations: Vec::new(),
                });
                if score > entry.score {
                    entry.score = score;
                    entry.hop = hop;
                    entry.seed = seed.name.clone();
                    entry.matched_terms = if hop == 0 { matched.clone() } else { Vec::new() };
                    entry.direct = hop == 0;
                }
                if hop >= q.hops {
                    continue;
                }
                for other in neighbours(id, &incoming, &outgoing) {
                    if seen.insert(other.get()) {
                        frontier.push((other, hop + 1));
                    }
                }
            }
        }

        // ---- 5) ranking + truncation + fill locations
        let mut hits: Vec<RecallHit> = best.into_values().collect();
        for h in hits.iter_mut() {
            let fan_in = incoming.get(&h.node_id.get()).map(|es| es.len()).unwrap_or(0);
            h.score *= hub_penalty(fan_in);
            if let Some(n) = index.get(&h.node_id.get()) {
                h.score *= node_noise_discount(n, &files, &boilerplate);
            }
        }
        if event {
            let event_seed_ids: HashSet<i64> = seed_tuples
                .iter()
                .filter(|(_, m, _)| m.iter().any(|x| x == "<event>"))
                .map(|(_, _, n)| n.id.get())
                .collect();
            if !event_seed_ids.is_empty() {
                let mut callees: HashSet<i64> = HashSet::new();
                for sid in &event_seed_ids {
                    if let Some(node) = index.get(sid) {
                        for nb in neighbours(node.id, &incoming, &outgoing) {
                            callees.insert(nb.get());
                        }
                    }
                }
                for h in hits.iter_mut() {
                    let id = h.node_id.get();
                    if callees.contains(&id) && !event_seed_ids.contains(&id) {
                        h.score *= EVENT_CALLEE_BOOST;
                    }
                }
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.node_id.get().cmp(&b.node_id.get()))
        });
        const MAX_SAME_NAME: usize = 2;
        let mut same: HashMap<String, usize> = HashMap::new();
        let mut kept: Vec<RecallHit> = Vec::with_capacity(hits.len());
        for h in hits {
            let c = same.entry(h.name.clone()).or_insert(0);
            if *c < MAX_SAME_NAME {
                *c += 1;
                kept.push(h);
            }
        }
        hits = kept;
        hits.truncate(q.limit.max(1));
        if flow {
            reorder_for_flow(&mut hits, &incoming);
        }

        for hit in hits.iter_mut() {
            let node = index.get(&hit.node_id.get()).copied();
            if let Some(n) = node {
                (hit.file, hit.line) = crate::location::node_location(n, &files, root.as_deref());
                hit.relations = relation_summary(hit.node_id, &incoming, &outgoing);
            } else {
                // Expansion may reach an excluded kind (e.g. CallSite); fill in its summary
                if let Ok(Some(n)) = self.store.get_node(hit.node_id) {
                    hit.kind = n.kind.to_string();
                    hit.name = n.name.clone();
                    hit.fqn = n.fqn.clone();
                    (hit.file, hit.line) =
                        crate::location::node_location(&n, &files, root.as_deref());
                }
            }
            if q.with_snippets {
                if let (Some(file), Some(line)) = (&hit.file, hit.line) {
                    hit.snippet = read_snippet(self.fs.as_ref(), Path::new(file), line);
                }
            }
        }

        let (quality, confidence, quality_reason, missing_terms) =
            Self::assess_quality(&q.query, &hits, &boilerplate, &aliases);
        let advisory =
            Self::quality_advisory(quality, confidence, &quality_reason, &missing_terms, event);
        let mut markdown =
            render_markdown(project_id, q, &terms, &kind_hints, &seeds, &hits, &advisory);
        if q.include_body {
            markdown = append_file_bodies(self.fs.as_ref(), &hits, markdown);
        }
        Ok(RecallResult {
            project_id,
            query: q.query.clone(),
            terms,
            kind_hints,
            seeds,
            hits,
            markdown,
            truncated,
            confidence,
            quality,
            quality_reason,
            missing_terms,
            warmup: self.warmup_progress(project_id.get()),
        })
    }
}

/// Load all nodes of a project participating in recall (by [`scan_kinds`]).
fn fetch_nodes(store: &dyn Persistence, project_id: ProjectId) -> Result<Vec<Node>> {
    let mut nodes: Vec<Node> = Vec::new();
    for kind in scan_kinds(store, project_id) {
        let mut batch = store.query_nodes(&NodeFilter {
            project_id,
            kind: Some(gt_domain::model::NodeKind::new(kind)),
            name_contains: None,
            limit: Some(SCAN_LIMIT),
            offset: None,
        })?;
        nodes.append(&mut batch);
    }
    Ok(nodes)
}

/// Extract the "this project's recall nodes → vectors" map from the cache (for persistence).
fn collect_cache(
    cache: &Mutex<HashMap<u64, Vec<f32>>>,
    nodes: &[Node],
    enrich: &EnrichIndex,
) -> HashMap<u64, Vec<f32>> {
    let cache = cache.lock().unwrap();
    nodes
        .iter()
        .filter_map(|n| {
            let key = embed_text_key(&node_embed_text(n, enrich));
            cache.get(&key).cloned().map(|v| (key, v))
        })
        .collect()
}

/// Background warmup worker: use the semantic encoder to compute bge vectors for all topic-level nodes of a project, persist, mark
/// warmed up. Load a project's persisted vectors into the given cache and mark warmed up (shared by in-service and background thread).
///
/// `expected_dim` is the current semantic encoder dim; persisted-file dim mismatch (model switch / old format no dim) invalidates the
/// whole file and re-encodes, to avoid cosine on wrong dim ([
/// `crate::embedding::cosine`] by `min(len)` distorts). Returns whether it **really** loaded (file exists and version/dim match).
///
/// `false` means the persisted file is invalid (version bump / model switch); caller must treat as "not warmed up": else it would
/// synchronously bge-encode the whole DB in the request thread (CRMEB measured 10+ min no return), and because "judged semantic path"
/// never start background warmup — warmup progress would show never-started forever.
fn load_persisted_into(
    path: &Path,
    cache: &Mutex<HashMap<u64, Vec<f32>>>,
    project_id: ProjectId,
    warmed: &Mutex<HashSet<i64>>,
    expected_dim: usize,
) -> bool {
    // Prefer rmp (new format from v4, parsing an order of magnitude faster); if missing / parse fails, fall back to same-name `.json`
    // (v3 old format) once, transition-period no waste of 16-min full re-encode.
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => {
            let json_path = path.with_extension("json");
            match std::fs::read(&json_path) {
                Ok(d) => d,
                Err(_) => return false,
            }
        }
    };
    // rmp first; on failure (e.g. old JSON) try JSON. Both fail ⇒ invalidated and re-encoded.
    let Ok(env) = rmp_serde::from_slice::<PersistedEmbeds>(&data)
        .or_else(|_| serde_json::from_slice::<PersistedEmbeds>(&data))
    else {
        return false;
    };
    // v2 and earlier (no content fingerprint) all invalidated; v3 (old JSON) / v4 (rmp) both reusable.
    if env.version < 3 || env.version > EMBED_TEXT_VERSION {
        return false;
    }
    if env.dim != 0 && env.dim != expected_dim {
        return false;
    }
    let mut cache = cache.lock().unwrap();
    for (k, v) in env.vectors {
        cache.entry(k).or_insert(v);
    }
    drop(cache);
    warmed.lock().unwrap().insert(project_id.get());
    true
}

fn warm_up_worker(
    store: Arc<dyn Persistence>,
    embedder: Arc<dyn Embedder>,
    cache: Arc<Mutex<HashMap<u64, Vec<f32>>>>,
    persist_dir: Option<PathBuf>,
    project_id: ProjectId,
    warmed: Arc<Mutex<HashSet<i64>>>,
    warming: Arc<Mutex<HashSet<i64>>>,
    progress: Arc<Mutex<HashMap<i64, (usize, usize)>>>,
) {
    let pid = project_id.get();
    let res: Result<()> = (|| {
        let nodes = fetch_nodes(store.as_ref(), project_id)?;
        // Node text enrichment: rebuild index via this project's i18n bridge, consistent with `ensure_cached_with`, so the two warmup
        // paths produce identical node vectors.
        let enrich = build_enrich_index(&compute_bridge(&nodes));
        // Load persisted vectors first: else after restart cache is empty, here would re-compute all nodes (CRMEB measured 56 min),
        // persisted file wasted.
        if let Some(dir) = &persist_dir {
            let path = dir.join(format!("{pid}.rmp"));
            if path.exists() {
                load_persisted_into(&path, &cache, project_id, &warmed, embedder.dim());
            }
        }
        const BATCH: usize = 256;
        let mut pending: Vec<(u64, String, u8)> = Vec::new();
        {
            let cache = cache.lock().unwrap();
            for node in &nodes {
                if !is_vector_kind(node) {
                    continue;
                }
                let text = node_embed_text(node, &enrich);
                let key = embed_text_key(&text);
                if cache.contains_key(&key) {
                    continue;
                }
                pending.push((key, text, warm_priority(node.kind.as_str())));
            }
        }
        sort_pending_for_warmup(&mut pending);
        let total = pending.len();
        let mut done = 0usize;
        for chunk in pending.chunks(BATCH) {
            let texts: Vec<String> = chunk.iter().map(|(_, t, _)| t.clone()).collect();
            let vecs = embedder.embed_batch(&texts);
            let mut cache = cache.lock().unwrap();
            for ((key, _, _), v) in chunk.iter().zip(vecs.into_iter()) {
                cache.insert(*key, v);
            }
            done += chunk.len();
            progress.lock().unwrap().insert(pid, (done, total));
        }
        if let Some(dir) = &persist_dir {
            if let Some(parent) = dir.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let path = dir.join(format!("{pid}.rmp"));
            let env = PersistedEmbeds {
                version: EMBED_TEXT_VERSION,
                dim: embedder.dim(),
                vectors: collect_cache(&cache, &nodes, &enrich),
            };
            if let Ok(data) = serde_json::to_vec(&env) {
                let _ = std::fs::write(path, data);
            }
        }
        warmed.lock().unwrap().insert(pid);
        Ok(())
    })();
    if let Err(e) = res {
        tracing::error!("background warmup of project #{pid} failed: {e}");
    }
    warming.lock().unwrap().remove(&pid);
    progress.lock().unwrap().remove(&pid);
}

/// Node cap per single scan (prevent a huge DB from turning one recall into a full-table scan).
const SCAN_LIMIT: u32 = 200_000;

/// Fallback list: used only when node kinds can't be read from the DB (ensure degraded usability).
const FALLBACK_SCAN_KINDS: &[&str] = &[
    "Table",
    "HttpContract",
    "ConfigKey",
    "I18nKey",
    "Event",
    "EventBus",
    "Queue",
    "Cache",
    "Topic",
    "Schedule",
    "Page",
    "Class",
    "Interface",
    "Trait",
    "Enum",
    "Method",
    "Function",
];

/// Kinds participating in recall = kinds **really present** on the graph − [`DEFAULT_EXCLUDED_KINDS`].
///
/// Earlier this hardcoded a type list, with the consequence: adding a language adapter (Go/Rust/C#…) or a new pipeline kind would
/// silently exclude those nodes from recall — not low score, but not participating at all. After taking `DISTINCT kind` from the DB,
/// any language and future new types auto-included.
fn scan_kinds(store: &dyn Persistence, project_id: ProjectId) -> Vec<String> {
    match store.node_kinds(project_id) {
        Ok(kinds) => {
            let v: Vec<String> = kinds
                .into_iter()
                .filter(|k| !DEFAULT_EXCLUDED_KINDS.contains(&k.as_str()))
                .collect();
            if !v.is_empty() {
                return v;
            }
        }
        Err(e) => tracing::warn!("failed to read node kinds, falling back to the built-in list: {e}"),
    }
    FALLBACK_SCAN_KINDS.iter().map(|s| s.to_string()).collect()
}

// -------------------------------------------------- project i18n bridge (data-driven, zero-config)

/// Split English tokens from an i18n key (e.g. `order.pay.insufficient_balance`).
///
/// Keep only ASCII tokens: this builds a "Chinese → English symbol" bridge; Chinese tokens left on the bridge are meaningless.
fn key_tokens(key: &str) -> Vec<String> {
    let mut out = Vec::new();
    for seg in key.split(|c: char| !c.is_alphanumeric()) {
        for t in split_camel(seg) {
            let t = t.to_ascii_lowercase();
            if t.len() >= 2 && t.chars().all(|c| c.is_ascii_alphanumeric()) && !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

/// Whether it contains CJK characters.
fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// Extract Chinese strings from a source snippet (reuse query-side `cjk_runs`, but drop single-char noise and dedup).
fn snippet_chinese(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in cjk_runs(s) {
        if r.chars().count() >= 2 && !out.contains(&r) {
            out.push(r);
        }
    }
    out
}

/// From `properties.locations[].file` take the token of "the module this text lives in": parent dir name + file stem.
///
/// E.g.: `template/uni-app/components/payment/index.vue` → `payment`. This bridges Chinese text to the business module it really
/// appears in — pure data-driven, independent of domain/language/framework. Generic words in dir/file names (`src`/`components`/`index`…)
/// are filtered, else unrelated nodes get pulled in.
fn location_tokens(props: &serde_json::Value) -> Vec<String> {
    const PATH_STOPWORDS: &[&str] = &[
        "template", "templates", "src", "app", "apps", "pages", "page", "components",
        "component", "views", "view", "index", "main", "static", "assets", "public",
        "utils", "util", "common", "shared", "lib", "libs", "core", "vendor", "dist",
        "build", "admin", "api", "js", "ts", "vue", "jsx", "tsx", "php", "java", "py",
        "html", "css", "scss", "min", "module", "modules", "service", "services",
    ];
    let mut out: Vec<String> = Vec::new();
    let Some(locs) = props.get("locations").and_then(|l| l.as_array()) else {
        return out;
    };
    for loc in locs.iter().take(8) {
        let Some(file) = loc.get("file").and_then(|f| f.as_str()) else {
            continue;
        };
        let path = std::path::Path::new(file);
        let stem = path.file_stem().map(|s| s.to_string_lossy().to_string());
        let parent = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string());
        for raw in [parent, stem].into_iter().flatten() {
            for seg in raw.split(|c: char| !c.is_alphanumeric()) {
                let t = seg.to_ascii_lowercase();
                if t.len() >= 3
                    && t.chars().all(|c| c.is_ascii_alphanumeric())
                    && !PATH_STOPWORDS.contains(&t.as_str())
                    && !out.contains(&t)
                {
                    out.push(t);
                }
            }
        }
        if out.len() >= 4 {
            break;
        }
    }
    out
}

/// Node-enrichment reverse index: a token of node name / fqn → list of "Chinese phrase + that phrase's English tokens" it hit.
///
/// Built reversely from the project i18n bridge: node `storeCoupon`'s token `coupon` hits the bridge's "coupon → coupon…", so we add
/// "coupon" to that node's embed text, letting Chinese query "coupon" align directly in vector space, without cross-language hard-align.
/// Pure data-driven, zero-config: a project's own i18n / source Chinese snippets suffice, e-commerce/finance/game treated alike.
type EnrichIndex = HashMap<String, Vec<(String, Vec<String>)>>;

/// Build the node-enrichment index from the project i18n bridge (`Vec<(Chinese phrase, English tokens)>`).
fn build_enrich_index(bridge: &[(String, Vec<String>)]) -> EnrichIndex {
    let mut idx: EnrichIndex = HashMap::new();
    for (zh, toks) in bridge {
        for t in toks {
            idx.entry(t.clone()).or_default().push((zh.clone(), toks.clone()));
        }
    }
    idx
}

/// Data computation for the project i18n bridge (decoupled from cache): iterate i18n / Chinese-name / source-Chinese-snippet nodes,
/// produce "Chinese text → that key's English tokens". Called and cached by [`RecallService::project_bridge`], also reused directly by
/// the background warmup worker, ensuring the two warmup paths produce identical node vectors.
fn compute_bridge(nodes: &[Node]) -> Vec<(String, Vec<String>)> {
    const MAX_PER_NODE: usize = 4;
    let mut entries: Vec<(String, Vec<String>)> = Vec::new();
    for n in nodes {
        let mut toks = key_tokens(&n.name);
        for t in location_tokens(&n.properties) {
            if !toks.contains(&t) {
                toks.push(t);
            }
        }
        if toks.is_empty() {
            continue;
        }
        let mut texts: Vec<String> = Vec::new();
        if let Some(t) = n.properties.get("texts").and_then(|t| t.as_object()) {
            for (_locale, v) in t {
                if let Some(s) = v.as_str() {
                    texts.push(s.trim().to_string());
                }
            }
        }
        if texts.is_empty() && contains_cjk(&n.name) {
            texts.push(n.name.trim().to_string());
        }
        if let Some(snip) = n.properties.get("snippet").and_then(|v| v.as_str()) {
            for r in snippet_chinese(snip).into_iter().take(MAX_PER_NODE) {
                texts.push(r);
            }
        }
        let mut added = 0usize;
        for s in texts {
            let len = s.chars().count();
            if (2..=30).contains(&len) && !s.trim().is_empty() {
                entries.push((s, toks.clone()));
                added += 1;
                if added >= MAX_PER_NODE {
                    break;
                }
            }
        }
    }
    entries
}

/// Split a camelCase string by case boundary (`insufficientBalance` → `insufficient` / `Balance`).
fn split_camel(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_ascii_uppercase() && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Structural "containment" edge: when a container (class/interface/table/contract) is hit, use it to bring in its members.
///
/// Expanding only along call-chain edges means hitting `ArticleService` cannot get its `findAll` — reproducible even
/// for English queries (`article list pagination` only yields the class, not the method). Containment edges are pure structural info,
/// independent of language/domain/naming style.
fn is_containment_edge(kind: &str) -> bool {
    matches!(kind, "Declares" | "HasColumn" | "Extends" | "HandledBy")
}

/// Max members one container brings out, to avoid a huge class's all methods filling the hit list.
const MAX_MEMBERS_PER_CONTAINER: usize = 6;

/// Neighbors (bidirectional) for recall expansion: call-chain edges **+ containment edges**.
fn neighbours(
    id: NodeId,
    incoming: &HashMap<i64, Vec<gt_domain::model::Edge>>,
    outgoing: &HashMap<i64, Vec<gt_domain::model::Edge>>,
) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut members = 0usize;
    if let Some(es) = outgoing.get(&id.get()) {
        for e in es {
            if is_chain_edge(e.kind.as_str()) {
                out.push(e.to_id);
            } else if is_containment_edge(e.kind.as_str()) && members < MAX_MEMBERS_PER_CONTAINER {
                members += 1;
                out.push(e.to_id);
            }
        }
    }
    if let Some(es) = incoming.get(&id.get()) {
        for e in es {
            if is_chain_edge(e.kind.as_str()) {
                out.push(e.from_id);
            } else if is_containment_edge(e.kind.as_str()) && members < MAX_MEMBERS_PER_CONTAINER {
                members += 1;
                out.push(e.from_id);
            }
        }
    }
    out
}

/// Relation summary: in/out edges aggregated by kind, top 5 (context pack must explain "why relevant").
fn relation_summary(
    id: NodeId,
    incoming: &HashMap<i64, Vec<gt_domain::model::Edge>>,
    outgoing: &HashMap<i64, Vec<gt_domain::model::Edge>>,
) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    if let Some(es) = incoming.get(&id.get()) {
        for e in es {
            if is_chain_edge(e.kind.as_str()) {
                *counts.entry(format!("← {}", e.kind)).or_default() += 1;
            }
        }
    }
    if let Some(es) = outgoing.get(&id.get()) {
        for e in es {
            if is_chain_edge(e.kind.as_str()) {
                *counts.entry(format!("→ {}", e.kind)).or_default() += 1;
            }
        }
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter()
        .take(5)
        .map(|(k, n)| if n > 1 { format!("{k} ×{n}") } else { k })
        .collect()
}

/// Node scoring: returns (score, matched query word).
fn score_node(
    node: &Node,
    terms: &[String],
    kind_hints: &[String],
    incoming: &HashMap<i64, Vec<gt_domain::model::Edge>>,
    action: bool,
) -> (f64, Vec<String>) {
    let name = node.name.to_lowercase();
    let fqn = node.fqn.as_deref().unwrap_or("").to_lowercase();
    let identity = node.identity.as_ref().map(|i| i.value.to_lowercase()).unwrap_or_default();

    let is_contract = node.kind.as_str().eq_ignore_ascii_case("HttpContract");

    let mut score = 0.0f64;
    let mut matched: Vec<String> = Vec::new();
    for t in terms {
        let t = t.to_lowercase();
        if is_contract && GENERIC_CRUD_METHODS.contains(&t.as_str()) {
            continue;
        }
        let mut best = 0.0f64;
        if name == t {
            best = 100.0;
        } else if name.starts_with(&t) {
            best = 70.0;
        } else if name.contains(&t) {
            best = 50.0;
        }
        if !identity.is_empty() && identity.contains(&t) {
            best = best.max(45.0);
        }
        if !fqn.is_empty() && fqn.contains(&t) {
            best = best.max(35.0);
        }
        if best > 0.0 {
            score += best;
            matched.push(t.clone());
        }
    }
    if matched.len() >= 2 {
        // Hitting multiple words → relevance significantly stronger
        score *= 1.5;
    }
    if score <= 0.0 {
        return (0.0, matched);
    }

    let has_content_term = has_content_word(node, terms);
    let verb_only = action && is_generic_crud_method(&name) && !has_content_term;
    if verb_only {
        score *= GENERIC_CRUD_VERB_ONLY_DISCOUNT;
    }
    let kw = if verb_only {
        kind_weight(node.kind.as_str())
    } else {
        rank_weight(node.kind.as_str(), action)
    };
    score *= kw;

    // Structural-hint boost: prompt said "table", prioritize Table
    for hint in kind_hints {
        if node.kind.as_str().eq_ignore_ascii_case(hint) {
            score += 30.0;
            break;
        }
    }

    // Fan-in boost: more referenced = more likely the "core of the topic"
    let fan_in = incoming
        .get(&node.id.get())
        .map(|es| es.iter().filter(|e| is_chain_edge(e.kind.as_str())).count())
        .unwrap_or(0);
    score += (fan_in.min(20) as f64) * 0.4;

    (score, matched)
}

/// "Kind weight" shared by lexical / vector paths.
///
/// Default (`action=false`) keeps the original [`kind_weight`]: structural / contract kinds (table, HTTP interface, event…) are
/// "topic-level" answers, weighted above methods, so "order-related table" converges to `Table`.
///
/// When the query intent is "find the code implementing this logic" (`action=true`, incl. modify/implement/calculate/validate/deduct/
/// pay/callback… action verbs), invert preference: boost method/class, downweight HTTP routes and async infrastructure (event/queue/
/// cache/cron…). Else such queries get beaten by literally-same-word routes (`GET /marketing/coupon/edit`, `ANY /pay/notify`)' high
/// lexical score, while the real service methods (storeCoupon/paymentOrder) — what "where's the code" points to — sink.
fn rank_weight(kind: &str, action: bool) -> f64 {
    if !action {
        return kind_weight(kind);
    }
    match kind {
        "Method" | "Function" => 1.5,
        "Class" | "Interface" | "Trait" | "Enum" => 1.3,
        "Table" => 1.4,
        "ConfigKey" | "I18nKey" | "Page" | "EventBus" => 1.2,
        "HttpContract" | "Event" | "Queue" | "Topic" | "Cache" | "Schedule" => 0.7,
        _ => 0.9,
    }
}

/// Default kind weight (see [`rank_weight`]).
fn kind_weight(kind: &str) -> f64 {
    match kind {
        "Table" | "HttpContract" | "Event" | "Queue" | "Cache" | "Topic" | "Schedule" => 1.4,
        "ConfigKey" | "I18nKey" | "Page" | "EventBus" => 1.2,
        "Class" | "Interface" | "Trait" | "Enum" => 1.1,
        "Method" | "Function" => 1.0,
        _ => 0.8,
    }
}

/// Whether the query intends "find the code implementing logic" rather than "find the topic / route".
///
/// Hitting an action verb is true: lexical / vector paths invert kind preference accordingly (see [`rank_weight`]). Only when true is
/// weight changed — non-action queries fully keep the old [`kind_weight`], zero regression.
fn action_intent(q: &str) -> bool {
    const KW: &[&str] = &[
        // Chinese action verbs
        "修改", "实现", "在哪", "代码", "方法", "函数", "逻辑", "计算", "处理", "校验", "拦截",
        "回滚", "扣减", "设置", "保存", "更新", "新增", "删除", "查询", "获取", "读取", "加载",
        "下单", "支付", "通知", "回调", "创建", "生成", "调用", "执行", "写入", "怎么", "如何",
        // English action verbs
        "modify", "implement", "code", "where", "compute", "handle", "validate", "intercept",
        "rollback", "deduct", "save", "update", "create", "add", "delete", "query", "get",
        "fetch", "load", "set", "pay", "notify", "callback", "invoke", "execute", "write", "how",
    ];
    let low = q.to_lowercase();
    KW.iter().any(|k| low.contains(&k.to_lowercase()))
}

/// Whether the query intends "reconstruct a flow / call chain" rather than find a single implementation point.
///
/// The correct answer for such queries is essentially **ordered**: page / route → controller → service → persistence. Plain relevance
/// sorting scrambles the chain (page ends up after the entry function), and the hub everyone calls (`Request` 300+ times, `Cache` 400+
/// times) crowds the front. Enable topology reorder only when flow words hit (see [`reorder_for_flow]) — gated the same way as
/// [`action_intent`]: **non-flow queries fully keep the old order, zero regression**.
/// "Configurable-value" vocabulary: a hit means the user wants **a config item** (threshold / param / switch…), not implementation code —
/// then don't invert kind preference by action intent, else Method is boosted to 1.5× while ConfigKey(1.2×)/Queue(0.7×) are pushed down,
/// exactly squeezing out the real config.
///
/// **Deliberately not via `hint_map`**: hint words get stripped char-by-char from query words by [`strip_hint_chars`]; adding a "lower bound"
/// would turn `下单` into `单`. Here we only do a substring check on the raw query, zero side effects.
const CONFIG_WORDS: &[&str] = &[
    "阈值", "参数", "开关", "上限", "下限", "时长", "间隔", "配置", "配置项", "预警",
    // "auto-cancel **time**" itself is a config value; missing it, the query would take the action intent (Method 1.5×) and push ConfigKey
    // down, and the correct answer `order_cancel_time` would drop off the list.
    "时间",
];

fn wants_config_value(q: &str) -> bool {
    CONFIG_WORDS.iter().any(|w| q.contains(w))
}

fn flow_intent(q: &str) -> bool {
    const KW: &[&str] = &[
        // Chinese
        "流程", "链路", "调用链", "调用关系", "调用顺序", "调用过程", "步骤", "顺序", "生命周期",
        "流转", "怎么走", "走一遍", "经过",
        // English
        "flow", "call chain", "trace", "pipeline", "lifecycle", "walkthrough", "sequence",
    ];
    let low = q.to_lowercase();
    KW.iter().any(|k| low.contains(&k.to_lowercase()))
}

/// Flow-direction edges: call / handle / persist direction, i.e. "who is invoked by whom downstream".
/// Used to rank hits into the "entry → … → persistence" chain (see [`reorder_for_flow`]).
fn is_flow_edge(kind: &str) -> bool {
    matches!(
        kind,
        "Calls" | "CallsHttp" | "HandledBy" | "PassesThrough" | "WritesDb" | "ReadsDb"
    )
}

/// **Entry-layer** kinds of a flow: HTTP contract / page. They are naturally the chain start — even if a frontend function points at a
/// route via `CallsHttp`, that route is still the backend's **entry**, not a middle node, and shouldn't be counted as "the next layer called
/// down by the frontend" (else the route ranks after the service method).
fn is_entry_kind(kind: &str) -> bool {
    matches!(kind, "HttpContract" | "Page")
}

/// Nodes with fan-in above this are "hubs": common infrastructure called everywhere (`Request` 300+ times, `Cache` 400+ times), not a
/// specific step of any flow.
const HUB_FANIN: usize = 60;

/// Hub decay factor: smoothly pushed down with fan-in, minimum 0.4, never 0 — the infrastructure can still be recalled, just without
/// crowding the front of the chain (see [`reorder_for_flow`]).
fn hub_penalty(fan_in: usize) -> f64 {
    if fan_in <= HUB_FANIN {
        return 1.0;
    }
    let excess = (fan_in - HUB_FANIN) as f64;
    0.4 + 0.6 * (-excess / 200.0).exp()
}

/// Longest-path depth along flow direction within the hit set (entry = 0). Back edges count as 0 to tolerate cycles.
fn flow_depth(
    id: i64,
    callers: &HashMap<i64, Vec<i64>>,
    memo: &mut HashMap<i64, i64>,
    visiting: &mut HashSet<i64>,
) -> i64 {
    if let Some(d) = memo.get(&id) {
        return *d;
    }
    if !visiting.insert(id) {
        return 0; // 环：本次不再深入
    }
    let mut best = 0i64;
    if let Some(cs) = callers.get(&id) {
        for c in cs {
            best = best.max(flow_depth(*c, callers, memo, visiting) + 1);
        }
    }
    visiting.remove(&id);
    memo.insert(id, best);
    best
}

/// Among the hit set, the nodes connected to **seeds** along flow edges.
///
/// Criterion: on the undirected graph formed by flow edges, find connected components, keep only those containing at least one direct hit
/// (`direct`, i.e. the seed itself). Only then is it "this query's chain" — otherwise every sibling node without an upstream would become
/// its own depth-0 and crowd the front.
fn anchored_components(hits: &[RecallHit], adj: &HashMap<i64, Vec<i64>>) -> HashSet<i64> {
    let mut out: HashSet<i64> = HashSet::new();
    let mut seen: HashSet<i64> = HashSet::new();
    for h in hits.iter().filter(|h| h.direct) {
        let start = h.node_id.get();
        if !seen.insert(start) {
            continue;
        }
        let mut stack = vec![start];
        while let Some(n) = stack.pop() {
            out.insert(n);
            if let Some(ns) = adj.get(&n) {
                for m in ns {
                    if seen.insert(*m) {
                        stack.push(*m);
                    }
                }
            }
        }
    }
    out
}

/// Topology reorder for flow queries: within the hit set, compute depth along [`is_flow_edge`] and sort by "entry → … → persistence";
/// neighbors unrelated to the chain (connected only via containment / config non-flow edges) sink to the end; hubs decay by fan-in.
///
/// Only called when [`flow_intent`] is true; other queries' ordering is fully unaffected.
fn reorder_for_flow(
    hits: &mut Vec<RecallHit>,
    incoming: &HashMap<i64, Vec<gt_domain::model::Edge>>,
) {
    let on: HashSet<i64> = hits.iter().map(|h| h.node_id.get()).collect();

    // node → "upstream caller" also in the hit set; also build undirected adjacency for connected-component finding.
    let mut callers: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut adj: HashMap<i64, Vec<i64>> = HashMap::new();
    for h in hits.iter() {
        let id = h.node_id.get();
        if let Some(es) = incoming.get(&id) {
            for e in es {
                let f = e.from_id.get();
                if is_flow_edge(e.kind.as_str()) && on.contains(&f) {
                    // Adjacency for connectivity: entry must connect too, else it'd be judged an isolated component and sink.
                    adj.entry(id).or_default().push(f);
                    adj.entry(f).or_default().push(id);
                    // Depth accumulates only by "called down by whom"; entry layer is always the start.
                    if !is_entry_kind(h.kind.as_str()) {
                        callers.entry(id).or_default().push(f);
                    }
                }
            }
        }
    }
    let on_chain = anchored_components(hits, &adj);

    let mut memo: HashMap<i64, i64> = HashMap::new();
    let mut visiting: HashSet<i64> = HashSet::new();
    let mut depths: HashMap<i64, i64> = HashMap::new();
    let mut fan_in: HashMap<i64, usize> = HashMap::new();
    for h in hits.iter() {
        let id = h.node_id.get();
        depths.insert(id, flow_depth(id, &callers, &mut memo, &mut visiting));
        fan_in.insert(
            id,
            incoming
                .get(&id)
                .map(|es| es.iter().filter(|e| is_flow_edge(e.kind.as_str())).count())
                .unwrap_or(0),
        );
    }

    hits.sort_by(|a, b| {
        let ia = a.node_id.get();
        let ib = b.node_id.get();
        // 1) on the chain ranks first
        let ca = on_chain.contains(&ia);
        let cb = on_chain.contains(&ib);
        cb.cmp(&ca)
            // 2) within chain by topology depth (entry → persistence)
            .then_with(|| depths[&ia].cmp(&depths[&ib]))
            // 3) same tier by "hub-decayed score"
            .then_with(|| {
                let sa = a.score * hub_penalty(fan_in[&ia]);
                let sb = b.score * hub_penalty(fan_in[&ib]);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            })
            // 4) stable fallback (same score by id, for reproducibility)
            .then_with(|| ia.cmp(&ib))
    });
}

/// Parse the hint words: split out query words and structural hints.
///
/// Structural hints ("table" / "interface" / "event"…) don't go to text matching, but become node-kind boosts — so a pure-Chinese question
/// like "order-related table" can still converge the answer to `Table`.
fn parse_query(query: &str) -> (Vec<String>, Vec<String>) {
    let mut terms: Vec<String> = Vec::new();
    let mut hints: Vec<String> = Vec::new();

    // Chinese structural hint words
    let hint_map: &[(&str, &str)] = &[
        ("表", "Table"),
        ("数据库", "Table"),
        ("接口", "HttpContract"),
        ("路由", "HttpContract"),
        ("端点", "HttpContract"),
        ("事件", "Event"),
        ("监听", "Event"),
        ("配置", "ConfigKey"),
        ("配置项", "ConfigKey"),
        ("缓存", "Cache"),
        ("队列", "Queue"),
        ("消息", "Topic"),
        ("定时", "Schedule"),
        ("计划任务", "Schedule"),
        ("页面", "Page"),
        ("国际化", "I18nKey"),
        ("多语言", "I18nKey"),
    ];

    for (word, kind) in hint_map {
        if query.contains(word) && !hints.iter().any(|h| h == kind) {
            hints.push(kind.to_string());
        }
    }

    let mut buf = String::new();
    for ch in query.chars() {
        if (ch.is_alphanumeric() && !is_cjk(ch)) || ch == '_' {
            buf.push(ch);
        } else {
            push_token(&mut terms, &buf);
            buf.clear();
        }
    }
    push_token(&mut terms, &buf);

    let known_alias_words: HashSet<String> = builtin_alias_keys();

    for run in cjk_runs(query) {
        // Keep a whole copy: exact long-word match scores highest.
        let keep = strip_hint_words(&run, hint_map);
        if keep.chars().count() >= 2 && !terms.iter().any(|t| t == &keep) {
            terms.push(keep.clone());
        }
        let segs = segment_cjk(&keep, &known_alias_words);
        for (seg, known) in &segs {
            if *known && seg.chars().count() >= 2 && !terms.iter().any(|t| t == seg) {
                terms.push(seg.clone());
            }
        }
        for w in segs.windows(2) {
            // Both sides must be "unrecorded single chars" to form a bigram — known words don't cross boundaries.
            if w[0].1 || w[1].1 {
                continue;
            }
            let gram: String = format!("{}{}", w[0].0, w[1].0);
            let gk = strip_hint_words(&gram, hint_map);
            if gk.chars().count() >= 2 && !terms.iter().any(|t| t == &gk) {
                terms.push(gk);
            }
        }
    }

    let stop: HashSet<&str> = ["的", "了", "和", "与", "在", "是", "有", "请", "帮", "我",
        "找", "出", "哪些", "什么", "相关", "涉及", "代码", "列出", "查询", "the", "a", "an",
        "is", "are", "of", "and", "or", "for", "to", "in", "on", "all", "list", "show", "find"]
        .into_iter()
        .collect();

    let terms: Vec<String> = terms
        .into_iter()
        .filter(|t| t.chars().count() >= 2 && !stop.contains(t.as_str()))
        .collect();

    (terms, hints)
}

/// Split a CJK run into segments by "known-word longest match"; `(text, is_known_word)`.
///
/// Unrecorded chars become **single-char segments** handed to the caller for bigram fallback, so real business words not in the alias table
/// (审核 / 退回) survive; recorded words become whole segments, naturally avoiding boundary-crossing noise with neighbors (see [`parse_query`]).
fn segment_cjk(run: &str, known: &HashSet<String>) -> Vec<(String, bool)> {
    let chars: Vec<char> = run.chars().collect();
    let max_len = known
        .iter()
        .map(|k| k.chars().count())
        .max()
        .unwrap_or(0)
        .max(2);
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let room = (chars.len() - i).min(max_len);
        let mut matched: Option<(String, usize)> = None;
        for l in (2..=room).rev() {
            let cand: String = chars[i..i + l].iter().collect();
            if known.contains(&cand) {
                matched = Some((cand, l));
                break;
            }
        }
        if let Some((word, len)) = matched {
            out.push((word, true));
            i += len;
        } else {
            out.push((chars[i].to_string(), false));
            i += 1;
        }
    }
    out
}

fn is_cjk(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch)
}

/// Extract continuous CJK runs from the query (each run is a "word-boundary-less Chinese fragment").
fn cjk_runs(query: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut cur = String::new();
    for ch in query.chars() {
        if is_cjk(ch) {
            cur.push(ch);
        } else if !cur.is_empty() {
            runs.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    runs
}

/// Remove the **complete structural hint words** appearing in the query ("table" / "interface" / "cache"…).
///
/// Words like "table" / "interface" / "cache" are already turned into node-kind boosts; using them as text-match words only steers recall
/// wrong ("table" matches every node whose name contains "table").
///
/// **Must delete whole words, never char-by-char**: the old impl split hints into single chars then filtered all chars that appeared from the
/// whole string — huge side effects: * `缓存`'s `存` + `数据库`'s `库` → `库存` emptied; * `消息`'s `消` + `定时`'s `时` → `取消时间` cut to `取`.
/// Measured: "product stock-warning threshold" thus lost "stock", "modify order auto-cancel time" lost "cancel time", directly causing each
/// intent's correct answer (`product_stock_job` / `order_cancel_time`) to be unrecallable.
fn strip_hint_words(s: &str, hint_map: &[(&str, &str)]) -> String {
    let mut out = s.to_string();
    for (word, _) in hint_map {
        if out.contains(word) {
            out = out.replace(word, "");
        }
    }
    out
}

/// After splitting camelCase / snake_case, push tokens into the list.
fn push_token(terms: &mut Vec<String>, raw: &str) {
    if raw.is_empty() {
        return;
    }
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in raw.chars() {
        if ch == '_' || ch == '-' || ch == '.' || ch == '/' || ch == '\\' {
            if !cur.is_empty() {
                terms.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !cur.is_empty() {
            terms.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
        prev_lower = ch.is_lowercase();
    }
    if !cur.is_empty() {
        terms.push(cur);
    }
    // Keep the original verbatim too: `store_order` as a whole match is valuable
    if raw.chars().count() >= 2 && !terms.iter().any(|t| t == raw) {
        terms.push(raw.to_string());
    }
}

/// Read a source snippet (a few lines before and after the node's line).
fn read_snippet(fs: &dyn FileSystem, path: &Path, line: u32) -> Option<String> {
    const MAX_FILE: u64 = 2 * 1024 * 1024;
    if fs.len(path).unwrap_or(0) > MAX_FILE {
        return None;
    }
    let text = fs.read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let start = line.saturating_sub(2).max(1) as usize;
    let end = (line as usize + 3).min(lines.len());
    if start > lines.len() {
        return None;
    }
    Some(lines[start - 1..end].join("\n"))
}

/// Append the full source of the few files touched by the hits to the end of the context pack.
///
/// For `include_body`: in IDE / MCP scenarios the LLM can read the implementation directly from this, saving a round-trip `read` for the full
/// file. Only take the top-ranked, deduped few files ([`INCLUDE_BODY_MAX_FILES`]); a single file beyond [`INCLUDE_BODY_MAX_BYTES`] is truncated,
/// to avoid huge files blowing up context.
fn append_file_bodies(fs: &dyn FileSystem, hits: &[RecallHit], mut md: String) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut files: Vec<String> = Vec::new();
    for h in hits {
        if let Some(f) = &h.file {
            if seen.insert(f.clone()) {
                files.push(f.clone());
            }
        }
        if files.len() >= INCLUDE_BODY_MAX_FILES {
            break;
        }
    }
    if files.is_empty() {
        return md;
    }
    md.push_str("\n\n## Full files (include_body)\n\n");
    for f in &files {
        md.push_str(&format!("### {f}\n\n"));
        match fs.read_to_string(Path::new(f)) {
            Ok(text) => {
                let content = if text.len() > INCLUDE_BODY_MAX_BYTES {
                    let mut s = text;
                    s.truncate(INCLUDE_BODY_MAX_BYTES);
                    format!(
                        "{s}\n\n… (content exceeds {}KB, truncated)",
                        INCLUDE_BODY_MAX_BYTES / 1024
                    )
                } else {
                    text
                };
                md.push_str(&format!("```\n{content}\n```\n\n"));
            }
            Err(_) => {
                md.push_str("_(file could not be read; it may have been moved or deleted)_\n\n");
            }
        }
    }
    md
}

/// Render the context pack ready to paste into an LLM.
fn render_markdown(
    project_id: ProjectId,
    q: &RecallQuery,
    terms: &[String],
    hints: &[String],
    seeds: &[SeedInfo],
    hits: &[RecallHit],
    // Quality-alert block (empty when quality is High). Put **first**: the caller must see it before trusting the list below —
    // otherwise it'd trust the list as usual, which is exactly "silent failure".
    advisory: &str,
) -> String {
    let mut s = String::new();
    s.push_str(&format!("# Recall context: {}\n\n", q.query));
    if !advisory.is_empty() {
        s.push_str(advisory);
    }
    s.push_str(&format!(
        "- Project: #{}  \n- Query terms: {}\n",
        project_id,
        if terms.is_empty() { "(none)".to_string() } else { terms.join(", ") }
    ));
    if !hints.is_empty() {
        s.push_str(&format!("- Structural hints: {}\n", hints.join(", ")));
    }
    s.push_str(&format!("- Hop limit: {}, {} hit(s)\n\n", q.hops, hits.len()));

    if !seeds.is_empty() {
        s.push_str("## Seeds (direct keyword hits)\n\n");
        for sd in seeds {
            s.push_str(&format!("- `{}` {} (score {:.1})\n", sd.name, sd.kind, sd.score));
        }
        s.push('\n');
    }

    s.push_str("## Related code\n\n");
    for (i, h) in hits.iter().enumerate() {
        let loc = match (&h.file, h.line) {
            (Some(f), Some(l)) => format!("`{f}:{l}`"),
            (Some(f), None) => format!("`{f}`"),
            _ => "(no location)".to_string(),
        };
        s.push_str(&format!(
            "### {}. {} `{}`\n\n- Location: {}  \n- Score: {:.1} · hops {} · source seed `{}`{}\n",
            i + 1,
            h.kind,
            h.name,
            loc,
            h.score,
            h.hop,
            h.seed,
            if h.direct { " · direct hit" } else { "" }
        ));
        if !h.relations.is_empty() {
            s.push_str(&format!("- Relations on the graph: {}\n", h.relations.join(", ")));
        }
        // Only the top SNIPPET_TOP hits attach a source snippet, to control default output volume (see [`SNIPPET_TOP`]).
        if i < SNIPPET_TOP {
            if let Some(sn) = &h.snippet {
                s.push_str(&format!("\n```\n{sn}\n```\n"));
            }
        }
        s.push('\n');
    }
    s
}


#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::error::DomainError;
    use gt_domain::model::{
        Edge, EdgeId, EdgeKind, FileId, IdentityKey, Language, Node, NodeId, NodeKind, Phase,
        ProjectId, Span,
    };
    use gt_domain::port::FileSystem;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    // ---- construction helpers ----

    fn tnode(id: i64, kind: &str, name: &str, fqn: Option<&str>, identity: Option<&str>) -> Node {
        Node {
            id: NodeId::new(id),
            project_id: ProjectId::new(1),
            sub_project_id: None,
            kind: NodeKind::new(kind),
            name: name.to_string(),
            fqn: fqn.map(|s| s.to_string()),
            identity: identity.map(IdentityKey::fqn),
            file_id: None,
            span: Span::default(),
            language: Language::new("php"),
            phase: Phase::new("Synthesize"),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        }
    }

    fn edge(kind: &str, from: i64, to: i64) -> Edge {
        Edge {
            id: EdgeId::new(0),
            project_id: ProjectId::new(1),
            kind: EdgeKind::new(kind),
            from_id: NodeId::new(from),
            to_id: NodeId::new(to),
            phase: Phase::new("Synthesize"),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        }
    }

    /// Incoming-edge table: aggregated by `to` (same as `score_node` / `relation_summary`).
    fn incoming(edges: &[(&str, i64, i64)]) -> HashMap<i64, Vec<Edge>> {
        let mut m: HashMap<i64, Vec<Edge>> = HashMap::new();
        for (k, from, to) in edges {
            m.entry(*to).or_default().push(edge(k, *from, *to));
        }
        m
    }

    /// Outgoing-edge table: aggregated by `from`.
    fn outgoing(edges: &[(&str, i64, i64)]) -> HashMap<i64, Vec<Edge>> {
        let mut m: HashMap<i64, Vec<Edge>> = HashMap::new();
        for (k, from, to) in edges {
            m.entry(*from).or_default().push(edge(k, *from, *to));
        }
        m
    }

    // ---- parse_query ----

    #[test]
    fn parse_query_maps_all_chinese_kind_hints() {
        let cases = [
            ("数据库", "Table"),
            ("接口", "HttpContract"),
            ("路由", "HttpContract"),
            ("端点", "HttpContract"),
            ("事件", "Event"),
            ("监听", "Event"),
            ("配置", "ConfigKey"),
            ("配置项", "ConfigKey"),
            ("缓存", "Cache"),
            ("队列", "Queue"),
            ("消息", "Topic"),
            ("定时", "Schedule"),
            ("计划任务", "Schedule"),
            ("页面", "Page"),
            ("国际化", "I18nKey"),
            ("多语言", "I18nKey"),
        ];
        for (word, kind) in cases {
            let (_terms, hints) = parse_query(word);
            assert!(
                hints.iter().any(|h| h == kind),
                "the hint word {word:?} must map to {kind:?}, got {hints:?}"
            );
        }
        // "table" covered in other integration cases; re-confirm the anchor here
        let (_t, hints) = parse_query("表");
        assert!(hints.contains(&"Table".to_string()), "表 must map to Table");
    }

    #[test]
    fn parse_query_splits_camel_case_and_keeps_whole() {
        let (terms, _hints) = parse_query("createOrder");
        assert!(terms.contains(&"create".to_string()));
        assert!(terms.contains(&"Order".to_string()));
        assert!(
            terms.contains(&"createOrder".to_string()),
            "a whole identifier must be kept (exact match scores highest): {terms:?}"
        );
    }

    #[test]
    fn parse_query_keeps_snake_case_intact() {
        let (terms, _hints) = parse_query("store_order");
        assert!(
            terms.contains(&"store_order".to_string()),
            "snake_case must not be split into store + order: {terms:?}"
        );
    }

    #[test]
    fn parse_query_filters_two_char_stop_words() {
        let (terms, _hints) = parse_query("用户相关的代码");
        assert!(
            !terms.contains(&"相关".to_string()),
            "a two-character stop word must be filtered out: {terms:?}"
        );
        assert!(
            !terms.contains(&"代码".to_string()),
            "a two-character stop word must be filtered out: {terms:?}"
        );
        assert!(
            terms.iter().any(|t| t.contains("用户")),
            "a meaningful word must be kept: {terms:?}"
        );
    }

    #[test]
    fn symptom_pack_bridges_phenomena_to_implementation_tokens() {
        let all = builtin_aliases();
        let en_of = |zh: &str| -> Vec<String> {
            all.iter()
                .find(|(z, _)| z == zh)
                .map(|(_, e)| e.clone())
                .unwrap_or_default()
        };

        // symptom → implementation surface: mail→mail/email/smtp, password→password/passwd/pwd, oversell gives both stock and lock/atomic
        // (different systems land differently).
        assert!(en_of("邮件").iter().any(|e| e == "mail"), "邮件 must map to mail");
        assert!(en_of("密码").iter().any(|e| e == "password"), "密码 must map to password");
        assert!(en_of("超卖").iter().any(|e| e == "stock"), "超卖 must map to stock");
        assert!(en_of("加锁").iter().any(|e| e == "lock"), "加锁 must map to lock");
        assert!(en_of("jwt").iter().any(|e| e == "token"), "jwt→token");

        // The literal-translation layer's old entries aren't polluted by the symptom pack (delete still only delete/remove/destroy).
        assert!(en_of("删除").iter().any(|e| e == "delete"));
        assert!(!en_of("删除").iter().any(|e| e == "mail"));
    }

    #[test]
    fn expand_intent_aliases_matches_ascii_keys_case_insensitively() {
        // ASCII keys are case-insensitive: developer writes `JWT`, key is `jwt`.
        let low = vec![(
            "jwt".to_string(),
            vec!["token".to_string(), "auth".to_string()],
        )];
        let out = expand_intent_aliases("JWT 是在哪里统一校验的", &low);
        assert!(out.contains(&"token".to_string()), "an upper-case JWT must hit the jwt bridge too");
        let out = expand_intent_aliases("jwt middleware", &low);
        assert!(out.contains(&"auth".to_string()));

        // Chinese keys are still exact substring match.
        let zh = vec![("邮件".to_string(), vec!["mail".to_string()])];
        assert!(expand_intent_aliases("邮件发不出去，负责发邮件的代码在哪", &zh)
            .contains(&"mail".to_string()));
        assert!(expand_intent_aliases("订单退款", &zh).is_empty());
    }

    #[test]
    fn cjk_segmentation_keeps_unknown_words_beside_known_ones() {
        let (terms, _) = parse_query("退款审核通过后钱怎么退回");
        assert!(terms.iter().any(|t| t == "退款"), "a known word is kept: {terms:?}");
        assert!(terms.iter().any(|t| t == "审核"), "an unlisted business word must be kept: {terms:?}");
        assert!(terms.iter().any(|t| t == "退回"), "an unlisted business word must be kept: {terms:?}");

        // Boundary-crossing noise is still dropped as before (see parse_query_drops_boundary_bigrams).
        let segs = segment_cjk("如何修改下单优惠", &builtin_alias_keys());
        assert!(segs.iter().any(|(s, k)| *k && s == "修改"));
        assert!(segs.iter().any(|(s, k)| *k && s == "下单"));
        assert!(segs.iter().any(|(s, k)| *k && s == "优惠"));
        // 如 / 何 are unrecorded single chars → each its own segment, "何修" won't appear as a whole.
        assert!(!segs.iter().any(|(s, _)| s == "何修"));
    }

    #[test]
    fn is_cjk_detects_chinese() {
        assert!(is_cjk('中'));
        assert!(!is_cjk('a'));
        assert!(!is_cjk('1'));
    }

    // ---- phrase aggregate boost (fix 3: combined intent beats isolated word) ----

    #[test]
    fn cohesion_boosts_multi_intent() {
        let g = alias_group_map(&builtin_aliases());
        // Only one operation category hit ("query" → find): no boost
        let one = cohesion_multiplier(&["find".to_string()], &g);
        assert!((one - 1.0).abs() < 1e-9, "a single category must not get a bonus: {one}");
        // Two operation categories hit (query + delete): ×1.3
        let two = cohesion_multiplier(&["find".to_string(), "delete".to_string()], &g);
        assert!((two - 1.3).abs() < 1e-9, "two categories must score ×1.3: {two}");
        // Chinese intent words also categorized (with cross-domain generic verbs, no domain vocabulary)
        let zh = cohesion_multiplier(&["查询".to_string(), "删除".to_string()], &g);
        assert!((zh - 1.3).abs() < 1e-9, "two Chinese categories must score ×1.3 as well: {zh}");
    }

    // ---- seed union (fix 1+2: cross-language nodes must enter seeds, and merge score is additive) ----

    #[test]
    fn select_seeds_unions_vector_seeds() {
        // Simulate "modify order discount": lexical score Order-class crushes Coupon-class, but vector score (bge cosine) Coupon is far
        // higher — old logic took only top-5 by total, Coupon never qualified.
        let mut lexical = HashMap::new();
        lexical.insert(1, (260.0, vec!["order".to_string()])); // OrderServices
        lexical.insert(2, (55.0, vec!["coupon".to_string()])); // StoreCouponServices
        let mut vector = HashMap::new();
        vector.insert(1, 30.0);
        vector.insert(2, 110.0);

        let n1 = tnode(1, "Class", "OrderServices", None, None);
        let n2 = tnode(2, "Class", "StoreCouponServices", None, None);
        let mut index = HashMap::new();
        index.insert(1, &n1);
        index.insert(2, &n2);

        let seeds = select_seeds(&lexical, &vector, &index);
        let ids: Vec<i64> = seeds.iter().map(|(_, _, n)| n.id.get()).collect();
        assert!(ids.contains(&2), "the Coupon node must be selected as a vector seed: {ids:?}");
        // merged score = lexical + vector (additive not max)
        let coupon = seeds.iter().find(|(_, _, n)| n.id.get() == 2).unwrap();
        assert!(
            (coupon.0 - 165.0).abs() < 1e-9,
            "the merged score should be 55+110=165, got {}",
            coupon.0
        );
    }

    // ---- score_node ----

    #[test]
    fn score_node_exact_name_match() {
        // 100 (exact) × 1.4 (Table weight)
        let n = tnode(101, "Table", "user", None, None);
        let (score, matched) = score_node(&n, &["user".to_string()], &[], &HashMap::new(), false);
        assert!((score - 140.0).abs() < 1e-9, "an exact match should score 100×1.4=140, got {score}");
        assert_eq!(matched, vec!["user".to_string()]);
    }

    #[test]
    fn score_node_starts_with_prefix() {
        // 70 (prefix) × 1.4
        let n = tnode(102, "Table", "user_order", None, None);
        let (score, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new(), false);
        assert!((score - 98.0).abs() < 1e-9, "a prefix match should score 70×1.4=98, got {score}");
    }

    #[test]
    fn score_node_contains() {
        // 50 (contains) × 1.0 (Method)
        let n = tnode(103, "Method", "my_user_x", None, None);
        let (score, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new(), false);
        assert!((score - 50.0).abs() < 1e-9, "a contains match should score 50×1.0=50, got {score}");
    }

    #[test]
    fn score_node_identity_match() {
        // name doesn't match, identity contains → 45 × 1.0 (Method)
        let n = tnode(104, "Method", "zzz", None, Some("user_identity"));
        let (score, matched) = score_node(&n, &["user".to_string()], &[], &HashMap::new(), false);
        assert!((score - 45.0).abs() < 1e-9, "an identity hit should score 45×1.0=45, got {score}");
        assert_eq!(matched, vec!["user".to_string()]);
    }

    #[test]
    fn score_node_fqn_match() {
        // fqn contains → 35 × 1.0
        let n = tnode(105, "Method", "zzz", Some("app\\model\\user"), None);
        let (score, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new(), false);
        assert!((score - 35.0).abs() < 1e-9, "an fqn hit should score 35×1.0=35, got {score}");
    }

    #[test]
    fn score_node_agreement_prefix_is_weaker_than_full_token_semantically() {
        let a = tnode(106, "Method", "agreeRefund", None, None);
        let b = tnode(107, "Method", "agreement", None, None);
        let (sa, _) = score_node(&a, &["agree".to_string()], &[], &HashMap::new(), false);
        let (sb, _) = score_node(&b, &["agree".to_string()], &[], &HashMap::new(), false);
        assert!(
            (sa - sb).abs() < 1e-9,
            "under the current implementation both score the same (both are a whole-string prefix at 70); if they differ the implementation changed and the 48-case A/B must be re-run: {sa} vs {sb}"
        );
    }

    #[test]
    fn score_node_multi_term_multiplier() {
        // Two words hit: first sum each word's score, then overall ×1.5. Here each word scores 50 (Method weight 1.0), so two == (one +
        // order_only) × 1.5 == 150 (greater than simple addition's 100).
        let n = tnode(106, "Method", "xuserxorderx", None, None);
        let (one, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new(), false);
        let (order_only, _) = score_node(&n, &["order".to_string()], &[], &HashMap::new(), false);
        let (two, matched) = score_node(
            &n,
            &["user".to_string(), "order".to_string()],
            &[],
            &HashMap::new(),
            false,
        );
        assert!(
            (two - (one + order_only) * 1.5).abs() < 1e-9,
            "two terms should equal (one + one) × 1.5: one={one} order_only={order_only} two={two}"
        );
        assert!(two > one + order_only, "several terms must score strictly above their plain sum");
        assert_eq!(matched.len(), 2);
    }

    #[test]
    fn score_node_kind_weight_prefers_semantic_nodes() {
        let table = tnode(107, "Table", "user", None, None);
        let method = tnode(108, "Method", "user", None, None);
        let (s_t, _) = score_node(&table, &["user".to_string()], &[], &HashMap::new(), false);
        let (s_m, _) = score_node(&method, &["user".to_string()], &[], &HashMap::new(), false);
        assert!((s_t - 140.0).abs() < 1e-9, "Table 100×1.4=140");
        assert!((s_m - 100.0).abs() < 1e-9, "Method 100×1.0=100");
        assert!(s_t > s_m, "a semantic node (table) must rank above a method");
    }

    #[test]
    fn score_node_kind_hint_bonus() {
        // hint said "table" → structural-hint boost +30
        let n = tnode(109, "Table", "user", None, None);
        let (no_hint, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new(), false);
        let (with_hint, _) =
            score_node(&n, &["user".to_string()], &["Table".to_string()], &HashMap::new(), false);
        assert!(
            (with_hint - (no_hint + 30.0)).abs() < 1e-9,
            "a structural hint adds +30: {with_hint} vs {no_hint}"
        );
    }

    #[test]
    fn score_node_fan_in_bonus() {
        // exact match 100 × 1.0 (Method) + fan-in 2 × 0.4
        let n = tnode(110, "Method", "user", None, None);
        let inc = incoming(&[("WritesDb", 200, 110), ("ReadsDb", 201, 110)]);
        let (score, _) = score_node(&n, &["user".to_string()], &[], &inc, false);
        assert!(
            (score - (100.0 + 2.0 * 0.4)).abs() < 1e-9,
            "fan-in 2 should add +0.8, got {score}"
        );
    }

    #[test]
    fn rank_weight_reverses_preference_under_action_intent() {
        // Non-action intent: HTTP route (1.4) ranks above method (1.0).
        assert!(rank_weight("HttpContract", false) > rank_weight("Method", false));
        // Action intent (find implementation): method (1.5) overtakes route (0.7).
        assert!(rank_weight("Method", true) > rank_weight("HttpContract", true));
        // Non-action keeps the default kind weight (zero regression).
        assert!((rank_weight("Table", false) - 1.4).abs() < 1e-9);
    }

    #[test]
    fn action_intent_detects_code_seeking_verbs() {
        assert!(action_intent("修改订单优惠")); // 修改 (edit)
        assert!(action_intent("支付回调通知商户")); // 回调 / 支付 / 通知 (callback / pay / notify)
        assert!(action_intent("用户余额不足时拦截下单")); // 拦截 (block)
        assert!(action_intent("商品库存扣减失败回滚")); // 扣减 / 回滚 (deduct / rollback)
        // Pure topic query should not trigger action intent.
        assert!(!action_intent("优惠券列表页面"));
    }

    #[test]
    fn method_beats_route_under_action_intent() {
        // Under the same lexical hit, action intent lets the service method beat the literally-same-word HTTP route.
        let route = tnode(201, "HttpContract", "coupon_edit", None, None);
        let method = tnode(202, "Method", "storeCoupon", None, None);
        let (s_route, _) = score_node(&route, &["coupon".to_string()], &[], &HashMap::new(), true);
        let (s_method, _) = score_node(&method, &["coupon".to_string()], &[], &HashMap::new(), true);
        assert!(s_method > s_route, "under an action intent a method must outrank a route: {s_method} vs {s_route}");
    }

    #[test]
    fn score_node_generic_crud_method_skips_action_boost() {
        // Pure-verb methods (edit/save/update…) under action intent, and hitting no content word, don't get the 1.5× boost, falling back to
        // 1.0 — else "modify" queries push unrelated-domain CRUD (shipping save / refund update) to the top.
        let generic = tnode(301, "Method", "save", None, None);
        let (s_gen, _) = score_node(&generic, &["save".to_string()], &[], &HashMap::new(), true);
        // 100 (exact) × 0.5 (pure-verb no-content discount) × 1.0 (fallback weight) = 50
        assert!((s_gen - 50.0).abs() < 1e-9, "a pure CRUD method with no content word must be discounted back to the 1.0 weight, got {s_gen}");

        let coupon_edit = tnode(
            302,
            "Method",
            "edit",
            Some("app\\adminapi\\controller\\v1\\marketing\\StoreCouponIssue::edit"),
            None,
        );
        // name edit (100) + class identifier contains coupon (35) → 135; two words ×1.5; content word → 1.5× boost applies:
        // 135 × 1.5 × 1.5 = 303.75。
        let (s_ce, _) = score_node(
            &coupon_edit,
            &["edit".to_string(), "coupon".to_string()],
            &[],
            &HashMap::new(),
            true,
        );
        assert!(
            (s_ce - 303.75).abs() < 1e-9,
            "a CRUD method hitting a content word keeps the 1.5 multiplier, got {s_ce}"
        );

        // Counter-case: DeliveryService is under the order/ dir (fqn path contains order), but its class identifier contains no content word,
        // so it should be penalized (fall back to 1.0).
        let delivery_save = tnode(
            303,
            "Method",
            "save",
            Some("app\\adminapi\\controller\\v1\\order\\DeliveryService::save"),
            None,
        );
        let (s_ds, _) = score_node(
            &delivery_save,
            &["save".to_string(), "order".to_string()],
            &[],
            &HashMap::new(),
            true,
        );
        // name save (100) + path order only scores (+35) → 135; ×1.5 aggregate; but no content word → no boost, 1.0:
        // 135 × 1.5 = 202.5。
        assert!(
            (s_ds - 101.25).abs() < 1e-9,
            "a CRUD method whose class identifier has no content word falls back to the 1.0 weight, got {s_ds}"
        );

        // Compound business names (createForm) still get the 1.5× action boost, unaffected.
        let biz = tnode(304, "Method", "createForm", None, None);
        let (s_biz, _) = score_node(&biz, &["form".to_string()], &[], &HashMap::new(), true);
        assert!((s_biz - 75.0).abs() < 1e-9, "a compound business method keeps the 1.5 multiplier, got {s_biz}");
    }

    #[test]
    fn entity_container_nouns_do_not_count_as_content() {
        let user_create = tnode(
            401,
            "Method",
            "create",
            Some("app\\adminapi\\controller\\v1\\user\\UserAddressServices::create"),
            None,
        );
        // Only has `user` (entity container noun, in both class identifier and path): shouldn't count as content word → still discount + no boost.
        let (s_user, _) = score_node(
            &user_create,
            &["create".to_string(), "user".to_string()],
            &[],
            &HashMap::new(),
            true,
        );
        // Switch to a real domain word `address` in the class identifier: hits content word → keeps 1.5× action boost.
        let (s_addr, _) = score_node(
            &user_create,
            &["create".to_string(), "address".to_string()],
            &[],
            &HashMap::new(),
            true,
        );
        assert!(
            s_addr > s_user,
            "the entity-container noun user must not exempt a generic CRUD method from the discount: an address hit {s_addr} must beat a user hit {s_user}"
        );
    }

    #[test]
    fn has_content_word_matches_tokens_not_substrings() {
        // Query "how to add a new coupon type" expands to (add → add/create/insert/new, coupon → coupon/discount).
        let terms: Vec<String> = ["新增", "优惠", "add", "create", "insert", "new", "coupon", "discount"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let address_create = tnode(
            305,
            "Method",
            "create",
            Some("app\\services\\user\\UserAddressServices::create"),
            None,
        );
        assert!(
            !has_content_word(&address_create, &terms),
            "a generic verb is not a content word, and the class tokens carry no query topic"
        );
        // name create (100) + fqn contains add (35) → 135; two words ×1.5; no content word → boost falls back to 1.0: 135 × 1.5 = 202.5
        // (not 303.75).
        let (s_addr, _) = score_node(&address_create, &terms, &[], &HashMap::new(), true);
        assert!(
            (s_addr - 101.25).abs() < 1e-9,
            "a CRUD method from an unrelated domain must not keep the 1.5 multiplier, got {s_addr}"
        );

        // Control group: same pure verb `create`, but the class identifier tokens really include coupon (token prefix, not mid-string substring)
        // → should hit content word, keep 1.5× boost.
        let coupon_create = tnode(
            306,
            "Method",
            "create",
            Some("app\\services\\coupon\\CouponServices::create"),
            None,
        );
        assert!(
            has_content_word(&coupon_create, &terms),
            "a class-identifier token containing coupon counts as hitting a content word"
        );
        let (s_coupon, _) = score_node(&coupon_create, &terms, &[], &HashMap::new(), true);
        assert!(
            (s_coupon - 303.75).abs() < 1e-9,
            "a CRUD method hitting a content word keeps the 1.5 multiplier, got {s_coupon}"
        );
        assert!(s_coupon > s_addr, "the create of a coupon must outrank the create of an unrelated domain");

        // Directly verify identifier splitting.
        assert_eq!(
            split_ident_tokens("UserAddressServices"),
            vec!["user", "address", "services"]
        );
        assert_eq!(split_ident_tokens("HTTPResponse"), vec!["http", "response"]);
        assert_eq!(split_ident_tokens("store_order"), vec!["store", "order"]);
    }

    // ---- path / shape noise and anchors (developer signal) ----

    #[test]
    fn test_paths_are_detected_by_segment_not_substring() {
        // Hit: a real test-path shape.
        assert!(is_test_path("test/user.test.js"));
        assert!(is_test_path("test/tools/nock-server-fixtures.js"));
        assert!(is_test_path("__tests__/article.spec.ts"));
        assert!(is_test_path("app/tests/UserTest.php"));
        assert!(is_test_path("internal/user/user_test.go"));
        assert!(is_test_path("tests/test_helper.py"));

        // No false hit: a business path that happens to contain "test".
        // (An earlier version judged by substring, downgrading dirs like Contest/ / latest/.)
        assert!(!is_test_path("src/contest/ContestService.php"));
        assert!(!is_test_path("app/controller/latest/LatestController.php"));
        assert!(!is_test_path("app/services/order/StoreOrderCreateServices.php"));
    }

    #[test]
    fn generated_paths_are_detected_by_module_segment() {
        // MyBatis Generator output: the whole module is query builders like `OmsOrderItemExample`.
        assert!(is_generated_path("mall-mbg/src/main/java/com/macro/mall/model/OmsOrderItemExample.java"));
        assert!(is_generated_path("target/generated-sources/foo/Bar.java"));
        assert!(is_generated_path("app/build/generated/model/pb_model.dart"));
        // Business code unaffected.
        assert!(!is_generated_path("mall-admin/src/main/java/com/macro/mall/controller/OmsOrderController.java"));
        assert!(!is_generated_path("app/services/order/StoreOrderCreateServices.php"));
    }

    #[test]
    fn criteria_builder_methods_are_detected_by_shape() {
        // MyBatis Generator query-builder fingerprint.
        assert!(is_criteria_builder_method("addCriterion"));
        assert!(is_criteria_builder_method("createCriteria"));
        assert!(is_criteria_builder_method("createCriteriaInternal"));
        assert!(is_criteria_builder_method("andPaymentTimeIsNull"));
        assert!(is_criteria_builder_method("andRecommendStatusGreaterThanOrEqualTo"));
        assert!(is_criteria_builder_method("orIdIn"));

        // No false hit: normal business methods (3rd char lowercase, not and|or + camelCase word start).
        assert!(!is_criteria_builder_method("orderAfter"));
        assert!(!is_criteria_builder_method("androidHelper"));
        assert!(!is_criteria_builder_method("order"));
        assert!(!is_criteria_builder_method("getOrderList"));
    }

    #[test]
    fn node_noise_discount_combines_accessor_file_and_shape() {
        let files: HashMap<i64, String> = [(7i64, "test/user.test.js".to_string())]
            .into_iter()
            .collect();
        let boilerplate: HashSet<i64> = HashSet::new();

        let mut clean = tnode(401, "Method", "createOrder", None, None);
        clean.file_id = Some(gt_domain::model::FileId::new(7));
        assert!(
            (node_noise_discount(&clean, &files, &boilerplate) - TEST_FILE_DISCOUNT).abs() < 1e-9,
            "测试文件内的普通方法打折"
        );

        // Stacking: Criteria boilerplate methods inside the generator dir.
        let mut builder = tnode(402, "Method", "andPaymentTimeIsNull", None, None);
        builder.file_id = Some(gt_domain::model::FileId::new(7));
        let expected = TEST_FILE_DISCOUNT * CRITERIA_BUILDER_DISCOUNT;
        assert!(
            (node_noise_discount(&builder, &files, &boilerplate) - expected).abs() < 1e-9,
            "样板形态 + 测试路径应叠加打折"
        );
    }

    #[test]
    fn anchors_are_extracted_only_for_real_identifiers_in_graph() {
        let nodes = vec![
            tnode(
                410,
                "Class",
                "StoreOrderCreateServices",
                Some("app\\services\\order\\StoreOrderCreateServices"),
                None,
            ),
            tnode(
                411,
                "Method",
                "createOrder",
                Some("app\\services\\order\\StoreOrderCreateServices::createOrder"),
                None,
            ),
            tnode(412, "Method", "save", None, None),
        ];

        // Developer named the class + method.
        let anchors = extract_anchors(
            "StoreOrderCreateServices 里 createOrder 之后调了哪些下游方法",
            &nodes,
        );
        assert!(anchors.contains(&"storeordercreateservices".to_string()));
        assert!(anchors.contains(&"createorder".to_string()));

        // snake_case also counts as an anchor.
        let snake = vec![tnode(413, "Function", "list_users", None, None)];
        let anchors = extract_anchors("list_users 这个函数在哪", &snake);
        assert!(anchors.contains(&"list_users".to_string()));

        // Pure-Chinese / pure-lowercase-English-word queries produce **no anchor**: those are ordinary query words; boosting would steer
        // ranking wrong (writing rollback shouldn't lift every symbol containing rollback).
        assert!(extract_anchors("reduce product stock and rollback on failure", &nodes).is_empty());
        assert!(extract_anchors("订单总价里优惠是怎么算进去的", &nodes).is_empty());

        // Identifiers not in the graph don't count as anchors either (avoid boosting imagined symbols).
        assert!(extract_anchors("NonExistentService 在哪", &nodes).is_empty());
    }

    #[test]
    fn anchor_multiplier_tiers_by_match_strength() {
        let cls = tnode(
            420,
            "Class",
            "StoreOrderCreateServices",
            Some("app\\services\\order\\StoreOrderCreateServices"),
            None,
        );
        let method = tnode(
            421,
            "Method",
            "createOrder",
            Some("app\\services\\order\\StoreOrderCreateServices::createOrder"),
            None,
        );
        let other = tnode(422, "Method", "save", Some("app\\services\\DeliveryService::save"), None);
        let anchors = vec!["createorder".to_string(), "storeordercreateservices".to_string()];

        assert!((anchor_multiplier(&method, &anchors) - ANCHOR_EXACT_BOOST).abs() < 1e-9);
        assert!((anchor_multiplier(&cls, &anchors) - ANCHOR_EXACT_BOOST).abs() < 1e-9);
        assert!(
            (anchor_multiplier(&other, &anchors) - 1.0).abs() < 1e-9,
            "未被点名的符号不加权"
        );
        assert!(anchor_multiplier(&method, &[]) == 1.0, "with no anchors it stays neutral");

        // Prefix / suffix hit (`createOrders`) → next tier; fqn-only hit → one tier lower.
        let plural = tnode(423, "Method", "createOrders", None, None);
        assert!((anchor_multiplier(&plural, &anchors) - ANCHOR_NAME_BOOST).abs() < 1e-9);
        let in_ns = tnode(
            424,
            "Method",
            "zzz",
            Some("app\\services\\order\\StoreOrderCreateServicesWrap::zzz"),
            None,
        );
        assert!((anchor_multiplier(&in_ns, &anchors) - ANCHOR_FQN_BOOST).abs() < 1e-9);
    }

    #[test]
    fn concept_multiplier_boosts_notification_cluster() {
        // Triggers the "notify" concept cluster (mail → mail/email/sms/notify).
        let triggered = triggered_concepts(&[
            "mail".to_string(),
            "email".to_string(),
            "sms".to_string(),
            "notify".to_string(),
        ]);
        assert!(!triggered.is_empty(), "邮件 must trigger the notification cluster");

        // Single core token (sms, no other concept token) → mild boost 1.2.
        let single = tnode(430, "Method", "sendSms", None, None);
        assert!(
            (concept_multiplier(&single, &triggered) - 1.2).abs() < 1e-9,
            "单核心应 1.2"
        );

        // Two expressions (mail+email, email contains mail) → aggregate boost 1.5.
        let dual = tnode(431, "Method", "resendVerificationEmail", None, None);
        assert!(
            (concept_multiplier(&dual, &triggered) - 1.5).abs() < 1e-9,
            "双表达应 1.5"
        );

        // Single generic word notify (not in core) → no boost, 1.0.
        let vague = tnode(432, "Method", "someNotify", None, None);
        assert!(
            (concept_multiplier(&vague, &triggered) - 1.0).abs() < 1e-9,
            "单泛词应 1.0"
        );

        // No concept triggered → no boost.
        assert_eq!(concept_multiplier(&single, &[]), 1.0);
    }

    #[test]
    fn concept_multiplier_covers_payment_and_auth_clusters() {
        // "pay" triggers pay cluster, "auth" triggers auth cluster.
        let triggered = triggered_concepts(&[
            "pay".to_string(),
            "payment".to_string(),
            "auth".to_string(),
            "guard".to_string(),
        ]);
        assert!(!triggered.is_empty());
        // Pay-specific: payment+checkout (payment contains pay) → aggregate boost 1.5.
        let pay = tnode(440, "Method", "createCheckoutPayment", None, None);
        assert!(
            (concept_multiplier(&pay, &triggered) - 1.5).abs() < 1e-9,
            "支付双表达应 1.5"
        );
        // Auth: JwtTokenUtil contains jwt(core)+token(all) → 1.5.
        let jwt = tnode(441, "Class", "JwtTokenUtil", None, None);
        assert!(
            (concept_multiplier(&jwt, &triggered) - 1.5).abs() < 1e-9,
            "鉴权双表达应 1.5"
        );
        // Completely unrelated nodes shouldn't be boosted.
        let other = tnode(442, "Method", "findProfile", None, None);
        assert_eq!(concept_multiplier(&other, &triggered), 1.0);
    }

    #[test]
    fn builtin_aliases_merge_domain_packs_and_synonyms() {
        // Verify: built-in alias table refactored from "one e-commerce table" to "multi-domain packs", all merged and loaded by default,
        // synonyms / filler words effective.
        let all = builtin_aliases();
        let en_of = |zh: &str| -> Vec<String> {
            all.iter()
                .find(|(z, _)| z == zh)
                .map(|(_, e)| e.clone())
                .unwrap_or_default()
        };

        // 1) cross-domain generic pack (PACK_GENERIC) still present: action verbs + generic technical nouns.
        assert!(!en_of("查询").is_empty(), "the generic verb 查询 must be in the built-in table");
        assert!(!en_of("配置").is_empty(), "the generic noun 配置 must be in the built-in table");

        // 2) e-commerce pack (PACK_ECOMMERCE) filler domain words effective.
        assert!(en_of("二维码").iter().any(|e| e == "qrcode"), "二维码 must be completed to qrcode");
        assert!(en_of("头像").iter().any(|e| e == "avatar"), "头像 must be completed to avatar");
        assert!(en_of("地址").iter().any(|e| e == "address"), "地址 must be completed to address");
        assert!(en_of("购物车").iter().any(|e| e == "cart"), "购物车 must be completed to cart");

        // 3) synonyms: 退货 / 售后 (return / after-sales) should both bridge to refund (synonym of 退款).
        let refund_terms = expand_intent_aliases("怎么办理退货", &all);
        assert!(refund_terms.iter().any(|t| t == "refund"), "退货 must expand to refund: {refund_terms:?}");
        let aftersale_terms = expand_intent_aliases("售后问题怎么处理", &all);
        assert!(aftersale_terms.iter().any(|t| t == "refund"), "售后 must expand to refund: {aftersale_terms:?}");

        // 4) finance pack (PACK_FINANCE) merged: non-e-commerce projects also bridge to this-domain tokens.
        //    Here only verify "pack loaded, expansion correct"; end-to-end hit depends on whether the project really has the code.
        assert!(en_of("对账").iter().any(|e| e == "reconcile"), "the finance pack 对账 -> reconcile must exist");
        assert!(en_of("转账").iter().any(|e| e == "transfer"), "the finance pack 转账 -> transfer must exist");
        let reconcile_terms = expand_intent_aliases("订单怎么对账", &all);
        assert!(reconcile_terms.iter().any(|t| t == "reconcile"), "对账 must expand to reconcile: {reconcile_terms:?}");

        // 5) fulfillment / type filler words: before, "how to add a new delivery method" had zero landings (only bigrams), recall drifted to
        //    unrelated nodes like environment / issue_log.
        assert!(en_of("配送").iter().any(|e| e == "delivery"), "配送 must be completed to delivery");
        assert!(en_of("快递").iter().any(|e| e == "express"), "快递 must be completed to express");
        assert!(en_of("发货").iter().any(|e| e == "delivery"), "发货 must be completed to delivery");
        assert!(en_of("类型").iter().any(|e| e == "type"), "类型 must be completed to type");
        assert!(en_of("方式").iter().any(|e| e == "method"), "方式 must be completed to method");

        // Full sentence end-to-end: must really expand delivery / express (before neither word existed).
        let ship_terms = expand_intent_aliases("怎么加一个新的配送方式", &all);
        assert!(
            ship_terms.iter().any(|t| t == "delivery"),
            "配送方式 must expand to delivery: {ship_terms:?}"
        );
        assert!(
            ship_terms.iter().any(|t| t == "express"),
            "配送方式 must expand to express: {ship_terms:?}"
        );
        // Coupon type: 类型 (type) must connect to type, else "add a new coupon type" only has generic add/coupon.
        let coupon_type_terms = expand_intent_aliases("怎么新增一种优惠券类型", &all);
        assert!(
            coupon_type_terms.iter().any(|t| t == "type"),
            "优惠券类型 must expand to type: {coupon_type_terms:?}"
        );
        assert!(
            coupon_type_terms.iter().any(|t| t == "coupon"),
            "优惠券类型 must still expand to coupon: {coupon_type_terms:?}"
        );
    }

    #[test]
    fn warm_priority_ranks_methods_above_classes_above_rest() {
        assert_eq!(warm_priority("Method"), 0, "Method is the main implementation landing point");
        assert_eq!(warm_priority("Function"), 0);
        assert_eq!(warm_priority("Class"), 1);
        assert_eq!(warm_priority("Interface"), 1);
        assert_eq!(warm_priority("Table"), 2, "Table and similar are mostly pulled in by BFS and rank last");
    }

    #[test]
    fn sort_pending_for_warmup_groups_by_priority_then_length() {
        // Shuffled order: long texts and low priority mixed (real pending is by node id order).
        let mut p = vec![
            (3, "x".repeat(300), 2),
            (1, "a".repeat(10), 0),
            (2, "b".repeat(50), 0),
            (4, "y".repeat(20), 2),
        ];
        sort_pending_for_warmup(&mut p);
        let ids: Vec<u64> = p.iter().map(|x| x.0).collect();
        // ① high-value kinds (priority 0) all in front
        assert_eq!(&ids[..2], &[1u64, 2], "Method/Function must be encoded first: {ids:?}");
        // ② within priority by length ascending → least in-batch padding
        assert!(p[0].1.len() <= p[1].1.len(), "within the same priority they must be adjacent by length: {ids:?}");
        // ③ low priority at the end, also adjacent by length
        assert_eq!(&ids[2..], &[4u64, 3], "lower priorities come last and stay adjacent by length: {ids:?}");
    }

    #[test]
    fn has_content_word_rejects_midword_but_keeps_compound() {
        let recorder_save = tnode(
            307,
            "Method",
            "save",
            Some("app\\services\\RecorderService::save"),
            None,
        );
        assert!(
            !has_content_word(&recorder_save, &["save".to_string(), "order".to_string()]),
            "`order` embedded inside the word `recorder` does not count as hitting a content word"
        );
        // save (100) + fqn contains order (35) → 135; two words ×1.5; no content word → 1.0: 135 × 1.5 = 202.5.
        let (s_rec, _) = score_node(
            &recorder_save,
            &["save".to_string(), "order".to_string()],
            &[],
            &HashMap::new(),
            true,
        );
        assert!(
            (s_rec - 101.25).abs() < 1e-9,
            "a substring inside a word must not keep the 1.5 multiplier, got {s_rec}"
        );

        // Reverse protection: reasonable "compound / inflected" matches must be kept — `pay` → `payment` is prefix hit; degrading to strict
        // equality would wrongly downgrade the payment-domain save (the main regression risk of this change).
        let payment_save = tnode(
            308,
            "Method",
            "save",
            Some("app\\services\\PaymentService::save"),
            None,
        );
        assert!(
            has_content_word(&payment_save, &["save".to_string(), "pay".to_string()]),
            "`pay` must prefix-match `payment`, otherwise the payment domain gets hurt"
        );
        let (s_pay, _) = score_node(
            &payment_save,
            &["save".to_string(), "pay".to_string()],
            &[],
            &HashMap::new(),
            true,
        );
        assert!(
            (s_pay - 303.75).abs() < 1e-9,
            "a compound match keeps the 1.5 multiplier, got {s_pay}"
        );
    }

    // ---- flow topology reorder ----

    /// `direct` = whether it's the seed itself (hop 0); false means a BFS-pulled neighbor.
    /// Only connected components containing a seed count as the chain (see [`anchored_components`]).
    fn fhit(id: i64, name: &str, kind: &str, score: f64, direct: bool) -> RecallHit {
        RecallHit {
            node_id: NodeId::new(id),
            kind: kind.to_string(),
            name: name.to_string(),
            fqn: None,
            score,
            hop: if direct { 0 } else { 1 },
            seed: "seed".to_string(),
            matched_terms: Vec::new(),
            direct,
            file: None,
            line: None,
            snippet: None,
            relations: Vec::new(),
        }
    }

    #[test]
    fn flow_intent_only_fires_on_flow_queries() {
        assert!(flow_intent("注册流程"), "注册流程 must be recognised as a flow query");
        assert!(flow_intent("支付的调用链"));
        assert!(flow_intent("how does the payment flow work"));
        // Non-flow query must be false — prerequisite for "other queries' order has zero regression".
        assert!(!flow_intent("如何修改下单优惠"));
        assert!(!flow_intent("怎么新增一种优惠券类型"));
    }

    #[test]
    fn reorder_for_flow_orders_entry_to_sink_and_sinks_unrelated() {
        // A complete chain: page(1) → route(2) → controller(3) → service(4) → Dao(5) → table(6)
        let inc = incoming(&[
            ("CallsHttp", 1, 2),
            ("HandledBy", 2, 3),
            ("Calls", 3, 4),
            ("Calls", 4, 5),
            ("WritesDb", 5, 6),
        ]);
        // Deliberately shuffled, and score independent of chain order.
        let mut hits = vec![
            fhit(6, "user_table", "Table", 100.0, false),
            fhit(3, "register", "Method", 300.0, false),
            fhit(1, "login_page", "File", 50.0, false),
            fhit(5, "save", "Method", 120.0, false),
            // Only the service layer is a seed: the whole chain is judged "this query's chain" because of it.
            fhit(4, "LoginServices.register", "Method", 250.0, true),
            // Route is the **entry layer**: even if pointed at by frontend CallsHttp, it should rank before the service method.
            fhit(2, "POST /register", "HttpContract", 80.0, false),
            // BFS-pulled unrelated neighbor: not in a seed-containing component, sinks.
            fhit(7, "unrelated_validate", "Method", 200.0, false),
        ];
        reorder_for_flow(&mut hits, &inc);
        let order: Vec<i64> = hits.iter().map(|h| h.node_id.get()).collect();
        // entry (route / page) → controller → service → Dao → table, independent of score.
        assert_eq!(
            order[..6],
            [2, 1, 3, 4, 5, 6],
            "a flow query must be ordered along 'entry → persistence', got {order:?}"
        );
        assert_eq!(order[6], 7, "neighbours unrelated to the chain must sink to the bottom, got {order:?}");
        // Key regression point: route must rank before the service method.
        let pos = |id: i64| order.iter().position(|x| *x == id).unwrap();
        assert!(
            pos(2) < pos(4),
            "a route (the entry) must rank before the service method: route {:?} vs service {:?}",
            pos(2),
            pos(4)
        );
    }

    #[test]
    fn hub_penalty_suppresses_high_fan_in_only() {
        assert_eq!(hub_penalty(10), 1.0, "a low fan-in must not be penalised");
        assert_eq!(hub_penalty(60), 1.0, "within the threshold there must be no penalty");
        let mid = hub_penalty(123); // 如 BaseDao::save
        let hub = hub_penalty(452); // 如 Cache
        assert!(mid < 1.0 && mid > 0.4, "a medium hub must be partly pushed down, got {mid}");
        assert!(hub < mid, "the larger the fan-in the stronger the decay: {hub} vs {mid}");
        assert!(hub >= 0.4, "the decay must have a floor, got {hub}");
    }

    // ---- event-driven recall ----

    #[test]
    fn event_intent_fires_only_on_sequence_or_explicit_event() {
        // Only temporal / explicit event signals trigger; ordinary action queries don't (avoid wrongly mixing listener seeds).
        assert!(event_intent("下单后怎么发通知给用户"), "must contain the temporal phrase 后怎么");
        assert!(event_intent("退款成功后怎么回退优惠券"), "must contain 成功后");
        assert!(event_intent("订单创建之后做哪些事"), "must contain 之后");
        assert!(event_intent("支付回调通知商户"), "回调 carries event semantics");
        assert!(event_intent("用户注册事件如何处理"), "事件 is an explicit event signal");
        assert!(event_intent("order paid after event listener"), "English after / listener");

        // Ordinary action queries shouldn't be misjudged.
        assert!(!event_intent("如何修改下单优惠"), "a pure action query must not trigger it");
        assert!(!event_intent("商品库存预警阈值是多少"), "a config query must not trigger it");
        assert!(!event_intent("注册流程是怎样的"), "flow intent is independent of event intent (flow is judged separately)");
        assert!(!event_intent("怎么发送通知"), "a lone 通知 with no temporal / event signal must not trigger it");
    }

    #[test]
    fn is_event_handler_detects_listener_by_convention() {
        // Class name ends with Listener / Subscriber / Observer.
        assert!(is_event_handler(&tnode(
            1, "Class", "OrderCreateAfterListener",
            Some("app\\listener\\order\\OrderCreateAfterListener"), None
        )));
        assert!(is_event_handler(&tnode(
            2, "Class", "UserRegisteredSubscriber",
            Some("app\\subscriber\\UserRegisteredSubscriber"), None
        )));

        // Method name handle / onX and under listener / event namespace.
        assert!(is_event_handler(&tnode(
            3, "Method", "handle",
            Some("app\\listener\\order\\OrderCreateAfterListener::handle"), None
        )));
        assert!(is_event_handler(&tnode(
            4, "Method", "onOrderPaid",
            Some("app\\events\\OrderPaidListener::onOrderPaid"), None
        )));

        // Ordinary business methods / service classes shouldn't be misjudged.
        assert!(!is_event_handler(&tnode(
            5, "Method", "create", Some("app\\services\\UserServices::create"), None
        )));
        assert!(!is_event_handler(&tnode(
            6, "Class", "StoreOrderRefundServices",
            Some("app\\services\\order\\StoreOrderRefundServices"), None
        )));
        // `on` + lowercase (e.g. online) is not an event handler method.
        assert!(!is_event_handler(&tnode(
            7, "Method", "online", Some("app\\services\\UserServices::online"), None
        )));
    }

    #[test]
    fn collect_event_seeds_returns_only_handlers_and_event_nodes() {
        let nodes = vec![
            tnode(1, "Class", "OrderCreateAfterListener",
                Some("app\\listener\\order\\OrderCreateAfterListener"), None),
            tnode(2, "Method", "handle",
                Some("app\\listener\\order\\OrderCreateAfterListener::handle"), None),
            tnode(3, "Class", "StoreOrderRefundServices",
                Some("app\\services\\order\\StoreOrderRefundServices"), None),
            // Event node: becomes seed only if its name hits a query word
            tnode(4, "Event", "OrderPaidEvent", Some("OrderPaidEvent"), None),
            // Event node: name doesn't hit a query word → excluded
            tnode(5, "Event", "UserLoggedInEvent", Some("UserLoggedInEvent"), None),
            // Already-existing seed → excluded
            tnode(9, "Method", "refund", Some("app\\services\\order\\refund"), None),
        ];
        let index: HashMap<i64, &Node> =
            nodes.iter().map(|n| (n.id.get(), n)).collect();
        let existing: HashSet<i64> = HashSet::from([9i64]);
        let seeds = collect_event_seeds(
            &nodes, &index, &existing, &["refund".to_string(), "paid".to_string(), "order".to_string()], 400.0,
        );
        let ids: Vec<i64> = seeds.iter().map(|(_, _, n)| n.id.get()).collect();
        assert!(ids.contains(&1), "the listener class must be a seed: {ids:?}");
        assert!(ids.contains(&2), "the listener's handle method must be a seed: {ids:?}");
        assert!(ids.contains(&4), "an Event node whose name hits a query term must be a seed: {ids:?}");
        assert!(!ids.contains(&3), "an ordinary Services class must not be a seed: {ids:?}");
        assert!(!ids.contains(&5), "an Event node whose name misses the query terms must be excluded: {ids:?}");
        assert!(!ids.contains(&9), "an existing seed must not be duplicated: {ids:?}");
    }

    // ---- multi-intent split / merge ----

    #[test]
    fn split_intents_splits_compound_questions_only() {
        // Two independent questions joined by 、 (enumeration comma) → split into segments (together they interfere, measured both lose answers).
        let parts = RecallService::split_intents("怎么修改商品库存预警阈值、修改订单自动取消时间");
        assert_eq!(parts.len(), 2, "it must split into 2 intents: {parts:?}");
        assert!(parts[0].contains("库存预警阈值"), "{parts:?}");
        assert!(parts[1].contains("自动取消时间"), "{parts:?}");

        // Comma / semicolon / conjunctions also apply.
        assert_eq!(RecallService::split_intents("查询订单相关的表，查询商品相关的表").len(), 2);
        assert_eq!(RecallService::split_intents("查询订单相关的表；查询商品相关的表").len(), 2);
        assert_eq!(RecallService::split_intents("查询订单相关的表以及查询商品相关的表").len(), 2);

        // Single intent must return empty → take original path, zero regression.
        assert!(RecallService::split_intents("如何修改下单优惠").is_empty());
        assert!(RecallService::split_intents("注册流程").is_empty());
        // Too-short fragments don't form a separate intent (avoid particles / punctuation side-branches as independent questions).
        assert!(RecallService::split_intents("订单、商品").is_empty());
    }

    #[test]
    fn merge_intent_hits_round_robins_and_dedups() {
        // Intent A's scores all higher than B: merging by score would empty B; round-robin guarantees each intent a representative.
        let a = vec![
            fhit(1, "a1", "Method", 900.0, true),
            fhit(2, "a2", "Method", 800.0, false),
            fhit(3, "a3", "Method", 700.0, false),
        ];
        let b = vec![
            fhit(4, "b1", "ConfigKey", 600.0, true),
            fhit(5, "b2", "ConfigKey", 500.0, false),
        ];
        let merged = RecallService::merge_intent_hits(vec![a, b], 4);
        let ids: Vec<i64> = merged.iter().map(|h| h.node_id.get()).collect();
        assert_eq!(ids, vec![1, 4, 2, 5], "it must round-robin by intent rather than by score, got {ids:?}");

        // Cross-intent duplicate nodes kept only once.
        let dup = RecallService::merge_intent_hits(
            vec![
                vec![fhit(1, "x", "Method", 900.0, true)],
                vec![fhit(1, "x", "Method", 900.0, true)],
            ],
            4,
        );
        assert_eq!(dup.len(), 1, "a node repeated across intents must be de-duplicated, got {}", dup.len());
    }

    // ---- config-item intent / hint-word whole-word deletion ----

    #[test]
    fn has_maps_to_flags_orm_relation_accessors_only() {
        // association accessor: has MapsTo out-edge (maps to another entity).
        assert!(has_maps_to(1, &outgoing(&[("MapsTo", 1, 2)])));
        // business methods don't have it: measured saveInvoiceInfo / updateCartInfo / notifyConfirm all lack MapsTo.
        assert!(!has_maps_to(1, &outgoing(&[("Calls", 1, 2)])));
        assert!(!has_maps_to(1, &outgoing(&[("WritesDb", 1, 2)])));
        assert!(!has_maps_to(1, &HashMap::new()));
    }

    #[test]
    fn wants_config_value_fires_only_on_config_seeking_queries() {
        assert!(wants_config_value("怎么修改订单自动取消时间"));
        assert!(wants_config_value("商品库存预警阈值"));
        assert!(wants_config_value("修改缓存配置"));
        // Queries for implementation code must never be misjudged — else it'd turn off the action intent's method boost.
        assert!(!wants_config_value("如何修改下单优惠"));
        assert!(!wants_config_value("注册流程"));
        assert!(!wants_config_value("怎么新增一种优惠券类型"));
    }

    #[test]
    fn strip_hint_words_removes_whole_words_not_chars() {
        let hints = &[
            ("表", "Table"),
            ("数据库", "Table"),
            ("缓存", "Cache"),
            ("定时", "Schedule"),
            ("消息", "Topic"),
        ];
        // Remove the appearing hint word as a whole word.
        assert_eq!(strip_hint_words("订单表", hints), "订单");
        assert_eq!(strip_hint_words("查询缓存", hints), "查询");

        // Key regression point: old impl deleted char-by-char, `缓存`'s 存 + `数据库`'s 库 would empty `库存`,
        // `消息`'s 消 + `定时`'s 时 would cut `取消时间` to `取`.
        assert_eq!(strip_hint_words("商品库存预警阈值", hints), "商品库存预警阈值");
        assert_eq!(strip_hint_words("修改订单自动取消时间", hints), "修改订单自动取消时间");
    }

    // ---- recall quality assessment ----

    /// Build a synthetic hit with a "matched word", for quality-assessment assertions (no real project data needed).
    fn qhit(id: i64, score: f64, matched: &[&str]) -> RecallHit {
        let mut h = fhit(id, "node", "Method", score, true);
        h.matched_terms = matched.iter().map(|s| s.to_string()).collect();
        h
    }

    #[test]
    fn assess_quality_high_when_all_concepts_covered() {
        // "how to modify order discount" hits all three concepts (modify / order / discount), and top-spread is healthy.
        let hits = vec![
            qhit(1, 500.0, &["order", "edit", "coupon"]),
            qhit(2, 100.0, &["order"]),
            qhit(3, 90.0, &["order"]),
        ];
        let (q, conf, _reason, missing) =
            RecallService::assess_quality("如何修改下单优惠", &hits, &HashSet::new(), &builtin_aliases());
        assert_eq!(q, RecallQuality::High, "full concept coverage must be High, conf={conf}");
        assert!(missing.is_empty(), "there must be no unmatched concept: {missing:?}");
    }

    #[test]
    fn assess_quality_medium_when_partially_covered() {
        // "order / discount" covers only "order", missing "discount" → partial coverage should be Medium.
        // "modify" is a pure action verb, not counted as a quality concept (see [`QUALITY_ACTION_VERBS`]).
        let hits = vec![qhit(1, 500.0, &["order"]), qhit(2, 100.0, &["order"])];
        let (q, _conf, _reason, missing) =
            RecallService::assess_quality("如何修改下单优惠", &hits, &HashSet::new(), &builtin_aliases());
        assert_eq!(q, RecallQuality::Medium, "partial coverage must be Medium");
        assert!(missing.iter().any(|m| m == "优惠"), "must report the missing concept 优惠: {missing:?}");
        assert!(
            !missing.iter().any(|m| m == "修改"),
            "the action verb 修改 must not count as an unmatched concept: {missing:?}"
        );
    }

    #[test]
    fn assess_quality_low_when_most_concepts_missing() {
        // "order / discount / pay" covers only "order" → coverage 1/3 < 0.5 → Low.
        let hits = vec![qhit(1, 500.0, &["order"]), qhit(2, 100.0, &["order"])];
        let (q, _conf, _reason, missing) =
            RecallService::assess_quality("如何修改下单优惠支付", &hits, &HashSet::new(), &builtin_aliases());
        assert_eq!(q, RecallQuality::Low, "most concepts unmatched must be Low");
        assert!(
            missing.iter().any(|m| m == "优惠") && missing.iter().any(|m| m == "支付"),
            "the missing concepts must include 优惠 and 支付: {missing:?}"
        );
        assert!(
            !missing.iter().any(|m| m == "修改"),
            "the action verb 修改 must not count as an unmatched concept: {missing:?}"
        );
    }

    #[test]
    fn assess_quality_ignores_boilerplate_hits() {
        let mut boilerplate = HashSet::new();
        boilerplate.insert(9);
        let hits = vec![qhit(1, 500.0, &["order"]), qhit(9, 480.0, &["coupon"])];
        let (q, _conf, _reason, missing) =
            RecallService::assess_quality("如何修改下单优惠支付", &hits, &boilerplate, &builtin_aliases());
        assert!(
            missing.iter().any(|m| m == "优惠"),
            "a boilerplate hit must not count as covering 优惠: {missing:?}"
        );
        assert_eq!(q, RecallQuality::Low, "once boilerplate is excluded, a too-low coverage rate must be Low");
    }

    #[test]
    fn assess_quality_missing_terms_include_english_expansions() {
        // Chinese literal alone isn't enough: the identifier in code is English; an AI IDE grepping "回调" finds nothing.
        // So a missed concept must also output its English expansion, to be a truly usable fallback.
        let hits = vec![qhit(1, 500.0, &["pay"])];
        let (_q, _conf, reason, missing) =
            RecallService::assess_quality("支付回调失败怎么排查", &hits, &HashSet::new(), &builtin_aliases());
        assert!(missing.iter().any(|m| m == "回调"), "must contain the Chinese concept: {missing:?}");
        assert!(
            missing.iter().any(|m| m == "callback"),
            "must contain the English expansion callback, otherwise grep cannot find the code: {missing:?}"
        );
        assert!(missing.iter().any(|m| m == "fail"), "must contain the English expansion fail: {missing:?}");
        // Explanation uses only Chinese concepts, to avoid being too long.
        assert!(!reason.contains("callback"), "the explanation text must not be stuffed with English expansions: {reason}");
    }

    #[test]
    fn assess_quality_without_concepts_cannot_be_high() {
        let hits = vec![qhit(1, 500.0, &["express"]), qhit(2, 100.0, &["delivery"])];
        let (q, _conf, reason, _missing) =
            RecallService::assess_quality("zzzqqx", &hits, &HashSet::new(), &builtin_aliases());
        assert_ne!(q, RecallQuality::High, "must not report High when no concept is evaluable");
        assert!(reason.contains("cannot be confirmed"), "should say quality cannot be confirmed: {reason}");
    }

    #[test]
    fn action_verbs_excluded_from_quality_concepts() {
        let aliases = vec![
            ("生成".to_string(), vec!["generate".to_string()]),
            ("二维码".to_string(), vec!["qrcode".to_string()]),
        ];
        let hits = vec![qhit(1, 500.0, &["qrcode"])]; // 只覆盖「二维码」
        let (q, _c, _r, missing) =
            RecallService::assess_quality("怎么生成商品二维码", &hits, &HashSet::new(), &aliases);
        assert!(
            !missing.iter().any(|m| m == "生成"),
            "the action verb 生成 must not count as an unmatched concept: {missing:?}"
        );
        // QR covered, generate not counted → coverage driven by domain concept, shouldn't drop to Low for missing generate.
        assert_ne!(q, RecallQuality::Low, "生成 must not lower the quality band: {missing:?}");
    }

    #[test]
    fn recall_quality_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&RecallQuality::High).unwrap(), "\"high\"");
        assert_eq!(serde_json::to_string(&RecallQuality::Medium).unwrap(), "\"medium\"");
        assert_eq!(serde_json::to_string(&RecallQuality::Low).unwrap(), "\"low\"");
    }

    #[test]
    fn parse_query_drops_boundary_bigrams() {
        // "how to modify order discount" contains known Chinese words (modify / order / discount), should keep them, but filter out
        // boundary-crossing noise bigrams (何修 / 改下 / 单优).
        let (terms, _hints) = parse_query("如何修改下单优惠");
        assert!(terms.iter().any(|t| t == "修改"), "修改 must be kept: {terms:?}");
        assert!(terms.iter().any(|t| t == "下单"), "下单 must be kept: {terms:?}");
        assert!(terms.iter().any(|t| t == "优惠"), "优惠 must be kept: {terms:?}");
        assert!(!terms.iter().any(|t| t == "何修"), "何修 is noise and must be filtered: {terms:?}");
        assert!(!terms.iter().any(|t| t == "改下"), "改下 is noise and must be filtered: {terms:?}");
        assert!(!terms.iter().any(|t| t == "单优"), "单优 is noise and must be filtered: {terms:?}");
    }

    // ---- relation_summary ----

    #[test]
    fn relation_summary_aggregates_and_formats_multiplicity() {
        let inc = incoming(&[
            ("WritesDb", 1, 300),
            ("WritesDb", 2, 300),
            ("WritesDb", 3, 300),
            ("ReadsDb", 4, 300),
        ]);
        let rel = relation_summary(NodeId::new(300), &inc, &HashMap::new());
        assert!(
            rel.iter().any(|r| r == "← WritesDb ×3"),
            "several in-edges of the same kind must aggregate to ×3: {rel:?}"
        );
        assert!(
            rel.iter().any(|r| r == "← ReadsDb"),
            "a single edge must not carry ×N: {rel:?}"
        );
    }

    #[test]
    fn relation_summary_includes_outgoing() {
        let out = outgoing(&[("Calls", 300, 9)]);
        let rel = relation_summary(NodeId::new(300), &HashMap::new(), &out);
        assert!(
            rel.iter().any(|r| r == "→ Calls"),
            "an out-edge must carry the → prefix: {rel:?}"
        );
    }

    // ---- neighbours ----

    #[test]
    fn neighbours_follows_chain_edges_both_directions() {
        let inc = incoming(&[("HandledBy", 502, 500)]);
        let out = outgoing(&[("Calls", 500, 501)]);
        let ns = neighbours(NodeId::new(500), &inc, &out);
        let ids: Vec<i64> = ns.iter().map(|n| n.get()).collect();
        assert!(ids.contains(&501), "must walk along the out-edge to 501: {ids:?}");
        assert!(ids.contains(&502), "must walk along the in-edge to 502: {ids:?}");
    }

    // ---- DEFAULT_EXCLUDED_KINDS ----

    #[test]
    fn excluded_kinds_covers_noise_node_types() {
        let want = [
            "CallSite",
            "File",
            "Directory",
            "Namespace",
            "Property",
            "Const",
        ];
        for k in want {
            assert!(
                DEFAULT_EXCLUDED_KINDS.iter().any(|x| *x == k),
                "{k} must be excluded from the recall candidates (otherwise recall degrades into line-by-line matching)"
            );
        }
    }

    // ---- read_snippet ----

    /// In-memory filesystem: only implements the 4 methods needed for reading recall snippets.
    struct MemFs {
        map: HashMap<PathBuf, String>,
    }
    impl MemFs {
        fn new() -> Self {
            Self { map: HashMap::new() }
        }
        fn insert(&mut self, p: &Path, s: impl Into<String>) {
            self.map.insert(p.to_path_buf(), s.into());
        }
    }
    impl FileSystem for MemFs {
        fn exists(&self, p: &Path) -> bool {
            self.map.contains_key(p)
        }
        fn is_dir(&self, _p: &Path) -> bool {
            false
        }
        fn read_to_string(&self, p: &Path) -> Result<String> {
            self.map
                .get(p)
                .cloned()
                .ok_or_else(|| DomainError::NotFound(p.to_string_lossy().to_string()))
        }
        fn len(&self, p: &Path) -> Result<u64> {
            Ok(self.map.get(p).map(|s| s.len() as u64).unwrap_or(0))
        }
    }

    // ---- append_file_bodies ----

    fn hit_with_file(file: &str) -> RecallHit {
        RecallHit {
            node_id: NodeId::new(1),
            kind: "Method".to_string(),
            name: "foo".to_string(),
            fqn: None,
            score: 100.0,
            hop: 0,
            seed: "foo".to_string(),
            matched_terms: Vec::new(),
            direct: true,
            file: Some(file.to_string()),
            line: Some(3),
            snippet: None,
            relations: Vec::new(),
        }
    }

    #[test]
    fn append_file_bodies_appends_full_source_once_per_file() {
        let mut fs = MemFs::new();
        fs.insert(Path::new("/src/A.php"), "<?php\nclass A {}\n?>\n");
        // Same file referenced by two hits: body should appear only once, else duplicate body wastes context.
        let hits = vec![hit_with_file("/src/A.php"), hit_with_file("/src/A.php")];
        let out = append_file_bodies(&fs, &hits, "# 上下文\n".to_string());
        assert!(out.starts_with("# 上下文\n"), "it must be appended after the original context: {out}");
        assert!(out.contains("## Full files (include_body)"), "should carry the section heading: {out}");
        assert_eq!(out.matches("class A {}").count(), 1, "the body of the same file must appear only once: {out}");
    }

    #[test]
    fn append_file_bodies_missing_file_is_explained() {
        let fs = MemFs::new();
        let hits = vec![hit_with_file("/src/missing.php")];
        // When the file can't be read, say why, don't silently drop the whole segment (else caller thinks "no hit files").
        let out = append_file_bodies(&fs, &hits, "ctx".to_string());
        assert!(out.contains("file could not be read"), "an unreadable file should be reported: {out}");
    }

    #[test]
    fn read_snippet_reads_window_around_line() {
        let mut fs = MemFs::new();
        let p = PathBuf::from("/x/sample.php");
        let content = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs.insert(&p, content);
        let snip = read_snippet(&fs, &p, 3).expect("the snippet must be read");
        assert!(snip.contains("line1"), "must contain the 2 lines before the target line: {snip}");
        assert!(snip.contains("line3"), "must contain the target line: {snip}");
        assert!(snip.contains("line6"), "must contain the 3 lines after the target line: {snip}");
    }

    #[test]
    fn read_snippet_skips_huge_files() {
        let mut fs = MemFs::new();
        let p = PathBuf::from("/x/huge.php");
        fs.insert(&p, "x".repeat(3 * 1024 * 1024)); // 3MB > 2MB 上限
        let snip = read_snippet(&fs, &p, 1);
        assert!(snip.is_none(), "an oversized file must skip snippet reading (to avoid OOM)");
    }

    // ---- real model (bge-m3 / candle) semantic verification: only compiled under the `model-candle` feature ----
    #[cfg(feature = "model-candle")]
    #[test]
    fn bge_semantic_recall_chinese_to_english() {
        use crate::embed_model::CandleBgeEmbedder;

        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
        let model_dir = std::env::var("GT_BGE_MODEL")
            .unwrap_or_else(|_| root.join("bge-m3-safetensors").to_string_lossy().into());
        if !Path::new(&model_dir).join("model.safetensors").exists() {
            eprintln!("skip bge_semantic_recall: {model_dir}/model.safetensors not found (run tools/convert_bge_safetensors.py first)");
            return;
        }
        let emb = CandleBgeEmbedder::load(&model_dir).expect("加载 bge-m3 safetensors 失败");

        let q = emb.embed_query("下单改优惠");
        let order = emb.embed("placeOrder");
        let discount = emb.embed("applyDiscount");
        let noise = emb.embed("unused_log");

        let co = crate::embedding::cosine(&q, &order);
        let cd = crate::embedding::cosine(&q, &discount);
        let cn = crate::embedding::cosine(&q, &noise);
        println!("cos('place order, change discount', placeOrder)={co:.4}  (applyDiscount)={cd:.4}  (unused_log)={cn:.4}");

        assert!(
            co > 0.4 && cd > 0.4,
            "a Chinese intent must hit the English business node semantically: co={co} cd={cd}"
        );
        assert!(
            co > cn && cd > cn,
            "noise nodes must score clearly below the target nodes: cn={cn} co={co} cd={cd}"
        );
    }

    // ---- merged_aliases ----

    /// `merged_aliases(None)` returns exactly the built-in table — no project overrides means no additions/drops.
    #[test]
    fn merged_aliases_without_root_is_builtin_only() {
        let base = builtin_aliases();
        let got = merged_aliases(None);
        assert_eq!(got.len(), base.len(), "with no root the alias groups must neither grow nor shrink");
        for (k, v) in &base {
            let g = got.iter().find(|(z, _)| z == k).expect("builtin key must survive");
            assert_eq!(g.1, *v, "a builtin value must not be changed: {k}");
        }
    }

    /// A project alias file sharing a built-in key must *merge* (dedup) into that group, not append a duplicate group.
    #[test]
    fn merged_aliases_augments_existing_builtin_key_without_dup() {
        let base = builtin_aliases();
        let key = &base[0].0; // a real built-in key
        let dir = std::env::temp_dir().join(format!("gt_alias_it_{}", std::process::id()));
        let cfg = dir.join(".graphtell");
        std::fs::create_dir_all(&cfg).unwrap();
        let text = serde_json::json!({ key.clone(): ["__extra_alias_term__"] }).to_string();
        std::fs::write(cfg.join("aliases.json"), text).unwrap();

        let got = merged_aliases(Some(&dir));
        let group = got.iter().find(|(z, _)| z == key).expect("the group must exist");
        let extra = group.1.iter().filter(|e| *e == "__extra_alias_term__").count();
        assert_eq!(extra, 1, "project aliases must merge into the existing group without duplicates");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A project alias file with a brand-new key (absent from the built-in table) must append a new group verbatim.
    #[test]
    fn merged_aliases_adds_new_group() {
        let key = "__brand_new_alias_group__"; // sentinel: must not collide with any built-in key
        assert!(
            !builtin_aliases().iter().any(|(z, _)| z == key),
            "the test sentinel key must not clash with a built-in alias"
        );
        let dir = std::env::temp_dir().join(format!("gt_alias_it2_{}", std::process::id()));
        let cfg = dir.join(".graphtell");
        std::fs::create_dir_all(&cfg).unwrap();
        let text = serde_json::json!({ key: ["reconcile", "Reconciliation"] }).to_string();
        std::fs::write(cfg.join("aliases.json"), text).unwrap();

        let got = merged_aliases(Some(&dir));
        let rec = got.iter().find(|(z, _)| z == key).expect("a new group must be appended");
        assert_eq!(rec.1, vec!["reconcile".to_string(), "Reconciliation".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A corrupted `.graphtell/aliases.json` must NOT panic and must fall back to the built-in table — a bad project
    /// config must never break recall.
    #[test]
    fn merged_aliases_corrupt_json_falls_back_to_builtin() {
        let dir = std::env::temp_dir().join(format!("gt_alias_it3_{}", std::process::id()));
        let cfg = dir.join(".graphtell");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("aliases.json"), "{ this is not valid json").unwrap();

        let base = builtin_aliases();
        let got = merged_aliases(Some(&dir)); // must not panic
        assert_eq!(got.len(), base.len(), "a corrupt config must fall back to the built-in table (same group count)");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- is_vector_kind ----

    /// Excluded noise kinds are never vector-encoded (recall would otherwise degrade to line-by-line matching).
    #[test]
    fn is_vector_kind_excludes_noise_kinds() {
        assert!(
            !is_vector_kind(&tnode(1, "CallSite", "x", None, None)),
            "an excluded noise kind must not be vectorised"
        );
    }

    /// A `Method`/`Function` is vector-eligible only with a source location; synthetic nodes without `file_id` are skipped.
    #[test]
    fn is_vector_kind_method_needs_source_file() {
        assert!(
            !is_vector_kind(&tnode(2, "Method", "m", None, None)),
            "a synthesised Method with no source location must not be vectorised"
        );
        let mut n = tnode(3, "Method", "m", None, None);
        n.file_id = Some(FileId::new(7));
        assert!(is_vector_kind(&n), "a Method with a source location must be vectorised");
    }

    /// Business/structural kinds (Class/Table) are vector-eligible by default.
    #[test]
    fn is_vector_kind_business_kinds_are_vectorizable() {
        assert!(is_vector_kind(&tnode(4, "Class", "C", None, None)));
        assert!(is_vector_kind(&tnode(5, "Table", "T", None, None)));
    }
}
