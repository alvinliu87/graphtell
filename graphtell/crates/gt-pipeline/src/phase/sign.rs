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
    AnnotationChannel, FactValue, Language, MergeStrategy, NewAnnotation, NodeId, Phase,
};
use serde_json::json;

use crate::context::PipelineContext;

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
        let Some(calc) = find_sign_calc(&ctx.ws, cmp) else {
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
        if call.language.0 != Language::PHP {
            continue;
        }
        let Some(method) = call.method.as_deref() else {
            continue;
        };
        if !matches!(method, "md5" | "sha1") {
            continue;
        }
        let arg = call.args.first().map(arg_text).unwrap_or_default();
        let in_sign_context = arg.to_ascii_lowercase().contains("sign")
            || arg.to_ascii_lowercase().contains("key=")
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
    ws: &crate::workspace::GraphWorkspace,
    cmp: &gt_domain::model::syntax::SignCompareFact,
) -> Option<crate::workspace::CallRecord> {
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
            .find(|c| c.owner_fqn == cmp.owner_fqn && is_sign_calc(c))
            .cloned()
    })
}

/// Whether a call site is "signature computation": a hash function, or a method whose name contains Sign (`CreatedSign` / `GetSign`).
fn is_sign_calc(c: &crate::workspace::CallRecord) -> bool {
    if c.language.0 != Language::PHP {
        return false;
    }
    let Some(method) = c.method.as_deref() else {
        return false;
    };
    if matches!(
        method,
        "md5" | "sha1" | "hash_hmac" | "hash_equals" | "openssl_verify" | "openssl_sign"
    ) {
        return true;
    }
    // A method name containing sign but not landing on a "check-in" word: `CreatedSign` / `GetSign` / `makeSign` / `verifySign`.
    let lower = method.to_ascii_lowercase();
    if !lower.contains("sign") {
        return false;
    }
    !(lower.contains("signtype")
        || lower.contains("signmode")
        || lower.contains("signin")
        || lower.contains("signup"))
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

    #[test]
    fn recognizes_sign_calc_calls() {
        assert!(is_sign_calc(&call("md5", vec![])));
        assert!(is_sign_calc(&call("sha1", vec![])));
        assert!(is_sign_calc(&call("hash_hmac", vec![])));
        assert!(is_sign_calc(&call("openssl_verify", vec![])));
        assert!(is_sign_calc(&call("CreatedSign", vec![])));
        assert!(is_sign_calc(&call("GetSign", vec![])));
    }

    #[test]
    fn ignores_non_sign_calls() {
        assert!(!is_sign_calc(&call("count", vec![])));
        assert!(!is_sign_calc(&call("find", vec![])));
    }
}
