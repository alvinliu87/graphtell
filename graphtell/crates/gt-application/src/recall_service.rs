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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

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

/// 向量路召回的权重（与词面分同量纲，便于合并排序）。
const VECTOR_WEIGHT: f64 = 200.0;
/// 余弦低于该值视为「不相关」，不进入候选。
pub const VECTOR_THRESHOLD: f64 = 0.3;
/// 词面路取前 N 个作为种子。
///
/// 试过放宽到 10 / 12：确实能把 `order_cancel_time` 捞回来，但会让无关方法
/// （`save`）进入「如何修改下单优惠」的前排，属于净损失，**已放弃**。
/// 真正解法是 [`wants_config_value`]：配置类查询不反转种类偏好后，
/// 该配置键自己就排到第 2，无需动名额。这里保持原值。
const SEED_COUNT: usize = 5;
/// 向量路额外取前 N 个作为种子（与词面种子并集），补足跨语言召回。
const VECTOR_SEED_COUNT: usize = 4;
/// 仅在上下文包里给排名前 N 的命中附带源码片段；其余命中只列名字 + 位置。
/// 片段是体积大头（每条约 7~15 行），全量附带会让默认输出膨胀到 ~800+ token，
/// 而模型真正需要细看的往往是最靠前几条。列表仍保留全部命中的名字/位置，信息不丢。
const SNIPPET_TOP: usize = 6;

/// 通用 CRUD 动词方法名（仅含动作、不含业务语义）。
///
/// 动作意图下 [`rank_weight`] 会整体给 `Method` 加权（1.5×），目的是让"找实现代码"
/// 时方法 / 类浮到路由与基础设施之上。但像 `edit` / `save` / `update` 这种**只叫动词**
/// 的方法本身没有业务信息，仅靠「修改」这类查询词被加权后，会把发货、退款等
/// **无关域的 CRUD** 顶到顶部（见 `如何修改下单优惠` 用例：DeliveryService.save/update
/// 曾排在第 4~5，压过真正相关的优惠券服务）。
/// 因此这类方法**只在"没命中任何内容词"时才取消动作加权**（退回默认 1.0）：
/// 若它同时命中了内容词（如优惠券 / 订单的 `edit`，其 fqn 带 `Coupon`/`Order`），
/// 说明它贴合查询主题，照常加权。复合业务名
/// （`createForm` / `getWorkbench` / `beUsableCouponList`）因名字不是纯动词而不命中，
/// 也照常加权 —— 精准只对"纯动词且无业务内容"的方法生效，零回归。
const GENERIC_CRUD_METHODS: &[&str] = &[
    "edit", "save", "update", "modify", "create", "add", "insert", "delete", "remove", "destroy",
    "new", "set", "get", "list", "find", "query", "fetch", "search", "select", "load", "read",
    "index",
];

/// 方法名是否只是通用 CRUD 动词（不含业务语义）。见 [`GENERIC_CRUD_METHODS`]。
fn is_generic_crud_method(name: &str) -> bool {
    GENERIC_CRUD_METHODS.contains(&name)
}

/// 纯 CRUD 动词方法**只靠动词命中**（无任何内容词）时，基础匹配分的折扣系数。
///
/// 只取消动作加权（见 [`GENERIC_CRUD_METHODS`]）还不够：这类方法靠同名就能拿到
/// 100 分的精确匹配，仍会霸榜。打折后它们让位于真正的领域答案。
/// 命中内容词的（优惠券的 edit / 订单的 update）完全不受影响。
const GENERIC_CRUD_VERB_ONLY_DISCOUNT: f64 = 0.5;

/// ORM **关联访问器**（样板）的折扣系数。
///
/// `user()` / `refund()` / `productInfo()` 这类方法体只有 `hasOne / hasMany / belongsTo`，
/// 不含任何业务逻辑，图上特征是带 `MapsTo` 出边（语义"模型映射到表/实体"）。
/// 它们靠与泛词**精确同名**拿到 100 分（"用户→user"），实测「下单后怎么发通知给用户」
/// 前四全是这种样板。降到 0.3 后不再霸占种子名额，但仍可被召到。
const RELATION_ACCESSOR_DISCOUNT: f64 = 0.3;

/// 是否为 ORM 关联访问器：带 `MapsTo` 出边（映射到另一个实体）。
fn has_maps_to(id: i64, outgoing: &HashMap<i64, Vec<gt_domain::model::Edge>>) -> bool {
    outgoing
        .get(&id)
        .map(|es| es.iter().any(|e| e.kind.as_str() == "MapsTo"))
        .unwrap_or(false)
}

/// 过于通用的「架构名词」别名词（service / api / model / entity …）。
///
/// 这些词常作为类名后缀出现在 fqn 里（`DeliveryService`、`StoreCouponIssue`
/// 都含 `service` / `api`）。若把它们当"内容词"，动作加权会误判：发货的
/// `DeliveryService.save` 因 fqn 含 `service` 被当成"命中内容"而保留加权 —— 这正是
/// 精确版修复一度回退的坑。只有**真正的领域词**（coupon / order / stock / pay …）
/// 才算内容词（见 [`score_node`] 与向量路里的 `has_content` 判定）。
const GENERIC_NOUNS: &[&str] = &[
    "service", "api", "interface", "model", "entity", "config", "configuration", "data", "file",
    "log", "cache", "task", "job", "message", "session", "property", "attribute", "field", "page",
    "event", "queue", "topic", "schedule", "eventbus", "dict", "dictionary", "workbench",
    "dashboard", "third", "party",
];

/// 该 token 是否为通用架构名词（见 [`GENERIC_NOUNS`]）。
fn is_generic_noun(t: &str) -> bool {
    GENERIC_NOUNS.contains(&t)
}

/// 事件驱动查询里，事件处理器（监听器 / 订阅者）作为种子的引导分下限。
///
/// 详见 [`collect_event_seeds`]：处理器方法名往往极泛（统一叫 `handle`），纯词面分≈0，
/// 必须靠引导分进入种子集合、参与 BFS，否则「下单后怎么发通知」「退款成功后怎么回退」
/// 这类问题只召回一堆泛词。实际分值取「最强词面种子 × 0.85」并夹在 [下限, 700]，
/// 让监听器与它的直接被调者都能浮到前排，又不至于压过真正按名字命中的强种子。
const EVENT_SEED_MIN: f64 = 400.0;
/// 事件处理器的「直接被调者」加分：把「监听器 → 业务处理器」链路相对普通的
/// 「种子 → 一跳邻居」上浮，缓解事件处理器被通用 CRUD 淹没。
const EVENT_CALLEE_BOOST: f64 = 1.6;
/// 事件种子上限：命中的监听器可能很多（全工程的 `*Listener` 都含 `order`/`user` 等泛词），
/// 只保留按相关度（命中查询词数）排序后的前 N 个，避免把 BFS 与种子清单刷屏。
const EVENT_SEED_CAP: usize = 10;

/// 事件驱动查询识别：用户问的是「X 之后怎么 Y」「成功后…」「事件 / 监听 / 回调…」这类
/// 还原「触发 → 监听器 → 处理器」链路的问题。正解往往是事件处理器（命名极泛的
/// `*Listener` / `*Subscriber` 类或 `handle` / `onX` 方法），而非按名字直接命中的普通方法。
///
/// 只认「时序 / 显式事件」信号，不认「通知 / 下单」这类动作词 —— 否则普通动作查询会被
/// 误判为事件查询而混入监听器种子（动作意图由 [`action_intent`] 单独处理）。
fn event_intent(q: &str) -> bool {
    const KW: &[&str] = &[
        // 中文：时序 / 后置动作
        "之后", "之后怎么", "后怎么", "成功后", "完成后", "到账后", "支付后", "下单后", "退款后",
        "发货后", "创建后", "登录后", "注册后", "支付成功", "下单成功",
        // 中文：显式事件语义
        "事件", "监听", "触发器", "回调", "订阅",
        // 英文
        "after", "on success", "once", "on complete",
        "event", "listener", "subscribe", "observer", "trigger", "callback",
    ];
    let low = q.to_lowercase();
    KW.iter().any(|k| low.contains(&k.to_lowercase()))
}

/// 该节点是否为事件处理器（监听者 / 订阅者 / 观察者）。纯按命名约定 + 命名空间判断，
/// 与具体框架无关（ThinkPHP 的 `*Listener`、Laravel 的 `EventListener`、Spring 的
/// `@EventListener`、NestJS 的 `@OnEvent` 都覆盖得到）。
fn is_event_handler(node: &Node) -> bool {
    let name = node.name.to_lowercase();
    let class_part = node
        .fqn
        .as_deref()
        .map(|f| f.split("::").next().unwrap_or(f).to_lowercase())
        .unwrap_or_default();
    const SUFFIXES: &[&str] = &[
        "listener", "subscriber", "observer", "eventhandler", "eventsubscriber",
        "eventlistener", "eventconsumer", "consumer",
    ];
    if SUFFIXES.iter().any(|s| class_part.ends_with(s) || name.ends_with(s)) {
        return true;
    }
    // 位于 event / listener / observer / subscriber / handler 命名空间或路径下的
    // `handle` / `listen` / `dispatch` / `__invoke` / `onX` 方法，视为事件处理方法。
    let in_event_ns = node.fqn.as_deref().map(|f| {
        let fl = f.to_lowercase();
        ["listener", "event", "observer", "subscriber", "handler", "eventbus", "events"]
            .iter()
            .any(|t| fl.contains(t))
    }).unwrap_or(false);
    if in_event_ns {
        const HANDLER_METHODS: &[&str] = &["handle", "listen", "dispatch", "__invoke", "onevent"];
        if HANDLER_METHODS.iter().any(|m| name == *m) {
            return true;
        }
        if name.starts_with("on")
            && name.chars().nth(2).map(|c| c.is_uppercase()).unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// 收集事件处理器种子（见 [`is_event_handler`]，及 `Event` 节点）。
///
/// 仅返回「尚未进入已有种子」且「可作为种子」的节点；事件处理器（监听器 / 订阅者 /
/// Event 节点）数量可能很多，必须要求它的**名字或 fqn 命中任一查询词**才作种子，
/// 否则会把全工程的监听器都灌进 BFS，退化成"监听器汤"，反而淹没真正相关的那一个。
///
/// 命中的查询词越多（横跨越多概念）越相关，按匹配词数降序只保留前 [`EVENT_SEED_CAP`]
/// 个 —— 例如「下单后发通知」下 `OrderCreateAfterListener`（含 order）理应压过无关的
/// `OrderDeliveryListener`，但二者都只命中 `order` 时由上限兜底，避免刷屏。
///
/// 种子分值按**命中词的稀有度（idf）**加权：一个监听器若只命中 `order` 这类泛词，
/// 它和同概念的所有兄弟监听器（`OrderCreate`/`OrderRefund`/`OrderPaySuccess`…）一样被
/// 拉到平手；但若它命中 `notify` / `coupon` / `refund` 这类稀有概念，就应压过只命中泛词的
/// 兄弟 —— 这正是「下单后**发通知**」要让 `NotifyListener` 压过 `OrderRefund*Listener`、
/// 「退款成功**回退优惠券**」要让 `OrderRefund*Listener` 压过 `OrderCreate*Listener` 的通用依据。
fn collect_event_seeds<'a>(
    nodes: &[Node],
    index: &'a HashMap<i64, &'a Node>,
    existing: &HashSet<i64>,
    match_terms: &[String],
    base_score: f64,
) -> Vec<(f64, Vec<String>, &'a Node)> {
    const SKIP_KINDS: &[&str] = &["I18nKey", "Page", "EventBus"];
    // 文档频率 df：含该词（名字或 fqn）的节点数。df 越小越稀有，idf = ln(N/df) 越大。
    let total = nodes.len().max(1);
    let df: HashMap<String, usize> = match_terms
        .iter()
        .map(|t| {
            let tl = t.to_lowercase();
            let c = nodes
                .iter()
                .filter(|n| {
                    n.name.to_lowercase().contains(&tl)
                        || n.fqn
                            .as_deref()
                            .map(|f| f.to_lowercase().contains(&tl))
                            .unwrap_or(false)
                })
                .count();
            (t.clone(), c)
        })
        .collect();
    // (分值, 节点) 收集后按分值降序再截断，保证最相关的监听器进 BFS。
    let mut candidates: Vec<(f64, i64)> = Vec::new();
    for node in nodes {
        let id = node.id.get();
        if existing.contains(&id)
            || DEFAULT_EXCLUDED_KINDS.contains(&node.kind.as_str())
            || SKIP_KINDS.contains(&node.kind.as_str())
        {
            continue;
        }
        if !(is_event_handler(node) || node.kind.as_str() == "Event") {
            continue;
        }
        // 事件处理器名字往往很泛（统一叫 `handle`），靠名字命中不了查询词；但**类名 /
        // fqn 通常带业务概念**（`OrderCreateAfterListener` 含 `order`、`NotifyListener`
        // 含 `notify`），统计它命中了哪些查询词。
        let low = node.name.to_lowercase();
        let fqn_low = node.fqn.as_deref().map(|f| f.to_lowercase()).unwrap_or_default();
        let matched: Vec<&String> = match_terms
            .iter()
            .filter(|t| {
                let t = t.to_lowercase();
                low.contains(&t) || fqn_low.contains(&t)
            })
            .collect();
        if matched.is_empty() {
            continue;
        }
        // 稀有度加权：命中词越稀有（idf 越大），该监听器越可能是「真正被触发的那条」。
        // 分值 = 词面相对基线 + idf 微调度，且整体夹在 [基线, 基线+200] 内 —— 这样事件种子
        // 只用于在「词面强种子之下」做**兄弟监听器间的排序微调**（notify 压过 order、refund
        // 压过 order），而不会整体压过 `create` / `notifyConfirm` 等强词面命中。
        let rarity: f64 = matched
            .iter()
            .map(|t| {
                let c = (*df.get(*t).unwrap_or(&0)).max(1);
                ((total as f64) / (c as f64)).ln().max(0.0)
            })
            .sum();
        let score = (base_score + rarity * 20.0).min(base_score + 200.0).max(EVENT_SEED_MIN);
        candidates.push((score, id));
    }
    // 分值高的优先；同分时用小 id 兜底，保证可复现。
    candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
    candidates
        .into_iter()
        .take(EVENT_SEED_CAP)
        .map(|(s, id)| (s, vec!["<event>".to_string()], index[&id]))
        .collect()
}

/// 把标识符按「驼峰边界 + 非字母数字边界」切成小写 token 序列。
///
/// 例：`StoreCouponIssue` → `["store","coupon","issue"]`；
/// `userAddressServices` → `["user","address","services"]`；
/// `HTTPResponse` → `["http","response"]`。
///
/// 旧实现把类标识符拼成无分隔串后做整串 `contains` 子串匹配，会把**词中间**的
/// 巧合片段当成命中：`Recorder` 含 `order`、`Reorder` 含 `order` 都会被误判成
/// 「命中订单内容」而保住动作加权。改成先切 token、再按 **token 前缀**匹配后，
/// `order` 只能命中 `order` / `orders`（复数）/ `orderItem`（驼峰首 token），
/// 不再误命中 `recorder`。规则纯依赖标识符形态，与具体工程无关。
///
/// 用前缀而非严格相等，是为了保住「复合 / 屈折」这类**合理**匹配：
/// `pay`→`payment`、`order`→`orders` 仍算命中，否则会把真正相关的领域类误降级。
fn split_ident_tokens(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    for i in 0..n {
        let c = chars[i];
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let is_upper = c.is_uppercase();
        if is_upper && !cur.is_empty() {
            let prev = chars[i - 1];
            let prev_is_lower = prev.is_lowercase();
            let prev_is_upper = prev.is_uppercase();
            let next_is_lower = i + 1 < n && chars[i + 1].is_lowercase();
            // 驼峰词首（foo|Bar）或缩写词尾（HTTP|Server）处切分。
            if prev_is_lower || (prev_is_upper && next_is_lower) {
                tokens.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// 该节点是否「命中内容词」：方法名或类标识符拆出的 token 里，存在某个**非通用**
/// token（coupon / order / stock …），且它不是通用 CRUD 动词、也不是架构名词
/// （service/api…）。仅当命中内容词时，纯 CRUD 动词方法才保留动作加权
/// （见 [`GENERIC_CRUD_METHODS`]）。
///
/// 注意：内容词来自「本次查询的 terms」，匹配用 **token 前缀**（而非整串子串），
/// 因此与具体工程无关 —— 问库存就认 `stock`、问支付就认 `pay`（含 `payment`），
/// 但不会因 `recorder` 里含 `order` 这种词中片段而误判。
fn has_content_word(node: &Node, terms: &[String]) -> bool {
    let name_tokens = split_ident_tokens(&node.name);
    let class_tokens = match node.fqn.as_deref() {
        Some(f) => {
            let class_part = f.split("::").next().unwrap_or(f);
            // 只取末段类名，剥离命名空间 / 目录路径 —— 否则 `order/DeliveryService`
            // 的目录 `order` 会被当成命中订单内容（一度回退的坑）。类本身与订单无关。
            let ident = class_part.rsplit(['\\', '/']).next().unwrap_or(class_part);
            if ident.is_empty() {
                name_tokens.clone()
            } else {
                split_ident_tokens(ident)
            }
        }
        None => name_tokens.clone(),
    };
    let all: Vec<&String> = name_tokens.iter().chain(class_tokens.iter()).collect();
    terms.iter().any(|raw| {
        let t = raw.to_lowercase();
        !GENERIC_CRUD_METHODS.contains(&t.as_str())
            && !is_generic_noun(&t)
            && all.iter().any(|tok| tok.starts_with(&t))
    })
}

/// 中文意图词 → 英文符号候选词（离线语义桥，无需模型）。
///
/// ⚠️ 这里**只保留跨领域通用的词**。曾经放过一整张电商词表（下单 / 订单 / 优惠 /
/// 折扣 / 优惠券 / 购物车 / 库存 / 商品 / 会员 …），后果是双向的：
/// * 在非电商工程里这些词永远不命中，桥等于不存在；
/// * `cohesion_multiplier` 的聚合加成又只对它们生效，排序行为随领域漂移。
///
/// 领域专属的「中文意图 → 本项目符号」一律走 [`RecallService::project_bridge`]：
/// 中文「意图词 → 英文代码 token」桥（**通用层**，与具体业务无关，任意代码库都成立）。
///
/// 两类都被收录，因为二者对跨语言召回同样关键：
/// * **操作动词**（查询 / 新增 / 删除 / 分页 / 撤销 / 回滚 …）→ 「自然语言 → 代码动作」的映射，
///   任何系统都有增删改查、分页、上传下载，是通用层。实测「分页查询文章列表」出不来 `findAll`，
///   正是缺了 `查询→find` / `列表→all` 这一层。
/// * **通用技术名词**（库存→stock / 令牌→token / 授权→oauth / 订单→order / 用户→user …）→
///   这些是**跨任意软件领域都成立**的基础词汇，不是某电商/金融工程的业务实体（具体商品类目、
///   业务单据类型才属于领域名词，交给工程自身 i18n 桥推导）。把它们收录进来，中文「撤销令牌」
///   才能展开出 `revoke token`、与 `revokeToken` 在向量空间里对齐——纯靠工程 i18n 桥覆盖不到。
///
/// 边界原则：只收「换个项目也大概率成立」的词；不收某个具体业务的专属实体/黑话。
const INTENT_ALIASES: &[(&str, &[&str])] = &[
    // ---- 通用操作动词（跨领域）----
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
    // ---- 通用技术名词（跨领域；非具体业务实体）----
    ("支付", &["pay", "payment", "checkout"]),
    ("付款", &["pay", "payment"]),
    ("用户", &["user", "member", "customer"]),
    ("角色", &["role"]),
    ("权限", &["permission", "authority"]),
    ("管理员", &["admin"]),
    ("订单", &["order"]),
    ("商品", &["product", "goods", "item"]),
    ("库存", &["stock", "inventory"]),
    // ---- 通用「可配置值」语汇：任意系统都有取消 / 时限 / 阈值 / 预警 ----
    // 缺了它们，「订单自动取消时间」这类提问只能命中宽泛的 order，
    // 而真正的配置键 `order_cancel_time` 因匹配不到 cancel / time 而落榜。
    ("取消", &["cancel"]),
    ("时间", &["time"]),
    ("时限", &["time", "timeout", "expire"]),
    ("阈值", &["threshold", "limit", "warn"]),
    ("预警", &["warn", "warning", "alert"]),
    ("余额", &["balance"]),
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
    // ---- 组合意图补词（中文特有复合说法 → 英文 token）----
    ("下单", &["order", "place", "create"]),
    ("扣减", &["deduct", "reduce", "decrement", "dec"]),
    ("失败", &["fail", "failure"]),
    ("不足", &["insufficient", "lack"]),
    // ---- 社交 / 信息流通用词（无 i18n 的工程也能靠这些命中 feed / follow）----
    ("动态流", &["feed", "activity"]),
    ("关注", &["follow"]),
    ("收藏", &["favorite", "bookmark"]),
    ("点赞", &["like"]),
    // ---- 支付 / 交易（让充值配置、余额等命中具体方法而非泛化 config 节点）----
    ("充值", &["recharge"]),
    ("退款", &["refund"]),
    ("支付", &["pay", "payment"]),
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

/// 意图词 → 类别标签（用于短语聚合加成）。
///
/// 中文意图词（"订单"）与它们展开出的英文 token（"order" / "orders"）归到同一类，
/// 这样节点只要同时命中跨类别的 token（如 "order" + "coupon"），就说明它贴合了
/// 「订单优惠」这种组合意图，而非孤立的「订单」。
fn alias_group_map() -> HashMap<String, String> {
    let mut m = HashMap::new();
    for (zh, en) in INTENT_ALIASES {
        m.insert((*zh).to_string(), (*zh).to_string());
        for e in *en {
            m.insert((*e).to_string(), (*zh).to_string());
        }
    }
    m
}

/// 命中多个意图类别（如「订单」+「优惠」）给聚合加成，让组合意图优先于孤立词。
///
/// 每多命中一个类别 +30%，封顶 +90%（最多 ×1.9）。
fn cohesion_multiplier(matched: &[String], group_map: &HashMap<String, String>) -> f64 {
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

/// 种子选择：词面 top-k 与向量 top-k 取并集（去重），合并分 = 词面分 + 向量分。
///
/// 关键：跨语言节点（优惠→Coupon）即便词面分低，也能作为「向量种子」被 BFS 扩到，
/// 不再被 Order* 这类泛词面匹配淹没；且合并用相加而非取大，纯语义命中也能参与排序。
fn select_seeds<'a>(
    lexical: &HashMap<i64, (f64, Vec<String>)>,
    vector: &HashMap<i64, f64>,
    index: &'a HashMap<i64, &'a Node>,
) -> Vec<(f64, Vec<String>, &'a Node)> {
    let mut lex: Vec<(f64, i64)> = lexical.iter().map(|(id, (s, _))| (*s, *id)).collect();
    // 同分按节点 id 兜底，保证种子选择可复现（同上：避免 HashMap 顺序引入随机）。
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

    // 文案 / 页面类节点只做上下文，不当种子。
    //
    // 中文查询的词面命中大量落在这两类上（中文 i18n 文案、`/pages/...` 路径），
    // 它们会把种子名额占满，而 BFS 从它们出发只通向前端文件 —— 实测中文查询的
    // 向量信号（余弦 0.47~0.53，远超阈值）就是这样被挤掉的。
    // i18n 桥的价值在于它提供的**英文 token**，不依赖这些节点本身当种子。
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

/// 节点用于向量编码的文本（名字 + 种类 + fqn + identity + i18n 桥富化）。
fn node_embed_text(node: &Node, enrich: &EnrichIndex) -> String {
    let mut s = String::new();
    s.push_str(node.kind.as_str());
    s.push(' ');
    s.push_str(&node.name);
    // 再补一份「切分后」的写法：多语言语义模型很难把整串粘连标识符
    // （`decProductAttrStock`）与中文查询（「库存扣减」）在同一空间里对齐，
    // 切成 `dec product attr stock` 后才能被语义匹配到。
    // 这是纯通用的处理 —— 不依赖任何领域词表，对任何语言/命名风格都成立。
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
    // 节点文本富化：用工程自身 i18n 桥「反向」命中本节点的 token，把中文短语 + 同义词补进
    // 嵌入文本。例：`storeCoupon` 的 token `coupon` 命中桥上「优惠券 → coupon…」，于是节点
    // 文本带上「优惠券」，中文查询「优惠」就能在向量空间里直接对齐，而不必靠跨语言硬对齐。
    // 纯数据驱动、零配置：工程自带 i18n / 源码中文片段即可，电商 / 金融 / 游戏一视同仁。
    // 取名字与 fqn 的全部 token 去查桥；中文短语优先（直接对齐中文查询），其次英文同义词；
    // 总量封顶，避免节点文本被无关短语撑爆。
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

/// 是否参与向量编码的节点。
///
/// `Method` / `Function` 曾因"占图大多数"被整体排除，副作用是中文语义查询无法直接命中
/// 业务方法 —— 只能靠 `INTENT_ALIASES` 词面桥，或先命中类再经 BFS 带出，质量有天花板。
/// 现把它们纳入向量空间以抬高召回质量上限，编码成本用 `ensure_cached_with` 的 `BATCH`
/// 分批 + 跳过 [`DEFAULT_EXCLUDED_KINDS`] 来兜底。
///
/// 早期版本额外跳过了 `getXxx` / `setXxx` 访问器，理由是"数量大、语义由所属字段承载"。
/// 但中文「获取 / 查询」意图的落点往往正是 `getXxx` 业务方法：`getWorkbench`、`getDictData`、
/// `getAdminByUsername`、`setRechargeConfig` 全是业务方法而非纯字段访问器，被一刀切排除后
/// 直接从向量空间消失，语义召回永远召不到它们。各种语言里 `getXxx` 都是中文召回的高频真答案，
/// 因此这里**不再**按访问器过滤 —— 纳入所有带源位置的方法 / 函数。
fn is_vector_kind(node: &Node) -> bool {
    let k = node.kind.as_str();
    if DEFAULT_EXCLUDED_KINDS.contains(&k) {
        return false;
    }
    if k == "Method" || k == "Function" {
        // 只要带源位置（有 `file_id`）就纳入：跳过无源位置的合成节点，但不按 get/set 过滤，
        // 以免误杀中文「获取 / 查询」意图直接对应的业务方法。
        return node.file_id.is_some();
    }
    true
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
    10
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

/// 召回质量档位。
///
/// 召回质量**方差极大**：有的查询正解在前二，有的**两个意图都落空**、前排全是
/// 泛词噪声。但两者返回的东西长得一样 —— 下游（AI IDE）会同等信任，于是
/// **静默失败**成了最坏的失败模式。这里把质量显式报出去，让调用方能降级到
/// grep / 自行阅读。判定只用与项目无关的信号（特征词覆盖率 + 头部分差）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum RecallQuality {
    /// 特征词基本命中、头部分差健康 —— 可直接采信。
    High,
    /// 部分特征词未命中，或命中分散 —— 建议结合列表自行判断。
    Medium,
    /// 多数特征词未命中，前排为泛词匹配 —— 建议改走 grep / 自行阅读。
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
    /// 召回置信度（0~1）：特征词覆盖率与头部分差的加权。
    pub confidence: f32,
    /// 质量档位（见 [`RecallQuality`]）。
    pub quality: RecallQuality,
    /// 档位判定依据（人话）。
    pub quality_reason: String,
    /// 未命中的**特征词**（去掉通用 CRUD 动词 / 架构名词后仍有区分度的查询词）。
    /// 质量偏低时可直接拿这些词去 grep。
    pub missing_terms: Vec<String>,
}

/// 节点嵌入文本（`node_embed_text`）的版本。
///
/// **改动 `node_embed_text` 的构造方式时必须递增此值**（例如加标识符切分）。
/// 否则 `ensure_cached_with` 只补"缺失节点"、从不重算已缓存的向量，过时向量会被
/// 一直静默复用 —— 曾因此把"向量过时"误判成"语义模型能力不足"。
const EMBED_TEXT_VERSION: u32 = 1;

/// 落盘的向量文件信封：带版本，版本不符即整份失效重算。
#[derive(Serialize, Deserialize)]
struct PersistedEmbeds {
    version: u32,
    vectors: HashMap<i64, Vec<f32>>,
}

/// 召回服务。
pub struct RecallService {
    store: Arc<dyn Persistence>,
    fs: Arc<dyn FileSystem>,
    _scanner: Arc<dyn FileScanner>,
    /// 快速编码器（始终可用，离线哈希）：语义向量未预热时，召回立刻用它对查询 + 节点编码，
    /// 保证 UI 永不阻塞；语义质量在后台预热完成后自动接管。
    fast_embedder: Arc<dyn Embedder>,
    /// 语义编码器（真实 bge-m3，按需加载）。为 `None` 时只有词面 / 快速向量路，无后台预热。
    semantic_embedder: Option<Arc<dyn Embedder>>,
    /// 快速向量缓存（哈希，瞬时可重算，不落盘）。
    fast_cache: Arc<Mutex<HashMap<i64, Vec<f32>>>>,
    /// 语义向量缓存（bge，落盘到 `embed_persist_dir`）。与快速缓存分开放，避免维度混用。
    node_embed_cache: Arc<Mutex<HashMap<i64, Vec<f32>>>>,
    /// 已预热（bge 向量已算好并落盘）的工程集合；按工程粒度判定是否走语义路。
    warmed_projects: Arc<Mutex<HashSet<i64>>>,
    /// 正在后台预热中的工程集合（防重复 spawn）。
    warming_projects: Arc<Mutex<HashSet<i64>>>,
    /// 是否允许后台异步预热（仅 HTTP 生产入口开启；CLI / 测试关闭，避免 spawn 线程）。
    enable_async_warmup: bool,
    /// 语义向量持久化目录（`<dir>/<project_id>.json`）。建图时【不】写入；
    /// 仅在召回后台预热 / 手动 `embed` 命令里计算并落盘，重启后直接加载。
    /// 为 `None` 时不持久化（纯内存缓存）。
    embed_persist_dir: Option<PathBuf>,
    /// 工程 i18n 桥缓存：`工程 id -> [(中文文案, 该文案 key 切出的英文 token)]`。
    /// 中文查询经由它映射到**本项目**的符号，不依赖任何领域专属词表。
    bridge_cache: Arc<Mutex<HashMap<i64, Vec<(String, Vec<String>)>>>>,
}

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
        )
    }

    /// 注入自定义语义编码器（如测试假编码器）。默认关闭后台预热，避免 spawn 线程。
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
        )
    }

    /// 注入语义编码器 + 共享语义向量缓存。
    /// `semantic_embedder` 为 `None` 时仅词面 / 快速向量路，无后台预热。
    pub fn with_embedder_and_cache(
        store: Arc<dyn Persistence>,
        fs: Arc<dyn FileSystem>,
        scanner: Arc<dyn FileScanner>,
        semantic_embedder: Option<Arc<dyn Embedder>>,
        node_embed_cache: Arc<Mutex<HashMap<i64, Vec<f32>>>>,
        embed_persist_dir: Option<PathBuf>,
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
            enable_async_warmup: false,
            embed_persist_dir,
            bridge_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 开启后台异步预热（仅 HTTP 生产入口调用）：语义向量未就绪时召回立即用快速编码器返回，
    /// 同时 spawn 线程把 bge 向量算好落盘；之后该工程自动切到语义路。
    pub fn with_async_warmup(mut self) -> Self {
        self.enable_async_warmup = true;
        self
    }

    /// 清空节点向量缓存（图被重建 / 重新扫描后调用，避免命中过期向量）。
    /// 同时复位预热状态，使下次召回重新走快速路并（生产入口）后台重新预热。
    pub fn clear_node_cache(&self) {
        self.fast_cache.lock().unwrap().clear();
        self.node_embed_cache.lock().unwrap().clear();
        self.warmed_projects.lock().unwrap().clear();
        self.warming_projects.lock().unwrap().clear();
        // 图重建后 i18n 文案可能变了，桥必须一起失效。
        self.bridge_cache.lock().unwrap().clear();
    }

    /// 从持久化文件载入某工程的 bge 向量到语义缓存（文件不存在则跳过），并标记该工程已预热。
    fn load_persisted(&self, path: &Path, project_id: ProjectId) {
        load_persisted_into(
            path,
            &self.node_embed_cache,
            project_id,
            &self.warmed_projects,
        );
    }

    /// 将本工程 bge 向量写入持久化文件（只写当前工程参与召回的节点，避免跨工程串味）。
    fn persist(&self, path: &Path, nodes: &[Node]) {
        let cache = self.node_embed_cache.lock().unwrap();
        let vectors: HashMap<i64, Vec<f32>> = nodes
            .iter()
            .filter_map(|n| cache.get(&n.id.get()).cloned().map(|v| (n.id.get(), v)))
            .collect();
        drop(cache);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let env = PersistedEmbeds {
            version: EMBED_TEXT_VERSION,
            vectors,
        };
        if let Ok(data) = serde_json::to_vec(&env) {
            let _ = std::fs::write(path, data);
        }
    }

    /// 确保参与向量召回的主题级节点都已编码进给定缓存（未编码的批量编码并回填）。
    /// 与具体查询无关，可被快速路 / 语义路 / 手动 `embed` 命令复用。
    fn ensure_cached_with(
        &self,
        project_id: ProjectId,
        nodes: &[Node],
        embedder: &Arc<dyn Embedder>,
        cache: &Mutex<HashMap<i64, Vec<f32>>>,
    ) {
        const BATCH: usize = 256;
        // 节点文本富化：用本工程 i18n 桥反建索引，让中文查询能直接在向量空间对齐领域实体。
        let enrich = build_enrich_index(&self.project_bridge(project_id, nodes));
        let mut pending: Vec<(i64, String)> = Vec::new();
        for node in nodes {
            if !is_vector_kind(node) {
                continue;
            }
            let id = node.id.get();
            if cache.lock().unwrap().contains_key(&id) {
                continue;
            }
            pending.push((id, node_embed_text(node, &enrich)));
        }
        for chunk in pending.chunks(BATCH) {
            let texts: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
            let vecs = embedder.embed_batch(&texts);
            let mut cache = cache.lock().unwrap();
            for ((id, _), v) in chunk.iter().zip(vecs.into_iter()) {
                cache.insert(*id, v);
            }
        }
    }

    /// 工程 i18n 桥：读该工程的 `I18nKey` 节点，产出「中文文案 → 该 key 的英文 token」。
    ///
    /// 这是**通用**的「中文意图 → 本项目符号」桥：不依赖任何领域词表，工程自带 i18n
    /// 即可工作 —— 电商 / 金融 / 游戏 / 内部系统一视同仁。结果按工程缓存，
    /// 图重建时随 [`Self::clear_node_cache`] 失效。
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

    /// 查询命中了哪些中文短语，返回「短语 → 本项目 token」。
    ///
    /// 命中多条时按短语长度降序取前 N 条：越长的短语越具体（"库存不足" 优于 "库存"）。
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
        // 越长的短语越具体（"库存不足" 优于 "库存"）。
        hits.sort_by(|a, b| b.0.chars().count().cmp(&a.0.chars().count()));
        hits.truncate(MAX_HITS);
        hits
    }

    /// 手动预热：把某工程全部主题级节点 bge 向量计算并持久化（建图不在此时发生）。
    /// 用户可在后台跑 `graphtell embed --project N`，之后所有召回与重启都瞬时命中缓存。
    pub fn warm_up(&self, project_id: ProjectId) -> Result<usize> {
        let emb = match &self.semantic_embedder {
            Some(e) => e,
            None => {
                tracing::warn!("未配置语义编码器（缺 bge 权重），embed 无操作");
                return Ok(0);
            }
        };
        let nodes = fetch_nodes(self.store.as_ref(), project_id)?;
        if let Some(dir) = &self.embed_persist_dir {
            let path = dir.join(format!("{}.json", project_id.get()));
            self.load_persisted(&path, project_id);
        }
        self.ensure_cached_with(project_id, &nodes, emb, &self.node_embed_cache);
        if let Some(dir) = &self.embed_persist_dir {
            let path = dir.join(format!("{}.json", project_id.get()));
            self.persist(&path, &nodes);
        }
        self.warmed_projects.lock().unwrap().insert(project_id.get());
        Ok(nodes
            .iter()
            .filter(|n| is_vector_kind(n))
            .count())
    }

    /// 诊断：给定查询，输出与各节点向量的余弦（降序）。
    ///
    /// 用来判定「中文查询召不到目标符号」到底属于哪一类问题：
    /// * **模型 / 文本问题** —— 目标符号的余弦本来就低于 [`VECTOR_THRESHOLD`]，
    ///   改进方向是节点嵌入文本或模型本身；
    /// * **阈值 / 排序问题** —— 余弦其实够高，却被阈值、向量种子预算或排序挡掉，
    ///   改进方向是调阈值 / 预算 / 排序。
    ///
    /// `name_filter` 非空时只看名字含这些子串的节点 —— 大工程全量编码要几十分钟，
    /// 诊断通常只关心目标符号。
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
        let alias_terms = expand_intent_aliases(query);
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

    /// 把一句话拆成多个**独立意图**。
    ///
    /// 「怎么修改商品库存预警阈值、修改订单自动取消时间」这类提问把两个独立问题
    /// 塞进一句话。若当成一个词袋召回，两个意图的词互相干扰，且 `limit` 会被
    /// 单一意图的高分噪声占满 —— 实测这两条子问题**单独查都能找到答案**
    /// （`product_stock_job` / `ConfigKey order_cancel_time`），合起来却双双跌出前 20。
    /// 因此拆开分别召回、再按意图合并（见 [`RecallService::recall`]）。
    ///
    /// 只有拆出 ≥2 段且都够长时才算多意图，否则退回单意图，零回归。
    fn split_intents(q: &str) -> Vec<String> {
        // 先按连接词切，再按标点切。
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
            // 过短片段（语气词 / 残留标点旁支）不单独成意图。
            .filter(|s| s.chars().count() >= 4)
            .collect();
        if out.len() >= 2 { out } else { Vec::new() }
    }

    /// 多意图结果合并：按意图**轮询**取一条，保证每个意图都有代表进入最终列表
    /// （否则高分意图会再次把其它意图挤空），全程按 node_id 去重。
    fn merge_intent_hits(groups: Vec<Vec<RecallHit>>, limit: usize) -> Vec<RecallHit> {
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

    /// 评估召回质量（见 [`RecallQuality`]）。
    ///
    /// 用三条**与项目无关**的信号，因此换任何工程都成立：
    /// 1. **概念覆盖率** —— 查询里的中文意图概念（下单 / 优惠 / 通知 …）有多少被
    ///    **有信息量的命中**（排除 ORM 关联样板）命中；按概念而非按词统计，
    ///    命中任一英文展开即算覆盖（优惠 → coupon 或 discount 都行）；
    /// 2. **头部分差** —— top1 相对第 2~5 名均值的领先幅度，一堆同质噪声时该值极小；
    /// 3. **概念内聚度** —— 是否有同一条命中同时覆盖 ≥2 个概念。缺了这条，
    ///    "下单后怎么发通知"这类查询会因每个概念分别被不同噪声节点撞上而**误判 High**。
    ///
    /// 阈值**保守**：宁可报低，也不给假信心（误判比不判更糟）。
    ///
    /// # 已知局限（不要重复踩）
    ///
    /// 本函数**无法区分"命中了"和"命中对了"**：
    /// 噪声节点往往也**真实**命中了查询词 —— `UserAddressServices::create` 确实同时
    /// 覆盖"下单 + 用户"，正如正解 `StoreCouponIssue::edit` 覆盖"修改 + 优惠"。
    /// 区别不在结构而在语义（本查询要的是"下单 + **通知**"这一对），词面 + 图结构判断不了。
    /// 因此**结构型信号到此为止**，再往下需要语义理解。
    ///
    /// 试过并**放弃**的两条收紧方案（都会误伤好查询，勿重试）：
    /// * 只认名字命中（不认 fqn）—— 会把「如何修改下单优惠」误判：其"优惠"只存在于
    ///   类名 `StoreCouponIssue`，名字里没有；
    /// * 认名字 + 类标识符 —— 挡不住 `UserAddressServices`（user 就在类名里），仍报健康。
    ///
    /// # TODO（能力缺口，非本函数可解）
    ///
    /// **事件驱动链路召回**：像"下单后怎么发通知给用户""退款成功后怎么回退优惠券"
    /// 这类查询，正解是「下单事件 → 监听器 → 消息服务」这条链，需要沿
    /// `Triggers` / `PublishesTo` / `ListensTo` 做**定向链路展开**，而不是按名字匹配。
    /// 在这之前，这类查询只能靠 [`RecallQuality`] 提示调用方降级（grep / 自行阅读）。
    fn assess_quality(
        query: &str,
        hits: &[RecallHit],
        boilerplate: &HashSet<i64>,
    ) -> (RecallQuality, f32, String, Vec<String>) {
        // 2) 只认"有信息量"的命中：ORM 关联样板（hasOne 之类）不算数。
        let mut matched: HashSet<String> = HashSet::new();
        for h in hits.iter().filter(|h| !boilerplate.contains(&h.node_id.get())) {
            for t in &h.matched_terms {
                matched.insert(t.to_lowercase());
            }
        }

        // 1) 特征**概念**（而非词）：查询里出现的中文意图词（下单 / 优惠 / 通知 …）。
        //
        // 按"概念"而不是"词"算覆盖率是必须的：
        // * 中文词本身匹配不到英文标识符，若逐词统计会把覆盖率无谓拉低（实测四条查询全被判 Low）；
        // * 一个概念只要命中它的**任一**英文展开即算覆盖（优惠 → coupon 或 discount 都行），
        //   否则同义展开会再次拉低。
        let concepts: Vec<(&str, &[&str])> = INTENT_ALIASES
            .iter()
            .filter(|(zh, _)| query.contains(*zh))
            .map(|(zh, ens)| (*zh, *ens))
            .collect();
        // 覆盖率按**概念数**算（不是关键词条数），所以未命中集合先只留概念。
        let missing_concepts: Vec<(&str, &[&str])> = concepts
            .iter()
            .filter(|(_, ens)| !ens.iter().any(|e| matched.contains(&e.to_lowercase())))
            .map(|(zh, ens)| (*zh, *ens))
            .collect();

        let coverage = if concepts.is_empty() {
            1.0
        } else {
            1.0 - (missing_concepts.len() as f32 / concepts.len() as f32)
        };

        // 对外输出的**可检索关键词**：中文概念 + 它的英文展开。
        //
        // 只给中文字面是不够的：代码库里的标识符是英文，让 AI IDE 拿「回调」去 grep
        // 一个 PHP 工程什么都搜不到 —— 实测这条退路建议形同虚设。两者都给才真能搜到。
        let mut missing: Vec<String> = Vec::new();
        for (zh, ens) in &missing_concepts {
            missing.push((*zh).to_string());
            for e in *ens {
                if !missing.iter().any(|m| m == e) {
                    missing.push((*e).to_string());
                }
            }
        }
        // 说明文案仍只用中文概念，避免过长。
        let missing_zh: Vec<String> =
            missing_concepts.iter().map(|(zh, _)| (*zh).to_string()).collect();

        // 3) 头部分差。
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

        // 4) **概念内聚度**：有没有同一条命中同时覆盖 ≥2 个概念。
        //
        // 这是覆盖率看不出来的失败模式：复合查询（"下单后发通知"）的每个概念
        // 可能**分别**被互不相关的节点命中（下单→某个 create、通知→某个 notify），
        // 于是覆盖率 = 1.0 被判 High —— 但它说明的恰恰是"泛词各自撞名字"，
        // 召回根本没落到真正的业务代码上。实测该查询就栽在这里（假信心）。
        // 正解必然同时关联多个概念，因此内聚度 < 2 就是强烈的噪声信号。
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

        // **没有任何可评估概念时绝不允许判 High**。
        // 查询越生僻、越超出词表，可算的概念越少 —— 极端情况下 concepts 为空，
        // coverage 取默认值 1.0，于是"越生僻越说没问题"，恰好在最需要提醒时失灵。
        // 这违背"宁可报低"原则：没有概念 = 无法确认质量，至少报 Medium 并说明。
        let unevaluable = concepts.is_empty();

        let quality = if coverage < 0.5 || (concepts.len() >= 3 && best_cohesion < 2) {
            RecallQuality::Low
        } else if coverage >= 0.9 && confidence >= 0.55 && !fragmented && !unevaluable {
            RecallQuality::High
        } else {
            RecallQuality::Medium
        };

        let reason = if unevaluable {
            "查询未包含可评估的意图概念（多为领域专有词 / 生僻说法），无法确认召回质量，结果需自行判断".to_string()
        } else if quality == RecallQuality::Low && coverage >= 0.5 {
            "各概念分别被互不相关的节点命中（无任何命中同时覆盖两个概念），多为泛词各自撞名".to_string()
        } else {
            match quality {
            RecallQuality::Low => format!(
                "多数特征词未命中（{}），前排为泛词匹配 —— 建议改用 grep 或自行阅读确认",
                if missing_zh.is_empty() {
                    "命中过少".to_string()
                } else {
                    missing_zh.join("、")
                }
            ),
            RecallQuality::Medium => {
                if missing_zh.is_empty() {
                    "特征词已命中但头部区分度不足，结果可能分散".to_string()
                } else {
                    format!("部分特征词未命中（{}），结果可能不完整", missing_zh.join("、"))
                }
            }
            RecallQuality::High => "特征词基本命中、头部区分度健康".to_string(),
            }
        };

        (quality, confidence, reason, missing)
    }

    /// 质量告警块（Markdown）。质量 High 时返回空串 —— 不打扰正常结果。
    ///
    /// 关键是**给出可操作的退路**：不只说"我不行"，而是把未命中的特征词列出来，
    /// 让调用方（AI IDE）知道该去 grep 什么。
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
            RecallQuality::Low => "低",
            RecallQuality::Medium => "中",
            RecallQuality::High => "高",
        };
        let mut s = format!("> ⚠️ **召回质量：{}（置信度 {:.2}）** — {}\n", label, confidence, reason);
        if !missing.is_empty() {
            s.push_str(&format!(
                ">\n> 建议改用以下特征词自行检索：{}\n",
                missing.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>().join("、")
            ));
        }
        // 事件驱动查询的额外退路：正解多为命名极泛的事件监听器（`*Listener` /
        // `*Subscriber`，方法统一叫 `handle`），词面召回几乎命中不了。直接点明该去
        // 读哪类文件，比笼统的"头部区分度不足"更有操作性。
        if event {
            s.push_str(
                ">\n> 该查询疑似事件 / 流程驱动（X 之后怎么 Y）。图上正解多为事件监听器 \
                 （`*Listener` / `*Subscriber` / `@EventListener` / `@OnEvent`）或订阅者，\
                 方法名通常很泛（统一叫 `handle`），词面召回难以命中。建议直接阅读工程里 \
                 `listener` / `event` / `observer` / `subscriber` 目录下的对应监听器，以及相关的 \
                 `*Services` 实现，确认「触发 → 监听器 → 处理器」链路。\n",
            );
        }
        s.push('\n');
        s
    }

    /// 执行一次召回：单意图直接走 [`Self::recall_single`]；
    /// 多意图（顿号 / 逗号连接的多个独立问题）拆开分别召回再合并
    /// （见 [`split_intents`] / [`merge_intent_hits`]）。
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
        for part in &parts {
            let sub = RecallQuery { query: part.clone(), limit, ..q.clone() };
            let r = self.recall_single(project_id, &sub)?;
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
            // 多意图取**最差**档位与最低置信度：宁可报低，不给假信心。
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
        let markdown =
            render_markdown(project_id, q, &terms, &hints, &seeds, &hits, &advisory);
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
        })
    }

    /// 单意图召回的完整流程（多意图时由 [`RecallService::recall`] 分派多次）。
    fn recall_single(&self, project_id: ProjectId, q: &RecallQuery) -> Result<RecallResult> {
        let (terms, kind_hints) = parse_query(&q.query);
        // 动作意图（找实现代码）会反转种类偏好：方法 / 类优先于 HTTP 路由与基础设施。
        //
        // 但查询若已**点名配置项**（阈值 / 参数 / 开关 / 配置 …），用户要找的是
        // **那个配置值**而不是实现代码，此时绝不能再反转偏好 —— 否则 Method 被抬到
        // 1.5×，而 ConfigKey(1.2×) / Queue(0.7×) 被压低，实测正好把 `order_cancel_time`
        // 与 `product_stock_job` 挤出结果。点名了语义节点就以该节点为准。
        let wants_config = wants_config_value(&q.query);
        let action = action_intent(&q.query) && !wants_config;
        // 流程意图（还原调用链）：结果改按图拓扑排序（见 [`reorder_for_flow`]）。
        let flow = flow_intent(&q.query);
        // 事件驱动意图（X 之后怎么 Y / 成功后 …）：把被命名约定淹没的事件处理器
        // （监听器 / 订阅者）补成种子并上浮，见 [`collect_event_seeds`] / [`is_event_handler`]。
        let event = event_intent(&q.query);

        // 中文意图词展开：构造一份「用于匹配」的词表（不污染对外返回的 terms）。
        // 两路来源：
        //  1) 内置通用别名表（少量通用词）；
        //  2) **工程自身**的 i18n 桥：i18n 中文文案 → 该文案 key 里的英文 token。
        //     第 2 路是通用的 —— 只要工程带 i18n，中文查询就能落到本项目符号，
        //     不需要为电商 / 金融 / 游戏各维护一张领域词表。
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
            for kind in scan_kinds(self.store.as_ref(), project_id) {
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

        // 工程语义桥（依赖已装载的节点，故放在候选集之后）。
        // 命中的中文短语按长度降序采用：越长的短语越具体（"库存不足" 优于 "库存"）。
        let bridge_hits = self.match_project_bridge(project_id, &q.query, &nodes);
        for (_zh, toks) in &bridge_hits {
            for t in toks {
                if !match_terms.iter().any(|x| x == t) {
                    match_terms.push(t.clone());
                }
            }
        }

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
        //     命中多个「意图类别」（如同时命中 订单 + 优惠）给予短语聚合加成，
        //     使「订单优惠」这类组合意图优先于孤立的「订单」。
        let mut group_map = alias_group_map();
        // 命中的 i18n 文案各自成组，使「命中多个业务概念 → 聚合加成」在任意领域都成立，
        // 而不只是对内置电商词表生效。
        for (zh, toks) in &bridge_hits {
            group_map.insert(zh.clone(), zh.clone());
            for t in toks {
                group_map.insert(t.clone(), zh.clone());
            }
        }
        // ORM 关联访问器（`hasOne / hasMany` 样板）全集：既用于打分降权，
        // 也用于质量评估（这类命中不算"有信息量的命中"）。
        let boilerplate: HashSet<i64> = nodes
            .iter()
            .filter(|n| has_maps_to(n.id.get(), &outgoing))
            .map(|n| n.id.get())
            .collect();

        let mut lexical: HashMap<i64, (f64, Vec<String>)> = HashMap::new();
        for node in &nodes {
            if DEFAULT_EXCLUDED_KINDS.contains(&node.kind.as_str()) {
                continue;
            }
            // 纯动词且无内容词的方法**不当种子**：它只是动作词的同名词（「修改」→
            // save/update/edit 会命中全工程同名方法），不含任何主题信息，让它当种子
            // 只会把 BFS 引向无关的通用 CRUD。与 score_node 里的折扣是同一条原则。
            let verb_only = action
                && is_generic_crud_method(&node.name.to_lowercase())
                && !has_content_word(node, &match_terms);
            if verb_only {
                continue;
            }
            let (mut score, matched) = score_node(node, &match_terms, &kind_hints, &incoming, action);
            if score > 0.0 {
                score *= cohesion_multiplier(&matched, &group_map);
                // ORM 关联访问器（hasOne/hasMany 样板）不含业务逻辑，压低到不再占种子名额。
                if boilerplate.contains(&node.id.get()) {
                    score *= RELATION_ACCESSOR_DISCOUNT;
                }
                lexical.insert(node.id.get(), (score, matched));
            }
        }

        // 3b) 向量路：离线编码器对节点文本做软匹配，补足词面漏掉的跨语言种子。
        //     查询文本已带入展开出的英文意图词，使中文意图能靠近英文符号。
        // ---- 向量路（按工程选择生效空间，永不阻塞建图 / UI）----
        // 语义空间 = 语义编码器可用 **且** 该工程向量已就绪（已预热 或 有落盘文件）。
        //
        // 必须先尝试载入落盘文件再做判定：原实现把载入写在 `if use_semantic` 内，
        // 而 `use_semantic` 又要求 `warmed_projects` 已置位 —— 死锁：
        // 重启后缓存为空 → 判为未预热 → 永不载入 → 每次重启都全量重算 bge
        // （CRMEB 实测 56 分钟），落盘文件形同废纸。
        let mut use_semantic = self.semantic_embedder.is_some()
            && self.warmed_projects.lock().unwrap().contains(&project_id.get());
        if !use_semantic && self.semantic_embedder.is_some() {
            if let Some(dir) = &self.embed_persist_dir {
                let path = dir.join(format!("{}.json", project_id.get()));
                if path.exists() {
                    // 内部会置 `warmed_projects`，之后该工程直接命中语义空间。
                    self.load_persisted(&path, project_id);
                    use_semantic = true;
                }
            }
        }
        let (chosen, cache): (&Arc<dyn Embedder>, &Mutex<HashMap<i64, Vec<f32>>>) = if use_semantic {
            (self.semantic_embedder.as_ref().unwrap(), &self.node_embed_cache)
        } else {
            (&self.fast_embedder, &self.fast_cache)
        };
        let qvec = chosen.embed_query(&query_embed_text(&q.query, &alias_terms));

        // 仅语义空间才落盘（快速哈希空间无需持久化，瞬时可重算）。
        // 计算与落盘都【不在建图时发生】。
        self.ensure_cached_with(project_id, &nodes, chosen, cache);
        if use_semantic {
            if let Some(dir) = &self.embed_persist_dir {
                let path = dir.join(format!("{}.json", project_id.get()));
                self.persist(&path, &nodes);
            }
        }

        // 后台异步预热：语义编码器存在、本工程未预热、且入口开启了异步预热时，
        // 上面已用快速空间立即返回，这里 spawn 线程把 bge 向量算好落盘；
        // 完成后 `warmed_projects` 标记置位，之后该工程的召回自动切到语义空间。
        if !use_semantic
            && self.enable_async_warmup
            && self.semantic_embedder.is_some()
            && self.warming_projects.lock().unwrap().insert(project_id.get())
        {
            let (store, emb, sem_cache, dir, warmed, warming) = (
                self.store.clone(),
                self.semantic_embedder.clone().unwrap(),
                self.node_embed_cache.clone(),
                self.embed_persist_dir.clone(),
                self.warmed_projects.clone(),
                self.warming_projects.clone(),
            );
            thread::spawn(move || {
                warm_up_worker(store, emb, sem_cache, dir, project_id, warmed, warming);
            });
        }

        // 2) 逐节点取缓存向量打分。
        //    必须读上面选定的 `cache`（= 当前生效的向量空间）：冷路径把向量编码进
        //    `fast_cache`，若此处写死读 `node_embed_cache`（此时为空），向量分永远不贡献，
        //    召回就退化成纯词面 + BFS，白算一遍编码。
        let mut vector: HashMap<i64, f64> = HashMap::new();
        for node in &nodes {
            if !is_vector_kind(node) {
                continue;
            }
            let nvec = match cache.lock().unwrap().get(&node.id.get()).cloned() {
                Some(v) => v,
                None => continue,
            };
            let c = cosine(&qvec, &nvec);
            if c >= VECTOR_THRESHOLD {
                // 语义分默认不乘种类权重（避免把 Method/Function 挤出向量种子）。
                // 仅当查询是「找实现代码」的动作意图时，才按 [`rank_weight`] 给方法 / 类
                // 加权、压低 HTTP 路由与基础设施 —— 否则这类查询会被字面同词的路由压过
                // 真正的服务方法（storeCoupon / paymentOrder）。
                let kw = if action {
                    // 纯 CRUD 动词方法，仅当它**未命中内容词**（coupon/order…，来自 fqn）
                    // 时才取消动作加权 —— 与词面路 [`score_node`] 保持一致：发货的 save/update
                    // 沉底，而优惠券 / 订单的 edit 因 fqn 带 Coupon/Order 仍保住加权。
                    let nlow = node.name.to_lowercase();
                    if is_generic_crud_method(&nlow) {
                        let has_content = has_content_word(node, &match_terms);
                        if has_content {
                            rank_weight(node.kind.as_str(), true)
                        } else {
                            kind_weight(node.kind.as_str())
                        }
                    } else {
                        rank_weight(node.kind.as_str(), true)
                    }
                } else {
                    1.0
                };
                let s = c * VECTOR_WEIGHT * kw;
                let entry = vector.entry(node.id.get()).or_insert(0.0);
                *entry = (*entry).max(s);
            }
        }

        // 合并：词面分 + 向量分「相加」（不再是取大），让纯语义命中也能参与排序；
        // 种子取「词面 top-k ∪ 向量 top-k」的并集 —— 否则跨语言节点（优惠→Coupon）
        // 永远挤不进仅按总分排序的前 5，被 Order* 泛洪淹没。
        let mut seed_tuples = select_seeds(&lexical, &vector, &index);
        // 事件驱动查询：把被命名约定淹没的事件处理器（监听器 / 订阅者 / Event 节点）
        // 补成种子，否则「下单后怎么发通知」「退款成功后怎么回退」这类问题只能召回泛词。
        // 种子分值由命中词的稀有度（idf）决定，让命中 notify/coupon/refund 这类稀有概念的
        // 监听器压过只命中 order/user 等泛词的兄弟监听器（见 [`collect_event_seeds`]）。
        if event {
            // 事件种子基线取「最强词面种子 × 0.55」：低于词面强命中，保证 `create` /
            // `notifyConfirm` 等仍排在前排、且质量评估不会因特征词被淹没而掉到「低」；
            // idf 微调只在该基线之上区分兄弟监听器（见 [`collect_event_seeds`]）。
            let top_lex = lexical.values().map(|(s, _)| *s).fold(0.0_f64, f64::max);
            let event_base = (top_lex * 0.55).max(EVENT_SEED_MIN);
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

        // ---- 4) 扩展：沿链边 BFS，跳数衰减
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

        // ---- 5) 排序 + 截断 + 补位置
        let mut hits: Vec<RecallHit> = best.into_values().collect();
        // 枢纽抑制（**所有查询**生效，不只流程查询）：被全工程到处引用的通用底座
        // （中间件被 800~1200 条路由穿过、Request / Cache 之类）对任何查询都不是答案，
        // 但它们常作为种子的**一跳邻居**被带进来（此时分数 = 父种子 × 衰减，
        // 在 score_node 里抑制没用，必须作用在最终得分上）。按总入度平滑衰减，
        // 下限 0.4 —— 永不为 0，仍可被召到，只是不再占位。
        for h in hits.iter_mut() {
            let fan_in = incoming.get(&h.node_id.get()).map(|es| es.len()).unwrap_or(0);
            h.score *= hub_penalty(fan_in);
        }
        // 事件驱动查询：把「事件处理器 → 直接被调者」与「沿事件语义边相连的节点」上浮，
        // 让「监听器 → 业务处理器」链路相对普通的「种子 → 一跳邻居」更靠前。
        // 否则监听器虽被补成种子，它调用的正解仍会被通用 CRUD 淹没。
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
        // 同分时按节点 id 稳定排序：HashMap 迭代顺序会让同分命中的先后随机变化，
        // 导致同一查询两次召回结果不同 —— 评测因此无法复现（实测同一构建两次跑
        // hit@10 会差 1 条）。排序必须完全确定。
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.node_id.get().cmp(&b.node_id.get()))
        });
        // 同名上限：通用动词（find / update / create …）会把各处的同名方法一并拉进来，
        // 挤满整个命中列表（实测出现过前 5 条全是 `findAll`）。同名最多保留 2 条，
        // 保证结果的多样性。
        const MAX_SAME_NAME: usize = 2;
        let mut same: HashMap<String, usize> = HashMap::new();
        let mut kept: Vec<RecallHit> = Vec::with_capacity(hits.len());
        for h in hits {
            let c = same.entry(h.name.clone()).or_insert(0);
            if *c < MAX_SAME_NAME {
                *c += 1;
                kept.push(h);
            }
        }
        hits = kept;
        hits.truncate(q.limit.max(1));
        // 流程查询：把**已按相关性选出的**结果改排成链路顺序（入口 → … → 落库）。
        // 必须放在截断**之后**：否则无关的浅层兄弟节点（每个路由 / 控制器方法都是
        // 深度 0）会被提前、挤掉真正相关的深层节点（实测 `del_level` / `appleLogin`
        // 挤走了 `LoginServices::register`）。这里只改顺序、不改入选集合。
        // 非流程查询不进入此分支，排序与改动前完全一致。
        if flow {
            reorder_for_flow(&mut hits, &incoming);
        }

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

        let (quality, confidence, quality_reason, missing_terms) =
            Self::assess_quality(&q.query, &hits, &boilerplate);
        let advisory =
            Self::quality_advisory(quality, confidence, &quality_reason, &missing_terms, event);
        let markdown =
            render_markdown(project_id, q, &terms, &kind_hints, &seeds, &hits, &advisory);
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
        })
    }
}

/// 装载某工程参与召回的全部节点（按 [`scan_kinds`]）。
fn fetch_nodes(store: &dyn Persistence, project_id: ProjectId) -> Result<Vec<Node>> {
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

/// 从缓存抽取「本工程参与召回节点 → 向量」映射（用于落盘）。
fn collect_cache(
    cache: &Mutex<HashMap<i64, Vec<f32>>>,
    nodes: &[Node],
) -> HashMap<i64, Vec<f32>> {
    let cache = cache.lock().unwrap();
    nodes
        .iter()
        .filter_map(|n| cache.get(&n.id.get()).cloned().map(|v| (n.id.get(), v)))
        .collect()
}

/// 后台预热 worker：用语义编码器算好某工程全部主题级节点 bge 向量、落盘、标记预热完成。
/// 把落盘的工程向量载入给定缓存并标记该工程已预热（服务内与后台线程共用）。
fn load_persisted_into(
    path: &Path,
    cache: &Mutex<HashMap<i64, Vec<f32>>>,
    project_id: ProjectId,
    warmed: &Mutex<HashSet<i64>>,
) {
    let Ok(data) = std::fs::read(path) else {
        return;
    };
    // 旧格式（裸 map）与版本不符一律视为失效：宁可重算，
    // 也不能让过时向量被静默复用。
    let Ok(env) = serde_json::from_slice::<PersistedEmbeds>(&data) else {
        return;
    };
    if env.version != EMBED_TEXT_VERSION {
        return;
    }
    let mut cache = cache.lock().unwrap();
    for (k, v) in env.vectors {
        cache.entry(k).or_insert(v);
    }
    drop(cache);
    warmed.lock().unwrap().insert(project_id.get());
}

fn warm_up_worker(
    store: Arc<dyn Persistence>,
    embedder: Arc<dyn Embedder>,
    cache: Arc<Mutex<HashMap<i64, Vec<f32>>>>,
    persist_dir: Option<PathBuf>,
    project_id: ProjectId,
    warmed: Arc<Mutex<HashSet<i64>>>,
    warming: Arc<Mutex<HashSet<i64>>>,
) {
    let pid = project_id.get();
    let res: Result<()> = (|| {
        let nodes = fetch_nodes(store.as_ref(), project_id)?;
        // 节点文本富化：用本工程 i18n 桥反建索引，与 `ensure_cached_with` 保持一致，
        // 保证两条预热路径产出的节点向量完全相同。
        let enrich = build_enrich_index(&compute_bridge(&nodes));
        // 先载入落盘向量：否则重启后缓存为空，这里会把全量节点重算一遍
        // （CRMEB 实测 56 分钟），落盘文件等于没用。
        if let Some(dir) = &persist_dir {
            let path = dir.join(format!("{pid}.json"));
            if path.exists() {
                load_persisted_into(&path, &cache, project_id, &warmed);
            }
        }
        const BATCH: usize = 256;
        let mut pending: Vec<(i64, String)> = Vec::new();
        {
            let cache = cache.lock().unwrap();
            for node in &nodes {
                if !is_vector_kind(node) {
                    continue;
                }
                if cache.contains_key(&node.id.get()) {
                    continue;
                }
                pending.push((node.id.get(), node_embed_text(node, &enrich)));
            }
        }
        for chunk in pending.chunks(BATCH) {
            let texts: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
            let vecs = embedder.embed_batch(&texts);
            let mut cache = cache.lock().unwrap();
            for ((id, _), v) in chunk.iter().zip(vecs.into_iter()) {
                cache.insert(*id, v);
            }
        }
        if let Some(dir) = &persist_dir {
            if let Some(parent) = dir.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let path = dir.join(format!("{pid}.json"));
            let env = PersistedEmbeds {
                version: EMBED_TEXT_VERSION,
                vectors: collect_cache(&cache, &nodes),
            };
            if let Ok(data) = serde_json::to_vec(&env) {
                let _ = std::fs::write(path, data);
            }
        }
        warmed.lock().unwrap().insert(pid);
        Ok(())
    })();
    if let Err(e) = res {
        tracing::error!("后台预热工程 #{pid} 失败：{e}");
    }
    warming.lock().unwrap().remove(&pid);
}

/// 单次扫描的节点上限（防止超大库把一次召回拖成全表扫描）。
const SCAN_LIMIT: u32 = 200_000;

/// 兜底清单：库里读不到节点种类时才用它（保证降级可用）。
const FALLBACK_SCAN_KINDS: &[&str] = &[
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

/// 参与召回的节点种类 = 图上**真实存在**的种类 − [`DEFAULT_EXCLUDED_KINDS`]。
///
/// 这里早先硬编码了一份类型清单，后果是：新增语言适配器（Go / Rust / C# …）或流水线
/// 新增 kind 时，这些节点会被**静默排除在召回之外** —— 不是分数低，而是压根不参与。
/// 改为从库里取 `DISTINCT kind` 后，任意语言与未来新增类型都会自动纳入。
fn scan_kinds(store: &dyn Persistence, project_id: ProjectId) -> Vec<String> {
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
        Err(e) => tracing::warn!("读取节点种类失败，回退内置清单: {e}"),
    }
    FALLBACK_SCAN_KINDS.iter().map(|s| s.to_string()).collect()
}

// -------------------------------------------------- 工程 i18n 桥（数据驱动，零配置）

/// 从 i18n key（如 `order.pay.insufficient_balance`）切出英文 token。
///
/// 只保留 ASCII token：这里建的是「中文 → 英文符号」的桥，中文 token 留在桥上没意义。
fn key_tokens(key: &str) -> Vec<String> {
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

/// 是否含中日韩汉字。
fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// 从源码片段里抽出中文串（复用查询侧的 `cjk_runs`，但去掉单字噪声并去重）。
fn snippet_chinese(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in cjk_runs(s) {
        if r.chars().count() >= 2 && !out.contains(&r) {
            out.push(r);
        }
    }
    out
}

/// 从 `properties.locations[].file` 取「该文案所在模块」的 token：父目录名 + 文件名主干。
///
/// 例：`template/uni-app/components/payment/index.vue` → `payment`。
/// 这把中文文案桥到它真实出现的业务模块 —— 纯数据驱动，与领域、语言、框架无关。
/// 目录/文件名里大量通用词（`src` / `components` / `index` …）会被过滤，否则会把
/// 无关节点一并拉进来。
fn location_tokens(props: &serde_json::Value) -> Vec<String> {
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

/// 节点富化反向索引：节点名 / fqn 的某个 token → 命中到的「中文短语 + 该短语的英文 token」列表。
///
/// 由工程 i18n 桥反向得到：节点 `storeCoupon` 的 token `coupon` 命中桥上「优惠券 → coupon…」，
/// 于是给该节点嵌入文本补上「优惠券」，让中文查询「优惠」能直接在向量空间对齐，而不必靠跨语言硬对齐。
/// 纯数据驱动、零配置：工程自带 i18n / 源码中文片段即可，电商 / 金融 / 游戏一视同仁。
type EnrichIndex = HashMap<String, Vec<(String, Vec<String>)>>;

/// 由工程 i18n 桥（`Vec<(中文短语, 英文token)>`）反建节点富化索引。
fn build_enrich_index(bridge: &[(String, Vec<String>)]) -> EnrichIndex {
    let mut idx: EnrichIndex = HashMap::new();
    for (zh, toks) in bridge {
        for t in toks {
            idx.entry(t.clone()).or_default().push((zh.clone(), toks.clone()));
        }
    }
    idx
}

/// 工程 i18n 桥的数据计算（与缓存解耦）：遍历 i18n / 中文名 / 源码中文片段节点，
/// 产出「中文文案 → 该 key 的英文 token」。被 [`RecallService::project_bridge`] 调用并缓存，
/// 也被后台预热 worker 直接复用，确保两条预热路径产出的节点向量完全一致。
fn compute_bridge(nodes: &[Node]) -> Vec<(String, Vec<String>)> {
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

/// 按大小写边界切驼峰串（`insufficientBalance` → `insufficient` / `Balance`）。
fn split_camel(s: &str) -> Vec<String> {
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

/// 结构性「包含」边：命中容器（类 / 接口 / 表 / 契约）时，用来把它的成员带进召回。
///
/// 召回原先只沿调用链边扩展，后果是命中 `ArticleService` 却拿不到它的 `findAll`
/// —— 这一点连英文查询都复现（`article list pagination` 只出类、不出方法）。
/// 包含边是纯结构信息，与语言、领域、命名风格都无关。
fn is_containment_edge(kind: &str) -> bool {
    matches!(kind, "Declares" | "HasColumn" | "Extends" | "HandledBy")
}

/// 单个容器最多带出的成员数，避免一个大类的全部方法把命中列表挤满。
const MAX_MEMBERS_PER_CONTAINER: usize = 6;

/// 召回扩展用的邻居（双向）：调用链边 **+ 包含边**。
fn neighbours(
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
    action: bool,
) -> (f64, Vec<String>) {
    let name = node.name.to_lowercase();
    let fqn = node.fqn.as_deref().unwrap_or("").to_lowercase();
    let identity = node.identity.as_ref().map(|i| i.value.to_lowercase()).unwrap_or_default();

    // HTTP 路由的路径里 `save` / `update` / `edit` / `create` 是**写接口的命名约定** ——
    // 几乎每个写接口都带，对主题毫无区分度。实测「怎么修改商品库存预警阈值」被
    // `POST /product/crawl/save`、`PUT /user/save_give_level_time` 这类路由灌满前排，
    // 它们只靠「修改→save」+ 宽泛的「商品/订单」凑数，与该意图的特征词毫无关系。
    // 因此路由节点的路径里，通用 CRUD 动词**不计入命中**（与方法侧的
    // "纯动词无内容词打折"是同一条原则）。路由仍可靠路径里的业务词命中。
    let is_contract = node.kind.as_str().eq_ignore_ascii_case("HttpContract");

    let mut score = 0.0f64;
    let mut matched: Vec<String> = Vec::new();
    for t in terms {
        let t = t.to_lowercase();
        if is_contract && GENERIC_CRUD_METHODS.contains(&t.as_str()) {
            continue;
        }
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

    // 种类权重：语义节点（表 / 契约 / 事件…）是"主题级"答案，优先于方法；
    // 动作意图（找实现代码）时反转偏好（见 [`rank_weight`]）。
    // 纯 CRUD 动词方法（edit/save/update…）**只有在同时命中内容词（coupon/order…）时**
    // 才享受动作加权；否则（如发货的 save/update，只靠「修改」被加权）退回默认 1.0，
    // 避免无关域 CRUD 顶到顶部（见 [`GENERIC_CRUD_METHODS`]）。
    let has_content_term = has_content_word(node, terms);
    let verb_only = action && is_generic_crud_method(&name) && !has_content_term;
    if verb_only {
        // 只靠动词命中、无任何内容词时，**连基础匹配分也打折**：
        // 「修改」会展开成 save/update/edit/modify，而全工程到处都有叫 save / update
        // 的方法，它们仅凭同名就拿到 100 分精确匹配 —— 实测「怎么修改订单自动取消时间」
        // 被这批同名方法霸榜，真正的答案（ConfigKey order_cancel_time）反而进不了前 12。
        // 这与上面取消动作加权是同一条原则：纯动词且无内容词，不该占高位。
        score *= GENERIC_CRUD_VERB_ONLY_DISCOUNT;
    }
    let kw = if verb_only {
        kind_weight(node.kind.as_str())
    } else {
        rank_weight(node.kind.as_str(), action)
    };
    score *= kw;

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

/// 词面 / 向量路共用的「种类权重」。
///
/// 默认（`action=false`）沿用原 [`kind_weight`]：结构 / 契约类（表、HTTP 接口、事件…）
/// 是「主题级」答案，权重大于方法，使"订单相关的表"收敛到 `Table`。
///
/// 当查询意图是「找实现这段逻辑的代码」（`action=true`，含 修改 / 实现 / 计算 /
/// 校验 / 扣减 / 支付 / 回调 … 等动作动词）时反转偏好：给方法 / 类加权、压低
/// HTTP 路由与异步基础设施（事件 / 队列 / 缓存 / 定时 …）。否则这类查询会被字面同词的
/// 路由（`GET /marketing/coupon/edit`、`ANY /pay/notify`）的词面高分压过真正的服务方法
/// （storeCoupon / paymentOrder），而后者才是"代码在哪"的意图所指。
fn rank_weight(kind: &str, action: bool) -> f64 {
    if !action {
        return kind_weight(kind);
    }
    match kind {
        "Method" | "Function" => 1.5,
        "Class" | "Interface" | "Trait" | "Enum" => 1.3,
        "Table" => 1.4,
        "ConfigKey" | "I18nKey" | "Page" | "EventBus" => 1.2,
        "HttpContract" | "Event" | "Queue" | "Topic" | "Cache" | "Schedule" => 0.7,
        _ => 0.9,
    }
}

/// 默认种类权重（见 [`rank_weight`]）。
fn kind_weight(kind: &str) -> f64 {
    match kind {
        "Table" | "HttpContract" | "Event" | "Queue" | "Cache" | "Topic" | "Schedule" => 1.4,
        "ConfigKey" | "I18nKey" | "Page" | "EventBus" => 1.2,
        "Class" | "Interface" | "Trait" | "Enum" => 1.1,
        "Method" | "Function" => 1.0,
        _ => 0.8,
    }
}

/// 查询是否意在「找实现逻辑的代码」而非「找主题 / 路由」。
///
/// 命中动作动词即为真：词面 / 向量路据此反转种类偏好（见 [`rank_weight`]）。
/// 仅当为真时才改权重 —— 非动作查询完全沿用旧 [`kind_weight`]，零回归。
fn action_intent(q: &str) -> bool {
    const KW: &[&str] = &[
        // 中文动作
        "修改", "实现", "在哪", "代码", "方法", "函数", "逻辑", "计算", "处理", "校验", "拦截",
        "回滚", "扣减", "设置", "保存", "更新", "新增", "删除", "查询", "获取", "读取", "加载",
        "下单", "支付", "通知", "回调", "创建", "生成", "调用", "执行", "写入", "怎么", "如何",
        // 英文动作
        "modify", "implement", "code", "where", "compute", "handle", "validate", "intercept",
        "rollback", "deduct", "save", "update", "create", "add", "delete", "query", "get",
        "fetch", "load", "set", "pay", "notify", "callback", "invoke", "execute", "write", "how",
    ];
    let low = q.to_lowercase();
    KW.iter().any(|k| low.contains(&k.to_lowercase()))
}

/// 查询是否意在「还原一条流程 / 调用链」，而不是找单个实现点。
///
/// 这类查询的正确答案本质上**有序**：页面 / 路由 → 控制器 → 服务 → 落库。
/// 单纯按相关分平铺会把链路打乱（页面排在入口函数之后），且让谁都连的基础设施
/// 枢纽（`Request` 被调 300+ 次、`Cache` 400+ 次）霸占前排。命中流程词时才启用
/// 拓扑重排（见 [`reorder_for_flow`]）—— 与 [`action_intent`] 同样的守门方式：
/// **非流程查询完全沿用旧排序，零回归**。
/// 「可配置值」语汇：命中即说明用户要找的是**一个配置项**（阈值 / 参数 / 开关 …），
/// 而不是实现代码 —— 此时不应按动作意图反转种类偏好，否则 Method 被抬到 1.5×、
/// 而 ConfigKey(1.2×) / Queue(0.7×) 被压低，正好把真正的配置挤出结果。
///
/// **故意不走 `hint_map`**：提示词会被 [`strip_hint_chars`] 逐字从查询词里剔除，
/// 加"下限"会让 `下单` 变成 `单`。这里只在原始 query 上做包含判断，零副作用。
const CONFIG_WORDS: &[&str] = &[
    "阈值", "参数", "开关", "上限", "下限", "时长", "间隔", "配置", "配置项", "预警",
    // 「自动取消**时间**」这类时限本身就是个配置值；漏了它，该查询会走动作意图
    // （Method 1.5×）而把 ConfigKey 压下去，正解 `order_cancel_time` 直接落榜。
    "时间",
];

fn wants_config_value(q: &str) -> bool {
    CONFIG_WORDS.iter().any(|w| q.contains(w))
}

fn flow_intent(q: &str) -> bool {
    const KW: &[&str] = &[
        // 中文
        "流程", "链路", "调用链", "调用关系", "调用顺序", "调用过程", "步骤", "顺序", "生命周期",
        "流转", "怎么走", "走一遍", "经过",
        // 英文
        "flow", "call chain", "trace", "pipeline", "lifecycle", "walkthrough", "sequence",
    ];
    let low = q.to_lowercase();
    KW.iter().any(|k| low.contains(&k.to_lowercase()))
}

/// 流程方向边：调用 / 承接 / 落库方向，即「谁被谁调下去」。
/// 用于把命中排成「入口 → … → 落库」的链路（见 [`reorder_for_flow`]）。
fn is_flow_edge(kind: &str) -> bool {
    matches!(
        kind,
        "Calls" | "CallsHttp" | "HandledBy" | "PassesThrough" | "WritesDb" | "ReadsDb"
    )
}

/// 流程的**入口层**种类：HTTP 契约 / 页面。它们天然是链路起点 ——
/// 即便前端函数用 `CallsHttp` 指向某个路由，该路由仍是后端的**入口**而非中间节点，
/// 不该被算成"被前端调下来的下一层"（否则路由会排到服务方法之后）。
fn is_entry_kind(kind: &str) -> bool {
    matches!(kind, "HttpContract" | "Page")
}

/// 扇入超过该值的节点视为「枢纽」：全工程到处都在调用的通用底座
/// （`Request` 被调 300+ 次、`Cache` 400+ 次），不是任一流程的特有环节。
const HUB_FANIN: usize = 60;

/// 枢纽衰减系数：随扇入平滑压低，最低 0.4，永不为 0 —— 底座仍可被召到，
/// 只是不再霸占链路的前排（见 [`reorder_for_flow`]）。
fn hub_penalty(fan_in: usize) -> f64 {
    if fan_in <= HUB_FANIN {
        return 1.0;
    }
    let excess = (fan_in - HUB_FANIN) as f64;
    0.4 + 0.6 * (-excess / 200.0).exp()
}

/// 命中集合内沿流程方向的最长路径深度（入口 = 0）。回边按 0 处理以容忍环。
fn flow_depth(
    id: i64,
    callers: &HashMap<i64, Vec<i64>>,
    memo: &mut HashMap<i64, i64>,
    visiting: &mut HashSet<i64>,
) -> i64 {
    if let Some(d) = memo.get(&id) {
        return *d;
    }
    if !visiting.insert(id) {
        return 0; // 环：本次不再深入
    }
    let mut best = 0i64;
    if let Some(cs) = callers.get(&id) {
        for c in cs {
            best = best.max(flow_depth(*c, callers, memo, visiting) + 1);
        }
    }
    visiting.remove(&id);
    memo.insert(id, best);
    best
}

/// 命中集合里，沿流程边与**种子**连通的那些节点。
///
/// 判据：在流程边构成的无向图上求连通分量，只保留含至少一个直接命中
/// （`direct`，即种子本身）的分量。这样才叫「这次查询的链路」——
/// 否则每个恰好没有上游的兄弟节点都会自成深度 0 而挤到前面。
fn anchored_components(hits: &[RecallHit], adj: &HashMap<i64, Vec<i64>>) -> HashSet<i64> {
    let mut out: HashSet<i64> = HashSet::new();
    let mut seen: HashSet<i64> = HashSet::new();
    for h in hits.iter().filter(|h| h.direct) {
        let start = h.node_id.get();
        if !seen.insert(start) {
            continue;
        }
        let mut stack = vec![start];
        while let Some(n) = stack.pop() {
            out.insert(n);
            if let Some(ns) = adj.get(&n) {
                for m in ns {
                    if seen.insert(*m) {
                        stack.push(*m);
                    }
                }
            }
        }
    }
    out
}

/// 流程查询的拓扑重排：命中集合内沿 [`is_flow_edge`] 求深度，按「入口 → … → 落库」
/// 排序；与链路无关的邻居（只经包含 / 配置等非流程边连着）沉到最后；枢纽按扇入衰减。
///
/// 仅在 [`flow_intent`] 为真时调用，其余查询的排序完全不受影响。
fn reorder_for_flow(
    hits: &mut Vec<RecallHit>,
    incoming: &HashMap<i64, Vec<gt_domain::model::Edge>>,
) {
    let on: HashSet<i64> = hits.iter().map(|h| h.node_id.get()).collect();

    // node → 同在命中集合里的「上游调用者」；同时建无向邻接用于求连通分量。
    let mut callers: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut adj: HashMap<i64, Vec<i64>> = HashMap::new();
    for h in hits.iter() {
        let id = h.node_id.get();
        if let Some(es) = incoming.get(&id) {
            for e in es {
                let f = e.from_id.get();
                if is_flow_edge(e.kind.as_str()) && on.contains(&f) {
                    // 邻接用于连通性判定：入口也要连上，否则会被判成孤立分量而沉底。
                    adj.entry(id).or_default().push(f);
                    adj.entry(f).or_default().push(id);
                    // 深度只按「被谁调下来」累加；入口层恒为起点。
                    if !is_entry_kind(h.kind.as_str()) {
                        callers.entry(id).or_default().push(f);
                    }
                }
            }
        }
    }
    // 只有「含至少一个直接命中（种子）」的连通分量才算链路。
    // 否则彼此无关、只是恰好没有上游的兄弟节点（mobile / postMore / 各控制器方法）
    // 全都拿到深度 0，反而浮到真正链路的前面。
    //
    // 注：试过更细的「主链路 = 入口 ∪ 种子祖先 ∪ 种子后代」三档方案，实测
    // **无法**把同类兄弟方法（mobile）降下去（它在图里确实与种子有可达关系），
    // 却会把页面挤到链路之后，属于净损失，故不采用。
    let on_chain = anchored_components(hits, &adj);

    let mut memo: HashMap<i64, i64> = HashMap::new();
    let mut visiting: HashSet<i64> = HashSet::new();
    let mut depths: HashMap<i64, i64> = HashMap::new();
    let mut fan_in: HashMap<i64, usize> = HashMap::new();
    for h in hits.iter() {
        let id = h.node_id.get();
        depths.insert(id, flow_depth(id, &callers, &mut memo, &mut visiting));
        fan_in.insert(
            id,
            incoming
                .get(&id)
                .map(|es| es.iter().filter(|e| is_flow_edge(e.kind.as_str())).count())
                .unwrap_or(0),
        );
    }

    hits.sort_by(|a, b| {
        let ia = a.node_id.get();
        let ib = b.node_id.get();
        // 1) 在链路上的排前面
        let ca = on_chain.contains(&ia);
        let cb = on_chain.contains(&ib);
        cb.cmp(&ca)
            // 2) 链路内按拓扑深度（入口 → 落库）
            .then_with(|| depths[&ia].cmp(&depths[&ib]))
            // 3) 同档按「枢纽衰减后的得分」
            .then_with(|| {
                let sa = a.score * hub_penalty(fan_in[&ia]);
                let sb = b.score * hub_penalty(fan_in[&ib]);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            })
            // 4) 稳定兜底（同分时按 id，保证可复现）
            .then_with(|| ia.cmp(&ib))
    });
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
        // 「阈值 / 参数 / 开关 / 上限 …」在任意系统里都是**一个可配置的值**，
        // 其落点是配置项（ConfigKey）而非某段代码 —— 复用上面同一套提示机制，
        // 让「怎么修改订单自动取消时间」收敛到 `order_cancel_time` 这类配置键，
        // 而不是被 1.5× 加权的方法（save / update …）淹没。
    ];
    // 注意：**不要**往 hint_map 里加「阈值 / 下限 / 时长 …」这类词。
    // `strip_hint_chars` 会把提示词里的**每个字**从所有查询词里剔除 ——
    // 加入"下限"会让 `下单` 被削成 `单`（"如何修改下单优惠" → "如何修改单优惠"）。
    // 「要找配置项」的判定改由 [`wants_config_value`] 独立承担，不碰 hint_map。

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
    // 已知中文词（内置别名表里的词条）：用于过滤 CJK bigram 里跨越词边界的
    // 无意义组合（何修 / 改下 / 单优 …）。仅当整段命中了已知词时才启用过滤 ——
    // 否则（OOV 中文）仍退化为全 bigram 兜底，保留对未知词的覆盖。
    let known_alias_words: HashSet<&str> =
        INTENT_ALIASES.iter().map(|(zh, _)| *zh).collect();

    for run in cjk_runs(query) {
        // 整段保留一份：精确长词命中时得分最高。
        let keep = strip_hint_words(&run, hint_map);
        if keep.chars().count() >= 2 && !terms.iter().any(|t| t == &keep) {
            terms.push(keep);
        }
        // 二字组。整段命中已知中文词时，仅保留「已知词」本身的 bigram，
        // 丢掉跨越词边界的噪音组合（何修 / 改下 / 单优）；
        // 整段无已知词时（OOV）保持原行为：全部 bigram 兜底。
        let has_known = INTENT_ALIASES.iter().any(|(zh, _)| run.contains(*zh));
        let chars: Vec<char> = run.chars().collect();
        for w in chars.windows(2) {
            let gram: String = w.iter().collect();
            let gk = strip_hint_words(&gram, hint_map);
            if gk.chars().count() >= 2
                && (!has_known || known_alias_words.contains(gk.as_str()))
                && !terms.iter().any(|t| t == &gk)
            {
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

/// 去掉查询里出现的**完整结构提示词**（"表" / "接口" / "缓存" …）。
///
/// "表" / "接口" / "缓存" 这类词已经转成节点种类加成，再拿去当文本匹配词
/// 只会把召回结果带偏（"表" 会命中所有名字含"表"的节点）。
///
/// **必须整词删除，绝不能逐字删除**：旧实现把提示词拆成单字后，从整段里过滤掉
/// 所有"出现过这些字"的字符 —— 副作用极大：
/// * 「缓存」的 `存` + 「数据库」的 `库` → `库存` 被削成空；
/// * 「消息」的 `消` + 「定时」的 `时` → `取消时间` 被削成 `取`。
/// 实测「商品库存预警阈值」因此丢掉"库存"、「修改订单自动取消时间」丢掉"取消时间"，
/// 直接导致这两个意图各自的正解（`product_stock_job` / `order_cancel_time`）召不回来。
fn strip_hint_words(s: &str, hint_map: &[(&str, &str)]) -> String {
    let mut out = s.to_string();
    for (word, _) in hint_map {
        if out.contains(word) {
            out = out.replace(word, "");
        }
    }
    out
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
    // 质量告警块（质量 High 时为空串）。放在**最前面**：调用方必须先看到它，
    // 否则会照常信任后面的列表 —— 那正是"静默失败"。
    advisory: &str,
) -> String {
    let mut s = String::new();
    s.push_str(&format!("# 召回上下文：{}\n\n", q.query));
    if !advisory.is_empty() {
        s.push_str(advisory);
    }
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
        // 仅排名前 SNIPPET_TOP 的命中附带源码片段，控制默认输出体积（见 [`SNIPPET_TOP`]）。
        if i < SNIPPET_TOP {
            if let Some(sn) = &h.snippet {
                s.push_str(&format!("\n```\n{sn}\n```\n"));
            }
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

    // ---- 短语聚合加成（修复 3：组合意图优先于孤立词）----

    #[test]
    fn cohesion_boosts_multi_intent() {
        let g = alias_group_map();
        // 只命中一个操作类别（"查询" 的英文 find）：无加成
        let one = cohesion_multiplier(&["find".to_string()], &g);
        assert!((one - 1.0).abs() < 1e-9, "单类别不应加成：{one}");
        // 命中两个操作类别（查询 + 删除）：×1.3
        let two = cohesion_multiplier(&["find".to_string(), "delete".to_string()], &g);
        assert!((two - 1.3).abs() < 1e-9, "双类别应 ×1.3：{two}");
        // 中文意图词同样归类（用跨领域的通用动词，不依赖任何领域词表）
        let zh = cohesion_multiplier(&["查询".to_string(), "删除".to_string()], &g);
        assert!((zh - 1.3).abs() < 1e-9, "中文双类别也应 ×1.3：{zh}");
    }

    // ---- 种子并集（修复 1+2：跨语言节点必须进种子，且合并分为相加）----

    #[test]
    fn select_seeds_unions_vector_seeds() {
        // 模拟"修改订单优惠"查询：词面分 Order 类碾压 Coupon 类，
        // 但向量分（bge 余弦）Coupon 远高 —— 旧逻辑只按总分取前 5，Coupon 永远落选。
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
        assert!(ids.contains(&2), "Coupon 节点必须作为向量种子入选：{ids:?}");
        // 合并分 = 词面 + 向量（相加而非取大）
        let coupon = seeds.iter().find(|(_, _, n)| n.id.get() == 2).unwrap();
        assert!(
            (coupon.0 - 165.0).abs() < 1e-9,
            "合并分应为 55+110=165，实际 {}",
            coupon.0
        );
    }

    // ---- score_node ----

    #[test]
    fn score_node_exact_name_match() {
        // 100（精确）× 1.4（Table 权重）
        let n = tnode(101, "Table", "user", None, None);
        let (score, matched) = score_node(&n, &["user".to_string()], &[], &HashMap::new(), false);
        assert!((score - 140.0).abs() < 1e-9, "精确匹配应得 100×1.4=140，实际 {score}");
        assert_eq!(matched, vec!["user".to_string()]);
    }

    #[test]
    fn score_node_starts_with_prefix() {
        // 70（前缀）× 1.4
        let n = tnode(102, "Table", "user_order", None, None);
        let (score, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new(), false);
        assert!((score - 98.0).abs() < 1e-9, "前缀匹配应得 70×1.4=98，实际 {score}");
    }

    #[test]
    fn score_node_contains() {
        // 50（包含）× 1.0（Method）
        let n = tnode(103, "Method", "my_user_x", None, None);
        let (score, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new(), false);
        assert!((score - 50.0).abs() < 1e-9, "包含匹配应得 50×1.0=50，实际 {score}");
    }

    #[test]
    fn score_node_identity_match() {
        // 名字不匹配，identity 包含 → 45 × 1.0（Method）
        let n = tnode(104, "Method", "zzz", None, Some("user_identity"));
        let (score, matched) = score_node(&n, &["user".to_string()], &[], &HashMap::new(), false);
        assert!((score - 45.0).abs() < 1e-9, "identity 命中应得 45×1.0=45，实际 {score}");
        assert_eq!(matched, vec!["user".to_string()]);
    }

    #[test]
    fn score_node_fqn_match() {
        // fqn 包含 → 35 × 1.0
        let n = tnode(105, "Method", "zzz", Some("app\\model\\user"), None);
        let (score, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new(), false);
        assert!((score - 35.0).abs() < 1e-9, "fqn 命中应得 35×1.0=35，实际 {score}");
    }

    #[test]
    fn score_node_multi_term_multiplier() {
        // 命中两词：先累加各词得分，再整体 ×1.5。这里两个词各得 50（Method 权重 1.0），
        // 所以 two == (one + order_only) × 1.5 == 150（大于简单相加的 100）。
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
            "双词应 = (单 + 单) × 1.5：one={one} order_only={order_only} two={two}"
        );
        assert!(two > one + order_only, "多词应严格高于简单相加");
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
        assert!(s_t > s_m, "语义节点（表）应优先于方法");
    }

    #[test]
    fn score_node_kind_hint_bonus() {
        // 提示词说了"表" → 结构提示加成 +30
        let n = tnode(109, "Table", "user", None, None);
        let (no_hint, _) = score_node(&n, &["user".to_string()], &[], &HashMap::new(), false);
        let (with_hint, _) =
            score_node(&n, &["user".to_string()], &["Table".to_string()], &HashMap::new(), false);
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
        let (score, _) = score_node(&n, &["user".to_string()], &[], &inc, false);
        assert!(
            (score - (100.0 + 2.0 * 0.4)).abs() < 1e-9,
            "扇入 2 应 +0.8，实际 {score}"
        );
    }

    #[test]
    fn rank_weight_reverses_preference_under_action_intent() {
        // 非动作意图：HTTP 路由（1.4）优先于方法（1.0）。
        assert!(rank_weight("HttpContract", false) > rank_weight("Method", false));
        // 动作意图（找实现代码）：方法（1.5）反超路由（0.7）。
        assert!(rank_weight("Method", true) > rank_weight("HttpContract", true));
        // 非动作时与默认种类权重一致（零回归）。
        assert!((rank_weight("Table", false) - 1.4).abs() < 1e-9);
    }

    #[test]
    fn action_intent_detects_code_seeking_verbs() {
        assert!(action_intent("修改订单优惠")); // 修改
        assert!(action_intent("支付回调通知商户")); // 回调 / 支付 / 通知
        assert!(action_intent("用户余额不足时拦截下单")); // 拦截
        assert!(action_intent("商品库存扣减失败回滚")); // 扣减 / 回滚
        // 纯主题查询不应触发动作意图。
        assert!(!action_intent("优惠券列表页面"));
    }

    #[test]
    fn method_beats_route_under_action_intent() {
        // 同一词面命中下，动作意图让服务方法压过字面同词的 HTTP 路由。
        let route = tnode(201, "HttpContract", "coupon_edit", None, None);
        let method = tnode(202, "Method", "storeCoupon", None, None);
        let (s_route, _) = score_node(&route, &["coupon".to_string()], &[], &HashMap::new(), true);
        let (s_method, _) = score_node(&method, &["coupon".to_string()], &[], &HashMap::new(), true);
        assert!(s_method > s_route, "动作意图下方法应压过路由：{s_method} vs {s_route}");
    }

    #[test]
    fn score_node_generic_crud_method_skips_action_boost() {
        // 纯动词方法（edit/save/update…）在动作意图下、且没命中内容词时，不享受 1.5× 加权，
        // 退回默认 1.0，否则「修改」类查询会把无关域 CRUD（发货 save / 退款 update）顶到顶部。
        let generic = tnode(301, "Method", "save", None, None);
        let (s_gen, _) = score_node(&generic, &["save".to_string()], &[], &HashMap::new(), true);
        // 100（精确匹配）× 0.5（纯动词无内容词折扣）× 1.0（退回默认权重）= 50
        assert!((s_gen - 50.0).abs() < 1e-9, "纯 CRUD 方法无内容词应打折并退回 1.0 权重，实际 {s_gen}");

        // 同一纯动词方法若类标识符命中内容词（如优惠券的 edit，类 StoreCouponIssue 含 Coupon），
        // 应保留 1.5× 加权。注意：目录路径里的 order 不应算内容词 —— 即便 fqn 路径含 order
        // （如 v1/order/DeliveryService），只要类标识符不含内容词仍应被惩罚。
        let coupon_edit = tnode(
            302,
            "Method",
            "edit",
            Some("app\\adminapi\\controller\\v1\\marketing\\StoreCouponIssue::edit"),
            None,
        );
        // 名字 edit（100）+ 类标识符含 coupon（35）→ 135；两词 ×1.5 聚合；内容词 → 1.5× 加权生效：
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
            "命中内容词的 CRUD 方法应保留 1.5 加权，实际 {s_ce}"
        );

        // 反例：DeliveryService 虽在 order/ 目录下（fqn 路径含 order），但类标识符不含任何内容词，
        // 应被惩罚（退回 1.0）。
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
        // 名字 save（100）+ 路径 order 仅作计分（+35）→ 135；×1.5 聚合；但无内容词 → 不加权 1.0：
        // 135 × 1.5 = 202.5。
        assert!(
            (s_ds - 101.25).abs() < 1e-9,
            "类标识符无内容词的 CRUD 方法应退回 1.0 权重，实际 {s_ds}"
        );

        // 复合业务名（createForm）仍享受动作加权 1.5×，不受波及。
        let biz = tnode(304, "Method", "createForm", None, None);
        let (s_biz, _) = score_node(&biz, &["form".to_string()], &[], &HashMap::new(), true);
        assert!((s_biz - 75.0).abs() < 1e-9, "复合业务方法应保留 1.5 加权，实际 {s_biz}");
    }

    #[test]
    fn has_content_word_matches_tokens_not_substrings() {
        // 查询「怎么新增一种优惠券类型」展开出的词（新增 → add/create/insert/new，优惠 → coupon/discount）。
        let terms: Vec<String> = ["新增", "优惠", "add", "create", "insert", "new", "coupon", "discount"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // 纯动词方法 `create`，类标识符 token 为 [user, address, services]：
        // add / create / insert / new 全是通用 CRUD 动词，本来就不具备「内容词」资格；
        // 其余词（新增 / 优惠 / coupon / discount）都不匹配任何 token → 无内容词。
        let address_create = tnode(
            305,
            "Method",
            "create",
            Some("app\\services\\user\\UserAddressServices::create"),
            None,
        );
        assert!(
            !has_content_word(&address_create, &terms),
            "通用动词不算内容词，且类 token 不含查询主题"
        );
        // 名字 create（100）+ fqn 含 add（35）→ 135；两词 ×1.5 聚合；无内容词 → 加权退回 1.0：
        // 135 × 1.5 = 202.5（而非 303.75）。
        let (s_addr, _) = score_node(&address_create, &terms, &[], &HashMap::new(), true);
        assert!(
            (s_addr - 101.25).abs() < 1e-9,
            "无关域 CRUD 不应保留 1.5 加权，实际 {s_addr}"
        );

        // 对照组：同类纯动词 `create`，但类标识符的 token 里真的有 coupon
        // （token 前缀命中，不是词中子串），应命中内容词、保留 1.5× 加权。
        let coupon_create = tnode(
            306,
            "Method",
            "create",
            Some("app\\services\\coupon\\CouponServices::create"),
            None,
        );
        assert!(
            has_content_word(&coupon_create, &terms),
            "类标识符 token 含 coupon 应算命中内容词"
        );
        let (s_coupon, _) = score_node(&coupon_create, &terms, &[], &HashMap::new(), true);
        assert!(
            (s_coupon - 303.75).abs() < 1e-9,
            "命中内容词的 CRUD 方法应保留 1.5 加权，实际 {s_coupon}"
        );
        assert!(s_coupon > s_addr, "优惠券的 create 应高于无关域的 create");

        // 直接验证标识符切分。
        assert_eq!(
            split_ident_tokens("UserAddressServices"),
            vec!["user", "address", "services"]
        );
        assert_eq!(split_ident_tokens("HTTPResponse"), vec!["http", "response"]);
        assert_eq!(split_ident_tokens("store_order"), vec!["store", "order"]);
    }

    #[test]
    fn has_content_word_rejects_midword_but_keeps_compound() {
        // 词中片段不应算命中：`RecorderService` 的 token 是 [recorder, service]，
        // `order` 只是 `recorder` 的**词中子串**（旧实现拼成 recorderservice 后
        // contains("order") 会误判），按 token 前缀匹配后不再命中。
        let recorder_save = tnode(
            307,
            "Method",
            "save",
            Some("app\\services\\RecorderService::save"),
            None,
        );
        assert!(
            !has_content_word(&recorder_save, &["save".to_string(), "order".to_string()]),
            "`order` 嵌在 `recorder` 词中，不应算命中内容词"
        );
        // save（100）+ fqn 含 order（35）→ 135；两词 ×1.5；无内容词 → 1.0：135 × 1.5 = 202.5。
        let (s_rec, _) = score_node(
            &recorder_save,
            &["save".to_string(), "order".to_string()],
            &[],
            &HashMap::new(),
            true,
        );
        assert!(
            (s_rec - 101.25).abs() < 1e-9,
            "词中子串不应保住 1.5 加权，实际 {s_rec}"
        );

        // 反向保护：合理的「复合 / 屈折」匹配必须保留 —— `pay` → `payment` 属前缀命中，
        // 若退化为严格相等，支付域的 save 会被误降级（这是本次改动的主要回归风险）。
        let payment_save = tnode(
            308,
            "Method",
            "save",
            Some("app\\services\\PaymentService::save"),
            None,
        );
        assert!(
            has_content_word(&payment_save, &["save".to_string(), "pay".to_string()]),
            "`pay` 应前缀命中 `payment`，否则会误伤支付域"
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
            "复合匹配应保留 1.5 加权，实际 {s_pay}"
        );
    }

    // ---- 流程拓扑重排 ----

    /// `direct` = 是否为种子本身（hop 0）；false 表示 BFS 带出的邻居。
    /// 只有含种子的连通分量才算链路（见 [`anchored_components`]）。
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
        assert!(flow_intent("注册流程"), "「注册流程」应识别为流程查询");
        assert!(flow_intent("支付的调用链"));
        assert!(flow_intent("how does the payment flow work"));
        // 非流程查询必须为 false —— 这是「其余查询排序零回归」的前提。
        assert!(!flow_intent("如何修改下单优惠"));
        assert!(!flow_intent("怎么新增一种优惠券类型"));
    }

    #[test]
    fn reorder_for_flow_orders_entry_to_sink_and_sinks_unrelated() {
        // 一条完整链路：页面(1) → 路由(2) → 控制器(3) → 服务(4) → Dao(5) → 表(6)
        let inc = incoming(&[
            ("CallsHttp", 1, 2),
            ("HandledBy", 2, 3),
            ("Calls", 3, 4),
            ("Calls", 4, 5),
            ("WritesDb", 5, 6),
        ]);
        // 故意打乱顺序、且分数与链路顺序无关。
        let mut hits = vec![
            fhit(6, "user_table", "Table", 100.0, false),
            fhit(3, "register", "Method", 300.0, false),
            fhit(1, "login_page", "File", 50.0, false),
            fhit(5, "save", "Method", 120.0, false),
            // 只有服务层是种子：整条链路因它而被判定为「本次查询的链路」。
            fhit(4, "LoginServices.register", "Method", 250.0, true),
            // 路由是**入口层**：即便被前端 CallsHttp 指着，也应排在服务方法之前。
            fhit(2, "POST /register", "HttpContract", 80.0, false),
            // BFS 带出的无关邻居：不在含种子的连通分量里，应沉底。
            fhit(7, "unrelated_validate", "Method", 200.0, false),
        ];
        reorder_for_flow(&mut hits, &inc);
        let order: Vec<i64> = hits.iter().map(|h| h.node_id.get()).collect();
        // 入口（路由 / 页面）→ 控制器 → 服务 → Dao → 表，与分数无关。
        assert_eq!(
            order[..6],
            [2, 1, 3, 4, 5, 6],
            "流程查询应沿「入口 → 落库」排序，实际 {order:?}"
        );
        assert_eq!(order[6], 7, "与链路无关的邻居应沉底，实际 {order:?}");
        // 关键回归点：路由必须早于服务方法。
        let pos = |id: i64| order.iter().position(|x| *x == id).unwrap();
        assert!(
            pos(2) < pos(4),
            "路由（入口）应排在服务方法之前：路由 {:?} vs 服务 {:?}",
            pos(2),
            pos(4)
        );
    }

    #[test]
    fn hub_penalty_suppresses_high_fan_in_only() {
        assert_eq!(hub_penalty(10), 1.0, "低扇入不应被惩罚");
        assert_eq!(hub_penalty(60), 1.0, "阈值内不应被惩罚");
        let mid = hub_penalty(123); // 如 BaseDao::save
        let hub = hub_penalty(452); // 如 Cache
        assert!(mid < 1.0 && mid > 0.4, "中等枢纽应被部分压低，实际 {mid}");
        assert!(hub < mid, "扇入越大衰减越多：{hub} vs {mid}");
        assert!(hub >= 0.4, "衰减应有下限，实际 {hub}");
    }

    // ---- 事件驱动召回 ----

    #[test]
    fn event_intent_fires_only_on_sequence_or_explicit_event() {
        // 时序 / 显式事件信号才触发，普通动作查询不触发（避免误混入监听器种子）。
        assert!(event_intent("下单后怎么发通知给用户"), "应包含时序词『后怎么』");
        assert!(event_intent("退款成功后怎么回退优惠券"), "应包含『成功后』");
        assert!(event_intent("订单创建之后做哪些事"), "应包含『之后』");
        assert!(event_intent("支付回调通知商户"), "『回调』是事件语义");
        assert!(event_intent("用户注册事件如何处理"), "『事件』是显式事件信号");
        assert!(event_intent("order paid after event listener"), "英文 after / listener");

        // 普通动作查询不应被误判。
        assert!(!event_intent("如何修改下单优惠"), "纯动作查询不应触发");
        assert!(!event_intent("商品库存预警阈值是多少"), "配置查询不应触发");
        assert!(!event_intent("注册流程是怎样的"), "流程意图与事件意图独立（flow 另判）");
        assert!(!event_intent("怎么发送通知"), "仅有『通知』无时序/事件信号不应触发");
    }

    #[test]
    fn is_event_handler_detects_listener_by_convention() {
        // 类名以 Listener / Subscriber / Observer 结尾。
        assert!(is_event_handler(&tnode(
            1, "Class", "OrderCreateAfterListener",
            Some("app\\listener\\order\\OrderCreateAfterListener"), None
        )));
        assert!(is_event_handler(&tnode(
            2, "Class", "UserRegisteredSubscriber",
            Some("app\\subscriber\\UserRegisteredSubscriber"), None
        )));

        // 方法名 handle / onX 且位于 listener / event 命名空间。
        assert!(is_event_handler(&tnode(
            3, "Method", "handle",
            Some("app\\listener\\order\\OrderCreateAfterListener::handle"), None
        )));
        assert!(is_event_handler(&tnode(
            4, "Method", "onOrderPaid",
            Some("app\\events\\OrderPaidListener::onOrderPaid"), None
        )));

        // 普通业务方法 / 服务类不应被误判。
        assert!(!is_event_handler(&tnode(
            5, "Method", "create", Some("app\\services\\UserServices::create"), None
        )));
        assert!(!is_event_handler(&tnode(
            6, "Class", "StoreOrderRefundServices",
            Some("app\\services\\order\\StoreOrderRefundServices"), None
        )));
        // `on` + 小写（如 online）不是事件处理方法。
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
            // Event 节点：名字命中查询词才作种子
            tnode(4, "Event", "OrderPaidEvent", Some("OrderPaidEvent"), None),
            // Event 节点：名字未命中查询词 → 排除
            tnode(5, "Event", "UserLoggedInEvent", Some("UserLoggedInEvent"), None),
            // 已存在的种子 → 排除
            tnode(9, "Method", "refund", Some("app\\services\\order\\refund"), None),
        ];
        let index: HashMap<i64, &Node> =
            nodes.iter().map(|n| (n.id.get(), n)).collect();
        let existing: HashSet<i64> = HashSet::from([9i64]);
        let seeds = collect_event_seeds(
            &nodes, &index, &existing, &["refund".to_string(), "paid".to_string(), "order".to_string()], 400.0,
        );
        let ids: Vec<i64> = seeds.iter().map(|(_, _, n)| n.id.get()).collect();
        assert!(ids.contains(&1), "监听器类应作种子：{ids:?}");
        assert!(ids.contains(&2), "监听器 handle 方法应作种子：{ids:?}");
        assert!(ids.contains(&4), "名字命中查询词的 Event 节点应作种子：{ids:?}");
        assert!(!ids.contains(&3), "普通 Services 类不应作种子：{ids:?}");
        assert!(!ids.contains(&5), "名字未命中查询词的 Event 节点应排除：{ids:?}");
        assert!(!ids.contains(&9), "已存在的种子不应重复：{ids:?}");
    }

    // ---- 多意图拆分 / 合并 ----

    #[test]
    fn split_intents_splits_compound_questions_only() {
        // 顿号连接的两个独立问题 → 拆成两段（合起来会互相干扰，实测双双丢答案）。
        let parts = RecallService::split_intents("怎么修改商品库存预警阈值、修改订单自动取消时间");
        assert_eq!(parts.len(), 2, "应拆成 2 个意图：{parts:?}");
        assert!(parts[0].contains("库存预警阈值"), "{parts:?}");
        assert!(parts[1].contains("自动取消时间"), "{parts:?}");

        // 逗号 / 分号 / 连接词同样生效。
        assert_eq!(RecallService::split_intents("查询订单相关的表，查询商品相关的表").len(), 2);
        assert_eq!(RecallService::split_intents("查询订单相关的表；查询商品相关的表").len(), 2);
        assert_eq!(RecallService::split_intents("查询订单相关的表以及查询商品相关的表").len(), 2);

        // 单意图必须返回空 → 走原路径，零回归。
        assert!(RecallService::split_intents("如何修改下单优惠").is_empty());
        assert!(RecallService::split_intents("注册流程").is_empty());
        // 过短片段不单独成意图（避免语气词 / 标点旁支被当成独立问题）。
        assert!(RecallService::split_intents("订单、商品").is_empty());
    }

    #[test]
    fn merge_intent_hits_round_robins_and_dedups() {
        // 意图 A 分数全面高于 B：若按分数合并，B 会被挤空；轮询保证每个意图都有代表。
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
        assert_eq!(ids, vec![1, 4, 2, 5], "应按意图轮询而非按分数，实际 {ids:?}");

        // 跨意图重复节点只保留一次。
        let dup = RecallService::merge_intent_hits(
            vec![
                vec![fhit(1, "x", "Method", 900.0, true)],
                vec![fhit(1, "x", "Method", 900.0, true)],
            ],
            4,
        );
        assert_eq!(dup.len(), 1, "跨意图重复节点应去重，实际 {}", dup.len());
    }

    // ---- 配置项意图 / 提示词整词删除 ----

    #[test]
    fn has_maps_to_flags_orm_relation_accessors_only() {
        // 关联访问器：带 MapsTo 出边（映射到另一个实体）。
        assert!(has_maps_to(1, &outgoing(&[("MapsTo", 1, 2)])));
        // 业务方法不带：实测 saveInvoiceInfo / updateCartInfo / notifyConfirm 均无 MapsTo。
        assert!(!has_maps_to(1, &outgoing(&[("Calls", 1, 2)])));
        assert!(!has_maps_to(1, &outgoing(&[("WritesDb", 1, 2)])));
        assert!(!has_maps_to(1, &HashMap::new()));
    }

    #[test]
    fn wants_config_value_fires_only_on_config_seeking_queries() {
        assert!(wants_config_value("怎么修改订单自动取消时间"));
        assert!(wants_config_value("商品库存预警阈值"));
        assert!(wants_config_value("修改缓存配置"));
        // 找实现代码的查询绝不能误判 —— 否则会关掉动作意图的方法加权。
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
        // 出现的提示词整词移除。
        assert_eq!(strip_hint_words("订单表", hints), "订单");
        assert_eq!(strip_hint_words("查询缓存", hints), "查询");

        // 关键回归点：旧实现逐字删除，「缓存」的 存 + 「数据库」的 库 会把 `库存` 削空，
        // 「消息」的 消 + 「定时」的 时 会把 `取消时间` 削成 `取`。
        assert_eq!(strip_hint_words("商品库存预警阈值", hints), "商品库存预警阈值");
        assert_eq!(strip_hint_words("修改订单自动取消时间", hints), "修改订单自动取消时间");
    }

    // ---- 召回质量评估 ----

    /// 构造带"命中词"的合成命中，用于质量评估断言（不依赖任何真实工程数据）。
    fn qhit(id: i64, score: f64, matched: &[&str]) -> RecallHit {
        let mut h = fhit(id, "node", "Method", score, true);
        h.matched_terms = matched.iter().map(|s| s.to_string()).collect();
        h
    }

    #[test]
    fn assess_quality_high_when_all_concepts_covered() {
        // 「如何修改下单优惠」三个概念（修改 / 下单 / 优惠）都命中，且头部区分度健康。
        let hits = vec![
            qhit(1, 500.0, &["order", "edit", "coupon"]),
            qhit(2, 100.0, &["order"]),
            qhit(3, 90.0, &["order"]),
        ];
        let (q, conf, _reason, missing) =
            RecallService::assess_quality("如何修改下单优惠", &hits, &HashSet::new());
        assert_eq!(q, RecallQuality::High, "概念全覆盖应为 High，conf={conf}");
        assert!(missing.is_empty(), "不应有未命中概念：{missing:?}");
    }

    #[test]
    fn assess_quality_medium_when_partially_covered() {
        // 只覆盖「下单 / 优惠」，缺「修改」。
        let hits = vec![qhit(1, 500.0, &["order", "coupon"]), qhit(2, 100.0, &["order"])];
        let (q, _conf, _reason, missing) =
            RecallService::assess_quality("如何修改下单优惠", &hits, &HashSet::new());
        assert_eq!(q, RecallQuality::Medium, "部分覆盖应为 Medium");
        assert!(missing.iter().any(|m| m == "修改"), "应报告缺失概念「修改」：{missing:?}");
    }

    #[test]
    fn assess_quality_low_when_most_concepts_missing() {
        // 只覆盖「下单」→ 覆盖率 1/3 < 0.5。
        let hits = vec![qhit(1, 500.0, &["order"]), qhit(2, 100.0, &["order"])];
        let (q, _conf, _reason, missing) =
            RecallService::assess_quality("如何修改下单优惠", &hits, &HashSet::new());
        assert_eq!(q, RecallQuality::Low, "多数概念未命中应为 Low");
        assert!(
            missing.iter().any(|m| m == "修改") && missing.iter().any(|m| m == "优惠"),
            "缺失概念应含「修改」「优惠」：{missing:?}"
        );
    }

    #[test]
    fn assess_quality_ignores_boilerplate_hits() {
        // 「优惠」只被 ORM 关联样板命中 → 不算覆盖（样板不含业务信息）。
        let mut boilerplate = HashSet::new();
        boilerplate.insert(9);
        let hits = vec![qhit(1, 500.0, &["order"]), qhit(9, 480.0, &["coupon"])];
        let (q, _conf, _reason, missing) =
            RecallService::assess_quality("如何修改下单优惠", &hits, &boilerplate);
        assert!(
            missing.iter().any(|m| m == "优惠"),
            "样板命中不应算覆盖「优惠」：{missing:?}"
        );
        assert_eq!(q, RecallQuality::Low, "排除样板后覆盖率过低应为 Low");
    }

    #[test]
    fn assess_quality_missing_terms_include_english_expansions() {
        // 只给中文字面是不够的：代码里的标识符是英文，AI IDE 拿「回调」grep 不到任何东西。
        // 因此未命中概念必须同时输出它的英文展开，才算真正可用的退路。
        let hits = vec![qhit(1, 500.0, &["pay"])];
        let (_q, _conf, reason, missing) =
            RecallService::assess_quality("支付回调失败怎么排查", &hits, &HashSet::new());
        assert!(missing.iter().any(|m| m == "回调"), "应含中文概念：{missing:?}");
        assert!(
            missing.iter().any(|m| m == "callback"),
            "应含英文展开 callback，否则 grep 不到代码：{missing:?}"
        );
        assert!(missing.iter().any(|m| m == "fail"), "应含英文展开 fail：{missing:?}");
        // 说明文案只用中文概念，避免过长。
        assert!(!reason.contains("callback"), "说明文案不应塞英文展开：{reason}");
    }

    #[test]
    fn assess_quality_without_concepts_cannot_be_high() {
        // 查询不含任何已知意图概念（领域专有词 / 生僻说法）时，可算概念为空、
        // 覆盖率取默认 1.0 —— 若不拦住就会出现"越生僻越说没问题"。
        // 这里用纯 ASCII 无意义串保证必然 OOV（等价于任何生僻领域查询）。
        let hits = vec![qhit(1, 500.0, &["express"]), qhit(2, 100.0, &["delivery"])];
        let (q, _conf, reason, _missing) =
            RecallService::assess_quality("zzzqqx", &hits, &HashSet::new());
        assert_ne!(q, RecallQuality::High, "无可评估概念时不得判 High");
        assert!(reason.contains("无法确认"), "应说明无法确认质量：{reason}");
    }

    #[test]
    fn recall_quality_serializes_lowercase() {
        // 必须序列化成小写：UI 按 'high'/'medium'/'low' 判断档位配色，
        // MCP 的 with_quality_guidance 也按小写匹配后追加"下一步该做什么"。
        // 默认 derive 会输出 "Medium"（大写）→ 两处同时失灵，且 UI 会**静默显示"高"**。
        assert_eq!(serde_json::to_string(&RecallQuality::High).unwrap(), "\"high\"");
        assert_eq!(serde_json::to_string(&RecallQuality::Medium).unwrap(), "\"medium\"");
        assert_eq!(serde_json::to_string(&RecallQuality::Low).unwrap(), "\"low\"");
    }

    #[test]
    fn parse_query_drops_boundary_bigrams() {
        // 「如何修改下单优惠」含已知中文词（修改 / 下单 / 优惠），应保留这些词，
        // 但过滤掉跨越词边界的噪音 bigram（何修 / 改下 / 单优）。
        let (terms, _hints) = parse_query("如何修改下单优惠");
        assert!(terms.iter().any(|t| t == "修改"), "修改 应保留：{terms:?}");
        assert!(terms.iter().any(|t| t == "下单"), "下单 应保留：{terms:?}");
        assert!(terms.iter().any(|t| t == "优惠"), "优惠 应保留：{terms:?}");
        assert!(!terms.iter().any(|t| t == "何修"), "何修 应为噪音被过滤：{terms:?}");
        assert!(!terms.iter().any(|t| t == "改下"), "改下 应为噪音被过滤：{terms:?}");
        assert!(!terms.iter().any(|t| t == "单优"), "单优 应为噪音被过滤：{terms:?}");
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

    // ---- 真实模型（bge-m3 / candle）语义验证：仅 `model-candle` feature 下编译 ----
    #[cfg(feature = "model-candle")]
    #[test]
    fn bge_semantic_recall_chinese_to_english() {
        use crate::embed_model::CandleBgeEmbedder;

        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models");
        let model_dir = std::env::var("GT_BGE_MODEL")
            .unwrap_or_else(|_| root.join("bge-m3-safetensors").to_string_lossy().into());
        if !Path::new(&model_dir).join("model.safetensors").exists() {
            eprintln!("skip bge_semantic_recall: 未找到 {model_dir}/model.safetensors（先跑 tools/convert_bge_safetensors.py）");
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
