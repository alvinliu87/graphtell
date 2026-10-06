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
/// Text used for a node's vector encoding (name + kind + fqn + identity + i18n-bridge enrichment).
pub(crate) fn node_embed_text(node: &Node, enrich: &EnrichIndex) -> String {
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
pub(crate) fn query_embed_text(query: &str, alias_terms: &[String]) -> String {
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
pub(crate) fn is_vector_kind(node: &Node) -> bool {
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
pub(crate) fn warm_priority(kind: &str) -> u8 {
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
pub(crate) fn sort_pending_for_warmup(pending: &mut Vec<(u64, String, u8)>) {
    pending.sort_by(|a, b| a.2.cmp(&b.2).then(a.1.len().cmp(&b.1.len())));
}
