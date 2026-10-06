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
// ============================================================================
// Built-in alias table (organized by domain pack)
// ----------------------------------------------------------------------------
// Old table was one big e-commerce/CMS-shaped table; non-e-commerce projects had poor out-of-box quality. Now split into domain packs:
//   - `PACK_GENERIC`   cross-domain generic; - `PACK_ECOMMERCE` e-commerce/CMS; - `PACK_FINANCE` finance (demonstrates infinite extension).
// Default [`BUILTIN_PACKS`] loads all; project-level `.graphtell/aliases.json` can append jargon (see [`merged_aliases`]).
// ============================================================================

/// Cross-domain generic alias pack: action verbs + generic technical nouns + configurable-value vocabulary.
/// These words appear in almost all software systems, not bound to a specific business domain.
pub(crate) const PACK_GENERIC: &[(&str, &[&str])] = &[
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
pub(crate) const PACK_ECOMMERCE: &[(&str, &[&str])] = &[
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
pub(crate) const PACK_FINANCE: &[(&str, &[&str])] = &[
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
pub(crate) const PACK_SYMPTOM: &[(&str, &[&str])] = &[
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
pub(crate) const BUILTIN_PACKS: &[&[(&str, &[&str])]] = &[
    PACK_GENERIC,
    PACK_ECOMMERCE,
    PACK_FINANCE,
    PACK_SYMPTOM,
];

/// Merge all built-in domain packs into one mergeable owned alias table.
///
/// Same key across packs merges the English expansions, dedup, no overwrite.
pub(crate) fn builtin_aliases() -> Vec<(String, Vec<String>)> {
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
pub(crate) fn builtin_alias_keys() -> std::collections::HashSet<String> {
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
pub(crate) fn merged_aliases(project_root: Option<&std::path::Path>) -> Vec<(String, Vec<String>)> {
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
pub(crate) fn expand_intent_aliases(query: &str, aliases: &[(String, Vec<String>)]) -> Vec<String> {
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
pub(crate) fn alias_group_map(aliases: &[(String, Vec<String>)]) -> HashMap<String, String> {
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
pub(crate) fn cohesion_multiplier(matched: &[String], group_map: &HashMap<String, String>) -> f64 {
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
pub(crate) fn select_seeds<'a>(
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

