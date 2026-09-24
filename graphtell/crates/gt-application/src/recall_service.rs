//! 代码召回用例：给定提示词，在图上找出"该看哪些代码"。
//!
//! # 与全文检索的区别
//!
//! 全文检索回答"哪个文件出现了这个字符串"；召回回答"这个主题涉及哪些代码"。
//! 后者必须靠图：命中一个种子之后，沿调用链 / 读写边向外扩展，把**没有出现
//! 关键词但确实相关**的代码一并带出来（例如搜 `store_order`，除了 Model 本身，
//! 还应带出写这张表的 Services 与读它的 Dao）。
//!
//! # MVP 的诚实边界
//!
//! 本实现是**结构化召回**：按标识符（英文 token）在图里找种子，再按图扩展。
//! 它不做语义向量、不调 LLM —— 因此纯中文提示词（"订单创建涉及的代码"）
//! 只有在提示词里含有标识符、或含有结构提示词（"表"/"接口"/"事件"…）时才有效。
//! 这是刻意的取舍：先让"图能召回"这件事可验证，再谈语义层。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use gt_domain::error::Result;
use gt_domain::model::{Node, NodeId, ProjectId};
use gt_domain::model::kinds::is_chain_edge;
use gt_domain::port::{FileScanner, FileSystem, NodeFilter, Persistence};
use serde::{Deserialize, Serialize};

use crate::embedding::{cosine, default_embedder, Embedder};

/// 默认不参与召回的节点种类。
///
/// `CallSite` 是方法体内的一次调用点（CRMEB 里占全部节点的 8 成），
/// 召回它等于把"每一行代码"都当成答案 —— 粒度太细、噪声极大。
pub const DEFAULT_EXCLUDED_KINDS: &[&str] = &[
    "CallSite",
    "File",
    "Directory",
    "Namespace",
    "Property",
    "Const",
];

/// 向量路召回的权重与阈值（与词面分同量纲，便于合并排序）。
const VECTOR_WEIGHT: f64 = 70.0;
/// 余弦低于该值视为「不相关」，不进入候选。
const VECTOR_THRESHOLD: f64 = 0.2;

/// 中文意图词 → 英文符号候选词（离线语义桥，无需模型）。
///
/// 查询时把命中的意图词展开成英文 token，使词面 / 向量召回都能 seed 到
/// 英文命名的业务节点（例：中文"下单" → `order` / `placeOrder`）。
/// 这是「中文意图 → 英文符号」最廉价可靠的桥；真正的语义模型到位后可弱化。
const INTENT_ALIASES: &[(&str, &[&str])] = &[
    ("下单", &["order", "placeorder", "createorder", "submitorder"]),
    ("订单", &["order", "orders"]),
    ("改优惠", &["discount", "coupon", "promotion", "applydiscount"]),
    ("优惠", &["discount", "coupon", "promotion", "vip"]),
    ("折扣", &["discount"]),
    ("优惠券", &["coupon", "voucher"]),
    ("支付", &["pay", "payment", "checkout"]),
    ("付款", &["pay", "payment"]),
    ("用户", &["user", "member", "customer"]),
    ("会员", &["member", "user", "vip"]),
    ("商品", &["product", "goods", "sku"]),
    ("购物车", &["cart", "basket"]),
    ("库存", &["stock", "inventory"]),
    ("登录", &["login", "auth", "signin"]),
    ("注册", &["register", "signup"]),
];

/// 把查询里出现的中文意图词展开成英文候选 token。
fn expand_intent_aliases(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (zh, en) in INTENT_ALIASES {
        if query.contains(zh) {
            for e in *en {
                if !out.iter().any(|x: &String| x == e) {
                    out.push((*e).to_string());
                }
            }
        }
    }
    out
}

/// 节点用于向量编码的文本（名字 + 种类 + fqn + identity）。
fn node_embed_text(node: &Node) -> String {
    let mut s = String::new();
    s.push_str(node.kind.as_str());
    s.push(' ');
    s.push_str(&node.name);
    if let Some(f) = &node.fqn {
        s.push(' ');
        s.push_str(f);
    }
    if let Some(i) = &node.identity {
        s.push(' ');
        s.push_str(&i.value);
    }
    s
}

/// 查询用于向量编码的文本（原查询 + 展开出的英文意图词）。
fn query_embed_text(query: &str, alias_terms: &[String]) -> String {
    let mut s = String::with_capacity(query.len() + alias_terms.join(" ").len() + 8);
    s.push_str(query);
    for t in alias_terms {
        s.push(' ');
        s.push_str(t);
    }
    s
}

/// 一次召回请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallQuery {
    /// 提示词（自然语言 + 标识符混写皆可）。
    pub query: String,
    /// 返回条数上限。
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// 从种子向外扩展的跳数（0 = 只要种子本身）。
    #[serde(default = "default_hops")]
    pub hops: u32,
    /// 只看给定种类；为空表示不限。
    #[serde(default)]
    pub kinds: Vec<String>,
    /// 是否带源码片段（需要读文件，成本略高）。
    #[serde(default = "default_true")]
    pub with_snippets: bool,
}

fn default_limit() -> usize {
    20
}
fn default_hops() -> u32 {
    2
}
fn default_true() -> bool {
    true
}

/// 召回命中。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallHit {
    pub node_id: NodeId,
    pub kind: String,
    pub name: String,
    pub fqn: Option<String>,
    /// 综合得分（种子分 × 跳数衰减）。
    pub score: f64,
    /// 距种子的跳数（0 = 种子本身）。
    pub hop: u32,
    /// 命中的种子节点名。
    pub seed: String,
    /// 命中的查询词。
    #[serde(default)]
    pub matched_terms: Vec<String>,
    /// 是否直接命中了关键词（false 表示靠图的扩展带出来的）。
    pub direct: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// 关键关系（如 `WritesDb ×3`），供上下文包说明"为什么相关"。
    #[serde(default)]
    pub relations: Vec<String>,
}

/// 种子信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedInfo {
    pub node_id: NodeId,
    pub kind: String,
    pub name: String,
    pub score: f64,
}

/// 召回结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallResult {
    pub project_id: ProjectId,
    pub query: String,
    /// 解析出的查询词。
    pub terms: Vec<String>,
    /// 命中的结构提示（如"表" → Table）。
    pub kind_hints: Vec<String>,
    pub seeds: Vec<SeedInfo>,
    pub hits: Vec<RecallHit>,
    /// 可直接粘给 LLM 的上下文包（Markdown）。
    pub markdown: String,
    /// 是否因扫描上限而被截断。
    pub truncated: bool,
}

/// 召回服务。
pub struct RecallService {
    store: Arc<dyn Persistence>,
    fs: Arc<dyn FileSystem>,
    _scanner: Arc<dyn FileScanner>,
    /// 离线文本编码器（向量召回的「编码」一侧）。默认本地哈希编码器，可注入真模型。
    embedder: Arc<dyn Embedder>,
}

impl RecallService {
    pub fn new(
        store: Arc<dyn Persistence>,
        fs: Arc<dyn FileSystem>,
        scanner: Arc<dyn FileScanner>,
    ) -> Self {
        Self {
            store,
            fs,
            _scanner: scanner,
            embedder: default_embedder(),
        }
    }

    /// 注入自定义编码器（如测试假编码器，或未来的 `bge-m3` / `unixcoder`）。
    pub fn with_embedder(
        store: Arc<dyn Persistence>,
        fs: Arc<dyn FileSystem>,
        scanner: Arc<dyn FileScanner>,
        embedder: Arc<dyn Embedder>,
    ) -> Self {
        Self {
            store,
            fs,
            _scanner: scanner,
            embedder,
        }
    }

    /// 执行一次召回。
    pub fn recall(&self, project_id: ProjectId, q: &RecallQuery) -> Result<RecallResult> {
        let (terms, kind_hints) = parse_query(&q.query);

        // 中文意图词展开：构造一份「用于匹配」的词表（不污染对外返回的 terms）。
        // 这样中文意图（"下单"）才能 seed 到英文命名的业务节点（"order"）。
        let alias_terms = expand_intent_aliases(&q.query);
        let mut match_terms = terms.clone();
        for t in &alias_terms {
            if !match_terms.iter().any(|x| x == t) {
                match_terms.push(t.clone());
            }
        }

        // ---- 1) 候选集：一次装载全部参与召回的节点
        let mut nodes: Vec<Node> = Vec::new();
        if q.kinds.is_empty() {
            for kind in scan_kinds() {
                let mut batch = self.store.query_nodes(&NodeFilter {
                    project_id,
                    kind: Some(gt_domain::model::NodeKind::new(kind)),
                    name_contains: None,
                    limit: Some(SCAN_LIMIT),
                    offset: None,
                })?;
                nodes.append(&mut batch);
            }
        } else {
            for kind in &q.kinds {
                let mut batch = self.store.query_nodes(&NodeFilter {
                    project_id,
                    kind: Some(gt_domain::model::NodeKind::new(kind.clone())),
                    name_contains: None,
                    limit: Some(SCAN_LIMIT),
                    offset: None,
                })?;
                nodes.append(&mut batch);
            }
        }
        let truncated = nodes.len() >= SCAN_LIMIT as usize;

        // ---- 2) 邻接（用于扩展与关系摘要）
        // id → 节点索引：BFS 里每跳都要回查节点概要，线性 find 会退化成 O(N²)。
        let index: HashMap<i64, &Node> = nodes.iter().map(|n| (n.id.get(), n)).collect();
        let ids: Vec<NodeId> = nodes.iter().map(|n| n.id).collect();
        let incoming = self.store.edges_incoming(&ids)?;
        let outgoing = self.store.edges_outgoing(&ids)?;
        let files = self.store.file_paths(project_id)?;
        let root = self
            .store
            .get_project(project_id)?
            .map(|p| std::path::PathBuf::from(p.root_path));

        // ---- 3) 打分：种子（词面路 + 向量路，合并）
        // 3a) 词面路：标识符 / fqn / identity 的子串匹配（沿用既有 score_node）。
        let mut lexical: HashMap<i64, (f64, Vec<String>)> = HashMap::new();
        for node in &nodes {
            if DEFAULT_EXCLUDED_KINDS.contains(&node.kind.as_str()) {
                continue;
            }
            let (score, matched) = score_node(node, &match_terms, &kind_hints, &incoming);
            if score > 0.0 {
                lexical.insert(node.id.get(), (score, matched));
            }
        }

        // 3b) 向量路：离线编码器对节点文本做软匹配，补足词面漏掉的跨语言种子。
        //     查询文本已带入展开出的英文意图词，使中文意图能靠近英文符号。
        let mut vector: HashMap<i64, f64> = HashMap::new();
        let qvec = self
            .embedder
            .embed(&query_embed_text(&q.query, &alias_terms));
        for node in &nodes {
            if DEFAULT_EXCLUDED_KINDS.contains(&node.kind.as_str()) {
                continue;
            }
            let nvec = self.embedder.embed(&node_embed_text(node));
            let c = cosine(&qvec, &nvec);
            if c >= VECTOR_THRESHOLD {
                let s = c * VECTOR_WEIGHT * kind_weight(node.kind.as_str());
                let entry = vector.entry(node.id.get()).or_insert(0.0);
                *entry = (*entry).max(s);
            }
        }

        // 合并：词面优先，向量补足（取较大值）。
        let mut scored: Vec<(f64, Vec<String>, &Node)> = Vec::new();
        for node in &nodes {
            let id = node.id.get();
            let (score, matched) = match lexical.get(&id) {
                Some((s, m)) => (*s, m.clone()),
                None => match vector.get(&id) {
                    Some(s) => (*s, vec!["<vector>".to_string()]),
                    None => continue,
                },
            };
            scored.push((score, matched, node));
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let seed_count = 5usize;
        let seeds: Vec<SeedInfo> = scored
            .iter()
            .take(seed_count)
            .map(|(s, _, n)| SeedInfo {
                node_id: n.id,
                kind: n.kind.to_string(),
                name: n.name.clone(),
                score: *s,
            })
            .collect();

        // ---- 4) 扩展：沿链边 BFS，跳数衰减
        let mut best: HashMap<i64, RecallHit> = HashMap::new();
        for (seed_score, matched, seed) in scored.iter().take(seed_count) {
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

        // ---- 5) 排序 + 截断 + 补位置
        let mut hits: Vec<RecallHit> = best.into_values().collect();
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(q.limit.max(1));

        for hit in hits.iter_mut() {
            let node = index.get(&hit.node_id.get()).copied();
            if let Some(n) = node {
                (hit.file, hit.line) = crate::location::node_location(n, &files, root.as_deref());
                hit.relations = relation_summary(hit.node_id, &incoming, &outgoing);
            } else {
                // 扩展可能走到被排除的种类（如 CallSite），补全它的概要
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

        let markdown = render_markdown(project_id, q, &terms, &kind_hints, &seeds, &hits);
        Ok(RecallResult {
            project_id,
            query: q.query.clone(),
            terms,
            kind_hints,
            seeds,
            hits,
            markdown,
            truncated,
        })
    }
}

/// 单次扫描的节点上限（防止超大库把一次召回拖成全表扫描）。
const SCAN_LIMIT: u32 = 200_000;

/// 参与召回的节点种类 = 语义节点 + 主要语法节点，排除 [`DEFAULT_EXCLUDED_KINDS`]。
fn scan_kinds() -> Vec<String> {
    [
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
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// 链边邻居（双向）。
fn neighbours(
    id: NodeId,
    incoming: &HashMap<i64, Vec<gt_domain::model::Edge>>,
    outgoing: &HashMap<i64, Vec<gt_domain::model::Edge>>,
) -> Vec<NodeId> {
    let mut out = Vec::new();
    if let Some(es) = outgoing.get(&id.get()) {
        for e in es {
            if is_chain_edge(e.kind.as_str()) {
                out.push(e.to_id);
            }
        }
    }
    if let Some(es) = incoming.get(&id.get()) {
        for e in es {
            if is_chain_edge(e.kind.as_str()) {
                out.push(e.from_id);
            }
        }
    }
    out
}

/// 关系摘要：入/出边按种类聚合，取前 5（上下文包要能说明"为什么相关"）。
fn relation_summary(
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

/// 节点打分：返回 (得分, 命中的查询词)。
fn score_node(
    node: &Node,
    terms: &[String],
    kind_hints: &[String],
    incoming: &HashMap<i64, Vec<gt_domain::model::Edge>>,
) -> (f64, Vec<String>) {
    let name = node.name.to_lowercase();
    let fqn = node.fqn.as_deref().unwrap_or("").to_lowercase();
    let identity = node.identity.as_ref().map(|i| i.value.to_lowercase()).unwrap_or_default();

    let mut score = 0.0f64;
    let mut matched: Vec<String> = Vec::new();
    for t in terms {
        let t = t.to_lowercase();
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
        // 命中多个词 → 相关性显著更强
        score *= 1.5;
    }
    if score <= 0.0 {
        return (0.0, matched);
    }

    // 种类权重：语义节点（表 / 契约 / 事件…）是"主题级"答案，优先于方法
    score *= kind_weight(node.kind.as_str());

    // 结构提示加成：提示词里说了"表"，就优先给 Table
    for hint in kind_hints {
        if node.kind.as_str().eq_ignore_ascii_case(hint) {
            score += 30.0;
            break;
        }
    }

    // 扇入加成：被引用得多 = 更可能是"主题的核心"
    let fan_in = incoming
        .get(&node.id.get())
        .map(|es| es.iter().filter(|e| is_chain_edge(e.kind.as_str())).count())
        .unwrap_or(0);
    score += (fan_in.min(20) as f64) * 0.4;

    (score, matched)
}

fn kind_weight(kind: &str) -> f64 {
    match kind {
        "Table" | "HttpContract" | "Event" | "Queue" | "Cache" | "Topic" | "Schedule" => 1.4,
        "ConfigKey" | "I18nKey" | "Page" | "EventBus" => 1.2,
        "Class" | "Interface" | "Trait" | "Enum" => 1.1,
        "Method" | "Function" => 1.0,
        _ => 0.8,
    }
}

/// 解析提示词：拆出查询词与结构提示。
///
/// 结构提示（"表" / "接口" / "事件" …）不参与文本匹配，而是转成节点种类加成 ——
/// 这样"订单相关的表"这种纯中文提问也能把答案收敛到 `Table`。
fn parse_query(query: &str) -> (Vec<String>, Vec<String>) {
    let mut terms: Vec<String> = Vec::new();
    let mut hints: Vec<String> = Vec::new();

    // 中文结构提示词
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

    // 英文/数字 token：按非字母数字切分，再拆 camelCase / snake_case。
    //
    // `_` 必须**留在 buffer 里**交给 `push_token` 处理：若在这里就当分隔符，
    // `store_order` 会被拆成 `store` + `order`，再也拿不到"完整标识符"这个词 ——
    // 而完整标识符恰恰是最有价值的召回词（精确匹配得分最高）。
    //
    // 注意：`is_cjk(ch)` 必须在这里**当分隔符处理**。中文字符对 Rust 来说也是
    // `is_alphanumeric()`，若不排除，整段中文会被当成一个 ASCII token 直接入列，
    // 于是"表"/"接口"这类**只该作种类加成**的词会残留成文本匹配词，把召回带偏。
    // 中文统一交给下面的 CJK 分支处理。
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

    // 中文：连续的 CJK 串。
    //
    // 关键设计：**光靠"整段作为一个 token"在长句上必死**。中文没有词边界，
    // "订单创建涉及哪些表和接口" 作为一个整体永远匹配不到任何节点名字，
    // 召回结果直接是空的 —— 而提示词恰恰经常是这种整句。
    //
    // 因此除整段外，再切出全部**相邻二字组（bigram）**：中文里有意义的最小
    // 词大多就是两个字（订单 / 创建 / 接口 / 缓存 …），bigram 能以近乎零的
    // 成本覆盖它们。噪音 bigram（单创 / 建涉）匹配不到任何节点，会被打分
    // 自然淘汰 —— 所以不需要词典、不需要分词库，也就没有新依赖。
    for run in cjk_runs(query) {
        // 整段保留一份：精确长词命中时得分最高。
        let keep = strip_hint_chars(&run, hint_map);
        if keep.chars().count() >= 2 && !terms.iter().any(|t| t == &keep) {
            terms.push(keep);
        }
        // 二字组
        let chars: Vec<char> = run.chars().collect();
        for w in chars.windows(2) {
            let gram: String = w.iter().collect();
            let gk = strip_hint_chars(&gram, hint_map);
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

fn is_cjk(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch)
}

/// 切出查询里连续的 CJK 串（每个串是一个"没有词边界"的中文片段）。
fn cjk_runs(query: &str) -> Vec<String> {
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

/// 去掉出现在结构提示词里的字符。
///
/// "表" / "接口" / "缓存" 这类词已经转成节点种类加成，再拿去当文本匹配词
/// 只会把召回结果带偏（"表" 会命中所有名字含"表"的节点）。
fn strip_hint_chars(s: &str, hint_map: &[(&str, &str)]) -> String {
    s.chars()
        .filter(|c| !hint_map.iter().any(|(w, _)| w.contains(*c)))
        .collect()
}

/// 拆分 camelCase / snake_case 后入列。
fn push_token(terms: &mut Vec<String>, raw: &str) {
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
    // 原样也保留一份：`store_order` 这种整体匹配很有价值
    if raw.chars().count() >= 2 && !terms.iter().any(|t| t == raw) {
        terms.push(raw.to_string());
    }
}

/// 读取源码片段（节点所在行前后各若干行）。
fn read_snippet(fs: &dyn FileSystem, path: &Path, line: u32) -> Option<String> {
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

/// 渲染可直接粘给 LLM 的上下文包。
fn render_markdown(
    project_id: ProjectId,
    q: &RecallQuery,
    terms: &[String],
    hints: &[String],
    seeds: &[SeedInfo],
    hits: &[RecallHit],
) -> String {
    let mut s = String::new();
    s.push_str(&format!("# 召回上下文：{}\n\n", q.query));
    s.push_str(&format!(
        "- 工程：#{}\n- 查询词：{}\n",
        project_id,
        if terms.is_empty() { "（无）".to_string() } else { terms.join(", ") }
    ));
    if !hints.is_empty() {
        s.push_str(&format!("- 结构提示：{}\n", hints.join(", ")));
    }
    s.push_str(&format!("- 跳数上限：{}，命中 {} 条\n\n", q.hops, hits.len()));

    if !seeds.is_empty() {
        s.push_str("## 种子（直接命中关键词）\n\n");
        for sd in seeds {
            s.push_str(&format!("- `{}` {}（得分 {:.1}）\n", sd.name, sd.kind, sd.score));
        }
        s.push('\n');
    }

    s.push_str("## 相关代码\n\n");
    for (i, h) in hits.iter().enumerate() {
        let loc = match (&h.file, h.line) {
            (Some(f), Some(l)) => format!("`{f}:{l}`"),
            (Some(f), None) => format!("`{f}`"),
            _ => "（无位置信息）".to_string(),
        };
        s.push_str(&format!(
            "### {}. {} `{}`\n\n- 位置：{}\n- 得分：{:.1} · 跳数 {} · 来源种子 `{}`{}\n",
            i + 1,
            h.kind,
            h.name,
            loc,
            h.score,
            h.hop,
            h.seed,
            if h.direct { " · 直接命中" } else { "" }
        ));
        if !h.relations.is_empty() {
            s.push_str(&format!("- 图上关系：{}\n", h.relations.join("，")));
        }
        if let Some(sn) = &h.snippet {
            s.push_str(&format!("\n```\n{sn}\n```\n"));
        }
        s.push('\n');
    }
    s
}

// ---------------------------------------------------------------- 单元测试
//
// 召回的"打分 / 查询解析 / 关系摘要"等纯逻辑此前没有任何针对性断言：
// 集成测试只验证"名字是否出现"，从不断言 score / matched_terms 数值，
// 因此一旦打分公式回归（如权重算错、跳数衰减失效）很难被发现。这里把
// 这些纯函数直接单测锁住。

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::error::DomainError;
    use gt_domain::model::{
        Edge, EdgeId, EdgeKind, IdentityKey, Language, Node, NodeId, NodeKind, Phase, ProjectId,
        Span,
    };
    use gt_domain::port::FileSystem;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    // ---- 构造辅助 ----

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

    /// 入边表：按 `to` 聚合（与 `score_node` / `relation_summary` 的取值方式一致）。
    fn incoming(edges: &[(&str, i64, i64)]) -> HashMap<i64, Vec<Edge>> {
        let mut m: HashMap<i64, Vec<Edge>> = HashMap::new();
        for (k, from, to) in edges {
            m.entry(*to).or_default().push(edge(k, *from, *to));
        }
        m
    }

    /// 出边表：按 `from` 聚合。
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
                "提示词 {word:?} 应映射到 {kind:?}，实际 {hints:?}"
            );
        }
        // "表" 在其它集成用例覆盖过，这里再确认一次锚点
        let (_t, hints) = parse_query("表");
        assert!(hints.contains(&"Table".to_string()), "表 → Table");
    }

    #[test]
    fn parse_query_splits_camel_case_and_keeps_whole() {
        let (terms, _hints) = parse_query("createOrder");
        assert!(terms.contains(&"create".to_string()));
        assert!(terms.contains(&"Order".to_string()));
        assert!(
            terms.contains(&"createOrder".to_string()),
            "整体标识符应保留（精确匹配得分最高）：{terms:?}"
        );
    }

    #[test]
    fn parse_query_keeps_snake_case_intact() {
        let (terms, _hints) = parse_query("store_order");
        assert!(
            terms.contains(&"store_order".to_string()),
            "snake_case 不能被拆成 store + order：{terms:?}"
        );
    }

    #[test]
    fn parse_query_filters_two_char_stop_words() {
        let (terms, _hints) = parse_query("用户相关的代码");
        assert!(
            !terms.contains(&"相关".to_string()),
            "2 字停用词应被过滤：{terms:?}"
        );
        assert!(
            !terms.contains(&"代码".to_string()),
            "2 字停用词应被过滤：{terms:?}"
        );
        assert!(
            terms.iter().any(|t| t.contains("用户")),
            "有意义的词应保留：{terms:?}"
        );
    }

    #[test]
    fn is_cjk_detects_chinese() {
        assert!(is_cjk('中'));
        assert!(!is_cjk('a'));
        assert!(!is_cjk('1'));
    }

    // ---- score_node ----

    #[test]
    fn score_node_exact_name_match() {
        // 100（精确）× 1.4（Table 权重）
        let n = tnode(101, "Table", "user", None, None);
        let (score, matched) = score_node(&n, &["user".to_string()], &[], &HashMap::new());
        assert!((score - 140.0).abs() < 1e-9, "精确匹配应得 100×1.4=140，实际 {score}");
        assert_eq!(matched, vec!["user".to_string()]);
    }

    #[test]
    fn score_node_starts_with_prefix() {
        // 70（前缀）× 1.4
        let n = tnode(102, "Table", "user_order", None, None);
        let (score, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new());
        assert!((score - 98.0).abs() < 1e-9, "前缀匹配应得 70×1.4=98，实际 {score}");
    }

    #[test]
    fn score_node_contains() {
        // 50（包含）× 1.0（Method）
        let n = tnode(103, "Method", "my_user_x", None, None);
        let (score, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new());
        assert!((score - 50.0).abs() < 1e-9, "包含匹配应得 50×1.0=50，实际 {score}");
    }

    #[test]
    fn score_node_identity_match() {
        // 名字不匹配，identity 包含 → 45 × 1.0（Method）
        let n = tnode(104, "Method", "zzz", None, Some("user_identity"));
        let (score, matched) = score_node(&n, &["user".to_string()], &[], &HashMap::new());
        assert!((score - 45.0).abs() < 1e-9, "identity 命中应得 45×1.0=45，实际 {score}");
        assert_eq!(matched, vec!["user".to_string()]);
    }

    #[test]
    fn score_node_fqn_match() {
        // fqn 包含 → 35 × 1.0
        let n = tnode(105, "Method", "zzz", Some("app\\model\\user"), None);
        let (score, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new());
        assert!((score - 35.0).abs() < 1e-9, "fqn 命中应得 35×1.0=35，实际 {score}");
    }

    #[test]
    fn score_node_multi_term_multiplier() {
        // 命中两词：先累加各词得分，再整体 ×1.5。这里两个词各得 50（Method 权重 1.0），
        // 所以 two == (one + order_only) × 1.5 == 150（大于简单相加的 100）。
        let n = tnode(106, "Method", "xuserxorderx", None, None);
        let (one, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new());
        let (order_only, _) = score_node(&n, &["order".to_string()], &[], &HashMap::new());
        let (two, matched) = score_node(
            &n,
            &["user".to_string(), "order".to_string()],
            &[],
            &HashMap::new(),
        );
        assert!(
            (two - (one + order_only) * 1.5).abs() < 1e-9,
            "双词应 = (单 + 单) × 1.5：one={one} order_only={order_only} two={two}"
        );
        assert!(two > one + order_only, "多词应严格高于简单相加");
        assert_eq!(matched.len(), 2);
    }

    #[test]
    fn score_node_kind_weight_prefers_semantic_nodes() {
        let table = tnode(107, "Table", "user", None, None);
        let method = tnode(108, "Method", "user", None, None);
        let (s_t, _) = score_node(&table, &["user".to_string()], &[], &HashMap::new());
        let (s_m, _) = score_node(&method, &["user".to_string()], &[], &HashMap::new());
        assert!((s_t - 140.0).abs() < 1e-9, "Table 100×1.4=140");
        assert!((s_m - 100.0).abs() < 1e-9, "Method 100×1.0=100");
        assert!(s_t > s_m, "语义节点（表）应优先于方法");
    }

    #[test]
    fn score_node_kind_hint_bonus() {
        // 提示词说了"表" → 结构提示加成 +30
        let n = tnode(109, "Table", "user", None, None);
        let (no_hint, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new());
        let (with_hint, _) =
            score_node(&n, &["user".to_string()], &["Table".to_string()], &HashMap::new());
        assert!(
            (with_hint - (no_hint + 30.0)).abs() < 1e-9,
            "结构提示应 +30：{with_hint} vs {no_hint}"
        );
    }

    #[test]
    fn score_node_fan_in_bonus() {
        // 精确匹配 100 × 1.0（Method）+ 扇入 2 × 0.4
        let n = tnode(110, "Method", "user", None, None);
        let inc = incoming(&[("WritesDb", 200, 110), ("ReadsDb", 201, 110)]);
        let (score, _) = score_node(&n, &["user".to_string()], &[], &inc);
        assert!(
            (score - (100.0 + 2.0 * 0.4)).abs() < 1e-9,
            "扇入 2 应 +0.8，实际 {score}"
        );
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
            "同种类多条入边应聚合为 ×3：{rel:?}"
        );
        assert!(
            rel.iter().any(|r| r == "← ReadsDb"),
            "单条不应带 ×N：{rel:?}"
        );
    }

    #[test]
    fn relation_summary_includes_outgoing() {
        let out = outgoing(&[("Calls", 300, 9)]);
        let rel = relation_summary(NodeId::new(300), &HashMap::new(), &out);
        assert!(
            rel.iter().any(|r| r == "→ Calls"),
            "出边应带 → 前缀：{rel:?}"
        );
    }

    // ---- neighbours ----

    #[test]
    fn neighbours_follows_chain_edges_both_directions() {
        let inc = incoming(&[("HandledBy", 502, 500)]);
        let out = outgoing(&[("Calls", 500, 501)]);
        let ns = neighbours(NodeId::new(500), &inc, &out);
        let ids: Vec<i64> = ns.iter().map(|n| n.get()).collect();
        assert!(ids.contains(&501), "应沿出边走到 501：{ids:?}");
        assert!(ids.contains(&502), "应沿入边走到 502：{ids:?}");
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
                "{k} 应被排除出召回候选（否则召回会退化成逐行匹配）"
            );
        }
    }

    // ---- read_snippet ----

    /// 内存文件系统：只实现召回片段读取所需的 4 个方法。
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

    #[test]
    fn read_snippet_reads_window_around_line() {
        let mut fs = MemFs::new();
        let p = PathBuf::from("/x/sample.php");
        let content = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs.insert(&p, content);
        let snip = read_snippet(&fs, &p, 3).expect("应读出片段");
        assert!(snip.contains("line1"), "应包含目标行之前 2 行：{snip}");
        assert!(snip.contains("line3"), "应包含目标行：{snip}");
        assert!(snip.contains("line6"), "应包含目标行之后 3 行：{snip}");
    }

    #[test]
    fn read_snippet_skips_huge_files() {
        let mut fs = MemFs::new();
        let p = PathBuf::from("/x/huge.php");
        fs.insert(&p, "x".repeat(3 * 1024 * 1024)); // 3MB > 2MB 上限
        let snip = read_snippet(&fs, &p, 1);
        assert!(snip.is_none(), "超大文件应跳过片段读取（防 OOM）");
    }

    // ---- 真实模型（bge-m3 ONNX）语义验证：仅 `model-ort` feature 下编译 ----
    #[cfg(feature = "model-ort")]
    #[test]
    fn bge_semantic_recall_chinese_to_english() {
        use crate::embed_ort::OrtBgeEmbedder;

        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
        let onnx = std::env::var("GT_BGE_ONNX")
            .unwrap_or_else(|_| root.join("bge-m3-onnx/model.onnx").to_string_lossy().into());
        let tok = std::env::var("GT_BGE_TOKENIZER")
            .unwrap_or_else(|_| root.join("bge-m3/tokenizer.json").to_string_lossy().into());
        if !Path::new(&onnx).exists() {
            eprintln!("skip bge_semantic_recall: 未找到 {onnx}（先跑 tools/export_bge_onnx.py）");
            return;
        }
        let emb = OrtBgeEmbedder::load(&onnx, &tok).expect("加载 bge-m3 ONNX 失败");

        let q = emb.embed("下单改优惠");
        let order = emb.embed("placeOrder");
        let discount = emb.embed("applyDiscount");
        let noise = emb.embed("unused_log");

        let co = crate::embedding::cosine(&q, &order);
        let cd = crate::embedding::cosine(&q, &discount);
        let cn = crate::embedding::cosine(&q, &noise);
        println!("cos(下单改优惠, placeOrder)={co:.4}  (applyDiscount)={cd:.4}  (unused_log)={cn:.4}");

        assert!(
            co > 0.4 && cd > 0.4,
            "中文意图应语义命中英文业务节点：co={co} cd={cd}"
        );
        assert!(
            co > cn && cd > cn,
            "噪声节点应明显低于目标节点：cn={cn} co={co} cd={cd}"
        );
    }
}
