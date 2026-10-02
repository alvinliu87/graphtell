//! FKB 装载：从 YAML 目录读取框架知识。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::fkb::{
    Action, CapabilityTemplate, Direction, LinkSpec, MatchMode, Rule, Selector,
    SynthesizeAction,
};
use gt_domain::model::kinds::{register_edge_kinds, EdgeKind, Phase};
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
        // 跨语言核心能力模板（识别机制，不属于 FKB），可按本目录可选的 `capabilities.yaml` 扩展。
        let templates = load_capability_templates(&[root.to_path_buf()]);
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
                        // 把 `capability_interfaces` 声明摊平成合成规则（引擎原样消费，零改动）。
                        expand_capability_interfaces(&mut fk, &templates);
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

/// 跨语言核心能力模板（识别机制，**不属于任何 FKB**）。
///
/// 描述「某能力如何落成图节点与边」。由各语言 / 各库的 `capability_interfaces`
/// 复用，识别逻辑只写一次。用户可在 `fkb/<dir>/capabilities.yaml` 扩展 / 覆盖
/// （新增一种语义节点类型，本质是扩图 schema，而非加框架知识）。
const DEFAULT_CAPABILITIES_YAML: &str = r#"
cache:
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
  read_link: ReadsCache
  write_link: WritesCache
  confidence: 0.85
config:
  node: ConfigKey
  identity:
    kind: Named
    value: { arg: 0, require_literal: true }
  fields:
    - name: side
      value: { literal: "backend" }
  read_link: ReadsConfig
  write_link: ReadsConfig
  confidence: 0.9
"#;

/// 载入核心能力模板：内置默认 + 各目录可选的 `capabilities.yaml` 扩展 / 覆盖。
fn load_capability_templates(dirs: &[PathBuf]) -> HashMap<String, CapabilityTemplate> {
    let mut map: HashMap<String, CapabilityTemplate> =
        serde_yaml::from_str(DEFAULT_CAPABILITIES_YAML)
            .map_err(|e| DomainError::InvalidKnowledge(format!("内置能力模板解析失败: {e}")))
            .unwrap_or_default();
    for dir in dirs {
        let path = dir.join("capabilities.yaml");
        if let Ok(text) = std::fs::read_to_string(&path) {
            match serde_yaml::from_str::<HashMap<String, CapabilityTemplate>>(&text) {
                Ok(extra) => {
                    for (k, v) in extra {
                        map.insert(k, v);
                    }
                }
                Err(e) => warn!("跳过损坏的 capabilities.yaml: {} ({e})", path.display()),
            }
        }
    }
    // 注册模板引入的语义边种类（如 ReadsCache / WritesCache / ReadsConfig）。
    let kinds: Vec<String> = map
        .values()
        .flat_map(|t| [t.read_link.0.clone(), t.write_link.0.clone()])
        .collect();
    register_edge_kinds(kinds.iter().cloned(), std::iter::empty());
    map
}

/// 把一份 FKB 里的 `capability_interfaces` 声明摊平成 `Rule`（匹配各库类型的合成规则）。
///
/// 纯数据变换：引擎（`engine.rs`）原样消费生成的 `Rule`，零改动。
fn expand_capability_interfaces(
    fk: &mut FrameworkKnowledge,
    templates: &HashMap<String, CapabilityTemplate>,
) {
    if fk.capability_interfaces.is_empty() {
        return;
    }
    let mut generated: Vec<Rule> = Vec::new();
    for ci in &fk.capability_interfaces {
        let Some(tmpl) = templates.get(&ci.capability) else {
            warn!("FKB {} 引用了未知能力 `{}`，已跳过", fk.id, ci.capability);
            continue;
        };
        let conf = ci.confidence.unwrap_or(tmpl.confidence);
        if !ci.read.is_empty() {
            generated.push(build_capability_rule(
                fk,
                tmpl,
                &ci.types,
                &ci.read,
                tmpl.read_link.clone(),
                conf,
                &ci.capability,
                "read",
                ci.match_mode,
            ));
        }
        if !ci.write.is_empty() {
            generated.push(build_capability_rule(
                fk,
                tmpl,
                &ci.types,
                &ci.write,
                tmpl.write_link.clone(),
                conf,
                &ci.capability,
                "write",
                ci.match_mode,
            ));
        }
    }
    fk.rules.append(&mut generated);
}

/// 生成一条能力合成规则。
///
/// * `Backend` 模式：对 `types × methods` 摊出 `Receiver::method|*Suffix::method`（按真实后端调用）。
/// * `Wrapper` 模式：对 `types` 摊出 `*Type::method`（按封装类命名约定，用于框架门面迷惑分发）。
fn build_capability_rule(
    fk: &FrameworkKnowledge,
    tmpl: &CapabilityTemplate,
    types: &[String],
    methods: &[String],
    link: EdgeKind,
    conf: f32,
    cap: &str,
    kind: &str,
    mode: MatchMode,
) -> Rule {
    let mut callees: Vec<String> = Vec::new();
    match mode {
        MatchMode::Backend => {
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
        MatchMode::Wrapper => {
            for t in types {
                // 按封装类命名约定：类后缀 + 方法名（如 `*CacheService::get`）。
                let base = t.trim_start_matches('*');
                for m in methods {
                    callees.push(format!("*{}::{m}", base));
                }
            }
        }
    }
    let mode_tag = match mode {
        MatchMode::Backend => "backend",
        MatchMode::Wrapper => "wrapper",
    };
    Rule {
        id: format!("{}-cap-{}-{}-{}", fk.id, cap, kind, mode_tag),
        phase: Phase(Phase::SYNTHESIZE.to_string()),
        selector: Selector::Call {
            callee: Some(callees.join("|")),
            r#where: Vec::new(),
        },
        binding: vec![Action::Synthesize(SynthesizeAction {
            node: tmpl.node.clone(),
            subtype: tmpl.subtype.clone(),
            identity: tmpl.identity.clone(),
            fields: tmpl.fields.clone(),
            link: Some(LinkSpec {
                kind: link,
                direction: Direction::Incoming,
                confidence: Some(conf),
                ..Default::default()
            }),
            confidence: conf,
            ..Default::default()
        })],
        confidence: conf,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::fkb::CapabilityInterface;

    fn templates() -> HashMap<String, CapabilityTemplate> {
        load_capability_templates(&[])
    }

    #[test]
    fn expands_cache_interface_into_rules() {
        let tmpl = templates();
        assert!(tmpl.contains_key("cache"));
        let mut fk = FrameworkKnowledge {
            id: "php-test".into(),
            language: Language("php".to_string()),
            capability_interfaces: vec![CapabilityInterface {
                capability: "cache".into(),
                types: vec!["Predis\\Client".into(), "Cache".into()],
                read: vec!["get".into()],
                write: vec!["set".into()],
                confidence: None,
                ..Default::default()
            }],
            ..Default::default()
        };
        expand_capability_interfaces(&mut fk, &tmpl);
        // 读 / 写各一条规则。
        assert_eq!(fk.rules.len(), 2);
        let read = &fk.rules[0];
        match &read.selector {
            Selector::Call { callee, .. } => assert_eq!(
                callee.as_deref(),
                Some("Predis\\Client::get|*Client::get|Cache::get|*Cache::get")
            ),
            _ => panic!("expected Call selector"),
        }
        match &read.binding[0] {
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
        let tmpl = templates();
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
        // Wrapper 模式按封装类命名约定匹配（`*CacheService::get`），用于框架门面迷惑分发、
        // P7 看不到内部真实调用的场景。
        let tmpl = templates();
        let mut fk = FrameworkKnowledge {
            id: "php-test".into(),
            language: Language("php".to_string()),
            capability_interfaces: vec![CapabilityInterface {
                capability: "cache".into(),
                types: vec!["*CacheService".into()],
                read: vec!["get".into()],
                write: vec![],
                confidence: None,
                match_mode: MatchMode::Wrapper,
            }],
            ..Default::default()
        };
        expand_capability_interfaces(&mut fk, &tmpl);
        assert_eq!(fk.rules.len(), 1);
        match &fk.rules[0].selector {
            Selector::Call { callee, .. } => {
                assert_eq!(callee.as_deref(), Some("*CacheService::get"))
            }
            _ => panic!("expected Call selector"),
        }
    }

    #[test]
    fn php_common_migrated_cache_rules_present() {
        // 规则迁移：common.yaml 用 `capability_interfaces` 声明缓存能力，装载后应摊平出
        // 与旧手写等价的 `Cache` 门面 / `Redis` / `*CacheService` 封装类合成规则，
        // 且 ConfigKey 手写规则仍在 `rules:` 下（未被误吞进 `capability_interfaces`）。
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fkb/php");
        let kb = YamlKnowledgeBase::load_dir(&root).expect("fkb/php 可加载");
        let fk = kb.by_id("php-common").expect("php-common 应成功加载");
        // ConfigKey 手写规则在 rules: 下。
        assert!(
            fk.rules.iter().any(|r| r.id == "php-common-config"),
            "ConfigKey 规则应保留在 rules: 下"
        );
        // 缓存能力摊平出的规则覆盖 Cache 门面、Redis、以及 *CacheService 封装类。
        let joined: String = fk
            .rules
            .iter()
            .filter_map(|r| match &r.selector {
                Selector::Call { callee: Some(c), .. } => Some(c.clone()),
                _ => None,
            })
            .collect();
        assert!(
            joined.contains("*CacheService::get"),
            "应生成 *CacheService::get 规则: {joined}"
        );
        assert!(joined.contains("Cache::get"), "应生成 Cache::get 规则");
        assert!(joined.contains("Redis::get"), "应生成 Redis::get 规则");
    }
}
