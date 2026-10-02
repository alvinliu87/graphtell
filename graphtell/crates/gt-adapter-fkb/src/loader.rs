//! FKB 装载：从 YAML 目录读取框架知识。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::fkb::{Action, MatchMode, Rule, Selector};
use gt_domain::model::kinds::{register_edge_kinds, Phase};
use gt_domain::model::{FrameworkKnowledge, Language};
use gt_domain::port::KnowledgeProvider;
use tracing::{info, warn};

/// 基于文件目录的 FKB 供给实现。
///
/// 目录结构：`fkb/<任意子目录>/*.yaml`，每份 YAML 描述一个框架。
pub struct YamlKnowledgeBase {
    entries: Vec<FrameworkKnowledge>,
    sources: Vec<PathBuf>,
}

impl YamlKnowledgeBase {
    /// 递归装载目录下的所有 `*.yaml` / `*.yml`。
    pub fn load_dir(root: &Path) -> Result<Self> {
        if !root.exists() {
            warn!("FKB 目录不存在: {}", root.display());
            return Ok(Self { entries: Vec::new(), sources: Vec::new() });
        }
        // 跨语言核心能力模板（带洞的 `Rule` 模板 + 数据字段，识别机制不属于 FKB），
        // 与 `DEFAULT_ANNOTATIONS_YAML`（注解识别器）同构：内核提供参数化 `Rule` 模板，
        // 各语言 FKB 只声明 `capability_interfaces` 的 `types` / `read` / `write`。
        let cap_templates = load_capability_rule_templates();
        // 跨语言核心注解识别器（P6 标注标准，不属于 FKB），见 `DEFAULT_ANNOTATIONS_YAML`。
        let ann_templates = load_annotation_templates();
        let mut entries = Vec::new();
        let mut sources = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let rd = std::fs::read_dir(&dir).map_err(DomainError::infra)?;
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let is_yaml = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
                    .unwrap_or(false);
                if !is_yaml {
                    continue;
                }
                match Self::load_file(&path) {
                    Ok(mut fk) => {
                        info!("装载 FKB: {} ({})", fk.id, path.display());
                        // FKB 声明的**语义节点种类**在此登记：新增一种语义节点
                        // （前端 `Store`、页面 `Page`…）无需再改内核清单。
                        if !fk.semantic_kinds.is_empty() {
                            gt_domain::model::kinds::register_semantic_kinds(
                                fk.semantic_kinds.iter().cloned(),
                            );
                        }
                        // FKB 声明的**语义 / 桥边种类**在此登记：新增一种边种类
                        // （如某框架的 `SendsWebhook`）无需再改 `kinds.rs` 的
                        // `SEMANTIC` / `BRIDGE` 清单 —— 与节点的登记同构。
                        if !fk.semantic_edge_kinds.is_empty()
                            || !fk.bridge_edge_kinds.is_empty()
                        {
                            gt_domain::model::kinds::register_edge_kinds(
                                fk.semantic_edge_kinds.iter().cloned(),
                                fk.bridge_edge_kinds.iter().cloned(),
                            );
                        }
                        // FKB 声明的**业务特有标注种类**在此登记：新增一种业务标注语义
                        // 无需改内核（与 `semantic_kinds` / `semantic_edge_kinds` 同构）。
                        // 内核标准种类（pii / data.criticality / auth.public …）由 `annotation_templates`
                        // 识别器产出，不在此列。
                        if !fk.annotation_kinds.is_empty() {
                            gt_domain::model::kinds::register_annotation_kinds(
                                fk.annotation_kinds.iter().cloned(),
                            );
                        }
                        // 把 `capability_interfaces` 声明摊平成合成规则（引擎原样消费，零改动）。
                        expand_capability_interfaces(&mut fk, &cap_templates);
                        // 把 `annotation_interfaces` 声明摊平成 P6 标注规则（内核标准识别器 + 本栈参数）。
                        expand_annotation_interfaces(&mut fk, &ann_templates);
                        entries.push(fk);
                        sources.push(path);
                    }
                    Err(e) => warn!("跳过损坏的 FKB {}: {e}", path.display()),
                }
            }
        }
        Ok(Self { entries, sources })
    }

    pub fn load_file(path: &Path) -> Result<FrameworkKnowledge> {
        let text = std::fs::read_to_string(path).map_err(DomainError::infra)?;
        let fk = deserialize_knowledge(&text)
            .map_err(|e| DomainError::InvalidKnowledge(format!("{}: {e}", path.display())))?;
        if fk.id.is_empty() {
            return Err(DomainError::InvalidKnowledge(format!(
                "{}: 缺少 id 字段",
                path.display()
            )));
        }
        Ok(fk)
    }

    /// 从 YAML 文本反序列化（供测试与单文件装载使用）。
    pub fn from_str(text: &str) -> Result<FrameworkKnowledge> {
        deserialize_knowledge(text)
    }

    /// 按多个目录顺序装载（后者覆盖同 id 的条目）。
    ///
    /// 每个目录由 `load_dir` 自行完成能力接口展开，这里只做合并，避免重复展开。
    pub fn load_dirs(dirs: &[PathBuf]) -> Result<Self> {
        let mut merged: Vec<FrameworkKnowledge> = Vec::new();
        let mut sources = Vec::new();
        for dir in dirs {
            let kb = Self::load_dir(dir)?;
            for fk in kb.entries {
                if let Some(existing) = merged.iter_mut().find(|e| e.id == fk.id) {
                    *existing = fk;
                } else {
                    merged.push(fk);
                }
            }
            sources.extend(kb.sources);
        }
        Ok(Self { entries: merged, sources })
    }

    pub fn sources(&self) -> &[PathBuf] {
        &self.sources
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl KnowledgeProvider for YamlKnowledgeBase {
    fn all(&self) -> Vec<&FrameworkKnowledge> {
        self.entries.iter().collect()
    }

    fn by_id(&self, id: &str) -> Option<&FrameworkKnowledge> {
        self.entries.iter().find(|e| e.id == id)
    }

    fn for_language(&self, language: &Language) -> Vec<&FrameworkKnowledge> {
        self.entries.iter().filter(|e| e.language == *language).collect()
    }
}

/// YAML → [`FrameworkKnowledge`]。
///
/// 先解析成 `serde_yaml::Value`，再转成 `serde_json::Value` 后反序列化：
/// 因为 `serde_yaml` 0.9 对**外部标签枚举**（`- Annotate:` / `target: matched` /
/// `- strip_prefix: [...]`）只接受 YAML 标签（`!Annotate`）写法，而 FKB 需要
/// 更接近自然 YAML 的映射写法。
pub fn deserialize_knowledge(text: &str) -> Result<FrameworkKnowledge> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(text).map_err(DomainError::infra)?;
    let json = serde_json::to_value(yaml).map_err(DomainError::infra)?;
    serde_json::from_value(json).map_err(DomainError::infra)
}

/// 跨语言核心能力定义（**带洞的 `Rule` 模板 + 读/写边种类 + 通用词根**，识别机制，**不属于任何 FKB**）。
///
/// 与 `DEFAULT_ANNOTATIONS_YAML`（P6 注解识别器）完全同构，且现在**完全落在 YAML 里**：
/// 每个能力 = 一条带 `{{callees}}` / `{{link}}` / `{{id}}` 占位符的 `Rule` 模板（键 `template`）
/// + 数据字段 `read_link` / `write_link` / `confidence` / `universal_types` / `universal_read` /
/// `universal_write`。装载器把「类型 × 方法 × 匹配模式」展开成调用匹配串填入 `{{callees}}`、
/// 读 / 写边种类填入 `{{link}}`，经 `render_template` 摊平成具体 `Rule`。于是「能力识别」与
/// 「注解识别」「手写 rules」共用同一套 `Selector` 匹配语言——内核只描述「合成什么节点 / 怎么取
/// 身份 / 边方向 / 读写出什么边」，具体库名（如 `Predis\Client`）由每语言 FKB 在
/// `capability_interfaces` 声明（接口实现）。**不再有任何 Rust 硬编码的能力数据。**
const DEFAULT_CAPABILITIES_YAML: &str = r#"
cache:
  template:
    id: "{{id}}"
    phase: Synthesize
    selector:
      kind: call
      callee: "{{callees}}"
    binding:
      - Synthesize:
          node: ExternalSystem
          subtype: Cache
          identity:
            kind: Named
            value: { arg: 0, require_literal: true }
            value_fallback: { literal: "Cache" }
          fields:
            - name: key
              value: { arg: 0, require_literal: true }
            - name: side
              value: { literal: "backend" }
          link: { kind: "{{link}}", direction: incoming, confidence: 0.85 }
          confidence: 0.85
  read_link: ReadsCache
  write_link: WritesCache
  confidence: 0.85
  universal_types: ["Cache","Redis","*Cache","*CacheService","*CacheManager","*CacheStore","*CacheClient","*RedisService","*RedisClient","*RedisStore","*SessionStore"]
  universal_read: [get,has,many,remember,fetch,load,read]
  universal_write: [set,put,save,add,forever,forget,delete,clear,remove]
config:
  template:
    id: "{{id}}"
    phase: Synthesize
    selector:
      kind: call
      callee: "{{callees}}"
    binding:
      - Synthesize:
          node: ConfigKey
          identity:
            kind: Named
            value: { arg: 0, require_literal: true }
          fields:
            - name: side
              value: { literal: "backend" }
          link: { kind: "{{link}}", direction: incoming, confidence: 0.9 }
          confidence: 0.9
  read_link: ReadsConfig
  write_link: ReadsConfig
  confidence: 0.9
"#;

/// 跨语言核心**注解识别器**（P6 标注的标准 / 接口，**不属于任何 FKB**）。
///
/// 每个识别器是一条带 `{{key}}` 占位符的 `Rule` 模板；各栈在 `annotation_interfaces`
/// 里引用它并填 `params`，装载器把占位符替换为参数后摊平成具体 `Annotate` 规则。
///
/// 这与 `DEFAULT_CAPABILITIES_YAML`（能力识别）完全同构：
/// * 识别器只写一次在内核，产出的 `kind` 全部来自内核标准词汇
///   （[`gt_domain::model::kinds::AnnotationKind`]）—— 栈**无法自己发明**注解种类；
/// * 各栈只声明「本栈数据长什么样」（PII 列名 / 配置源表 / 重要度阈值 / 鉴权能力名单），
///   即「接口的实现」。通用参数（如 `phone`/`email` 这类 PII 列名）已是跨应用约定，
///   但源表名 / 业务域列名仍是栈特有，故留作参数。
const DEFAULT_ANNOTATIONS_YAML: &str = r#"
pii:
  id: "{{id}}-pii"
  phase: AnnotatePost
  selector:
    kind: node
    node_kind: Table
    where:
      - columns_match: { table: "{{table}}", names: "{{names}}" }
  binding:
    - Annotate:
        phase: Post
        channel: FkbMark
        target: matched
        annotations:
          - kind: pii
            subkind: { literal: "{{subkind}}" }
            confidence: 0.9
        merge: MaxByKind
table_criticality:
  id: "{{id}}-criticality"
  phase: AnnotatePost
  selector:
    kind: node
    node_kind: Table
  binding:
    - Annotate:
        phase: Post
        channel: FkbMark
        target: matched
        annotations:
          - kind: data.criticality
            subkind: { from_fan_in: { thresholds: "{{thresholds}}" } }
            confidence: 0.85
        merge: MaxByKind
config_metadata:
  id: "{{id}}-config"
  phase: AnnotatePost
  selector:
    kind: node
    node_kind: ConfigKey
  binding:
    - Annotate:
        phase: Post
        channel: FkbMark
        target: matched
        annotations:
          - kind: config.storage
            subkind: { from_symbol_table: { table: "{{table}}", field: storage } }
            confidence: 1.0
          - kind: config.mutability
            subkind: { from_symbol_table: { table: "{{table}}", field: mutable } }
            confidence: 1.0
        merge: MaxByKind
public_endpoint:
  id: "{{id}}-public"
  phase: AnnotatePost
  selector:
    kind: node
    node_kind: HttpContract
    where:
      - none_of_capability: "{{caps}}"
  binding:
    - Annotate:
        phase: Post
        channel: FkbMark
        target: matched
        annotations:
          - kind: auth.public
            confidence: 0.9
        merge: MaxByKind
i18n_coverage:
  id: "{{id}}-i18n"
  phase: AnnotatePost
  selector:
    kind: node
    node_kind: I18nKey
  binding:
    - Annotate:
        phase: Post
        channel: FkbMark
        target: matched
        annotations:
          - kind: i18n.missing_locale
            subkind: { computed: missing_locales }
            confidence: 1.0
        merge: MaxByKind
"#;

/// 从能力 YAML 定义（`HashMap<String, Value>`，每条含 `template` + 数据字段）读取辅助项。
fn cap_read_link(t: &serde_json::Value) -> String {
    t.get("read_link")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}
fn cap_write_link(t: &serde_json::Value) -> String {
    t.get("write_link")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}
fn cap_confidence(t: &serde_json::Value) -> f32 {
    t.get("confidence")
        .and_then(|v| v.as_f64())
        .map(|f| f as f32)
        .unwrap_or(0.9)
}
fn cap_strs(t: &serde_json::Value, key: &str) -> Vec<String> {
    t.get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// 跨语言核心能力模板（内置默认，目前不接受目录扩展，与 `load_annotation_templates` 同构）。
///
/// 返回 `HashMap<String, serde_json::Value>`：每个能力 = 一条带洞 `Rule`（`template` 键）
/// + 数据字段（`read_link` / `write_link` / `confidence` / `universal_types` /
/// `universal_read` / `universal_write`）。**完全落在 YAML，无 Rust 硬编码能力数据**，
/// 由 `expand_capability_interfaces` 经 `render_template` 摊平成具体 `Rule`。
fn load_capability_rule_templates() -> HashMap<String, serde_json::Value> {
    let yaml: serde_yaml::Value = match serde_yaml::from_str(DEFAULT_CAPABILITIES_YAML) {
        Ok(y) => y,
        Err(e) => {
            warn!("内置能力模板解析失败: {e}");
            return HashMap::new();
        }
    };
    let json = match serde_json::to_value(&yaml) {
        Ok(j) => j,
        Err(e) => {
            warn!("内置能力模板转换失败: {e}");
            return HashMap::new();
        }
    };
    let map: HashMap<String, serde_json::Value> = match serde_json::from_value(json) {
        Ok(m) => m,
        Err(e) => {
            warn!("内置能力模板结构错误: {e}");
            return HashMap::new();
        }
    };
    // 注册能力引入的语义边种类（如 ReadsCache / WritesCache / ReadsConfig）。
    let kinds: Vec<String> = map
        .values()
        .flat_map(|t| [cap_read_link(t), cap_write_link(t)])
        .filter(|k| !k.is_empty())
        .collect();
    register_edge_kinds(kinds.iter().cloned(), std::iter::empty());
    map
}

/// 把一份 FKB 里的 `capability_interfaces` 声明摊平成 `Rule`（匹配各库类型的合成规则）。
///
/// 与 `expand_annotation_interfaces` 完全同构：内核提供**带 `{{callees}}` / `{{link}}`
/// 占位符的 `Rule` 模板**（见 `DEFAULT_CAPABILITIES_YAML` 的 `template` 键），本函数把
/// 「类型 × 方法 × 匹配模式」展开成调用匹配串填入 `{{callees}}`、读 / 写边种类填入 `{{link}}`，
/// 经 `render_template` 摊平成具体 `Rule`，引擎原样消费（共用同一套 `Selector` 匹配语言）。
/// 读 / 写边种类与跨语言通用词根都来自能力 YAML 定义本身（无 Rust 硬编码）。
///
/// 合并两层约定后摊平：
/// * **Group B（每语言 / 库特有）**：来自本 FKB `capability_interfaces` 的 `types` / `read` /
///   `write`，按 `match_mode`（默认 `by_type`）展开，覆盖词根不合通用约定的具体库。
/// * **Group A（跨语言通用）**：来自能力 YAML 的 `universal_types` / `universal_read` /
///   `universal_write`（封装类命名约定，弱证据兜底），按 `by_name` 模式对所有引用该能力的
///   FKB 各生成一次。
fn expand_capability_interfaces(
    fk: &mut FrameworkKnowledge,
    templates: &HashMap<String, serde_json::Value>,
) {
    if fk.capability_interfaces.is_empty() {
        return;
    }
    let mut generated: Vec<Rule> = Vec::new();
    // Group A 每种能力只生成一次（跨语言通用约定按能力名去重）。
    let mut universal_done: HashSet<String> = HashSet::new();
    for ci in &fk.capability_interfaces {
        let Some(tmpl) = templates.get(&ci.capability) else {
            warn!("FKB {} 引用了未知能力 `{}`，已跳过", fk.id, ci.capability);
            continue;
        };
        let Some(rule_tmpl) = tmpl.get("template") else {
            warn!("能力 `{}` 模板缺少 `template` 字段，已跳过", ci.capability);
            continue;
        };
        let read_link = cap_read_link(tmpl);
        let write_link = cap_write_link(tmpl);
        let conf = ci.confidence.unwrap_or(cap_confidence(tmpl));
        let utypes = cap_strs(tmpl, "universal_types");
        let uread = cap_strs(tmpl, "universal_read");
        let uwrite = cap_strs(tmpl, "universal_write");
        let mode = ci.match_mode;
        let mode_tag = match mode {
            MatchMode::ByType => "by_type",
            MatchMode::ByName => "by_name",
        };
        // Group B：每语言 / 库特有类型（按 ci.match_mode）。
        if !ci.read.is_empty() {
            generated.push(render_cap_rule(
                fk.id.as_str(),
                &ci.capability,
                rule_tmpl,
                &gen_callees(&ci.types, &ci.read, mode),
                &read_link,
                "read",
                mode_tag,
                conf,
            ));
        }
        if !ci.write.is_empty() {
            generated.push(render_cap_rule(
                fk.id.as_str(),
                &ci.capability,
                rule_tmpl,
                &gen_callees(&ci.types, &ci.write, mode),
                &write_link,
                "write",
                mode_tag,
                conf,
            ));
        }
        // Group A：跨语言通用命名约定（by_name，名称弱证据），仅当声明了通用 types。
        // 用 `-universal` 后缀区分于同能力的每语言声明，避免规则 id 重复。
        if universal_done.insert(ci.capability.clone()) && !utypes.is_empty() {
            if !uread.is_empty() {
                let mut rule = render_cap_rule(
                    fk.id.as_str(),
                    &ci.capability,
                    rule_tmpl,
                    &gen_callees(&utypes, &uread, MatchMode::ByName),
                    &read_link,
                    "read",
                    "universal",
                    conf,
                );
                rule.id = format!("{}-cap-{}-read-universal", fk.id, ci.capability);
                generated.push(rule);
            }
            if !uwrite.is_empty() {
                let mut rule = render_cap_rule(
                    fk.id.as_str(),
                    &ci.capability,
                    rule_tmpl,
                    &gen_callees(&utypes, &uwrite, MatchMode::ByName),
                    &write_link,
                    "write",
                    "universal",
                    conf,
                );
                rule.id = format!("{}-cap-{}-write-universal", fk.id, ci.capability);
                generated.push(rule);
            }
        }
    }
    fk.rules.append(&mut generated);
}

/// 把「类型 × 方法」按匹配模式展开成 `callee` 匹配串（`Type::method` / `*Suffix::method` /
/// `*Type::method`，以 `|` 连接）。
///
/// * `by_type`：对每个类型摊出 `Type::method` 与 `*Suffix::method`（接收者以 `\Suffix`
///   结尾即命中）—— 精确类型 + 尾部片段兜底。
/// * `by_name`：对每个类型摊出 `*Type::method`（按封装类命名约定）。
fn gen_callees(types: &[String], methods: &[String], mode: MatchMode) -> String {
    let mut callees: Vec<String> = Vec::new();
    match mode {
        MatchMode::ByType => {
            for t in types {
                // 尾部片段用于 `*Suffix::method` 通配（接收者以 `\Suffix` 结尾即命中）。
                let last = t
                    .rsplit(|c| matches!(c, '\\' | '.' | '/' | ':'))
                    .next()
                    .unwrap_or(t.as_str());
                for m in methods {
                    callees.push(format!("{t}::{m}"));
                    callees.push(format!("*{}::{m}", last));
                }
            }
        }
        MatchMode::ByName => {
            for t in types {
                // 按封装类命名约定：类后缀 + 方法名（如 `*CacheService::get`）。
                let base = t.trim_start_matches('*');
                for m in methods {
                    callees.push(format!("*{}::{m}", base));
                }
            }
        }
    }
    callees.join("|")
}

/// 用 `{{callees}}` / `{{link}}` / `{{id}}` 占位符渲染一条能力 `Rule` 模板，并应用置信度覆盖。
fn render_cap_rule(
    fk_id: &str,
    cap: &str,
    tmpl: &serde_json::Value,
    callees: &str,
    link: &str,
    kind: &str,
    mode_tag: &str,
    conf: f32,
) -> Rule {
    let id = format!("{fk_id}-cap-{cap}-{kind}-{mode_tag}");
    let mut params: HashMap<String, serde_json::Value> = HashMap::new();
    params.insert("id".to_string(), serde_json::json!(id));
    params.insert("callees".to_string(), serde_json::json!(callees));
    params.insert("link".to_string(), serde_json::json!(link));
    let rendered = render_template(tmpl, &params);
    let mut rule: Rule = match serde_json::from_value(rendered) {
        Ok(r) => r,
        Err(e) => {
            warn!("能力模板 `{cap}` 渲染失败: {e}");
            // 兜底空规则（不应发生），避免整个装载中断。
            return Rule {
                id,
                phase: Phase("Synthesize".into()),
                selector: Selector::Call { callee: None, r#where: Vec::new() },
                binding: Vec::new(),
                confidence: conf,
            };
        }
    };
    // 应用置信度覆盖（同时覆盖 Rule 与 Synthesize 动作 / 边）。
    rule.confidence = conf;
    for act in &mut rule.binding {
        if let Action::Synthesize(s) = act {
            s.confidence = conf;
            if let Some(l) = &mut s.link {
                l.confidence = Some(conf);
            }
        }
    }
    rule
}

/// 载入核心注解识别器模板（内置默认，目前不接受目录扩展）。
fn load_annotation_templates() -> HashMap<String, serde_json::Value> {
    let yaml: serde_yaml::Value = match serde_yaml::from_str(DEFAULT_ANNOTATIONS_YAML) {
        Ok(y) => y,
        Err(e) => {
            warn!("内置注解识别器模板解析失败: {e}");
            return HashMap::new();
        }
    };
    let json = match serde_json::to_value(&yaml) {
        Ok(j) => j,
        Err(e) => {
            warn!("内置注解识别器模板转换失败: {e}");
            return HashMap::new();
        }
    };
    match serde_json::from_value(json) {
        Ok(m) => m,
        Err(e) => {
            warn!("内置注解识别器模板结构错误: {e}");
            HashMap::new()
        }
    }
}

/// 把模板 JSON 里的 `{{key}}` 字符串占位符递归替换成 `params` 中的值。
///
/// 任何等于 `{{key}}` 的 JSON 字符串整体被替换（包括列表 / 对象参数）：
/// 例如 `names: "{{names}}"` 替换成 `[...]`、`thresholds: "{{thresholds}}"` 替换成对象。
fn render_template(
    tmpl: &serde_json::Value,
    params: &HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    match tmpl {
        serde_json::Value::String(s) => {
            let inner = s.trim();
            if let Some(key) = inner.strip_prefix("{{").and_then(|r| r.strip_suffix("}}")) {
                if let Some(v) = params.get(key) {
                    return v.clone();
                }
            }
            serde_json::Value::String(s.clone())
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(|x| render_template(x, params)).collect())
        }
        serde_json::Value::Object(o) => serde_json::Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), render_template(v, params)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// 把一份 FKB 里的 `annotation_interfaces` 声明摊平成 `Rule`（P6 标注规则）。
///
/// 查内核标准识别器 `annotation_templates`，把模板的 `{{key}}` 占位符替换成各栈
/// `params` 后，反序列化为具体 `Rule` 追加进 `fk.rules`。纯数据变换，引擎原样消费。
fn expand_annotation_interfaces(
    fk: &mut FrameworkKnowledge,
    templates: &HashMap<String, serde_json::Value>,
) {
    if fk.annotation_interfaces.is_empty() {
        return;
    }
    let mut generated: Vec<Rule> = Vec::new();
    for (i, ai) in fk.annotation_interfaces.iter().enumerate() {
        let Some(tmpl) = templates.get(&ai.annotation) else {
            warn!("FKB {} 引用了未知注解识别器 `{}`，已跳过", fk.id, ai.annotation);
            continue;
        };
        // 合并参数：各栈 params ∪ 内置 id；interface.confidence 覆盖模板默认置信度。
        let mut params: HashMap<String, serde_json::Value> = ai.params.clone();
        params.insert("id".to_string(), serde_json::json!(fk.id));
        let rendered = render_template(tmpl, &params);
        let mut rule: Rule = match serde_json::from_value(rendered) {
            Ok(r) => r,
            Err(e) => {
                warn!("FKB {} 注解识别器 `{}` 渲染失败: {e}", fk.id, ai.annotation);
                continue;
            }
        };
        rule.id = format!("{}-ann-{}-{}", fk.id, ai.annotation, i);
        if let Some(c) = ai.confidence {
            rule.confidence = c;
            for act in &mut rule.binding {
                if let Action::Annotate(a) = act {
                    for ann in &mut a.annotations {
                        ann.confidence = c;
                    }
                }
            }
        }
        generated.push(rule);
    }
    fk.rules.append(&mut generated);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::fkb::CapabilityInterface;

    fn cap_templates() -> HashMap<String, serde_json::Value> {
        load_capability_rule_templates()
    }

    #[test]
    fn expands_cache_interface_into_rules() {
        // 合并两层：模板跨语言通用约定（by_name，`*CacheService::get` 等）
        // + 每语言精确类型（by_type，`Predis\Client::get`）。
        let tmpl = cap_templates();
        assert!(tmpl.contains_key("cache"));
        let mut fk = FrameworkKnowledge {
            id: "php-test".into(),
            language: Language("php".to_string()),
            capability_interfaces: vec![CapabilityInterface {
                capability: "cache".into(),
                types: vec!["Predis\\Client".into()],
                read: vec!["get".into()],
                write: vec!["set".into()],
                confidence: None,
                match_mode: MatchMode::ByType,
            }],
            ..Default::default()
        };
        expand_capability_interfaces(&mut fk, &tmpl);
        // 4 条：Group B（Predis 后端）读/写 + Group A（模板通用）读/写。
        assert_eq!(fk.rules.len(), 4);
        let find = |suffix: &str| {
            fk.rules
                .iter()
                .find(|r| r.id.ends_with(suffix))
                .unwrap_or_else(|| panic!("缺少以 {suffix} 结尾的规则"))
        };
        // Group B：每语言精确类型（by_type）。
        match &find("-cap-cache-read-by_type").selector {
            Selector::Call { callee, .. } => assert_eq!(
                callee.as_deref(),
                Some("Predis\\Client::get|*Client::get")
            ),
            _ => panic!("expected Call selector"),
        }
        // Group A：跨语言通用约定（by_name），不含具体库名。
        let universal_read = match &find("-cap-cache-read-universal").selector {
            Selector::Call { callee, .. } => callee.as_deref().unwrap(),
            _ => panic!("expected Call selector"),
        };
        assert!(
            universal_read.contains("*CacheService::get"),
            "通用 *CacheService::get 缺失: {universal_read}"
        );
        assert!(universal_read.contains("*Cache::get"), "通用 *Cache::get 缺失: {universal_read}");
        assert!(universal_read.contains("*Redis::get"), "通用 *Redis::get 缺失: {universal_read}");
        assert!(
            !universal_read.contains("Predis"),
            "通用约定不应含具体库名: {universal_read}"
        );
        // 合成节点形状正确（来自带洞的 `Rule` 模板）。
        match &find("-cap-cache-read-by_type").binding[0] {
            Action::Synthesize(s) => {
                assert_eq!(s.node.as_str(), "ExternalSystem");
                assert_eq!(s.subtype.as_deref(), Some("Cache"));
                assert_eq!(s.link.as_ref().unwrap().kind.as_str(), "ReadsCache");
            }
            _ => panic!("expected Synthesize"),
        }
    }

    #[test]
    fn unknown_capability_is_skipped() {
        let tmpl = cap_templates();
        let mut fk = FrameworkKnowledge {
            id: "php-test".into(),
            language: Language("php".to_string()),
            capability_interfaces: vec![CapabilityInterface {
                capability: "nope".into(),
                types: vec!["X".into()],
                read: vec!["y".into()],
                write: vec![],
                confidence: None,
                ..Default::default()
            }],
            ..Default::default()
        };
        expand_capability_interfaces(&mut fk, &tmpl);
        assert!(fk.rules.is_empty());
    }

    #[test]
    fn wrapper_mode_matches_by_class_suffix() {
        // by_name 模式按封装类命名约定匹配（`*CacheAdapter::get`）：用于框架门面迷惑分发、
        // P7 看不到内部真实调用的场景。模板还会附上跨语言通用约定（`*CacheService::get` 等）。
        let tmpl = cap_templates();
        let mut fk = FrameworkKnowledge {
            id: "php-test".into(),
            language: Language("php".to_string()),
            capability_interfaces: vec![CapabilityInterface {
                capability: "cache".into(),
                types: vec!["*CacheAdapter".into()],
                read: vec!["get".into()],
                write: vec![],
                confidence: None,
                match_mode: MatchMode::ByName,
            }],
            ..Default::default()
        };
        expand_capability_interfaces(&mut fk, &tmpl);
        let adapter = fk.rules.iter().any(|r| match &r.selector {
            Selector::Call { callee, .. } => {
                callee.as_deref().unwrap_or("").contains("*CacheAdapter::get")
            }
            _ => false,
        });
        assert!(adapter, "应生成 *CacheAdapter::get 规则");
        let universal = fk.rules.iter().any(|r| match &r.selector {
            Selector::Call { callee, .. } => {
                callee.as_deref().unwrap_or("").contains("*CacheService::get")
            }
            _ => false,
        });
        assert!(universal, "模板通用 *CacheService::get 应存在");
    }

    #[test]
    fn expands_annotation_interfaces_into_rules() {
        // 内核标准识别器（pii / table_criticality …）+ 每栈参数，摊平成具体 Annotate 规则。
        // 「哪些注解种类存在、怎么识别」是内核标准，栈只填本栈数据（列名 / 源表 / 阈值）。
        use gt_domain::model::fkb::{AnnotationInterface, Predicate};
        let ann = load_annotation_templates();
        assert!(ann.contains_key("pii"));
        assert!(ann.contains_key("table_criticality"));
        assert!(ann.contains_key("config_metadata"));
        assert!(ann.contains_key("public_endpoint"));
        assert!(ann.contains_key("i18n_coverage"));
        let mut fk = FrameworkKnowledge {
            id: "php-crmeb".into(),
            language: Language("php".to_string()),
            annotation_interfaces: vec![
                AnnotationInterface {
                    annotation: "pii".into(),
                    params: serde_json::json!({
                        "table": "schema",
                        "names": ["phone", "mobile", "contact_phone", "idcard", "email"],
                        "subkind": "phone"
                    })
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                    confidence: None,
                },
                AnnotationInterface {
                    annotation: "table_criticality".into(),
                    params: serde_json::json!({ "thresholds": { "high": 50, "medium": 10 } })
                        .as_object()
                        .unwrap()
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    confidence: None,
                },
            ],
            ..Default::default()
        };
        expand_annotation_interfaces(&mut fk, &ann);
        // 2 条：pii + table_criticality。
        assert_eq!(fk.rules.len(), 2);
        // pii 规则命中 schema 列名，产出内核标准种类 `pii`。
        let pii = fk
            .rules
            .iter()
            .find(|r| r.id == "php-crmeb-ann-pii-0")
            .expect("pii 规则应存在");
        match &pii.selector {
            Selector::Node { node_kind, r#where } => {
                assert!(node_kind.is_some());
                assert!(r#where.iter().any(|p| matches!(
                    p,
                    Predicate::ColumnsMatch { table, .. } if table == "schema"
                )));
            }
            _ => panic!("expected Node selector"),
        }
        match &pii.binding[0] {
            Action::Annotate(a) => {
                assert_eq!(a.annotations[0].kind, "pii");
                assert_eq!(a.annotations[0].confidence, 0.9);
            }
            _ => panic!("expected Annotate"),
        }
        // table_criticality 规则产出内核标准种类 `data.criticality`，且 id 不冲突。
        let crit = fk
            .rules
            .iter()
            .find(|r| r.id == "php-crmeb-ann-table_criticality-1")
            .expect("criticality 规则应存在");
        match &crit.binding[0] {
            Action::Annotate(a) => assert_eq!(a.annotations[0].kind, "data.criticality"),
            _ => panic!("expected Annotate"),
        }
    }

    #[test]
    fn unknown_annotation_is_skipped() {
        use gt_domain::model::fkb::AnnotationInterface;
        let ann = load_annotation_templates();
        let mut fk = FrameworkKnowledge {
            id: "php-x".into(),
            language: Language("php".to_string()),
            annotation_interfaces: vec![AnnotationInterface {
                annotation: "nope".into(),
                params: HashMap::new(),
                confidence: None,
            }],
            ..Default::default()
        };
        expand_annotation_interfaces(&mut fk, &ann);
        assert!(fk.rules.is_empty());
    }

    #[test]
    fn php_common_migrated_cache_rules_present() {
        // 规则迁移：common.yaml 用 `capability_interfaces` 只声明 PHP 特有类型（Predis\Client），
        // 跨语言通用约定（`Cache` / `Redis` / `*CacheService` …）来自核心能力模板，
        // 二者取并集摊平成合成规则。ConfigKey 手写规则仍在 `rules:` 下。
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fkb/php");
        let kb = YamlKnowledgeBase::load_dir(&root).expect("fkb/php 可加载");
        let fk = kb.by_id("php-common").expect("php-common 应成功加载");
        // ConfigKey 手写规则在 rules: 下。
        assert!(
            fk.rules.iter().any(|r| r.id == "php-common-config"),
            "ConfigKey 规则应保留在 rules: 下"
        );
        let joined: String = fk
            .rules
            .iter()
            .filter_map(|r| match &r.selector {
                Selector::Call { callee: Some(c), .. } => Some(c.clone()),
                _ => None,
            })
            .collect();
        // 跨语言通用约定（来自模板，Wrapper）：覆盖后端词根 + 封装类约定。
        assert!(
            joined.contains("*CacheService::get"),
            "模板通用 *CacheService::get 应存在: {joined}"
        );
        assert!(joined.contains("*Cache::get"), "模板通用 *Cache::get 应存在: {joined}");
        assert!(joined.contains("*Redis::get"), "模板通用 *Redis::get 应存在: {joined}");
        // 每语言特有类型（来自 common.yaml，Backend）：精确声明不合通用约定的库。
        assert!(
            joined.contains("Predis\\Client::get"),
            "PHP 特有 Predis\\Client::get 应存在: {joined}"
        );
        assert!(
            joined.contains("*Client::get"),
            "PHP 特有 *Client::get 应存在: {joined}"
        );
    }
}
