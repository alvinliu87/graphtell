//! P11 Sign: the **quality of signature verification** in callbacks from external systems.
//!
//! # Why it needs its own phase
//!
//! "Signature verification" has two levels:
//!
//! 1. **Is there verification at all** — this cannot be judged on the graph: the PHP parser's `exclude_dirs` drops
//!    `vendor`, and Java does not even scan Maven dependencies, so `verify()` from EasyWeChat / yansongda-pay /
//!    official SDKs **is not in the graph at all**. Any predicate of the form "no verify call on the call chain"
//!    then holds for every callback — a 100% false-positive rate. This shares a cause with the already-rejected
//!    "a table is written but no model maps to it" in the README: **it is a gap in the graph, not a problem in the
//!    code**.
//! 2. **Is the verification done correctly** — this is a **local fact** and can be judged: how the signature is
//!    compared once it has been computed.
//!
//! This phase does only level 2 and judges two things:
//!
//! * **Loose comparison**: `$sign == $calc` / `$this->CreatedSign($params) != $params['sign']`. PHP's `==` / `!=`
//!   are loose comparisons, so md5 digests of the `0e...` form judge each other equal
//!   (`md5('240610708') == md5('QNKCDZO')`), and they are not constant time — a timing side channel.
//!   The correct form is `hash_equals()`.
//! * **Weak algorithm**: the signature uses `md5` / `sha1`. Note that WeChat Pay V2, older Alipay and some shipping
//!   gateways **officially require MD5**, so this only raises `info`, and the copy must read "needs confirmation"
//!   rather than "has a vulnerability".
//!
//! # Noise gate
//!
//! In e-commerce code, `sign` is overwhelmingly about **check-ins** (`$sign_mode` / `$sign_last_date` /
//! `$sign_total_days` / `$points_sign_enabled`). Measured: of 32 "`==` comparisons containing sign", 24 were
//! check-ins. So the judgement requires that **a signature-computation call really exists inside the same function
//! body** (md5 / sha1 / hash_hmac / hash_equals / openssl_verify / `*Sign()`); matching on variable names alone
//! gets drowned in check-ins.

use gt_domain::model::{
    AnnotationChannel, FactValue, MergeStrategy, NewAnnotation, NodeId, Phase, SignCheckSpec,
    SubProjectId,
};
use serde_json::json;

use crate::context::PipelineContext;

/// The signature vocabulary to judge a call site with: the sub-project's own, else the global fallback.
///
/// A sub-project whose language declares nothing gets `None` — P11 then judges nothing, rather than
/// applying another stack's vocabulary (which is what a hard-coded `language == php` gate used to do).
fn spec_for(ctx: &PipelineContext, sub: Option<SubProjectId>) -> Option<&SignCheckSpec> {
    sub.and_then(|s| ctx.sign_check.get(&s.get()))
        .or(ctx.sign_check_default.as_ref())
}

/// The loose-comparison annotation (rules match it via `has_annotation: weak_sign_compare`).
const LOOSE_COMPARE: &str = "weak_sign_compare";
/// The weak-hash signature annotation (rules match it via `has_annotation: weak_sign_hash`).
const WEAK_HASH: &str = "weak_sign_hash";

pub fn run(ctx: &mut PipelineContext) {
    let phase = Phase("Sign".to_string());
    let mut loose = 0usize;
    let mut weak = 0usize;

    // Collect first, annotate after: avoids a conflict between the immutable borrow of `ctx.ws` and the mutable borrow in `annotate`.
    let mut targets: Vec<(NodeId, &'static str, serde_json::Value)> = Vec::new();

    for cmp in ctx.ws.sign_compares.iter() {
        let Some(calc) = find_sign_calc(ctx, cmp) else {
            continue;
        };
        targets.push((
            calc.node,
            LOOSE_COMPARE,
            json!({
                "file": cmp.file,
                "line": cmp.span.start_line,
                "snippet": format!("{} {} {}", cmp.left, cmp.operator, cmp.right),
                "operator": cmp.operator,
            }),
        ));
    }

    for call in ctx.ws.calls.iter() {
        // Which algorithms are weak is declared by FKB, not assumed — and a stack that declares none is skipped.
        let Some(spec) = spec_for(ctx, call.sub) else {
            continue;
        };
        let Some(method) = call.method.as_deref() else {
            continue;
        };
        // Case-insensitive on purpose: PHP's function names are case-insensitive, so `MD5($str)` is the
        // same call as `md5($str)` and must be judged the same way.
        if !spec.weak_algos.iter().any(|a| a.eq_ignore_ascii_case(method)) {
            continue;
        }
        let arg = call.args.first().map(arg_text).unwrap_or_default();
        let lower = arg.to_ascii_lowercase();
        let in_sign_context = spec.value_hints.iter().any(|h| lower.contains(h))
            || spec.value_hints_require_compare.iter().any(|h| lower.contains(h))
                && ctx
                    .ws
                    .sign_compares
                    .iter()
                    .any(|c| c.owner_fqn == call.owner_fqn);
        if !in_sign_context {
            continue;
        }
        targets.push((
            call.node,
            WEAK_HASH,
            json!({
                "file": call.file,
                "line": call.span.start_line,
                "snippet": format!("{}({})", method, arg),
                "algo": method,
            }),
        ));
    }

    for (node, kind, evidence) in targets {
        if kind == LOOSE_COMPARE {
            loose += 1;
        } else {
            weak += 1;
        }
        ctx.ws.annotate(NewAnnotation {
            node_id: node,
            channel: AnnotationChannel("Sign".to_string()),
            kind: kind.to_string(),
            subkind: Some(subkind_of(kind).to_string()),
            confidence: 0.85,
            evidence,
            phase: phase.clone(),
            merge: MergeStrategy::Coexist,
        });
    }

    tracing::info!("P11 signature check done: {} loose comparisons, {} weak-hash signatures", loose, weak);
}

fn subkind_of(kind: &str) -> &'static str {
    match kind {
        LOOSE_COMPARE => "LooseSignatureCompare",
        _ => "WeakSignatureHash",
    }
}

/// Locate the **signature-computation call site** this comparison compares (the annotation landing point).
///
/// Two levels:
/// 1. One side of the comparison is itself a call (`$this->hashEncrypt($str) == $signVerify`,
///    `md5($body.$secret) != $_SERVER['HTTP_KWAISIGN']`) — take that call site directly.
///    This step cannot be skipped: a home-grown verification function need not have `sign` in its name
///    (beikeshop's `hashEncrypt` does not), and recognising by name alone misses the whole project.
/// 2. Both sides are variables (`$sign == $ipay_signature`) — fall back to a hash / `*Sign()` call in the same function.
fn find_sign_calc(
    ctx: &PipelineContext,
    cmp: &gt_domain::model::syntax::SignCompareFact,
) -> Option<crate::workspace::CallRecord> {
    let ws = &ctx.ws;
    let exact = ws
        .calls
        .iter()
        .filter(|c| c.owner_fqn == cmp.owner_fqn)
        .find(|c| {
            let call = format!("{}(", c.callee);
            cmp.left.contains(&call) || cmp.right.contains(&call)
        })
        .cloned();
    exact.or_else(|| {
        ws.calls
            .iter()
            .find(|c| {
                c.owner_fqn == cmp.owner_fqn
                    && spec_for(ctx, c.sub)
                        .map(|s| is_sign_calc(c, s))
                        .unwrap_or(false)
            })
            .cloned()
    })
}

/// Whether a call site is "signature computation": a declared hash / verify call, or a method whose name
/// matches the declared naming convention (`CreatedSign` / `GetSign`) while not hitting a declared
/// look-alike (`signin` / `signmode`). All vocabulary comes from FKB `sign_check` — none is built in.
fn is_sign_calc(c: &crate::workspace::CallRecord, spec: &SignCheckSpec) -> bool {
    let Some(method) = c.method.as_deref() else {
        return false;
    };
    if spec.hash_calls.iter().any(|h| h == method) {
        return true;
    }
    let Some(needle) = spec.name_contains.as_deref() else {
        return false;
    };
    let lower = method.to_ascii_lowercase();
    if !lower.contains(&needle.to_ascii_lowercase()) {
        return false;
    }
    !spec.name_excludes.iter().any(|x| lower.contains(x))
}

fn arg_text(fv: &FactValue) -> String {
    match fv {
        FactValue::String(s) => s.clone(),
        FactValue::Unknown(Some(t)) => t.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::CallRecord;
    use gt_domain::model::syntax::SignCompareFact;
    use gt_domain::model::{Language, NodeId, Span};

    fn call(method: &str, args: Vec<FactValue>) -> CallRecord {
        CallRecord {
            node: NodeId(1),
            owner: NodeId(2),
            owner_fqn: "App\\Pay::respond".to_string(),
            owner_class: None,
            callee: method.to_string(),
            receiver: None,
            method: Some(method.to_string()),
            args,
            span: Span { start_line: 10, end_line: 10, start_byte: 0, end_byte: 0 },
            file: "app/pay.php".to_string(),
            language: Language::new("php"),
            sub: None,
            db_table: None,
            in_loop: false,
            entity: None,
        }
    }

    /// The PHP vocabulary, as `fkb/php/common.yaml` declares it — the same shape any other stack would write.
    fn php_spec() -> SignCheckSpec {
        SignCheckSpec {
            hash_calls: vec![
                "md5".into(),
                "sha1".into(),
                "hash_hmac".into(),
                "hash_equals".into(),
                "openssl_verify".into(),
                "openssl_sign".into(),
            ],
            weak_algos: vec!["md5".into(), "sha1".into()],
            name_contains: Some("sign".into()),
            name_excludes: vec![
                "signtype".into(),
                "signmode".into(),
                "signin".into(),
                "signup".into(),
            ],
            value_hints: vec!["sign".into()],
            value_hints_require_compare: vec!["key=".into()],
        }
    }

    #[test]
    fn recognizes_sign_calc_calls() {
        let s = php_spec();
        assert!(is_sign_calc(&call("md5", vec![]), &s));
        assert!(is_sign_calc(&call("sha1", vec![]), &s));
        assert!(is_sign_calc(&call("hash_hmac", vec![]), &s));
        assert!(is_sign_calc(&call("openssl_verify", vec![]), &s));
        assert!(is_sign_calc(&call("CreatedSign", vec![]), &s));
        assert!(is_sign_calc(&call("GetSign", vec![]), &s));
    }

    #[test]
    fn ignores_non_sign_calls() {
        let s = php_spec();
        assert!(!is_sign_calc(&call("count", vec![]), &s));
        assert!(!is_sign_calc(&call("find", vec![]), &s));
    }

    /// The check-in look-alikes are declared data, so a stack that never sees them simply declares none.
    #[test]
    fn checkin_lookalikes_come_from_the_declaration() {
        let s = php_spec();
        assert!(!is_sign_calc(&call("signIn", vec![]), &s));
        assert!(!is_sign_calc(&call("signMode", vec![]), &s));
        // Same method, but a declaration without the look-alike list judges it as signature computation —
        // proving the behaviour is driven by knowledge, not by a hard-coded language.
        let mut bare = php_spec();
        bare.name_excludes.clear();
        assert!(is_sign_calc(&call("signIn", vec![]), &bare));
    }

    /// A stack that declares **no** vocabulary is not judged at all: PHP's `md5` must not be recognised
    /// through it. This is what replaces the old `language == php` gate.
    #[test]
    fn empty_declaration_recognizes_nothing() {
        let s = SignCheckSpec::default();
        assert!(!is_sign_calc(&call("md5", vec![]), &s));
        assert!(!is_sign_calc(&call("hash_hmac", vec![]), &s));
        assert!(!is_sign_calc(&call("GetSign", vec![]), &s));
    }

    // ------------------------------------------------------- the phase itself (`run`)

    fn new_ctx() -> PipelineContext {
        PipelineContext::new(gt_domain::model::Project {
            id: gt_domain::model::ProjectId(1),
            name: "t".into(),
            root_path: std::path::PathBuf::from("/t"),
            description: None,
            status: gt_domain::model::ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        })
    }

    fn compare(owner_fqn: &str) -> SignCompareFact {
        SignCompareFact {
            left: "$sign".into(),
            right: "$calc".into(),
            operator: "==".into(),
            owner_fqn: owner_fqn.to_string(),
            file: "app/pay.php".into(),
            span: Span { start_line: 20, end_line: 20, start_byte: 0, end_byte: 0 },
        }
    }

    fn weak_hits(ctx: &PipelineContext, node: NodeId) -> usize {
        ctx.ws
            .annotations_of(node)
            .iter()
            .filter(|a| a.kind == WEAK_HASH)
            .count()
    }

    /// Which vocabulary judges a call site is per sub-project, falling back to the global one — and a stack
    /// that declares none is not judged at all (the behaviour that replaced the `language == php` gate).
    #[test]
    fn spec_for_prefers_the_sub_project_and_judges_nothing_without_a_declaration() {
        let mut ctx = new_ctx();
        assert!(spec_for(&ctx, None).is_none(), "什么都没声明时不做判断");
        assert!(spec_for(&ctx, Some(SubProjectId(1))).is_none());

        ctx.sign_check_default = Some(php_spec());
        assert!(spec_for(&ctx, None).is_some(), "全局兜底应生效");
        assert!(
            spec_for(&ctx, Some(SubProjectId(1))).is_some(),
            "子工程没有自己的声明时应回落到全局兜底"
        );

        let mut sub = php_spec();
        sub.weak_algos = vec!["sha1".into()];
        ctx.sign_check.insert(1, sub);
        assert_eq!(
            spec_for(&ctx, Some(SubProjectId(1))).map(|s| s.weak_algos.clone()),
            Some(vec!["sha1".to_string()]),
            "子工程自己的声明应覆盖全局兜底"
        );
    }

    /// The two hint levels differ: `value_hints` marks a call as signature-related **on its own**, while
    /// `value_hints_require_compare` (too generic — here `key=`) only counts when the enclosing function
    /// also compares a signature. This pins the `||` / `&&` precedence that separates them.
    #[test]
    fn weak_hash_gating_separates_hints_from_hints_that_need_a_comparison() {
        // (1) `key=` alone, no signature comparison in the function -> not judged.
        let mut ctx = new_ctx();
        ctx.sign_check_default = Some(php_spec());
        ctx.ws
            .calls
            .push(call("md5", vec![FactValue::String("key=abc".to_string())]));
        run(&mut ctx);
        assert_eq!(
            weak_hits(&ctx, NodeId(1)),
            0,
            "仅有 key= 提示且无签名比较时不应判定"
        );

        // (2) same hint, now with a signature comparison in the same function -> judged.
        let mut ctx = new_ctx();
        ctx.sign_check_default = Some(php_spec());
        ctx.ws
            .calls
            .push(call("md5", vec![FactValue::String("key=abc".to_string())]));
        ctx.ws.sign_compares.push(compare("App\\Pay::respond"));
        run(&mut ctx);
        assert_eq!(weak_hits(&ctx, NodeId(1)), 1, "有签名比较时 key= 提示应成立");

        // (3) a `value_hints` hit (`sign`) needs no comparison at all.
        let mut ctx = new_ctx();
        ctx.sign_check_default = Some(php_spec());
        ctx.ws
            .calls
            .push(call("md5", vec![FactValue::String("sign=abc".to_string())]));
        run(&mut ctx);
        assert_eq!(weak_hits(&ctx, NodeId(1)), 1, "value_hints 单独命中即可");
    }

    #[test]
    fn subkind_of_marks_the_two_annotation_kinds() {
        assert_eq!(subkind_of(LOOSE_COMPARE), "LooseSignatureCompare");
        assert_eq!(subkind_of(WEAK_HASH), "WeakSignatureHash");
    }

    /// `weak_algos` is matched **case-insensitively**: PHP's function names are case-insensitive, so
    /// `MD5($str)` is the same call as `md5($str)` and must be judged the same. An algorithm not declared
    /// weak is still not judged — which stays the case whether or not the spelling differs.
    #[test]
    fn weak_algos_are_matched_case_insensitively() {
        let mut ctx = new_ctx();
        ctx.sign_check_default = Some(php_spec());

        let mut sha256 = call("sha256", vec![FactValue::String("sign=abc".to_string())]);
        sha256.node = NodeId(11);
        let mut upper = call("MD5", vec![FactValue::String("sign=abc".to_string())]);
        upper.node = NodeId(12);
        let mut lower = call("md5", vec![FactValue::String("sign=abc".to_string())]);
        lower.node = NodeId(13);
        ctx.ws.calls.extend([sha256, upper, lower]);

        run(&mut ctx);

        assert_eq!(weak_hits(&ctx, NodeId(13)), 1, "声明为弱的 md5 应被判定");
        assert_eq!(
            weak_hits(&ctx, NodeId(11)),
            0,
            "未声明为弱的算法不应判定"
        );
        assert_eq!(
            weak_hits(&ctx, NodeId(12)),
            1,
            "大写 MD5 与 md5 是同一次调用，应同样被判定"
        );
    }

    fn loose_hits(ctx: &PipelineContext, node: NodeId) -> usize {
        ctx.ws
            .annotations_of(node)
            .iter()
            .filter(|a| a.kind == LOOSE_COMPARE)
            .count()
    }

    // ------------------------------------------------------- the loose-comparison path (untested so far)

    /// The phase's other annotation: a loose `==` / `!=` comparing a computed signature against the incoming
    /// one lands a `weak_sign_compare` annotation on the signature-computation call site — and only there.
    #[test]
    fn loose_compare_annotates_the_sign_computation_call_site() {
        let mut ctx = new_ctx();
        let mut cmp = compare("App\\Pay::respond");
        cmp.left = "md5($body.$secret)".into();
        cmp.right = "$_SERVER['HTTP_KWAISIGN']".into();
        cmp.operator = "!=".into();
        ctx.ws.sign_compares.push(cmp);

        let mut calc = call("md5", vec![FactValue::String("body.secret".into())]);
        calc.owner_fqn = "App\\Pay::respond".into();
        ctx.ws.calls.push(calc.clone());

        run(&mut ctx);
        assert_eq!(loose_hits(&ctx, calc.node), 1, "松比较应标注到签名计算调用点");
        assert_eq!(
            weak_hits(&ctx, calc.node),
            0,
            "该参数不含签名提示，不应同时判弱哈希"
        );
    }

    /// A loose comparison whose signature-computation call site cannot be located produces no annotation —
    /// the phase must not claim "verified correctly" on a comparison it can't tie to a computation.
    #[test]
    fn loose_compare_with_no_calc_call_site_annotates_nothing() {
        let mut ctx = new_ctx();
        let mut cmp = compare("App\\Pay::respond");
        cmp.left = "$sign".into();
        cmp.right = "$ipay_signature".into();
        ctx.ws.sign_compares.push(cmp);
        // an unrelated call in the same function, no signature computation
        let mut other = call("count", vec![]);
        other.owner_fqn = "App\\Pay::respond".into();
        ctx.ws.calls.push(other);

        run(&mut ctx);
        assert_eq!(
            loose_hits(&ctx, NodeId(1)),
            0,
            "无可定位的签名计算时不该标注"
        );
    }

    // ------------------------------------------------------- find_sign_calc (the landing-point resolver)

    /// Level 1: one side of the comparison is itself a call (`$this->hashEncrypt($str) == ...`) — take that
    /// call site directly, even when the function is a home-grown verifier not in the hash vocabulary.
    #[test]
    fn find_sign_calc_takes_the_call_site_named_in_the_comparison() {
        let mut ctx = new_ctx();
        let owner = "App\\Pay::respond";
        let mut calc = call("hashEncrypt", vec![]);
        calc.owner_fqn = owner.into();
        ctx.ws.calls.push(calc.clone());

        let mut cmp = compare(owner);
        cmp.left = "$this->hashEncrypt($str)".into();
        cmp.right = "$signVerify".into();
        assert_eq!(
            find_sign_calc(&ctx, &cmp).map(|c| c.node),
            Some(calc.node),
            "比较一侧本身是调用时直接取该调用点"
        );
    }

    /// Level 2: both sides are variables (`$sign == $ipay_signature`) — fall back to a `*Sign()` call in the
    /// same function body (the noise gate that keeps check-ins from drowning the signal).
    #[test]
    fn find_sign_calc_falls_back_to_a_sign_calc_in_the_same_function() {
        let mut ctx = new_ctx();
        ctx.sign_check_default = Some(php_spec());
        let owner = "App\\Pay::respond";
        let mut cmp = compare(owner);
        cmp.left = "$sign".into();
        cmp.right = "$ipay_signature".into();

        let mut calc = call("CreatedSign", vec![]);
        calc.owner_fqn = owner.into();
        ctx.ws.calls.push(calc.clone());
        assert_eq!(
            find_sign_calc(&ctx, &cmp).map(|c| c.node),
            Some(calc.node),
            "两侧都是变量时回落到同函数内的签名计算调用"
        );
    }

    /// When neither a named call nor a same-function sign calc exists, no landing point.
    #[test]
    fn find_sign_calc_returns_none_when_no_sign_calc_exists() {
        let mut ctx = new_ctx();
        ctx.sign_check_default = Some(php_spec());
        let owner = "App\\Pay::respond";
        let mut cmp = compare(owner);
        cmp.left = "$sign".into();
        cmp.right = "$ipay_signature".into();

        let mut other = call("count", vec![]);
        other.owner_fqn = owner.into();
        ctx.ws.calls.push(other);
        assert!(
            find_sign_calc(&ctx, &cmp).is_none(),
            "既无比较中命名的调用也无同函数签名计算时返回 None"
        );
    }

    // ------------------------------------------------------- arg_text (evidence snippet builder)

    #[test]
    fn arg_text_extracts_string_and_unknown_but_blanks_other_values() {
        assert_eq!(arg_text(&FactValue::String("abc".into())), "abc");
        assert_eq!(arg_text(&FactValue::Unknown(Some("x".into()))), "x");
        assert_eq!(arg_text(&FactValue::Int(5)), "");
        assert_eq!(arg_text(&FactValue::Null), "");
    }
}
