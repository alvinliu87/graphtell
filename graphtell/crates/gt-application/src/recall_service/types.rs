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
pub(crate) const VECTOR_WEIGHT: f64 = 200.0;
/// Cosine below this is treated as "not related" and excluded from candidates.
pub const VECTOR_THRESHOLD: f64 = 0.3;
/// Top-N lexical-path entries used as seeds.
///
/// Tried widening to 10/12: brings back `order_cancel_time` but pushes unrelated methods (`save`) to the top of
/// "how to modify order discount" — a net loss, abandoned. The real fix is [`wants_config_value`]: config queries
/// stop inverting kind preference, the config key rises to #2 without touching the quota. Keep original value.
pub(crate) const SEED_COUNT: usize = 5;
/// Top-N extra vector-path entries as seeds (union with lexical seeds) to fill cross-language recall.
pub(crate) const VECTOR_SEED_COUNT: usize = 4;
/// Only attach source snippets to the top-N hits in the context pack; other hits list name + location only.
/// Snippets are the bulk of volume (each ~7-15 lines); attaching all would bloat default output to ~800+ tokens,
/// while the model needs to read closely only the top few. The list still keeps all hit names/locations, no info lost.
pub(crate) const SNIPPET_TOP: usize = 6;
/// Cap on the number of full source files returned for hits (avoid stuffing too many files and blowing up context).
pub(crate) const INCLUDE_BODY_MAX_FILES: usize = 8;
/// Byte cap per hit file's full source; truncate beyond it (avoid huge files blowing up context).
pub(crate) const INCLUDE_BODY_MAX_BYTES: usize = 128 * 1024;

/// Generic CRUD verb method names (action only, no business semantics).
///
/// Under action intent, [`rank_weight`] gives `Method` an overall 1.5× boost to float methods/classes above routes and
/// infrastructure. But verb-only methods like `edit`/`save`/`update` carry no business info; once boosted by a query word
/// like "modify", they push CRUD of unrelated domains to the top. So such methods lose the action boost only when "no
/// content word is hit" (fall back to 1.0): if they also hit a content word (coupon/order `edit`), they keep the boost.
/// Compound business names (`createForm`/`getWorkbench`) are not pure-verb so keep the boost — precision only applies to
/// "pure-verb methods with no business content", zero regression.
pub(crate) const GENERIC_CRUD_METHODS: &[&str] = &[
    "edit", "save", "update", "modify", "create", "add", "insert", "delete", "remove", "destroy",
    "new", "set", "get", "list", "find", "query", "fetch", "search", "select", "load", "read",
    "index",
];

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

pub(crate) fn default_limit() -> usize {
    10
}
pub(crate) fn default_hops() -> u32 {
    2
}
pub(crate) fn default_true() -> bool {
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
pub(crate) const EMBED_TEXT_VERSION: u32 = 4;

/// Envelope for persisted vector files: versioned; on mismatch the whole file is invalidated and recomputed.
#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedEmbeds {
    pub(crate) version: u32,
    /// Vector dimension: must match the current semantic encoder to reuse. After switching to a smaller model (dim change, e.g. bge-m3
    /// 1024 → e5 768) old files dim-mismatch, whole file invalidated and re-encoded; else `cosine` by `min(len)` would silently use
    /// wrong dim and distort. `#[serde(default)]` drops old format (no dim field) to 0 — then reuse only when current encoder dim
    /// matches the vector's actual dim (same-dim model like bge-m3), no forced re-encode.
    #[serde(default)]
    pub(crate) dim: usize,
    pub(crate) vectors: HashMap<u64, Vec<f32>>,
}

/// Semantic vector cache key: deterministic content fingerprint (FNV-1a 64) of the node embed text.
///
/// Key by content not `node.id`: after graph rebuild (`reset_project` reassigns ids), unchanged nodes' embed text is unchanged →
/// fingerprint unchanged → persisted vectors survive rebuild/restart, dropping "full re-embed (tens of min on CPU)" to "only embed
/// changed nodes". Fast-vector cache key uses `node.id as u64` directly (instantly recomputable, no fingerprint needed).
pub(crate) fn embed_text_key(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Recall service.
pub struct RecallService {
    pub(crate) store: Arc<dyn Persistence>,
    pub(crate) fs: Arc<dyn FileSystem>,
    pub(crate) _scanner: Arc<dyn FileScanner>,
    /// Fast encoder (always available, offline hash): when semantic vectors aren't warmed up, recall immediately encodes query + nodes
    /// with it, so the UI never blocks; semantic quality auto-takes over after background warmup.
    pub(crate) fast_embedder: Arc<dyn Embedder>,
    /// Semantic encoder (real bge-m3, loaded on demand). `None` ⇒ only lexical / fast-vector path, no background warmup.
    pub(crate) semantic_embedder: Option<Arc<dyn Embedder>>,
    /// Fast-vector cache (hash, instantly recomputable, not persisted). Keyed by `node.id as u64`.
    pub(crate) fast_cache: Arc<Mutex<HashMap<u64, Vec<f32>>>>,
    /// Semantic vector cache (bge, persisted to `embed_persist_dir`). Keyed by content hash of node embed text (see [`embed_text_key`]),
    /// so unchanged nodes' vectors survive rebuild.
    pub(crate) node_embed_cache: Arc<Mutex<HashMap<u64, Vec<f32>>>>,
    /// Set of projects with warmed-up (bge vectors computed and persisted) vectors; judged per project whether to take the semantic path.
    pub(crate) warmed_projects: Arc<Mutex<HashSet<i64>>>,
    /// Set of projects currently warming up in background (prevent duplicate spawn).
    pub(crate) warming_projects: Arc<Mutex<HashSet<i64>>>,
    /// Background-warmup progress counter: project id → (encoded nodes, total to encode). Kept only during warmup, cleared on
    /// finish/failure, for `/api/.../warmup` and MCP to expose "warmup progress".
    pub(crate) warm_progress: Arc<Mutex<HashMap<i64, (usize, usize)>>>,
    /// Whether to allow background async warmup (only HTTP production entry enables; CLI/tests disable, to avoid spawning threads).
    pub(crate) enable_async_warmup: bool,
    /// Semantic vector persistence dir (`<dir>/<project_id>.rmp`). **Not** written at graph build; only computed and persisted during
    /// recall background warmup / manual `embed` command, loaded directly on restart. `None` ⇒ no persistence (in-memory cache only).
    pub(crate) embed_persist_dir: Option<PathBuf>,
    /// Candidate snapshot persistence dir (`<dir>/<project_id>.json`). Cold start loads in seconds from here, no rebuilding all nodes +
    /// edges from SQLite live (CRMEB measured ~10s → <1s). `None` ⇒ no persistence. On graph rebuild, [`Self::clear_node_cache`] deletes
    /// the whole dir to force invalidation.
    pub(crate) snapshot_persist_dir: Option<PathBuf>,
    /// Project i18n bridge cache: `project id -> [(Chinese text, English tokens split from that text's key)]`. Chinese queries map via
    /// it to **this project's** symbols, depending on no domain-specific vocabulary.
    pub(crate) bridge_cache: Arc<Mutex<HashMap<i64, Vec<(String, Vec<String>)>>>>,
    /// **Candidate-set snapshot** cache: `project id -> nodes + neighbors + file paths participating in recall`.
///
/// The biggest fixed cost in one recall isn't scoring but pulling the candidate set from SQLite (CRMEB 12k nodes measured 856ms +
/// neighbors 110ms), and multi-intent queries pull it again per sub-intent. The graph only changes on rebuild: rebuild calls
/// [`Self::clear_node_cache`]; also a cheap `stats` check before each reuse (see [`Self::snapshot_stale`]). `Arc` lets the caller borrow
/// the snapshot for the whole recall without holding a write lock.
    pub(crate) candidate_cache: Arc<Mutex<HashMap<i64, Arc<CandidateSet>>>>,
    /// Query-vector cache (`query text -> vector`): same prompt asked again (common in IDE) needs no more bge forward (~750ms).
    /// Very small, a plain LRU suffices.
    pub(crate) query_vec_cache: Arc<Mutex<Vec<(String, Vec<f32>)>>>,
}

/// The "graph snapshot" needed for one recall: candidate nodes + neighbors + file paths. See [`RecallService::candidate_cache`].
///
/// Derives `Clone`/`Serialize`/`Deserialize` for **disk reuse**: cold start reads the persisted snapshot (<1s) instead of
/// rebuilding from SQLite live (CRMEB ~10s). See [`RecallService::candidate_set`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CandidateSet {
    pub(crate) nodes: Vec<Node>,
    pub(crate) incoming: HashMap<i64, Vec<gt_domain::model::Edge>>,
    pub(crate) outgoing: HashMap<i64, Vec<gt_domain::model::Edge>>,
    pub(crate) files: HashMap<i64, String>,
    /// Total **whole-graph** node/edge count at snapshot build (from `stats`), for [`RecallService::snapshot_stale`]'s cheap check.
/// Must be whole-graph scope: `nodes` only holds participating kinds, naturally smaller than `stats.nodes`; comparing it to `stats.nodes`
/// would always judge "stale", making snapshot reuse moot.
    pub(crate) node_count: u64,
    pub(crate) edge_count: u64,
    /// The `kinds` filter used at snapshot build (empty = all kinds). Filtered requests don't share the full snapshot.
    pub(crate) kinds: Vec<String>,
}

/// Envelope for persisted candidate snapshots: versioned; on mismatch the whole file is invalidated and rebuilt (like [`PersistedEmbeds`]).
pub(crate) const SNAPSHOT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedSnapshot {
    pub(crate) version: u32,
    pub(crate) set: CandidateSet,
}

/// Query-vector cache capacity (see [`RecallService::query_vec_cache`]).
pub(crate) const QUERY_VEC_CACHE_CAP: usize = 64;

/// Query text → vector, with a small LRU cache (see [`RecallService::query_vec_cache`]).
///
/// Extracted as a free function so query encoding can run in a **separate thread** parallel with candidate load / disk load (all
/// independent; query encoding is one bge forward, ~800ms on CPU, the biggest serial item in cold start). Moving this logic to a thread
/// cuts the cold-start latency of "first question after restart" by ~1s.
///
/// `semantic` participates in the key: two encoders have different spaces, can't reuse each other.
pub(crate) fn encode_query_cached(
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
