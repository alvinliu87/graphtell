//! P11 Sign：外部系统回调的**验签质量**。
//!
//! # 为什么要单独一个阶段
//!
//! 「验签」有两个层次：
//!
//! 1. **有没有验签** —— 这个图上判不了：PHP 解析器 `exclude_dirs` 排除了 `vendor`，
//!    Java 连 Maven 依赖都不扫描，于是 EasyWeChat / yansongda-pay / 官方 SDK 的
//!    `verify()` **根本不在图里**。任何"调用链上没有验签调用"的判据都会对每个回调
//!    成立 —— 100% 误报。这与 README 里已否决的"表被写但无模型映射"同因：
//!    **是图的缺口，不是代码的问题**。
//! 2. **验签做得对不对** —— 这是**局部事实**，能判：签名算出来之后用什么方式比较。
//!
//! 本阶段只做第 2 层，判两类：
//!
//! * **松散比较**：`$sign == $calc` / `$this->CreatedSign($params) != $params['sign']`。
//!   PHP 的 `==` / `!=` 是松散比较，`0e...` 形式的 md5 摘要会互判相等
//!   （`md5('240610708') == md5('QNKCDZO')`），且非恒定时间 —— 可计时侧信道。
//!   正确写法是 `hash_equals()`。
//! * **弱算法**：签名用 `md5` / `sha1`。注意微信支付 V2、支付宝旧版、部分快递网关
//!   **官方就要求 MD5**，所以这条只发 `info`，文案必须写成"需确认"而不是"有漏洞"。
//!
//! # 噪声闸口
//!
//! 电商代码里 `sign` 绝大多数是**签到**（`$sign_mode` / `$sign_last_date` /
//! `$sign_total_days` / `$points_sign_enabled`）。实测 32 处"含 sign 的 == 比较"
//! 里 24 处是签到。因此判定要求**同函数体内确实存在签名计算调用**
//! （md5 / sha1 / hash_hmac / hash_equals / openssl_verify / `*Sign()`），
//! 纯变量名匹配会被签到淹没。

use gt_domain::model::{
    AnnotationChannel, FactValue, Language, MergeStrategy, NewAnnotation, NodeId, Phase,
};
use serde_json::json;

use crate::context::PipelineContext;

/// 松散比较标注（规则用 `has_annotation: weak_sign_compare` 命中）。
const LOOSE_COMPARE: &str = "weak_sign_compare";
/// 弱哈希签名标注（规则用 `has_annotation: weak_sign_hash` 命中）。
const WEAK_HASH: &str = "weak_sign_hash";

pub fn run(ctx: &mut PipelineContext) {
    let phase = Phase("Sign".to_string());
    let mut loose = 0usize;
    let mut weak = 0usize;

    // 先收集再标注：避免 `ctx.ws` 的不可变借用与 `annotate` 的可变借用冲突。
    let mut targets: Vec<(NodeId, &'static str, serde_json::Value)> = Vec::new();

    // ---- 形态一：签名值被 == / != 比较
    //
    // 标注落在**同函数内的签名计算调用点**上：那正是要改的地方
    // （算完之后别用 == 比），而不是落在无法建节点的比较表达式上。
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

    // ---- 形态二：签名用 md5 / sha1
    //
    // 上下文限定：参数文本含 sign（如 `md5($sign.'key='.$key)`），
    // 或同一个函数里存在签名比较 —— 否则 `md5($fileContent)` 这类无关哈希会被误报。
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

    tracing::info!("P11 验签完成：松散比较 {} 处，弱哈希签名 {} 处", loose, weak);
}

fn subkind_of(kind: &str) -> &'static str {
    match kind {
        LOOSE_COMPARE => "LooseSignatureCompare",
        _ => "WeakSignatureHash",
    }
}

/// 定位这次比较所比较的**签名计算调用点**（标注落点）。
///
/// 两级：
/// 1. 比较的某一侧就是一次调用（`$this->hashEncrypt($str) == $signVerify`、
///    `md5($body.$secret) != $_SERVER['HTTP_KWAISIGN']`）—— 直接取那个调用点。
///    这一步不能省：自研验签函数名里未必有 `sign`（beikeshop 的 `hashEncrypt` 就是），
///    只按名字认会漏掉整个工程。
/// 2. 两侧都是变量（`$sign == $ipay_signature`）—— 退回同函数内的哈希 / `*Sign()` 调用。
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

/// 调用点是否是「签名计算」：哈希函数，或名字带 Sign 的方法（`CreatedSign` / `GetSign`）。
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
    // 方法名含 sign 但不落在"签到"词上：`CreatedSign` / `GetSign` / `makeSign` / `verifySign`。
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
