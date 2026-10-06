#![allow(unused_imports)]
use super::*;

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
pub(crate) fn is_generic_crud_method(name: &str) -> bool {
    GENERIC_CRUD_METHODS.contains(&name)
}

/// Discount on the base match score for pure CRUD verb methods that hit only by the verb (no content word).
///
/// Just dropping the action boost isn't enough: such methods still get 100 on exact same-name match and dominate. After the
/// discount they yield to real domain answers. Those hitting a content word are completely unaffected.
pub(crate) const GENERIC_CRUD_VERB_ONLY_DISCOUNT: f64 = 0.5;

/// Discount factor for ORM **association accessors** (boilerplate).
///
/// Methods like `user()`/`refund()` have bodies of only `hasOne/hasMany/belongsTo`, no business logic; on the graph they
/// carry a `MapsTo` out-edge. They get 100 by exact same-name match with generic words; measured "notify user after order"
/// had four such in top five. At 0.3 they do not hog seed slots but can still be recalled.
pub(crate) const RELATION_ACCESSOR_DISCOUNT: f64 = 0.3;

/// Whether it's an ORM association accessor: has a `MapsTo` out-edge (maps to another entity).
pub(crate) fn has_maps_to(id: i64, outgoing: &HashMap<i64, Vec<gt_domain::model::Edge>>) -> bool {
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
pub(crate) const ANCHOR_EXACT_BOOST: f64 = 3.0;
/// Identifier prefix / suffix match (`createOrders` ↔ anchor `createOrder`).
pub(crate) const ANCHOR_NAME_BOOST: f64 = 2.2;
/// Located in the fqn (parent/child namespace or directory), weaker than the name.
pub(crate) const ANCHOR_FQN_BOOST: f64 = 1.6;

/// Discount factor for **test files**.
///
/// Developers don't look for business impl in unit tests, but `test/user.test.js` is full of symbols that "look like answers"
/// (`makeFakeUser`/`fixtureFilename`); measured: 4 of top 5 of "thumbnail after avatar upload" came from `test/**`. After
/// downweighting they can still be recalled, just without occupying slots.
pub(crate) const TEST_FILE_DISCOUNT: f64 = 0.55;

/// Discount factor for **generator boilerplate files** (MyBatis Generator `mall-mbg`/`generated-sources`…).
///
/// Such files are full of `andPaymentTimeIsNull`/`createCriteria`; a Chinese query gets full marks by lexical match yet has
/// zero business semantics — measured: 7 of mall's top8 came from here.
pub(crate) const GENERATED_FILE_DISCOUNT: f64 = 0.5;

/// Discount for Criteria / Example DSL chained method names.
///
/// Complements the path check: even when not under `mbg` (a `*Example` class copied into a business package also pollutes),
/// the shape `andXxxEqualTo`/`createCriteria` is itself a MyBatis Generator fingerprint.
pub(crate) const CRITERIA_BUILDER_DISCOUNT: f64 = 0.45;

/// Whether the path is **test code**: by path segments / filename, to avoid wrongly hitting business dirs like `Contest/`/`latest/`.
pub(crate) fn is_test_path(path: &str) -> bool {
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
pub(crate) fn is_generated_path(path: &str) -> bool {
    let norm = path.replace('\\', "/").to_lowercase();
    norm.split('/').any(|seg| {
        seg.contains("mbg") || seg.contains("generated") || seg.contains("_pb2")
    })
}

/// Whether it's a MyBatis Generator query-builder method: `example.createCriteria()`, `addCriterion(...)`,
/// `andPaymentTimeIsNull()`/`orStatusEqualTo()`…
///
/// Judged only by **shape** (camelCase + `and`/`or` prefix + capitalized 3rd letter), framework-independent.
pub(crate) fn is_criteria_builder_method(name: &str) -> bool {
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
pub(crate) fn file_noise_discount(path: &str) -> f64 {
    if is_test_path(path) {
        TEST_FILE_DISCOUNT
    } else if is_generated_path(path) {
        GENERATED_FILE_DISCOUNT
    } else {
        1.0
    }
}

/// The node's **file path** pointer (for [`node_noise_discount`] lookup).
pub(crate) fn node_file_path<'a>(node: &Node, files: &'a HashMap<i64, String>) -> Option<&'a str> {
    let fid = node.file_id.as_ref()?;
    files.get(&fid.get()).map(|s| s.as_str())
}

/// A node's combined downweight: ORM accessor × file (test/generated) × query-builder method.
pub(crate) fn node_noise_discount(
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
pub(crate) fn extract_anchors(query: &str, nodes: &[Node]) -> Vec<String> {
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
pub(crate) fn is_identifier_shaped(s: &str) -> bool {
    if s.chars().count() < 6 {
        return false;
    }
    if !s.chars().any(|c| c == '_' || c.is_uppercase()) {
        return false;
    }
    split_ident_tokens(s).len() >= 2
}

/// Whether the node hits some anchor, and the corresponding boost tier (see [`ANCHOR_EXACT_BOOST`]).
pub(crate) fn anchor_multiplier(node: &Node, anchors: &[String]) -> f64 {
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
pub(crate) const CONCEPT_CLUSTERS: &[(&[&str], &[&str])] = &[
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
pub(crate) fn concept_multiplier(node: &Node, clusters: &[(&[&str], &[&str])]) -> f64 {
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
pub(crate) fn triggered_concepts(
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
pub(crate) const GENERIC_NOUNS: &[&str] = &[
    "service", "api", "interface", "model", "entity", "config", "configuration", "data", "file",
    "log", "cache", "task", "job", "message", "session", "property", "attribute", "field", "page",
    "event", "queue", "topic", "schedule", "eventbus", "dict", "dictionary", "workbench",
    "dashboard", "third", "party",
    "user", "member", "admin", "agent", "common", "base",
];

/// Whether this token is a generic architecture noun (see [`GENERIC_NOUNS`]).
pub(crate) fn is_generic_noun(t: &str) -> bool {
    GENERIC_NOUNS.contains(&t)
}

/// Floor for the bootstrap score of event handlers (listeners/subscribers) as seeds in event-driven queries.
///
/// See [`collect_event_seeds`]: handler names are often extremely generic (uniformly `handle`), lexical score ≈0, they must enter
/// the seed set via bootstrap score to participate in BFS. Actual score is "strongest lexical seed × 0.85" clamped to [floor, 700],
/// so listeners and direct callees float to top without beating truly name-matched strong seeds.
pub(crate) const EVENT_SEED_MIN: f64 = 400.0;
/// Boost for an event handler's "direct callee": float the "listener → business handler" chain above an ordinary "seed → 1-hop neighbor".
pub(crate) const EVENT_CALLEE_BOOST: f64 = 1.6;
/// Event seed cap: many listeners may match; keep only top-N by relevance (matched query-word count) to avoid flooding BFS/list.
pub(crate) const EVENT_SEED_CAP: usize = 10;

/// Event-driven query recognition: user asks "how to Y after X"/"after success…"/"event/listen/callback…", reconstructing the
/// "trigger → listener → handler" chain. The answer is usually an event handler (generic `*Listener`/`handle`), not a name-matched method.
///
/// Only recognize "temporal / explicit event" signals, not action words like "notify/place order" — otherwise action queries would be
/// misjudged as event queries and mixed with listener seeds (action intent handled separately by [`action_intent`]).
pub(crate) fn event_intent(q: &str) -> bool {
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
pub(crate) fn is_event_handler(node: &Node) -> bool {
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
pub(crate) fn collect_event_seeds<'a>(
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
pub(crate) fn split_ident_tokens(s: &str) -> Vec<String> {
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
pub(crate) fn has_content_word(node: &Node, terms: &[String]) -> bool {
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
/// Node scoring: returns (score, matched query word).
pub(crate) fn score_node(
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
pub(crate) fn rank_weight(kind: &str, action: bool) -> f64 {
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
pub(crate) fn kind_weight(kind: &str) -> f64 {
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
pub(crate) fn action_intent(q: &str) -> bool {
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
pub(crate) const CONFIG_WORDS: &[&str] = &[
    "阈值", "参数", "开关", "上限", "下限", "时长", "间隔", "配置", "配置项", "预警",
    // "auto-cancel **time**" itself is a config value; missing it, the query would take the action intent (Method 1.5×) and push ConfigKey
    // down, and the correct answer `order_cancel_time` would drop off the list.
    "时间",
];

pub(crate) fn wants_config_value(q: &str) -> bool {
    CONFIG_WORDS.iter().any(|w| q.contains(w))
}

pub(crate) fn flow_intent(q: &str) -> bool {
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
pub(crate) fn is_flow_edge(kind: &str) -> bool {
    matches!(
        kind,
        "Calls" | "CallsHttp" | "HandledBy" | "PassesThrough" | "WritesDb" | "ReadsDb"
    )
}

/// **Entry-layer** kinds of a flow: HTTP contract / page. They are naturally the chain start — even if a frontend function points at a
/// route via `CallsHttp`, that route is still the backend's **entry**, not a middle node, and shouldn't be counted as "the next layer called
/// down by the frontend" (else the route ranks after the service method).
pub(crate) fn is_entry_kind(kind: &str) -> bool {
    matches!(kind, "HttpContract" | "Page")
}

/// Nodes with fan-in above this are "hubs": common infrastructure called everywhere (`Request` 300+ times, `Cache` 400+ times), not a
/// specific step of any flow.
pub(crate) const HUB_FANIN: usize = 60;

/// Hub decay factor: smoothly pushed down with fan-in, minimum 0.4, never 0 — the infrastructure can still be recalled, just without
/// crowding the front of the chain (see [`reorder_for_flow`]).
pub(crate) fn hub_penalty(fan_in: usize) -> f64 {
    if fan_in <= HUB_FANIN {
        return 1.0;
    }
    let excess = (fan_in - HUB_FANIN) as f64;
    0.4 + 0.6 * (-excess / 200.0).exp()
}

/// Longest-path depth along flow direction within the hit set (entry = 0). Back edges count as 0 to tolerate cycles.
pub(crate) fn flow_depth(
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
pub(crate) fn anchored_components(hits: &[RecallHit], adj: &HashMap<i64, Vec<i64>>) -> HashSet<i64> {
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
pub(crate) fn reorder_for_flow(
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
pub(crate) fn parse_query(query: &str) -> (Vec<String>, Vec<String>) {
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
pub(crate) fn segment_cjk(run: &str, known: &HashSet<String>) -> Vec<(String, bool)> {
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

pub(crate) fn is_cjk(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch)
}

/// Extract continuous CJK runs from the query (each run is a "word-boundary-less Chinese fragment").
pub(crate) fn cjk_runs(query: &str) -> Vec<String> {
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
pub(crate) fn strip_hint_words(s: &str, hint_map: &[(&str, &str)]) -> String {
    let mut out = s.to_string();
    for (word, _) in hint_map {
        if out.contains(word) {
            out = out.replace(word, "");
        }
    }
    out
}

/// After splitting camelCase / snake_case, push tokens into the list.
pub(crate) fn push_token(terms: &mut Vec<String>, raw: &str) {
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
pub(crate) fn read_snippet(fs: &dyn FileSystem, path: &Path, line: u32) -> Option<String> {
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
pub(crate) fn append_file_bodies(fs: &dyn FileSystem, hits: &[RecallHit], mut md: String) -> String {
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
pub(crate) fn render_markdown(
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


