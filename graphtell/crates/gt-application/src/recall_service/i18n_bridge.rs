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
// -------------------------------------------------- project i18n bridge (data-driven, zero-config)

/// Split English tokens from an i18n key (e.g. `order.pay.insufficient_balance`).
///
/// Keep only ASCII tokens: this builds a "Chinese → English symbol" bridge; Chinese tokens left on the bridge are meaningless.
pub(crate) fn key_tokens(key: &str) -> Vec<String> {
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
pub(crate) fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// Extract Chinese strings from a source snippet (reuse query-side `cjk_runs`, but drop single-char noise and dedup).
pub(crate) fn snippet_chinese(s: &str) -> Vec<String> {
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
pub(crate) fn location_tokens(props: &serde_json::Value) -> Vec<String> {
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
pub(crate) type EnrichIndex = HashMap<String, Vec<(String, Vec<String>)>>;

/// Build the node-enrichment index from the project i18n bridge (`Vec<(Chinese phrase, English tokens)>`).
pub(crate) fn build_enrich_index(bridge: &[(String, Vec<String>)]) -> EnrichIndex {
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
pub(crate) fn compute_bridge(nodes: &[Node]) -> Vec<(String, Vec<String>)> {
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
pub(crate) fn split_camel(s: &str) -> Vec<String> {
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
pub(crate) fn is_containment_edge(kind: &str) -> bool {
    matches!(kind, "Declares" | "HasColumn" | "Extends" | "HandledBy")
}

/// Max members one container brings out, to avoid a huge class's all methods filling the hit list.
pub(crate) const MAX_MEMBERS_PER_CONTAINER: usize = 6;

/// Neighbors (bidirectional) for recall expansion: call-chain edges **+ containment edges**.
pub(crate) fn neighbours(
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
pub(crate) fn relation_summary(
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

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::{Edge, EdgeId, EdgeKind, NodeId, Phase, ProjectId};
    use serde_json::json;
    use std::collections::HashMap;

    fn mk_edge(from: i64, to: i64, kind: &str) -> Edge {
        Edge {
            id: EdgeId(0),
            project_id: ProjectId(1),
            kind: EdgeKind(kind.to_string()),
            from_id: NodeId(from),
            to_id: NodeId(to),
            phase: Phase("Test".to_string()),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        }
    }

    #[test]
    fn key_tokens_extracts_ascii_from_i18n_key() {
        let toks = key_tokens("order.pay.insufficientBalance");
        assert!(toks.contains(&"order".to_string()));
        assert!(toks.contains(&"pay".to_string()));
        assert!(toks.contains(&"balance".to_string()));
    }

    #[test]
    fn cjk_detection_and_snippet() {
        assert!(contains_cjk("订单"));
        assert!(!contains_cjk("order"));
        let zh = snippet_chinese("订单优惠 some table");
        assert!(zh.iter().any(|s| s == "订单优惠"));
    }

    #[test]
    fn split_camel_boundary() {
        assert_eq!(split_camel("insufficientBalance"), vec!["insufficient", "Balance"]);
    }

    #[test]
    fn containment_edge_classification() {
        assert!(is_containment_edge("Declares"));
        assert!(is_containment_edge("HasColumn"));
        assert!(!is_containment_edge("Calls"));
    }

    #[test]
    fn build_enrich_index_inverts_tokens() {
        let idx = build_enrich_index(&[("支付".to_string(), vec!["pay".to_string(), "payment".to_string()])]);
        assert_eq!(idx.get("pay").map(|v| v.len()), Some(1));
        assert_eq!(idx.get("payment").map(|v| v.len()), Some(1));
    }

    #[test]
    fn location_tokens_filters_path_stopwords() {
        let props = json!({"locations":[{"file":"src/components/payment/index.vue"}]});
        let toks = location_tokens(&props);
        assert!(toks.contains(&"payment".to_string()));
        assert!(!toks.iter().any(|t| t == "index")); // stopword
    }

    #[test]
    fn neighbours_follows_chain_and_containment_edges() {
        // Caller along a chain edge: edge 2 -> 1.
        let mut incoming: HashMap<i64, Vec<Edge>> = HashMap::new();
        incoming.insert(1, vec![mk_edge(2, 1, "Calls")]);
        let out = neighbours(NodeId(1), &incoming, &HashMap::new());
        assert!(out.contains(&NodeId(2)));

        // Containment edge: 1 Declares 3.
        let mut outgoing: HashMap<i64, Vec<Edge>> = HashMap::new();
        outgoing.insert(1, vec![mk_edge(1, 3, "Declares")]);
        let out2 = neighbours(NodeId(1), &HashMap::new(), &outgoing);
        assert!(out2.contains(&NodeId(3)));
    }

    #[test]
    fn relation_summary_counts_incoming_chain_edges() {
        let mut incoming: HashMap<i64, Vec<Edge>> = HashMap::new();
        incoming.insert(1, vec![mk_edge(2, 1, "Calls"), mk_edge(3, 1, "Calls")]);
        let summary = relation_summary(NodeId(1), &incoming, &HashMap::new());
        assert!(summary.iter().any(|s| s.contains("← Calls")));
    }
}

