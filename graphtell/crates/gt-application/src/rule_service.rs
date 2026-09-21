//! 合规检查用例：在图上运行 [`CheckRule`]，产出 [`Violation`]。
//!
//! # 定位
//!
//! 这是**建图之后**的只读用例：不改节点、不改边，只往诊断表写 `rule:*` 违规。
//! 之所以直接落成诊断，是因为诊断已经是一等产物（有 severity / location / payload，
//! 前端 DiagnosticsPage 直接渲染）—— 规则引擎因此不需要新的存储与新的页面。
//!
//! # 为什么预装载而不是逐节点查库
//!
//! 一条规则要对成千上万个节点判断「有没有某标注 / 有没有某条边」。
//! 逐节点 `annotations_of` / `edges_of` 是 N+1 往返（万级节点即秒级抖动），
//! 这里先一次性把标注、入边、出边、文件路径装进内存，求值全程零往返。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use gt_domain::error::Result;
use gt_domain::model::{
    Annotation, CheckPredicate, CheckReport, CheckRule, Diagnostic, Edge, Node, NodeId, NodeKind,
    ProjectId, RuleRequirements, Severity, Violation, RULE_CODE_PREFIX,
};
use gt_domain::model::kinds::{is_semantic_edge, AnnotationChannel};
use gt_domain::port::{NodeFilter, Persistence, RuleProvider};

// ---------------------------------------------------------------- 规则引擎

/// 检查服务。
pub struct RuleService {
    store: Arc<dyn Persistence>,
    rules: Arc<dyn RuleProvider>,
}

impl RuleService {
    pub fn new(store: Arc<dyn Persistence>, rules: Arc<dyn RuleProvider>) -> Self {
        Self { store, rules }
    }

    /// 全部已装载规则。
    pub fn rules(&self) -> Vec<CheckRule> {
        self.rules.rules().to_vec()
    }

    /// 对工程执行检查。
    ///
    /// * `only` —— 只跑给定 id 的规则；为空表示跑全部启用规则。
    /// * `persist` —— 是否把违规写回诊断表（CLI 预览时可关闭）。
    pub fn check(
        &self,
        project_id: ProjectId,
        only: Option<&[String]>,
        persist: bool,
    ) -> Result<CheckReport> {
        let started = Instant::now();
        let all = self.rules.rules();
        let selected: Vec<&CheckRule> = all
            .iter()
            .filter(|r| r.enabled)
            .filter(|r| match only {
                Some(ids) => ids.iter().any(|id| id == &r.id),
                None => true,
            })
            .collect();

        // ---- 0) 环境闸门 + 判据校验：把规则分成「能跑 / 不适用 / 不可用」三类
        //
        // 两层缺一不可，它们挡的是不同的失效：
        //   * 环境闸门（languages / frameworks）—— 规则作者**先验**声明的适用范围。
        //     PHP 的事件语义（Triggers / Emits）在 Java 工程里压根不存在，
        //     把 orphan-event 放到纯 Java 工程上跑，会把每个事件节点都报成"没人触发"。
        //   * 判据校验（RuleRequirements）—— 从谓词**自动推导**的图事实依赖。
        //     就算语言对得上，某个 FKB 也可能没产出对应标注（如 pii、
        //     Capability:Authentication），此时 `no_annotation` / `no_capability` 恒真，
        //     跑出来的全是恒真误报。
        let env = ProjectEnv::load(&*self.store, project_id)?;
        let mut runnable: Vec<&CheckRule> = Vec::new();
        let mut not_applicable: Vec<String> = Vec::new();
        let mut unavailable: Vec<String> = Vec::new();
        // 环境对得上、但判据不成立的规则：显式重跑时要一并清掉它们的旧违规，
        // 否则"这次跑不了"会被读成"上次的结果是当前的"。
        let mut unavailable_rules: Vec<&CheckRule> = Vec::new();
        for rule in selected {
            if !rule.applies_to_env(&env.languages, &env.frameworks) {
                not_applicable.push(format!(
                    "{}（需要 {}，本工程为 {}）",
                    rule.id,
                    rule.applies_to.languages.join("/"),
                    env.describe()
                ));
                continue;
            }
            if let Some(why) = env.missing_requirement(&rule.requirements()) {
                unavailable.push(format!("{}（{}）", rule.id, why));
                unavailable_rules.push(rule);
                continue;
            }
            runnable.push(rule);
        }

        let mut report = CheckReport {
            project_id,
            rules_total: all.len(),
            rules_run: runnable.len(),
            rules_not_applicable: not_applicable,
            rules_unavailable: unavailable,
            ..Default::default()
        };
        if runnable.is_empty() {
            report.duration_ms = started.elapsed().as_millis() as u64;
            return Ok(report);
        }

        // ---- 1) 候选集：按 (kind, name_contains) 缓存，规则共享同一次查询
        let root = self
            .store
            .get_project(project_id)?
            .map(|p| std::path::PathBuf::from(p.root_path));
        let mut facts = Facts::new(root);
        let mut cache: HashMap<(String, Option<String>), Vec<Node>> = HashMap::new();
        for rule in &runnable {
            for kind in scope_kinds(rule) {
                let key = (kind.clone(), rule.applies_to.name_contains.clone());
                if cache.contains_key(&key) {
                    continue;
                }
                // `applies_to.kinds` 为空表示**不限种类**（见 RuleScope 的文档）。
                // 此时必须传 `None`：若传 `Some(NodeKind(""))`，SQL 会变成
                // `kind = ''`，匹配不到任何节点 —— 规则静默 0 命中，
                // 与"不限种类"的承诺正好相反。
                let nodes = self.store.query_nodes(&NodeFilter {
                    project_id,
                    kind: if kind.is_empty() {
                        None
                    } else {
                        Some(NodeKind::new(kind.clone()))
                    },
                    name_contains: rule.applies_to.name_contains.clone(),
                    limit: Some(rule.applies_to.limit),
                    offset: None,
                })?;
                cache.insert(key, nodes);
            }
        }

        // ---- 2) 预装载：标注 / 入边 / 出边 / 文件路径
        let mut ids: Vec<NodeId> = Vec::new();
        for nodes in cache.values() {
            for n in nodes {
                ids.push(n.id);
            }
        }
        facts.annotations = self.store.annotations_of_project(project_id)?;
        facts.files = self.store.file_paths(project_id)?;
        facts.incoming = self.store.edges_incoming(&ids)?;
        facts.outgoing = self.store.edges_outgoing(&ids)?;

        // ---- 3) 逐规则求值
        //
        // 同时记录"跑了但 0 命中"的规则：候选集为空或一条都没匹配上，
        // 都要显式暴露（见 CheckReport::rules_silent 的说明）。
        let mut violations: Vec<Violation> = Vec::new();
        let mut silent: Vec<String> = Vec::new();
        for rule in &runnable {
            let mut hit = 0usize;
            let mut candidates = 0usize;
            for kind in scope_kinds(rule) {
                let key = (kind.clone(), rule.applies_to.name_contains.clone());
                let Some(nodes) = cache.get(&key) else { continue };
                candidates += nodes.len();
                for node in nodes {
                    if !matches_all(&rule.when, node, &facts) {
                        continue;
                    }
                    hit += 1;
                    violations.push(build_violation(project_id, rule, node, &facts));
                }
            }
            if hit == 0 {
                silent.push(if candidates == 0 {
                    format!("{}（候选集为空：图上没有这类节点）", rule.id)
                } else {
                    format!("{}（{} 个候选均未命中）", rule.id, candidates)
                });
            }
        }

        // ---- 4) 落库：先清旧再写新，保证用户看到的是"当前代码的结论"
        //
        // 清理范围**只覆盖本次真正跑过的规则**：跑全量时清空整个 `rule:` 前缀，
        // 只跑某几条时只清这几条 —— 否则"单独重跑 A 规则"会顺手抹掉 B/C 的结果，
        // 用户看到的报告会莫名缺一块。
        if persist {
            match only {
                None => {
                    self.store.clear_diagnostics(project_id, RULE_CODE_PREFIX)?;
                }
                Some(_) => {
                    for rule in runnable.iter().chain(unavailable_rules.iter()) {
                        self.store.clear_diagnostics(project_id, &rule.code())?;
                    }
                }
            }
            let diags: Vec<Diagnostic> = violations.iter().map(|v| v.to_diagnostic()).collect();
            if !diags.is_empty() {
                // 分批写入：SQLite 单条语句的变量数有上限。
                for chunk in diags.chunks(500) {
                    self.store.push_diagnostics(chunk)?;
                }
            }
        }

        let mut by_severity: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        let mut by_rule: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        for v in &violations {
            *by_severity.entry(severity_key(v.severity).to_string()).or_default() += 1;
            *by_rule.entry(v.rule_id.clone()).or_default() += 1;
        }
        report.by_severity = by_severity;
        report.by_rule = by_rule;
        report.violations = violations;
        report.rules_silent = silent;
        report.sort_violations();
        report.duration_ms = started.elapsed().as_millis() as u64;
        Ok(report)
    }

    /// 读取已落库的违规（不重跑规则）。
    pub fn violations(&self, project_id: ProjectId, limit: u32) -> Result<Vec<Violation>> {
        let diags = self
            .store
            .list_diagnostics_by_code(project_id, RULE_CODE_PREFIX, limit)?;
        Ok(diags.iter().filter_map(Violation::from_diagnostic).collect())
    }

    /// 已落库违规的轻量汇总（按严重度计数），供菜单角标这类场景使用。
    ///
    /// 不重跑规则、不拉全量违规，只取分组计数；建图后会自动跑检查并落库，
    /// 因此这里看到的是上一次自动（或手动）检查的结论。
    pub fn summary(&self, project_id: ProjectId) -> Result<CheckSummary> {
        let counts = self.store.count_diagnostics_by_code(project_id, RULE_CODE_PREFIX)?;
        let mut s = CheckSummary::default();
        for (sev, n) in counts {
            match sev.as_str() {
                "critical" => s.critical = n,
                "error" => s.error = n,
                "warning" => s.warning = n,
                "info" => s.info = n,
                _ => {}
            }
        }
        Ok(s)
    }
}

/// 合规检查的严重度汇总（菜单角标用）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CheckSummary {
    pub critical: u64,
    pub error: u64,
    pub warning: u64,
    pub info: u64,
}

/// 工程的技术栈环境 + 图上真实存在的图事实种类。
///
/// 两者都是规则的**适用性判据**，但来源不同：
/// * `languages` / `frameworks` 来自子工程识别（这个工程"是"什么栈）；
/// * `edge_kinds` / `annotation_kinds` 来自已建成的图（这个工程"产出了"什么事实）。
struct ProjectEnv {
    languages: Vec<String>,
    frameworks: Vec<String>,
    edge_kinds: Vec<String>,
    /// `(channel, kind)`。
    annotation_kinds: Vec<(String, String)>,
}

impl ProjectEnv {
    fn load(store: &dyn Persistence, project_id: ProjectId) -> Result<Self> {
        let mut languages: Vec<String> = Vec::new();
        let mut frameworks: Vec<String> = Vec::new();
        for sub in store.list_sub_projects(project_id)? {
            let l = sub.language.to_string();
            if !languages.iter().any(|x| x.eq_ignore_ascii_case(&l)) {
                languages.push(l);
            }
            for f in sub.frameworks {
                if !frameworks.iter().any(|x| x.eq_ignore_ascii_case(&f)) {
                    frameworks.push(f);
                }
            }
        }
        Ok(Self {
            languages,
            frameworks,
            edge_kinds: store.edge_kinds(project_id)?,
            annotation_kinds: store.annotation_kinds(project_id)?,
        })
    }

    fn describe(&self) -> String {
        let l = if self.languages.is_empty() {
            "未知".to_string()
        } else {
            self.languages.join("/")
        };
        if self.frameworks.is_empty() {
            l
        } else {
            format!("{l}（{}）", self.frameworks.join("/"))
        }
    }

    fn has_edge(&self, kind: &str) -> bool {
        self.edge_kinds.iter().any(|k| k.eq_ignore_ascii_case(kind))
    }

    fn has_annotation(&self, kind: &str) -> bool {
        self.annotation_kinds.iter().any(|(_, k)| k.eq_ignore_ascii_case(kind))
    }

    /// 判据是否成立；返回 `Some(原因)` 表示这条规则在本工程上**不能跑**。
    ///
    /// 只拦"图上一个都没有"的情况：只要有哪怕一条同名的事实存在，
    /// 说明产出链路是通的，剩下的（比如只产出了 3 条）属于覆盖率问题，
    /// 应该由"静默归零"告警去提示，而不是直接停掉规则。
    fn missing_requirement(&self, req: &RuleRequirements) -> Option<String> {
        for e in &req.edges {
            if !self.has_edge(e) {
                return Some(format!("图上没有任何 {e} 边，`no/has_{e}` 判据不成立"));
            }
        }
        for a in &req.annotations {
            if !self.has_annotation(a) {
                return Some(format!("图上没有任何 {a} 标注，`no/has_annotation` 判据不成立"));
            }
        }
        if !req.capabilities.is_empty() {
            let any_cap = self
                .annotation_kinds
                .iter()
                .any(|(ch, _)| ch == AnnotationChannel::CAPABILITY);
            if !any_cap {
                return Some(format!(
                    "图上一个 {} 通道标注都没有，能力判据（{}）不成立",
                    AnnotationChannel::CAPABILITY,
                    req.capabilities.join("/")
                ));
            }
        }
        None
    }
}

fn severity_key(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

/// 规则作用的节点种类；为空时退化为"不限种类"（用空串作缓存键）。
fn scope_kinds(rule: &CheckRule) -> Vec<String> {
    if rule.applies_to.kinds.is_empty() {
        vec![String::new()]
    } else {
        rule.applies_to.kinds.clone()
    }
}

fn build_violation(
    project_id: ProjectId,
    rule: &CheckRule,
    node: &Node,
    facts: &Facts,
) -> Violation {
    let (file, line) = crate::location::node_location(node, &facts.files, facts.root.as_deref());
    let identity = node.identity.as_ref().map(|i| i.value.clone());
    let message = rule.render(
        &node.name,
        node.kind.as_str(),
        node.fqn.as_deref(),
        identity.as_deref(),
        file.as_deref(),
        line,
    );
    Violation {
        project_id,
        rule_id: rule.id.clone(),
        title: rule.title.clone(),
        category: rule.category.clone(),
        severity: rule.severity,
        node_id: node.id,
        node_name: node.name.clone(),
        node_kind: node.kind.to_string(),
        message,
        remediation: rule.remediation.clone(),
        file,
        line,
    }
}

// ---------------------------------------------------------------- 求值上下文

/// 一次检查预装载的图事实。
///
/// 只读、不可变 —— 所有规则的求值都在这份快照上进行，保证同一份输入得到同一份输出。
#[derive(Default)]
struct Facts {
    annotations: HashMap<i64, Vec<Annotation>>,
    incoming: HashMap<i64, Vec<Edge>>,
    outgoing: HashMap<i64, Vec<Edge>>,
    files: HashMap<i64, String>,
    /// 工程根路径（合成节点的 `locations` 是相对路径，需要它拼成绝对路径）。
    root: Option<std::path::PathBuf>,
}

impl Facts {
    fn new(root: Option<std::path::PathBuf>) -> Self {
        Self { root, ..Default::default() }
    }

    /// 节点是否有给定种类的标注（大小写不敏感）。
    fn has_annotation(&self, node: NodeId, kind: &str) -> bool {
        self.annotations
            .get(&node.get())
            .map(|list| list.iter().any(|a| a.kind.eq_ignore_ascii_case(kind)))
            .unwrap_or(false)
    }

    /// 节点（及其作用域链上）是否具备给定能力。
    ///
    /// 能力标注在 `Capability` 通道上（`Authentication` / `RateLimiting`）。
    fn has_capability(&self, node: NodeId, cap: &str) -> bool {
        self.annotations
            .get(&node.get())
            .map(|list| {
                list.iter()
                    .any(|a| a.channel.as_str() == AnnotationChannel::CAPABILITY
                        && a.kind.eq_ignore_ascii_case(cap))
            })
            .unwrap_or(false)
    }

    /// 扇入：语义入边条数（口径与视图层一致，避免"入边 N"对不上画布）。
    fn fan_in(&self, node: NodeId) -> u64 {
        self.incoming
            .get(&node.get())
            .map(|es| es.iter().filter(|e| is_semantic_edge(e.kind.as_str())).count() as u64)
            .unwrap_or(0)
    }

    fn fan_out(&self, node: NodeId) -> u64 {
        self.outgoing
            .get(&node.get())
            .map(|es| es.iter().filter(|e| is_semantic_edge(e.kind.as_str())).count() as u64)
            .unwrap_or(0)
    }

    fn has_incoming_kind(&self, node: NodeId, kind: &str) -> bool {
        self.incoming
            .get(&node.get())
            .map(|es| es.iter().any(|e| e.kind.as_str().eq_ignore_ascii_case(kind)))
            .unwrap_or(false)
    }

    fn has_outgoing_kind(&self, node: NodeId, kind: &str) -> bool {
        self.outgoing
            .get(&node.get())
            .map(|es| es.iter().any(|e| e.kind.as_str().eq_ignore_ascii_case(kind)))
            .unwrap_or(false)
    }
}

/// 节点的"可检索文本"：name / fqn / identity。
fn node_text(node: &Node) -> String {
    let mut s = node.name.clone();
    if let Some(fqn) = &node.fqn {
        s.push('\n');
        s.push_str(fqn);
    }
    if let Some(id) = &node.identity {
        s.push('\n');
        s.push_str(&id.value);
    }
    s.to_lowercase()
}

fn property_value(node: &Node, name: &str) -> Option<String> {
    let v = node.properties.get(name)?;
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// 全部谓词满足才算命中（`when` 为空 = 范围内全部命中）。
fn matches_all(when: &[CheckPredicate], node: &Node, facts: &Facts) -> bool {
    when.iter().all(|p| eval(p, node, facts))
}

fn eval(p: &CheckPredicate, node: &Node, facts: &Facts) -> bool {
    match p {
        CheckPredicate::KindIn(kinds) => kinds
            .iter()
            .any(|k| node.kind.as_str().eq_ignore_ascii_case(k)),
        CheckPredicate::NameContains(sub) => {
            node.name.to_lowercase().contains(&sub.to_lowercase())
        }
        CheckPredicate::NameStartsWith(prefix) => {
            node.name.to_lowercase().starts_with(&prefix.to_lowercase())
        }
        CheckPredicate::FqnContains(sub) => node
            .fqn
            .as_deref()
            .map(|f| f.to_lowercase().contains(&sub.to_lowercase()))
            .unwrap_or(false),
        CheckPredicate::IdentityContains(sub) => node
            .identity
            .as_ref()
            .map(|i| i.value.to_lowercase().contains(&sub.to_lowercase()))
            .unwrap_or(false),
        CheckPredicate::TextContains(sub) => {
            node_text(node).contains(&sub.to_lowercase())
        }
        CheckPredicate::HasAnnotation(kind) => facts.has_annotation(node.id, kind),
        CheckPredicate::NoAnnotation(kind) => !facts.has_annotation(node.id, kind),
        CheckPredicate::PropertyIs { name, value } => property_value(node, name)
            .map(|v| v.eq_ignore_ascii_case(value))
            .unwrap_or(false),
        CheckPredicate::PropertyMissing(name) => property_value(node, name).is_none(),
        CheckPredicate::NoCapability(caps) => {
            !caps.iter().any(|c| facts.has_capability(node.id, c))
        }
        CheckPredicate::FanInGte(n) => facts.fan_in(node.id) >= *n,
        CheckPredicate::FanInLte(n) => facts.fan_in(node.id) <= *n,
        CheckPredicate::FanOutGte(n) => facts.fan_out(node.id) >= *n,
        CheckPredicate::NoIncoming(kind) => !facts.has_incoming_kind(node.id, kind),
        CheckPredicate::HasIncoming(kind) => facts.has_incoming_kind(node.id, kind),
        CheckPredicate::NoOutgoing(kind) => !facts.has_outgoing_kind(node.id, kind),
        CheckPredicate::HasOutgoing(kind) => facts.has_outgoing_kind(node.id, kind),
        CheckPredicate::AllOf(list) => list.iter().all(|p| eval(p, node, facts)),
        CheckPredicate::AnyOf(list) => list.iter().any(|p| eval(p, node, facts)),
        CheckPredicate::Not(inner) => !eval(inner, node, facts),
    }
}
