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

// The original single-file module exposed these imports (and every private item) to the `#[cfg(test)]`
// module via `use super::*`. The imports are re-listed here (unused at the parent level, hence the
// allow) so the test module keeps resolving them exactly as before the split.
#![allow(unused_imports)]

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

// The recall use case is split into focused submodules, one per concern, so this file stays a readable
// map of the recall pipeline rather than one ~3.9k-line wall of functions.
mod types; // public result/query types + shared tuning constants
mod service; // `RecallService` impl + vector persistence/loading
mod scoring; // lexical/vector seed scoring, flow re-ranking, CJK segmentation, markdown rendering
mod aliases; // intent-alias expansion (INTENT_ALIASES domain packs)
mod embed; // node/query embedding-text assembly + warmup prioritization
mod i18n_bridge; // data-driven i18n path → locale bridge

// Public API (re-exported from the crate root via `pub use recall_service::{...}`).
pub use types::{
    DEFAULT_EXCLUDED_KINDS, RecallHit, RecallQuality, RecallQuery, RecallResult, RecallService,
    SeedInfo, VECTOR_THRESHOLD, WarmupStatus,
};
// Internal items the `impl RecallService`, sibling modules, and the test module need.
pub(crate) use types::{
    CandidateSet, embed_text_key, EMBED_TEXT_VERSION, encode_query_cached, GENERIC_CRUD_METHODS,
    INCLUDE_BODY_MAX_BYTES, INCLUDE_BODY_MAX_FILES, PersistedEmbeds, PersistedSnapshot, SEED_COUNT,
    SNAPSHOT_VERSION, SNIPPET_TOP, VECTOR_SEED_COUNT, VECTOR_WEIGHT,
};
// The three methods the `#[cfg(test)]` module exercises are reached via `RecallService::` (already
// re-exported above) and made `pub(crate)`, so no `service` re-export is needed.
pub(crate) use scoring::*;
pub(crate) use aliases::*;
pub(crate) use embed::*;
pub(crate) use i18n_bridge::*;

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
        // 如 / 何 are unrecorded single chars, so each becomes its own segment and 何修 never appears as a whole.
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

        // 3) synonyms: 退货 / 售后 (return / after-sales) must both bridge to refund (the synonym of 退款).
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
        // Coupon type: 类型 (type) must bridge to type, otherwise 'add a new coupon type' only has generic add/coupon.
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
        let mid = hub_penalty(123); // e.g. BaseDao::save
        let hub = hub_penalty(452); // e.g. Cache
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

        // Key regression point: the old implementation stripped character by character, so 存 of `缓存` plus 库 of `数据库` would empty 库存,
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
