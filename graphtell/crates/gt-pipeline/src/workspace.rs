//! 流水线内存工作区。
//!
//! 各阶段在内存中建图、查询、打标；阶段结束后由应用层把累积的
//! [`GraphDelta`] 一次性落库（保证阶段级原子性，又让领域逻辑不依赖事务 API）。

use std::collections::{BTreeMap, HashMap, HashSet};

use gt_domain::model::{
    AliasEntry, Annotation, Diagnostic, Edge, EdgeKind, FactValue, GraphDelta, IdentityKey,
    Language, MergeStrategy, NewAnnotation, NewEdge, NewNode, Node, NodeId, NodeKind, Phase,
    ProjectId, Severity, Span, SubProjectId, SynthesizedKind,
};
use gt_domain::model::syntax::{HeaderAssignFact, SignCompareFact};
use serde_json::Value;

/// 一次调用点在工作区中的记录。
#[derive(Debug, Clone)]
pub struct CallRecord {
    /// 该调用点自身的 `CallSite` 节点（Taint 标注的精确落点）。
    pub node: NodeId,
    /// 所在方法的节点（用于建语义边，如 `Method --ReadsDb--> Table`）。
    pub owner: NodeId,
    pub owner_fqn: String,
    /// 该调用点所在的**类** FQN（由 parser 从 `CallSiteFact.owner_class` 透传）。
    /// 供 `owner_class` 绑定直接取用；PHP 侧为 `None`（退回字符串切分）。
    pub owner_class: Option<String>,
    pub callee: String,
    pub receiver: Option<String>,
    pub method: Option<String>,
    pub args: Vec<FactValue>,
    /// 链式门面调用里透传下来的目标表名（`Db::name('goods')->insert()`），
    /// 供 P7 把末端动词落成 `WritesDb` / `ReadsDb`。
    pub db_table: Option<String>,
    /// 该调用点是否位于循环体内（parser 事实，与 CallSite 节点的 `in_loop` 属性同源）。
    /// 供 P12/P13 这类"循环 / 批量"判定使用 —— 规则侧读节点属性，阶段侧读这里。
    pub in_loop: bool,
    pub span: Span,
    pub file: String,
    pub sub: Option<SubProjectId>,
    pub language: Language,
}

/// 配置条目记录（来自 `return [...]` 型 PHP 文件）。
#[derive(Debug, Clone)]
pub struct ConfigRecord {
    pub file: String,
    pub key_path: String,
    pub value: FactValue,
    pub span: Span,
    pub sub: Option<SubProjectId>,
    pub locale: Option<String>,
    pub file_stem: Option<String>,
}

/// 继承 / 实现 / trait 记录。
#[derive(Debug, Clone)]
pub struct InheritRecord {
    pub child: NodeId,
    pub child_fqn: String,
    pub base: String,
    pub kind: EdgeKind,
    pub sub: Option<SubProjectId>,
    pub file: String,
    pub span: Span,
}

/// 路由组区间：`Route::group('v2', function () { ... })`。
///
/// ThinkPHP 会把组前缀拼到组内**所有**路由的路径上：
/// `Route::group('v2', fn(){ Route::get('order/x') })` 的真实路径是 `/v2/order/x`。
/// 若只取 `arg:0` 建契约 ID，就会丢掉 `v2`，导致：
/// * 后端契约与真实请求路径不符，无法与前端 `CallsHttp` 汇聚；
/// * 不同版本 / 分组下的同名子路径会被幂等合并成同一个节点。
#[derive(Debug, Clone)]
pub struct RouteGroup {
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub prefix: String,
}

/// 是否参与「短名 → FQN」索引的**类型节点**。
///
/// 只有类 / 接口 / trait / 枚举参与：短名索引的语义本就是
/// 「`StoreOrderServices` → `app\services\order\StoreOrderServices`」。
/// 方法与函数若也进索引，高频全局函数名（`config` / `get` / `app` …）
/// 会撞上同名方法，制造大量错误 Calls 边（详见 `add_node` 注释）。
fn is_type_kind(kind: &str) -> bool {
    matches!(
        kind,
        NodeKind::CLASS | NodeKind::INTERFACE | NodeKind::TRAIT | NodeKind::ENUM
    )
}

/// 尚未解析的动态链接（P5 产出，P7 解析）。
#[derive(Debug, Clone)]
pub struct PendingLink {
    pub from: NodeId,
    pub kind: EdgeKind,
    pub raw: String,
    /// **目标成员名**（可选）：FKB 在 `link.to_method` 里显式给出的入口方法，
    /// 典型如资源路由 `{ expand_entry: true }` 展开出的 `index` / `delete`。
    ///
    /// 为什么必须带到 P7：`handler` 常常只写到类（`Route::resource('level','v1.agent.AgentLevel')`），
    /// 真实入口方法来自**展开变体**（框架知识），P5 链接阶段还不知道该类的 FQN
    /// （要按 FKB 的 `class_templates` 拼出来），于是把 `to_method` 直接传给
    /// `find_target_node` 是查不中的，只能降级成 PendingLink。若不把方法名带上，
    /// P7 拿到的 `raw` 就只剩类名 —— 无论该类有没有对应方法，边都会退化到类级
    /// （实测 CRMEB 有 164 条资源路由因此落到 `Class`，但 `AgentLevel::delete` 明明在图内）。
    pub method: Option<String>,
    pub resolve: gt_domain::model::ResolveAs,
    pub confidence: f32,
    pub sub: Option<SubProjectId>,
    pub file: String,
    pub line: u32,
}

/// 图工作区。
pub struct GraphWorkspace {
    project_id: ProjectId,
    next_node: i64,
    next_edge: i64,
    next_ann: i64,
    nodes: BTreeMap<i64, Node>,
    edges: Vec<Edge>,
    edge_keys: HashSet<(String, i64, i64)>,
    annotations: Vec<Annotation>,
    by_fqn: HashMap<String, i64>,
    by_identity: HashMap<String, i64>,
    /// 契约桥（HttpContract）按 `path` 索引到节点 id，并标记该节点是否为通配方法
    /// （`ANY` / `RULE`）。用于把"不限方法"的自动路由端点与前端具体方法
    /// （`POST` / `GET`…）汇聚到同一个节点，否则路由视角里前后端对不齐。
    contract_path_index: HashMap<String, (i64, bool)>,
    by_alias: BTreeMap<(String, String, String), i64>,
    fan_in: HashMap<i64, u32>,
    fan_out: HashMap<i64, u32>,
    pub calls: Vec<CallRecord>,
    /// **同行调用索引**：`(文件, 起始行) → [(方法名, 首个实参里的字符串)]`。

    /// 链式修饰（`Route::resource('x', C::class)->except(['read'])`）被解析成
    /// **同一行**上的另一个调用点，展开表要按行取到它的实参才能知道哪些动作生效。
    /// 之所以另建索引而不是遍历 `calls`：P4/P5 执行时 `calls` 被
    /// `std::mem::take` 临时移出工作区（避免借用冲突），此刻遍历会拿到空表。
    /// 故在 P2 建调用点时顺手登记（只有带实参的调用才占空间）。
    chained: HashMap<(String, u32), Vec<(String, Vec<String>)>>,
    pub configs: Vec<ConfigRecord>,
    /// CORS 头赋值（由 `cf_ast` 从解析事实灌入），供 `phase::cors` 判定反射源站。
    pub header_assignments: Vec<HeaderAssignFact>,
    /// 签名值的相等性比较（由 `cf_ast` 从解析事实灌入），供 `phase::sign` 判定验签质量。
    pub sign_compares: Vec<SignCompareFact>,
    pub inherits: Vec<InheritRecord>,
    pub pending_links: Vec<PendingLink>,
    /// 路由组区间（`Route::group('v2', ...)`），供契约 ID 补齐组前缀。
    pub route_groups: Vec<RouteGroup>,
    pub symbols: BTreeMap<String, BTreeMap<String, Value>>,
    /// 类属性默认值：`(class_node_id, property_name) → value`。
    prop_values: HashMap<(i64, String), FactValue>,
    /// 文件路径 → File 节点。
    file_nodes: HashMap<String, i64>,
    /// 文件路径 → 源文件 id（`resolve_name_in_file` 按路径查导入表用）。
    file_id_by_path: HashMap<String, i64>,
    /// 源文件 id → 路径（`file_id_by_path` 的反查表）。
    ///
    /// P6 的选择器作用在**图节点**上，节点的 `file_id` 要还原成 `文件:行号`
    /// 才能给解析节点 / 合成节点记上可跳转的出处
    /// （此前返回的是 identity 字符串，前端跳转会拿到一条假路径）。
    source_path_by_id: HashMap<i64, String>,
    /// **每个文件自己的** `use` 导入表：`源文件 id → (短名小写 → FQN)`。
    ///
    /// 为什么必须按文件存：PHP 的短名是按**文件**解析的（`use think\facade\Cache;`
    /// 与 `use app\model\other\Cache;` 在不同文件里含义完全不同）。曾经只有一个
    /// 全局短名索引（`by_short`）与一个先到先得的全局 `imports` 符号表，于是
    /// `Cache::tag()` 被当成 `app\model\other\Cache`（一个 Model），凭空造出
    /// `Model --MapsTo--> Table(cache)` 的类级语义边并污染整条调用链。
    file_imports: HashMap<i64, HashMap<String, String>>,
    /// 出边邻接表：`from → [(kind, to)]`，用于祖先链判定。
    out_edges: HashMap<i64, Vec<(String, i64)>>,
    /// 入边邻接表：`to → [(kind, from)]`。
    ///
    /// 约定类规则要据此判断「这个节点是不是已经有权威来源了」：已显式注册的路由
    /// 指向某个控制器方法时，就不该再为它兜一条按约定推断的契约（显式优先于推断）。
    in_edges: HashMap<i64, Vec<(String, i64)>>,
    /// 短名索引：`短名(小写) → [node_id]`，替代全表线性扫描。
    by_short: HashMap<String, Vec<i64>>,
    /// 表名索引：`(归一化)表名 → Table 节点 id`，供 P7 按 `Db::name('x')` 透传的
    /// 表名反查合成出的 Table 节点（合成节点没有 fqn，不能走 `by_fqn`）。
    by_table_name: HashMap<String, i64>,
    /// 数据库表前缀（来自工程配置，用于 identity 归一化与符号表查找）。
    table_prefixes: Vec<String>,
    /// 父类型名索引：`子 FQN → [父 FQN]`。
    ///
    /// 必须用**名字**而不是节点边：CRMEB 的链路是
    /// `StoreOrder → crmeb\basic\BaseModel → think\Model`，
    /// 而 `think\Model` 在 vendor 里（P0 已排除），图上没有这条边。
    supertypes: HashMap<String, Vec<String>>,
    /// 子类型名索引（继承链下游）：`父 FQN → [子 FQN]`，由 `supertypes` 反转得到。
    ///
    /// 用于「基类方法里的读 / 写动词」反查其实例（子类）映射到的表：yoshop / CRMEB 的
    /// 读库动词（`$this->select` / `getAll`）常写在 `app\common\model\X` 这类基类里，
    /// 而 `MapsTo` 边只挂在具体子类（`app\api\model\X`）上 —— 不反向走到子类，这些动词
    /// 永远落不出 `ReadsDb`，路由只能退回含糊的「映射到」。
    subtypes: HashMap<String, Vec<String>>,
    /// 方法参数类型：`方法 FQN → [(变量名, 类型 FQN)]`，供 P7 解析 `$var->method()`。
    param_types: HashMap<String, Vec<(String, String)>>,
    /// 类属性类型：`类 FQN → {属性名 → 类型 FQN}`（来自构造器注入 `$this->p = $param`、
    /// 类型化属性声明、以及 `$this->p = new Y()` 这类右侧自带类型的赋值）。
    prop_types: HashMap<String, HashMap<String, String>>,
    /// 类用 `@method` 声明的魔法方法名（`类 FQN → {方法名}`，供 `__call` 转发解析）。
    magic_methods: HashMap<String, HashSet<String>>,
    /// 方法内局部变量类型：`方法 FQN → {变量名 → 类型 FQN}`（`$x = new Y()` / `Y::make()`）。
    ///
    /// 服务于 `$x->m()` 这类"临时对象调用"：它们既不是参数类型提示也不是字段，
    /// 没有这一层就整条链断在最常见的一句上。
    local_types: HashMap<String, HashMap<String, String>>,
    /// 子工程事实（app_root 等），键为 sub_project_id。
    pub facts: BTreeMap<i64, BTreeMap<String, Value>>,
    pub diagnostics: Vec<Diagnostic>,
    delta: GraphDelta,
}

impl GraphWorkspace {
    pub fn new(project_id: ProjectId) -> Self {
        Self {
            project_id,
            next_node: 1,
            next_edge: 1,
            next_ann: 1,
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            edge_keys: HashSet::new(),
            annotations: Vec::new(),
            by_fqn: HashMap::new(),
            by_identity: HashMap::new(),
            contract_path_index: HashMap::new(),
            by_alias: BTreeMap::new(),
            fan_in: HashMap::new(),
            fan_out: HashMap::new(),
            calls: Vec::new(),
            chained: HashMap::new(),
            configs: Vec::new(),
            header_assignments: Vec::new(),
            sign_compares: Vec::new(),
            inherits: Vec::new(),
            pending_links: Vec::new(),
            route_groups: Vec::new(),
            symbols: BTreeMap::new(),
            prop_values: HashMap::new(),
            file_nodes: HashMap::new(),
            file_id_by_path: HashMap::new(),
            source_path_by_id: HashMap::new(),
            file_imports: HashMap::new(),
            out_edges: HashMap::new(),
            in_edges: HashMap::new(),
            by_short: HashMap::new(),
            by_table_name: HashMap::new(),
            supertypes: HashMap::new(),
            subtypes: HashMap::new(),
            param_types: HashMap::new(),
            prop_types: HashMap::new(),
            magic_methods: HashMap::new(),
            local_types: HashMap::new(),
            table_prefixes: Vec::new(),
            facts: BTreeMap::new(),
            diagnostics: Vec::new(),
            delta: GraphDelta::new(project_id),
        }
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    /// 登记一个调用点的「链式修饰」信息（P2 建调用点时调用）。
    ///
    /// 只记 (方法名, 首个实参里的字符串)：`->except(['read'])` → `("except", ["read"])`。
    pub fn index_chained(&mut self, file: &str, line: u32, method: Option<&str>, args: &[FactValue]) {
        let Some(m) = method else { return };
        let mut strings: Vec<String> = Vec::new();
        for a in args {
            match a {
                FactValue::String(s) => strings.push(s.clone()),
                FactValue::Array(items) => {
                    for (_, v) in items {
                        if let FactValue::String(s) = v {
                            strings.push(s.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        if strings.is_empty() {
            return;
        }
        self.chained
            .entry((file.to_string(), line))
            .or_default()
            .push((m.to_string(), strings));
    }

    /// 取**同一行**上某链式调用的实参数组（`expanded_actions` 用）。
    pub fn chained_strings(&self, file: &str, line: u32, method: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (m, vals) in self
            .chained
            .get(&(file.to_string(), line))
            .into_iter()
            .flatten()
        {
            if m == method {
                out.extend(vals.iter().cloned());
            }
        }
        out
    }

    /// 记录一个参数的类型：`方法 FQN → (变量名, 类型 FQN)`。
    pub fn add_param_type(&mut self, owner_fqn: &str, var: &str, type_fqn: &str) {
        let entry = self.param_types.entry(owner_fqn.to_string()).or_default();
        if !entry.iter().any(|(v, _)| v == var) {
            entry.push((var.to_string(), type_fqn.to_string()));
        }
    }

    /// 查某方法内某变量的类型（供 `$var->method()` 解析）。
    pub fn param_type(&self, owner_fqn: &str, var: &str) -> Option<&str> {
        self.param_types
            .get(owner_fqn)
            .and_then(|list| list.iter().find(|(v, _)| v == var))
            .map(|(_, t)| t.as_str())
    }

    /// 记录一个类的属性类型（来自构造器注入 `$this->p = $param`）。
    pub fn set_prop_type(&mut self, class_fqn: &str, prop: &str, type_fqn: &str) {
        self.prop_types
            .entry(class_fqn.to_string())
            .or_default()
            .insert(prop.to_string(), type_fqn.to_string());
    }

    /// 查某类属性的类型（供 `$this->prop->method()` 解析）。
    pub fn prop_type(&self, class_fqn: &str, prop: &str) -> Option<&str> {
        self.prop_types
            .get(class_fqn)
            .and_then(|m| m.get(prop))
            .map(|s| s.as_str())
    }

    /// 登记一个类的 `@method` 魔法方法名。
    pub fn set_magic_methods(&mut self, class_fqn: &str, names: &[String]) {
        if names.is_empty() {
            return;
        }
        self.magic_methods
            .entry(class_fqn.to_string())
            .or_default()
            .extend(names.iter().cloned());
    }

    /// 该类（含祖先）通过 `MapsTo` 映射到的全部表节点。
    ///
    /// 「类映射到表」是模型类的静态身份；P7 据此把 `$model->save()` 这类
    /// FKB 声明的读 / 写动词调用升级成真正的 `WritesDb` / `ReadsDb`。
    pub fn mapped_tables(&self, class_fqn: &str, maps_to_kind: &str) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = Vec::new();
        let mut stack = vec![class_fqn.to_string()];
        let mut visited: HashSet<String> = HashSet::new();
        let mut steps = 0;
        while let Some(c) = stack.pop() {
            steps += 1;
            if steps > 50 || !visited.insert(c.clone()) {
                continue;
            }
            if let Some(edges) = self.out_edges.get(&{
                // 类 FQN → 节点 id：走短名/全名索引
                self.find_by_name(&c).map(|id| id.get()).unwrap_or(0)
            }) {
                for (k, to) in edges {
                    if k == maps_to_kind {
                        out.push(NodeId(*to));
                    } else if k == EdgeKind::RESOLVES_TO {
                        // 数据访问对象（如 CRMEB 的 `app\dao\X`）本身不映射到表，
                        // 但它 `ResolvesTo` 到真正的模型类（`app\model\X`），模型类才
                        // 有 `MapsTo`。顺着这条边找到表，才能把 service 的 `save` 落成
                        // `WritesDb`（否则 CRMEB 的写操作全退回「映射到」）。
                        if let Some(tf) = self.node(NodeId(*to)).and_then(|n| n.fqn.clone()) {
                            stack.push(tf);
                        }
                    }
                }
            }
            for p in self.parents_of(&c) {
                stack.push(p);
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// 该类（或它的祖先）是否声明了名为 `method` 的魔法方法。
    pub fn declares_magic_method(&self, class_fqn: &str, method: &str) -> bool {
        let mut stack = vec![class_fqn.to_string()];
        let mut visited: HashSet<String> = HashSet::new();
        let mut steps = 0;
        while let Some(c) = stack.pop() {
            steps += 1;
            if steps > 50 || !visited.insert(c.clone()) {
                continue;
            }
            if self
                .magic_methods
                .get(&c)
                .map(|s| s.contains(method))
                .unwrap_or(false)
            {
                return true;
            }
            for p in self.parents_of(&c) {
                stack.push(p);
            }
        }
        false
    }

    /// 记一个方法内局部变量的类型（`$x = new Y()`）。
    pub fn set_local_type(&mut self, owner_fqn: &str, var: &str, type_fqn: &str) {
        self.local_types
            .entry(owner_fqn.to_string())
            .or_default()
            .insert(var.to_string(), type_fqn.to_string());
    }

    /// 查某方法内局部变量的类型（供 `$x->method()` 解析）。
    pub fn local_type(&self, owner_fqn: &str, var: &str) -> Option<&str> {
        self.local_types
            .get(owner_fqn)
            .and_then(|m| m.get(var))
            .map(|s| s.as_str())
    }

    // ------------------------------------------------------------ 节点

    /// 跨工程唯一分配：把节点 id 计数器抬到全局最大值之上。
    ///
    /// 节点的 `id` 主键跨工程共享，而 `next_node` 每轮从 1 起算；若不抬升，
    /// 后建工程会用 `INSERT OR REPLACE` 覆盖先建工程的节点行（见 `GraphSink::max_node_id`）。
    pub fn seed_node_id(&mut self, max: i64) {
        self.next_node = self.next_node.max(max) + 1;
    }

    /// 新增语法节点。
    pub fn add_node(&mut self, mut new: NewNode) -> NodeId {
        let id = NodeId(self.next_node);
        self.next_node += 1;
        let node = Node {
            id,
            project_id: new.project_id,
            sub_project_id: new.sub_project_id,
            kind: new.kind.clone(),
            name: new.name.clone(),
            fqn: new.fqn.clone(),
            identity: new.identity.clone(),
            file_id: new.file_id,
            span: new.span,
            language: new.language.clone(),
            phase: new.phase.clone(),
            confidence: new.confidence,
            properties: new.properties.clone(),
        };
        if let Some(fqn) = &new.fqn {
            self.by_fqn.entry(fqn.clone()).or_insert(id.get());
            // 短名索引**只登记类型节点**（Class / Interface / Trait / Enum）。
            //
            // 曾经把 Method / Function 一并登记，于是 `config()` / `get()` / `index()`
            // 这类高频全局函数被短名解析撞到**同名方法**上
            // （`config()` → `app\api\controller\v1\PayController::config`），
            // 凭空生成几十条错误 Calls 边，再经 P8 传播把无关的配置依赖扩散到全部入口
            // —— 表现就是"路由连到了毫不相关的文件"。
            //
            // 短名索引的语义本就只是「类短名 → 类 FQN」；方法一律用
            // `Class::method` 全限定名精确查找，不需要也不该进短名索引。
            if is_type_kind(new.kind.as_str()) {
                if let Some(short) = fqn.rsplit(['\\', ':', '/']).next() {
                    if !short.is_empty() {
                        self.by_short
                            .entry(short.to_ascii_lowercase())
                            .or_default()
                            .push(id.get());
                    }
                }
            }
        }
        // 合成节点（Table / HttpContract / ConfigKey / I18nKey）没有 fqn，按 name 建索引，
        // 供 P7 按 `Db::name('x')` 透传的表名反查。只有 Table 需要反查，其余忽略。
        if new.kind.as_str() == NodeKind::TABLE && !new.name.is_empty() {
            self.by_table_name.entry(new.name.clone()).or_insert(id.get());
        }
        new.id = Some(id);
        self.delta.nodes.push(new);
        self.nodes.insert(id.get(), node);
        id
    }

    /// 新增或复用合成节点（**幂等合并**：相同 identity 只建一次）。
    ///
    /// 合并分两层：
    /// 1. **精确身份**（`Method /path`）：同端点同方法只建一个节点。
    /// 2. **通配方法感知**：后端「自动路由 / `Route::rule`」不限方法（method = `ANY`
    ///    / `RULE`），应与前端具体方法（`POST` / `GET`…）汇聚到同一个契约桥节点，
    ///    否则路由视角里"前端调用方 ↔ 后端 handler"会落在两张节点上、对不齐。
    ///    只要一侧是通配方法就复用同一节点（无论谁先建）。
    pub fn get_or_create_synthesized(&mut self, new: NewNode) -> (NodeId, bool) {
        if let Some(identity) = &new.identity {
            let key = identity.key();
            // 1) 精确身份合并
            if let Some(existing) = self.by_identity.get(&key).copied() {
                self.merge_synthesized(existing, &new);
                return (NodeId(existing), false);
            }
            // 2) 通配方法感知合并
            if new.kind.as_str() == NodeKind::HTTP_CONTRACT
                && identity.kind.as_str() == SynthesizedKind::CONTRACT_ID
            {
                if let Some((method, path)) = identity.contract_parts() {
                    let new_is_wild = gt_domain::model::is_wildcard_http_method(&method);
                    if let Some(&(eid, e_is_wild)) = self.contract_path_index.get(&path) {
                        if e_is_wild || new_is_wild {
                            self.merge_synthesized(eid, &new);
                            return (NodeId(eid), false);
                        }
                    }
                }
            }
            // 把契约桥登记进路径索引（通配或具体都登记，供通配合并命中）。
            // 必须在 `add_node` 移动 `new` 之前算好（path 与通配标志都是 owned 值）。
            let contract_entry = if new.kind.as_str() == NodeKind::HTTP_CONTRACT {
                identity
                    .contract_parts()
                    .map(|(method, path)| (path, gt_domain::model::is_wildcard_http_method(&method)))
            } else {
                None
            };
            let id = self.add_node(new);
            self.by_identity.insert(key, id.get());
            if let Some((path, is_wild)) = contract_entry {
                self.contract_path_index.entry(path).or_insert((id.get(), is_wild));
            }
            return (NodeId(id.get()), true);
        }
        let id = self.add_node(new);
        (id, true)
    }

    /// 复用已有合成节点时合并置信度（取 max）与 properties。
    fn merge_synthesized(&mut self, existing: i64, new: &NewNode) {
        if let Some(node) = self.nodes.get_mut(&existing) {
            if new.confidence > node.confidence {
                node.confidence = new.confidence;
            }
            merge_properties(&mut node.properties, &new.properties);
        }
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id.0)
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(&id.0)
    }

    /// 修改节点属性（同时同步到待落库的 delta）。
    pub fn patch_properties(&mut self, id: NodeId, patch: Value) {
        let key = id.get();
        if let Some(node) = self.nodes.get_mut(&key) {
            merge_properties(&mut node.properties, &patch);
        }
        // delta 中的节点在插入时已带 properties；后续 patch 单独记一条 upsert
        self.delta.property_patches.push((id, patch));
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn node_ids(&self) -> Vec<NodeId> {
        self.nodes.keys().map(|k| NodeId(*k)).collect()
    }

    pub fn nodes_of_kind(&self, kind: &str) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|(_, n)| n.kind.as_str() == kind)
            .map(|(k, _)| NodeId(*k))
            .collect()
    }

    pub fn find_by_name(&self, fqn: &str) -> Option<NodeId> {
        self.by_fqn.get(fqn).copied().map(NodeId)
    }

    /// 按表名反查合成出的 Table 节点。
    ///
    /// `Db::name('goods')` 这类写法在 P5 经 `strip_prefix → singularize → ...` 归一化后，
    /// 表节点的 `name` 可能与原始字面量不同（`goods` → `good`）。这里对齐同一套归一化：
    /// 依次尝试原串、singularize、去前缀、去前缀后 singularize，命中即返回。
    pub fn find_table_by_name(&self, raw: &str) -> Option<NodeId> {
        use crate::normalize::{singularize, strip_prefixes};
        let prefixes = self.table_prefixes();
        let candidates = [
            raw.to_string(),
            singularize(raw),
            strip_prefixes(raw, &prefixes),
            singularize(&strip_prefixes(raw, &prefixes)),
        ];
        for c in candidates {
            let key = c.trim();
            if key.is_empty() {
                continue;
            }
            if let Some(id) = self.by_table_name.get(key) {
                return Some(NodeId(*id));
            }
            // 大小写不敏感兜底
            if let Some(id) = self
                .by_table_name
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| *v)
            {
                return Some(NodeId(id));
            }
        }
        None
    }

    /// 短名解析：`StoreOrderServices` → `app\services\order\StoreOrderServices`。
    ///
    /// 走 [`Self::by_short`] 索引，O(1)；否则在大库上会退化成 O(n) 全表扫描
    /// （CRMEB 有约 5 万个 FQN，每次线性扫描会让 P5/P7 慢几十秒）。
    /// **歧义短名一律拒绝**：只有唯一候选才采纳。
    ///
    /// 曾经"先到先得"地返回第一个候选，而候选顺序取决于节点插入顺序（非确定）。
    /// 本仓库 897 个类型短名里有 112 个重名（`User` / `StoreProduct` / `Login` 各有
    /// 3~5 个候选，跨 `adminapi` / `api` / `model` 等命名空间）—— 猜中的那个
    /// 常常是 controller 而不是 model，于是 `Model --MapsTo--> Table` 这类
    /// 类级语义边会被挂到完全不相干的类上。
    ///
    /// 猜不出来就返回 `None`（留下 `UnresolvedLink` 诊断），**胜过连错一条边**。
    /// 需要精确结果时请改用 `resolve_name_in_file`（按该文件的 `use` 解析）。
    pub fn resolve_short_name(&self, short: &str) -> Option<String> {
        let target = short.trim_start_matches('\\').to_ascii_lowercase();
        let ids = self.by_short.get(&target)?;
        let mut found: Option<&str> = None;
        for id in ids {
            let Some(fqn) = self.nodes.get(id).and_then(|n| n.fqn.as_deref()) else {
                continue;
            };
            match found {
                Some(prev) if prev == fqn => {}
                Some(_) => return None, // 多个不同 FQN → 歧义，拒绝
                None => found = Some(fqn),
            }
        }
        found.map(|f| f.to_string())
    }

    // ------------------------------------------------------------ 边

    pub fn add_edge(&mut self, new: NewEdge) -> bool {
        let key = (new.kind.to_string(), new.from_id.get(), new.to_id.get());
        if !self.edge_keys.insert(key) {
            return false;
        }
        let id = self.next_edge;
        self.next_edge += 1;
        self.out_edges.entry(new.from_id.get()).or_default().push((new.kind.to_string(), new.to_id.get()));
        self.in_edges
            .entry(new.to_id.get())
            .or_default()
            .push((new.kind.to_string(), new.from_id.get()));
        *self.fan_out.entry(new.from_id.get()).or_insert(0) += 1;
        *self.fan_in.entry(new.to_id.get()).or_insert(0) += 1;
        self.delta.edges.push(new.clone());
        self.edges.push(Edge {
            id: gt_domain::model::EdgeId(id),
            project_id: new.project_id,
            kind: new.kind.clone(),
            from_id: new.from_id,
            to_id: new.to_id,
            phase: new.phase.clone(),
            confidence: new.confidence,
            properties: new.properties.clone(),
        });
        true
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// 全部边（只读）。传播阶段据此构建反向调用索引。
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    pub fn fan_in(&self, id: NodeId) -> u32 {
        self.fan_in.get(&id.get()).copied().unwrap_or(0)
    }

    pub fn fan_out(&self, id: NodeId) -> u32 {
        self.fan_out.get(&id.get()).copied().unwrap_or(0)
    }

    /// 传递闭包判定：`child` 是否（直接或间接）继承/实现了 `base_fqn`。
    ///
    /// CRMEB 的模型是 `StoreOrder extends BaseModel extends Model`，
    /// 只匹配直接基类会漏掉几乎所有表。
    pub fn has_ancestor(&self, child: NodeId, base_fqn: &str) -> bool {
        let base = base_fqn.trim_start_matches('\\').to_ascii_lowercase();
        if base.is_empty() {
            return false;
        }
        let mut visited: HashSet<i64> = HashSet::new();
        let mut stack = vec![child.get()];
        let mut depth = 0;
        while let Some(cur) = stack.pop() {
            if depth > 200 { break; }
            depth += 1;
            if !visited.insert(cur) {
                continue;
            }
            let Some(outs) = self.out_edges.get(&cur) else { continue };
            for (kind, to) in outs {
                if kind != "Extends" && kind != "Implements" {
                    continue;
                }
                let Some(node) = self.nodes.get(to) else { continue };
                let name = node
                    .fqn
                    .clone()
                    .unwrap_or_else(|| node.name.clone())
                    .trim_start_matches('\\')
                    .to_ascii_lowercase();
                if name == base || name.ends_with(&format!("\\{}", base)) {
                    return true;
                }
                stack.push(*to);
            }
        }
        false
    }

    // ------------------------------------------------------------ 标注

    /// 按合并策略打标。
    pub fn annotate(&mut self, new: NewAnnotation) {
        let key = new.node_id.get();
        match new.merge {
            MergeStrategy::MaxByKind => {
                let idx = self.annotations.iter().position(|a| {
                    a.node_id == new.node_id && a.channel == new.channel && a.kind == new.kind
                });
                if let Some(idx) = idx {
                    if new.confidence > self.annotations[idx].confidence {
                        let updated = self.materialize(&new);
                        self.annotations[idx] = updated;
                    }
                    return;
                }
            }
            MergeStrategy::Replace => {
                self.annotations
                    .retain(|a| !(a.node_id == new.node_id && a.channel == new.channel && a.kind == new.kind));
            }
            MergeStrategy::Coexist | MergeStrategy::Accumulate => {}
        }
        let ann = self.materialize(&new);
        self.delta.annotations.push(new);
        self.annotations.push(ann);
        let _ = key;
    }

    fn materialize(&mut self, new: &NewAnnotation) -> Annotation {
        let id = self.next_ann;
        self.next_ann += 1;
        Annotation {
            id,
            node_id: new.node_id,
            channel: new.channel.clone(),
            kind: new.kind.clone(),
            subkind: new.subkind.clone(),
            confidence: new.confidence,
            evidence: new.evidence.clone(),
            phase: new.phase.clone(),
        }
    }

    pub fn annotations_of(&self, id: NodeId) -> Vec<&Annotation> {
        self.annotations.iter().filter(|a| a.node_id == id).collect()
    }

    pub fn has_annotation(&self, id: NodeId, kind: &str) -> bool {
        self.annotations.iter().any(|a| a.node_id == id && a.kind == kind)
    }

    pub fn annotation_count(&self) -> usize {
        self.annotations.len()
    }

    // ------------------------------------------------------------ 别名

    pub fn put_alias(&mut self, entry: AliasEntry) {
        self.by_alias.insert(
            (
                entry.namespace.clone(),
                entry.key.clone(),
                entry.qualifier.clone().unwrap_or_default(),
            ),
            entry.node_id.get(),
        );
        self.delta.aliases.push(entry);
    }

    pub fn find_by_alias(&self, ns: &str, key: &str, qualifier: Option<&str>) -> Option<NodeId> {
        self.by_alias
            .get(&(ns.to_string(), key.to_string(), qualifier.unwrap_or_default().to_string()))
            .copied()
            .map(NodeId)
            .or_else(|| {
                // 无 qualifier 时退化为「同 namespace + key 唯一命中」
                self.by_alias
                    .iter()
                    .find(|((n, k, _), _)| n == ns && k == key)
                    .map(|(_, v)| NodeId(*v))
            })
    }

    pub fn alias_count(&self) -> usize {
        self.by_alias.len()
    }

    /// 追加一个"共现位置"。
    ///
    /// 合成节点（如 `Table:user`）既可能来自 `crmeb.sql`，也可能来自 Model 的
    /// `$table` 定义。必须**全部保留**，前端才能给出多位置列表供用户跳转验证，
    /// 而不是编造一个单一位置。
    pub fn append_location(
        &mut self,
        id: NodeId,
        file: impl Into<String>,
        line: u32,
        symbol: Option<String>,
        note: Option<String>,
        snippet: Option<String>,
    ) {
        const CAP: usize = 50;
        let loc = gt_domain::model::SourceLocation {
            file: file.into(),
            line,
            symbol,
            note,
            snippet,
        };
        // 同一「文件 + 行」只保留一份：`append_location` 的语义是记录该对象的
        // **不同**共现位置，而不是"每次规则命中都追加一条"。
        // 不去重时，同一调用点被重复命中就会让前端看到成对重复的条目。
        let dup = self
            .nodes
            .get(&id.get())
            .and_then(|n| n.properties.get("locations"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|item| {
                    item.get("file").and_then(|v| v.as_str()) == Some(loc.file.as_str())
                        && item.get("line").and_then(|v| v.as_u64()) == Some(loc.line as u64)
                })
            })
            .unwrap_or(false);
        if dup {
            return;
        }
        if let Some(node) = self.nodes.get_mut(&id.get()) {
            let existing = node
                .properties
                .as_object()
                .and_then(|o| o.get("locations"))
                .cloned();
            let mut arr = match existing {
                Some(Value::Array(a)) => a,
                _ => Vec::new(),
            };
            if arr.len() < CAP {
                arr.push(serde_json::to_value(&loc).unwrap_or(Value::Null));
            }
            if let Some(obj) = node.properties.as_object_mut() {
                obj.insert("locations".into(), Value::Array(arr));
            }
        }
        self.delta.location_patches.push((id, loc));
    }

    // ------------------------------------------------------------ 配置

    pub fn set_table_prefixes(&mut self, prefixes: Vec<String>) {
        self.table_prefixes = prefixes;
    }

    pub fn table_prefixes(&self) -> &[String] {
        &self.table_prefixes
    }

    /// 登记路由组区间（P3 从 `Route::group('v2', ...)` 调用点收集）。
    ///
    /// 用**追加**而非覆盖：loaders 按子工程逐个调用，而收集时遍历的是全量调用点，
    /// 覆盖会丢掉先处理子工程的数据（追加后由 `route_group_prefix` 去重）。
    pub fn add_route_groups(&mut self, groups: Vec<RouteGroup>) {
        self.route_groups.extend(groups);
    }

    /// 求某个调用点（文件 + 行号）所在的路由组前缀，外层在前（如 `v2` / `v2/inner`）。
    ///
    /// 用**行号区间包含**而非 AST 遍历：调用点的 `span` 天然覆盖整个
    /// `Route::group(...)` 表达式（含闭包体），判断区间包含即可还原嵌套层级。
    pub fn route_group_prefix(&self, file: &str, line: u32) -> String {
        let mut matched: Vec<&RouteGroup> = self
            .route_groups
            .iter()
            .filter(|g| g.file == file && g.start_line <= line && line <= g.end_line)
            .collect();
        if matched.is_empty() {
            return String::new();
        }
        // 外层组 start_line 更小、end_line 更大：按 (start 升序, end 降序) 即由外到内。
        matched.sort_by_key(|g| (g.start_line, std::cmp::Reverse(g.end_line), g.prefix.clone()));
        // 同一区间被重复登记时只算一次（loaders 按子工程重复遍历全量调用点）。
        matched.dedup_by_key(|g| (g.start_line, g.end_line, g.prefix.clone()));
        matched
            .iter()
            .map(|g| g.prefix.trim().trim_matches('/'))
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("/")
    }

    /// 去掉已知表前缀；同时尝试若干通用前缀。
    pub fn strip_table_prefix(&self, name: &str) -> String {
        crate::normalize::strip_prefixes(name, &self.table_prefixes)
    }

    // ------------------------------------------------------------ 继承链

    pub fn record_supertype(&mut self, child_fqn: &str, base: &str) {
        self.supertypes
            .entry(child_fqn.to_string())
            .or_default()
            .push(base.to_string());
        // 同步维护反向索引（子类 → 父类方向），供基类方法反查子类映射表。
        self.subtypes
            .entry(base.to_string())
            .or_default()
            .push(child_fqn.to_string());
    }

    /// 直接父类型名列表（`子 FQN → [父 FQN]`）。
    pub fn parents_of(&self, fqn: &str) -> Vec<String> {
        self.supertypes.get(fqn).cloned().unwrap_or_default()
    }

    /// 直接子类型名列表（`父 FQN → [子 FQN]`），继承链下游。
    pub fn children_of(&self, fqn: &str) -> Vec<String> {
        self.subtypes.get(fqn).cloned().unwrap_or_default()
    }

    /// 以 `root` 为起点沿继承链下游（子类型）做有界 BFS，返回所有可达的子类 FQN。
    ///
    /// `max_depth` 限制下探深度、`max_nodes` 限制访问节点总数，避免共享泛型基类
    /// （如 `BaseModel`）瞬间展开到几十张表造成动作边爆炸。命中 `MapsTo` 的子类
    /// 才真正有用，这里只负责把候选子类交出去。
    pub fn subtypes_bfs(&self, root: &str, max_depth: usize, max_nodes: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack: Vec<(String, usize)> = vec![(root.to_string(), 0)];
        while let Some((cur, depth)) = stack.pop() {
            if out.len() >= max_nodes {
                break;
            }
            if !visited.insert(cur.clone()) {
                continue;
            }
            if depth > 0 {
                out.push(cur.clone());
            }
            if depth >= max_depth {
                continue;
            }
            for c in self.children_of(&cur) {
                stack.push((c, depth + 1));
            }
        }
        out
    }

    /// `child` 是否（传递地）继承/实现了与 `base` 同名的类型。
    ///
    /// 比较采用「小写相等 或 以 `\base` 结尾」，从而 `Model` 能匹配 `think\Model`。
    pub fn has_supertype(&self, child_fqn: &str, base: &str) -> bool {
        let want = base.trim_start_matches('\\').to_ascii_lowercase();
        if want.is_empty() {
            return false;
        }
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack = vec![child_fqn.to_string()];
        let mut steps = 0;
        while let Some(cur) = stack.pop() {
            steps += 1;
            if steps > 200 || !visited.insert(cur.clone()) {
                continue;
            }
            let Some(parents) = self.supertypes.get(&cur) else { continue };
            for p in parents {
                let name = p.trim_start_matches('\\').to_ascii_lowercase();
                if name == want || name.ends_with(&format!("\\{}", want)) {
                    return true;
                }
                stack.push(p.clone());
            }
        }
        false
    }

    // ------------------------------------------------------------ 文件节点

    pub fn record_file_node(&mut self, path: &str, id: NodeId) {
        self.file_nodes.insert(path.to_string(), id.get());
    }

    pub fn file_node(&self, path: &str) -> Option<NodeId> {
        self.file_nodes.get(path).copied().map(NodeId)
    }

    // ------------------------------------------------------------ 属性默认值

    pub fn record_property(&mut self, class_node: NodeId, name: &str, value: FactValue) {
        self.prop_values.insert((class_node.get(), name.to_string()), value);
    }

    pub fn property_of(&self, node: NodeId, name: &str) -> Option<FactValue> {
        self.prop_values.get(&(node.get(), name.to_string())).cloned()
    }

    // ------------------------------------------------------------ 符号表 / 事实

    pub fn put_symbol(&mut self, project_id: ProjectId, table: &str, key: &str, value: Value) {
        self.symbols
            .entry(table.to_string())
            .or_default()
            .insert(key.to_string(), value.clone());
        self.delta.symbols.push(gt_domain::model::SymbolEntry {
            project_id,
            table: table.to_string(),
            key: key.to_string(),
            value,
        });
    }

    pub fn get_symbol(&self, table: &str, key: &str) -> Option<&Value> {
        self.symbols.get(table).and_then(|m| m.get(key))
    }

    /// 经由 import 别名把短名还原为 FQN（通用机制，**不绑定任何框架**）。
    ///
    /// `use think\facade\Queue as QueueThink;` 会被 P2 写进 `imports` 符号表，
    /// 于是 `QueueThink` 能还原为 `think\facade\Queue`，从而命中 FKB 里
    /// `Queue::push` 这类「以伞名结尾」的匹配模式。
    ///
    /// 这是全局索引（按短名小写），与 `resolve_short_name` 同样的启发式权衡：
    /// 不同文件里同名别名可能指向不同 FQN，但匹配是尽力而为、可叠加的。
    /// 登记**某个文件**的 `use` 导入表（P2 调用，一个文件一份）。
    pub fn record_file_imports(
        &mut self,
        file_id: i64,
        path: &str,
        imports: HashMap<String, String>,
    ) {
        self.file_id_by_path.insert(path.to_string(), file_id);
        self.source_path_by_id.insert(file_id, path.to_string());
        self.file_imports.insert(file_id, imports);
    }

    /// 源文件 id → 路径。
    pub fn source_path_of(&self, file_id: i64) -> Option<String> {
        self.source_path_by_id.get(&file_id).cloned()
    }

    /// 该节点是否**已经被认领**：已有该种类的入边，或有该种类的待定链接指向它。
    ///
    /// 为什么单看入边不够：路由 handler 的边大多在 **P7** 才落成（P5 只能排个
    /// `PendingLink`，因为类名要按 FKB 的模板拼），而"约定推断"规则跑在 **P6**，
    /// 此时边还不存在，只看入边会把显式注册的路由也让 before/after 约定重复兜一遍。
    ///
    /// handler 的形状是通用的 `Class/method`（写方法名）或 `Class`（REST 资源路由，
    /// 等价于认领整个类的标准动作），故按形状比对即可，内核不需要认识具体框架。
    pub fn claimed_by(&self, node: NodeId, kind: &str) -> bool {
        if self
            .in_edges
            .get(&node.get())
            .map(|v| v.iter().any(|(k, _)| k == kind))
            .unwrap_or(false)
        {
            return true;
        }
        let Some(n) = self.node(node) else { return false };
        let Some(fqn) = n.fqn.clone() else { return false };
        let (class_part, member) = match fqn.rfind("::") {
            Some(i) => (&fqn[..i], Some(fqn[i + 2..].to_string())),
            None => (fqn.as_str(), None),
        };
        let short_name_of = |fqn: &str| -> String {
            fqn.rsplit(['\\', '.', '/', ':'])
                .next()
                .unwrap_or(fqn)
                .to_string()
        };
        let self_class = short_name_of(class_part).to_ascii_lowercase();
        if self_class.is_empty() {
            return false;
        }
        self.pending_links.iter().filter(|p| p.kind.0 == kind).any(|p| {
            let raw = p.raw.trim_start_matches('\\');
            let (head, tail_method) = match raw.rsplit_once('/') {
                Some((h, m)) => (h, Some(m.to_ascii_lowercase())),
                None => (raw, None),
            };
            let claimed_class = short_name_of(head).to_ascii_lowercase();
            if claimed_class != self_class {
                return false;
            }
            match tail_method {
                // 写了方法名 → 只认领这一个方法
                Some(m) => member.as_ref().map(|x| x.to_ascii_lowercase()) == Some(m),
                // 没写方法名（资源路由） → 整个控制器都被显式路由接管
                None => true,
            }
        })
    }

    /// 取**某个文件**的 `use` 导入表：短名(小写) → FQN。
    ///
    /// P7 解析 `Class::method` 的接收者时必须先查它 —— 这是 PHP 真实的解析规则，
    /// 与框架无关。查不到才允许退回全局短名索引。
    pub fn imports_of_file(&self, file_id: i64) -> Option<&HashMap<String, String>> {
        self.file_imports.get(&file_id)
    }

    pub fn imports_of_path(&self, path: &str) -> Option<&HashMap<String, String>> {
        self.file_id_by_path
            .get(path)
            .and_then(|id| self.file_imports.get(id))
    }

    /// **在某个文件里**把短名还原成 FQN —— 所有需要"猜类名"的地方都该走这里。
    ///
    /// 规则（与 PHP 一致，不绑定任何框架）：
    /// * 该文件 `use` 过这个短名 → **只**认它导入的 FQN，哪怕那个类不在图里
    ///   （框架类，vendor 已被 P0 排除）。此时**绝不**退回全局索引去猜同名项目类；
    /// * 否则退回全局短名索引，且**歧义短名一律拒绝**（见 `resolve_short_name`）。
    pub fn resolve_name_in_file(&self, file: Option<&str>, raw: &str) -> Option<String> {
        if let Some(path) = file {
            if let Some(fqn) = self
                .imports_of_path(path)
                .and_then(|m| m.get(&raw.to_ascii_lowercase()))
            {
                return Some(fqn.clone());
            }
        }
        self.resolve_short_name(raw)
    }

    /// 在**某个节点所属文件**里把短名还原成 FQN（先 `use`，再全局短名索引）。
    ///
    /// 供那些只有"调用方节点"而没有现成文件路径的解析点使用（接收者类型、自由函数）。
    pub fn resolve_name_at(&self, owner: NodeId, raw: &str) -> Option<String> {
        if let Some(fqn) = self
            .node(owner)
            .and_then(|n| n.file_id)
            .and_then(|id| self.file_imports.get(&id.get()))
            .and_then(|m| m.get(&raw.to_ascii_lowercase()))
        {
            return Some(fqn.clone());
        }
        self.resolve_short_name(raw)
    }

    pub fn resolve_import_alias(&self, name: &str) -> Option<String> {
        let key = name.trim_start_matches('\\').to_ascii_lowercase();
        self.get_symbol("imports", &key)
            .and_then(|v| v.get("fqn"))
            .and_then(|f| f.as_str())
            .map(|s| s.to_string())
    }

    pub fn symbol_table(&self, table: &str) -> Option<&BTreeMap<String, Value>> {
        self.symbols.get(table)
    }

    pub fn set_fact(&mut self, sub: SubProjectId, key: &str, value: Value) {
        self.facts.entry(sub.get()).or_default().insert(key.to_string(), value);
    }

    pub fn get_fact(&self, sub: SubProjectId, key: &str) -> Option<&Value> {
        self.facts.get(&sub.get()).and_then(|m| m.get(key))
    }

    /// 某子工程的全部事实快照（用于回写到数据库）。
    pub fn facts_snapshot(&self, sub: SubProjectId) -> Option<Value> {
        self.facts
            .get(&sub.get())
            .map(|m| Value::Object(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()))
    }

    // ------------------------------------------------------------ 诊断

    pub fn diagnose(
        &mut self,
        phase: &Phase,
        code: &str,
        severity: Severity,
        message: impl Into<String>,
        location: Option<String>,
    ) {
        self.diagnostics.push(Diagnostic {
            project_id: self.project_id,
            sub_project_id: None,
            phase: phase.clone(),
            code: code.to_string(),
            severity,
            message: message.into(),
            location,
            payload: Value::Null,
        });
    }

    // ------------------------------------------------------------ 落库

    /// 取出并清空累积的变更。
    pub fn take_delta(&mut self) -> GraphDelta {
        std::mem::replace(&mut self.delta, GraphDelta::new(self.project_id))
    }

    pub fn remaining_diagnostics(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.diagnostics)
    }
}

/// 浅合并两个 JSON 对象（后者覆盖前者）。
fn merge_properties(base: &mut Value, patch: &Value) {
    match (base.as_object_mut(), patch.as_object()) {
        (Some(b), Some(p)) => {
            for (k, v) in p {
                match (b.get(k), v.as_object()) {
                    (Some(Value::Object(_)), Some(_)) => {
                        let mut cur = b.get(k).cloned().unwrap_or(Value::Null);
                        merge_properties(&mut cur, v);
                        b.insert(k.clone(), cur);
                    }
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        _ => {
            if !patch.is_null() {
                *base = patch.clone();
            }
        }
    }
}

/// 便捷构造：带 identity 的合成节点。
pub fn synthesized_node(
    project_id: ProjectId,
    kind: &str,
    identity: IdentityKey,
    sub: Option<SubProjectId>,
    phase: &Phase,
    confidence: f32,
    language: &Language,
    span: Span,
) -> NewNode {
    NewNode {
        id: None,
        project_id,
        sub_project_id: sub,
        kind: NodeKind(kind.to_string()),
        name: identity.value.clone(),
        fqn: None,
        identity: Some(identity),
        file_id: None,
        span,
        language: language.clone(),
        phase: phase.clone(),
        confidence,
        properties: Value::Null,
    }
}
