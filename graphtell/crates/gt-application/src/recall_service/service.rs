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
    /// `Self::load_persisted_snapshot`]); if size validation passes, reuse directly, avoiding live rebuild from SQLite (sample_project ~10s → <1s);
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
    /// Use rmp (msgpack) not JSON: sample_project's full vector JSON is ~153MB and serde_json parse ~5s — the root cause of "slow first query";
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
/// Use rmp not JSON: sample_project's full vector JSON is ~153MB and serde_json parse ~5s — the root cause of "slow first query"; rmp is ~1/2
/// the volume and parses an order of magnitude faster.
pub(crate) fn persist_vectors(
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
    /// the whole project's vectors (sample_project 157MB / ~0.5s), and almost every recall misses zero nodes, so persisting every time is pure write
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
    pub(crate) fn split_intents(q: &str) -> Vec<String> {
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
    pub(crate) fn merge_intent_hits(groups: Vec<Vec<RecallHit>>, limit: usize) -> Vec<RecallHit> {
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

    pub(crate) fn assess_quality(
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
        // the whole project's vectors (sample_project 157MB / ~0.5s) — content that never changed once.
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
        // Bridged kinds (HttpContract): the front/back copies share a name and must not both
        // occupy top-k slots. Collapse to the single primary copy (backend preferred). Other kinds
        // keep the original per-name cap so behaviour is unchanged for them.
        let http_keep = select_bridged_primary(
            hits
                .iter()
                .filter(|h| h.kind.as_str() == "HttpContract")
                .map(|h| {
                    let is_backend = index
                        .get(&h.node_id.get())
                        .and_then(|n| n.properties.get("side").and_then(|v| v.as_str()))
                        == Some("backend");
                    (h.node_id.get(), h.name.as_str(), h.score, is_backend)
                }),
        );
        const MAX_SAME_NAME: usize = 2;
        let mut same: HashMap<String, usize> = HashMap::new();
        let mut kept: Vec<RecallHit> = Vec::with_capacity(hits.len());
        for h in hits {
            if h.kind.as_str() == "HttpContract" {
                if !http_keep.contains(&h.node_id.get()) {
                    continue;
                }
                let c = same.entry(h.name.clone()).or_insert(0);
                if *c < 1 {
                    *c += 1;
                    kept.push(h);
                }
                continue;
            }
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
pub(crate) fn fetch_nodes(store: &dyn Persistence, project_id: ProjectId) -> Result<Vec<Node>> {
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
pub(crate) fn collect_cache(
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
/// synchronously bge-encode the whole DB in the request thread (sample_project measured 10+ min no return), and because "judged semantic path"
/// never start background warmup — warmup progress would show never-started forever.
pub(crate) fn load_persisted_into(
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

pub(crate) fn warm_up_worker(
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
        // Load persisted vectors first: else after restart cache is empty, here would re-compute all nodes (sample_project measured 56 min),
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
pub(crate) const SCAN_LIMIT: u32 = 200_000;

/// Fallback list: used only when node kinds can't be read from the DB (ensure degraded usability).
pub(crate) const FALLBACK_SCAN_KINDS: &[&str] = &[
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
pub(crate) fn scan_kinds(store: &dyn Persistence, project_id: ProjectId) -> Vec<String> {
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

/// Bridged kinds (e.g. `HttpContract`) are emitted as one node per sub-project — a frontend
/// call site and a backend handler — bridged by a `ResolvesTo` edge. In the flat recall list
/// only one copy should appear, otherwise the front/back duplicates waste top-k slots. This
/// picks the `node_id` to keep per name: prefer the backend (implementation) copy, tie-break
/// by score. Pure and side-agnostic so it can be unit-tested without a `Node`.
fn select_bridged_primary<'a>(
    items: impl Iterator<Item = (i64, &'a str, f64, bool)>,
) -> HashSet<i64> {
    let mut best: HashMap<String, (i64, f64, bool)> = HashMap::new();
    for (id, name, score, is_backend) in items {
        let better = match best.get(name) {
            None => true,
            Some((_, s, b)) => is_backend && !*b || (is_backend == *b && score > *s),
        };
        if better {
            best.insert(name.to_string(), (id, score, is_backend));
        }
    }
    best.into_values().map(|(id, _, _)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_bridged_primary_prefers_backend_then_higher_score() {
        // Same endpoint emitted twice (frontend call site + backend handler): keep the backend.
        let keep = select_bridged_primary(
            [
                (1i64, "GET /order", 50.0, false), // frontend
                (2i64, "GET /order", 90.0, true),  // backend
            ]
            .into_iter(),
        );
        assert_eq!(keep, HashSet::from([2i64]));

        // Only a frontend copy exists: keep it (no backend to prefer).
        let keep = select_bridged_primary(
            [(3i64, "GET /cart", 10.0, false)].into_iter(),
        );
        assert_eq!(keep, HashSet::from([3i64]));

        // Two backend copies of the same name: keep the higher-scored one.
        let keep = select_bridged_primary(
            [
                (4i64, "GET /pay", 10.0, true),
                (5i64, "GET /pay", 80.0, true),
            ]
            .into_iter(),
        );
        assert_eq!(keep, HashSet::from([5i64]));

        // Distinct endpoints are independent.
        let keep = select_bridged_primary(
            [
                (6i64, "GET /a", 10.0, true),
                (7i64, "GET /b", 20.0, true),
            ]
            .into_iter(),
        );
        assert_eq!(keep, HashSet::from([6i64, 7i64]));
    }
}
