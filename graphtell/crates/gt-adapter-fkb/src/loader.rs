//! FKB loading: read framework knowledge from a YAML directory.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::fkb::{Action, MatchMode, Rule, Selector};
use gt_domain::model::kinds::{register_edge_kinds, Phase};
use gt_domain::model::{FrameworkKnowledge, Language};
use gt_domain::port::KnowledgeProvider;
use tracing::{info, warn};

/// A file-directory-based FKB provider.
///
/// Directory layout: `fkb/<any subdirectory>/*.yaml`, each YAML describing one framework.
pub struct YamlKnowledgeBase {
    entries: Vec<FrameworkKnowledge>,
    sources: Vec<PathBuf>,
}

impl YamlKnowledgeBase {
    /// Recursively load every `*.yaml` / `*.yml` under the directory.
    pub fn load_dir(root: &Path) -> Result<Self> {
        if !root.exists() {
            warn!("FKB directory does not exist: {}", root.display());
            return Ok(Self { entries: Vec::new(), sources: Vec::new() });
        }
        // The cross-language core capability template (a `Rule` template with holes + data fields; the
        // recognition mechanism is not part of FKB), isomorphic to `DEFAULT_ANNOTATIONS_YAML` (the annotation
        // recognizer): the kernel supplies a parameterised `Rule` template, and each language's FKB only declares
        // `capability_interfaces`' `types` / `read` / `write`.
        let cap_templates = load_capability_rule_templates();
        // Cross-language core annotation recognizers (the P6 annotation standard, not part of FKB); see `DEFAULT_ANNOTATIONS_YAML`.
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
                        info!("loading FKB: {} ({})", fk.id, path.display());
                        // **Semantic node kinds** declared by FKB are registered here: adding a semantic node
                        // (a front-end `Store`, a page `Page`…) needs no change to the kernel list.
                        if !fk.semantic_kinds.is_empty() {
                            gt_domain::model::kinds::register_semantic_kinds(
                                fk.semantic_kinds.iter().cloned(),
                            );
                        }
                        // **Semantic / bridge edge kinds** declared by FKB are registered here: adding an edge kind
                        // (such as some framework's `SendsWebhook`) needs no change to the `SEMANTIC` / `BRIDGE`
                        // lists in `kinds.rs` — isomorphic to the node registration.
                        if !fk.semantic_edge_kinds.is_empty()
                            || !fk.bridge_edge_kinds.is_empty()
                        {
                            gt_domain::model::kinds::register_edge_kinds(
                                fk.semantic_edge_kinds.iter().cloned(),
                                fk.bridge_edge_kinds.iter().cloned(),
                            );
                        }
                        // **Business-specific annotation kinds** declared by FKB are registered here: adding a
                        // business annotation semantics needs no kernel change (isomorphic to `semantic_kinds` /
                        // `semantic_edge_kinds`).
                        // Kernel-standard kinds (pii / data.criticality / auth.public …) are produced by the
                        // `annotation_templates` recognizer and are not in this list.
                        if !fk.annotation_kinds.is_empty() {
                            gt_domain::model::kinds::register_annotation_kinds(
                                fk.annotation_kinds.iter().cloned(),
                            );
                        }
                        // Flatten the `capability_interfaces` declarations into synthetic rules (consumed as-is by the engine, zero changes).
                        expand_capability_interfaces(&mut fk, &cap_templates);
                        // Flatten the `annotation_interfaces` declarations into P6 annotation rules (kernel-standard recognizer + this stack's parameters).
                        expand_annotation_interfaces(&mut fk, &ann_templates);
                        entries.push(fk);
                        sources.push(path);
                    }
                    Err(e) => warn!("skipping a corrupt FKB {}: {e}", path.display()),
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
                "{}: missing the id field",
                path.display()
            )));
        }
        Ok(fk)
    }

    /// Deserialize from YAML text (for tests and single-file loading).
    pub fn from_str(text: &str) -> Result<FrameworkKnowledge> {
        deserialize_knowledge(text)
    }

    /// Load from several directories in order (later ones override entries with the same id).
    ///
    /// Each directory expands capability interfaces itself inside `load_dir`; this only merges, so nothing is expanded twice.
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

/// YAML -> [`FrameworkKnowledge`].
///
/// Parse into `serde_yaml::Value` first, then convert to `serde_json::Value` before deserializing:
/// because `serde_yaml` 0.9 only accepts the YAML-tag spelling (`!Annotate`) for **externally tagged enums**
/// (`- Annotate:` / `target: matched` / `- strip_prefix: [...]`), while FKB needs a mapping spelling that stays
/// closer to natural YAML.
pub fn deserialize_knowledge(text: &str) -> Result<FrameworkKnowledge> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(text).map_err(DomainError::infra)?;
    let json = serde_json::to_value(yaml).map_err(DomainError::infra)?;
    serde_json::from_value(json).map_err(DomainError::infra)
}

/// Cross-language core capability definitions (**a `Rule` template with holes + read/write edge kinds + common word roots**; the recognition mechanism, **not belonging to any FKB**).
///
/// Fully isomorphic to `DEFAULT_ANNOTATIONS_YAML` (the P6 annotation recognizer), and now **entirely expressed in YAML**:
/// each capability = one `Rule` template (key `template`) with `{{callees}}` / `{{link}}` / `{{id}}` placeholders
/// + the data fields `read_link` / `write_link` / `confidence` / `universal_types` / `universal_read` /
/// `universal_write`. The loader expands "type x method x match mode" into a call-match string filled into
/// `{{callees}}` and the read / write edge kinds into `{{link}}`, then flattens it into a concrete `Rule` via
/// `render_template`. So "capability recognition", "annotation recognition" and "hand-written rules" all share one
/// `Selector` match language — the kernel only describes "what node to synthesize / how to derive identity / edge
/// direction / which read-write edges to emit", while concrete library names (e.g. `Predis\Client`) are declared by
/// each language's FKB under `capability_interfaces` (the interface implementation).
/// **No capability data is hard-coded in Rust any more.**
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

/// Cross-language core **annotation recognizers** (the P6 annotation standard / interface, **not belonging to any FKB**).
///
/// Each recognizer is a `Rule` template with `{{key}}` placeholders; each stack references it in
/// `annotation_interfaces` and supplies `params`, and the loader substitutes the placeholders and flattens the
/// result into a concrete `Annotate` rule.
///
/// This is fully isomorphic to `DEFAULT_CAPABILITIES_YAML` (capability recognition):
/// * the recognizer is written once in the kernel, and every `kind` it produces comes from the kernel's standard
///   vocabulary ([`gt_domain::model::kinds::AnnotationKind`]) — a stack **cannot invent** annotation kinds;
/// * each stack only declares "what this stack's data looks like" (PII column names / config source tables /
///   criticality thresholds / auth capability lists), i.e. "the implementation of the interface". Generic parameters
///   (PII column names such as `phone` / `email`) are already a cross-application convention, but source table names
///   and business-domain column names remain stack-specific and are therefore kept as parameters.
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

/// Read auxiliary items from a capability YAML definition (`HashMap<String, Value>`, one `template` + data fields per entry).
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

/// Cross-language core capability templates (built-in defaults; directory extension is not accepted yet, isomorphic to `load_annotation_templates`).
///
/// Returns `HashMap<String, serde_json::Value>`: each capability = one `Rule` with holes (the `template` key)
/// + data fields (`read_link` / `write_link` / `confidence` / `universal_types` / `universal_read` /
/// `universal_write`). **Entirely in YAML with no Rust hard-coded capability data**, flattened into concrete `Rule`s
/// by `expand_capability_interfaces` via `render_template`.
fn load_capability_rule_templates() -> HashMap<String, serde_json::Value> {
    let yaml: serde_yaml::Value = match serde_yaml::from_str(DEFAULT_CAPABILITIES_YAML) {
        Ok(y) => y,
        Err(e) => {
            warn!("failed to parse the built-in capability template: {e}");
            return HashMap::new();
        }
    };
    let json = match serde_json::to_value(&yaml) {
        Ok(j) => j,
        Err(e) => {
            warn!("failed to convert the built-in capability template: {e}");
            return HashMap::new();
        }
    };
    let map: HashMap<String, serde_json::Value> = match serde_json::from_value(json) {
        Ok(m) => m,
        Err(e) => {
            warn!("the built-in capability template has a structural error: {e}");
            return HashMap::new();
        }
    };
    // Register the semantic edge kinds introduced by capabilities (e.g. ReadsCache / WritesCache / ReadsConfig).
    let kinds: Vec<String> = map
        .values()
        .flat_map(|t| [cap_read_link(t), cap_write_link(t)])
        .filter(|k| !k.is_empty())
        .collect();
    register_edge_kinds(kinds.iter().cloned(), std::iter::empty());
    map
}

/// Flatten a FKB's `capability_interfaces` declarations into `Rule`s (synthesis rules matching each library type).
///
/// Fully isomorphic to `expand_annotation_interfaces`: the kernel supplies a **`Rule` template with `{{callees}}` /
/// `{{link}}` placeholders** (see the `template` key of `DEFAULT_CAPABILITIES_YAML`); this function expands
/// "type x method x match mode" into a call-match string filled into `{{callees}}` and the read / write edge kinds
/// into `{{link}}`, then flattens it into a concrete `Rule` via `render_template`, consumed as-is by the engine
/// (sharing the same `Selector` match language). Both the read / write edge kinds and the cross-language common word
/// roots come from the capability YAML definition itself (no Rust hard-coding).
///
/// Two layers of convention are merged before flattening:
/// * **Group B (per language / library specific)**: `types` / `read` / `write` from this FKB's
///   `capability_interfaces`, expanded by `match_mode` (default `by_type`), covering concrete libraries whose word
///   root does not fit the common convention.
/// * **Group A (cross-language common)**: `universal_types` / `universal_read` / `universal_write` from the
///   capability YAML (wrapper-class naming conventions, a weak-evidence fallback), generated once per FKB that
///   references the capability, in `by_name` mode.
fn expand_capability_interfaces(
    fk: &mut FrameworkKnowledge,
    templates: &HashMap<String, serde_json::Value>,
) {
    if fk.capability_interfaces.is_empty() {
        return;
    }
    let mut generated: Vec<Rule> = Vec::new();
    // Group A is generated once per capability (the cross-language common convention is deduped by capability name).
    let mut universal_done: HashSet<String> = HashSet::new();
    for ci in &fk.capability_interfaces {
        let Some(tmpl) = templates.get(&ci.capability) else {
            warn!("FKB {} references an unknown capability `{}`, skipping", fk.id, ci.capability);
            continue;
        };
        let Some(rule_tmpl) = tmpl.get("template") else {
            warn!("the template for capability `{}` lacks a `template` field, skipping", ci.capability);
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
        // Group B: per-language / library-specific types (by ci.match_mode).
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
        // Group A: cross-language common naming conventions (by_name, weak name evidence), only when universal types are declared.
        // A `-universal` suffix distinguishes it from the per-language declaration of the same capability, avoiding duplicate rule ids.
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

/// Expand "type x method" into a `callee` match string by match mode (`Type::method` / `*Suffix::method` /
/// `*Type::method`, joined with `|`).
///
/// * `by_type`: for each type emit `Type::method` and `*Suffix::method` (a receiver ending in `\Suffix` matches)
///   — exact type plus a tail-fragment fallback.
/// * `by_name`: for each type emit `*Type::method` (by wrapper-class naming convention).
fn gen_callees(types: &[String], methods: &[String], mode: MatchMode) -> String {
    let mut callees: Vec<String> = Vec::new();
    match mode {
        MatchMode::ByType => {
            for t in types {
                // The tail fragment drives the `*Suffix::method` wildcard (a receiver ending in `\Suffix` matches).
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
                // By wrapper-class naming convention: class suffix + method name (e.g. `*CacheService::get`).
                let base = t.trim_start_matches('*');
                for m in methods {
                    callees.push(format!("*{}::{m}", base));
                }
            }
        }
    }
    callees.join("|")
}

/// Render a capability `Rule` template with `{{callees}}` / `{{link}}` / `{{id}}` placeholders and apply the confidence override.
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
            warn!("failed to render the capability template `{cap}`: {e}");
            // Fallback to an empty rule (should not happen) so the whole load does not abort.
            return Rule {
                id,
                phase: Phase("Synthesize".into()),
                selector: Selector::Call { callee: None, r#where: Vec::new() },
                binding: Vec::new(),
                confidence: conf,
            };
        }
    };
    // Apply the confidence override (covering both the Rule and the Synthesize action / edge).
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

/// Load the core annotation recognizer templates (built-in defaults; directory extension is not accepted yet).
fn load_annotation_templates() -> HashMap<String, serde_json::Value> {
    let yaml: serde_yaml::Value = match serde_yaml::from_str(DEFAULT_ANNOTATIONS_YAML) {
        Ok(y) => y,
        Err(e) => {
            warn!("failed to parse the built-in annotation recogniser template: {e}");
            return HashMap::new();
        }
    };
    let json = match serde_json::to_value(&yaml) {
        Ok(j) => j,
        Err(e) => {
            warn!("failed to convert the built-in annotation recogniser template: {e}");
            return HashMap::new();
        }
    };
    match serde_json::from_value(json) {
        Ok(m) => m,
        Err(e) => {
            warn!("the built-in annotation recogniser template has a structural error: {e}");
            HashMap::new()
        }
    }
}

/// Recursively replace `{{key}}` string placeholders in the template JSON with the values from `params`.
///
/// Any JSON string exactly equal to `{{key}}` is replaced wholesale (including list / object parameters):
/// e.g. `names: "{{names}}"` becomes `[...]`, and `thresholds: "{{thresholds}}"` becomes an object.
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

/// Flatten a FKB's `annotation_interfaces` declarations into `Rule`s (P6 annotation rules).
///
/// Look up the kernel-standard recognizer `annotation_templates`, replace the template's `{{key}}` placeholders with
/// each stack's `params`, then deserialize into concrete `Rule`s appended to `fk.rules`. A pure data transform,
/// consumed as-is by the engine.
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
            warn!("FKB {} references an unknown annotation recogniser `{}`, skipping", fk.id, ai.annotation);
            continue;
        };
        // Merge parameters: each stack's params ∪ the built-in id; interface.confidence overrides the template's default confidence.
        let mut params: HashMap<String, serde_json::Value> = ai.params.clone();
        params.insert("id".to_string(), serde_json::json!(fk.id));
        let rendered = render_template(tmpl, &params);
        let mut rule: Rule = match serde_json::from_value(rendered) {
            Ok(r) => r,
            Err(e) => {
                warn!("FKB {} annotation recogniser `{}` failed to render: {e}", fk.id, ai.annotation);
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
        // Merge the two layers: the template's cross-language common conventions (by_name, `*CacheService::get`, etc.)
        // + each language's precise types (by_type, `Predis\Client::get`).
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
        // 4 rules: Group B (Predis backend) read/write + Group A (template common) read/write.
        assert_eq!(fk.rules.len(), 4);
        let find = |suffix: &str| {
            fk.rules
                .iter()
                .find(|r| r.id.ends_with(suffix))
                .unwrap_or_else(|| panic!("missing a rule ending with {suffix}"))
        };
        // Group B: per-language precise types (by_type).
        match &find("-cap-cache-read-by_type").selector {
            Selector::Call { callee, .. } => assert_eq!(
                callee.as_deref(),
                Some("Predis\\Client::get|*Client::get")
            ),
            _ => panic!("expected Call selector"),
        }
        // Group A: cross-language common conventions (by_name), with no concrete library names.
        let universal_read = match &find("-cap-cache-read-universal").selector {
            Selector::Call { callee, .. } => callee.as_deref().unwrap(),
            _ => panic!("expected Call selector"),
        };
        assert!(
            universal_read.contains("*CacheService::get"),
            "the generic *CacheService::get is missing: {universal_read}"
        );
        assert!(universal_read.contains("*Cache::get"), "the generic *Cache::get is missing: {universal_read}");
        assert!(universal_read.contains("*Redis::get"), "the generic *Redis::get is missing: {universal_read}");
        assert!(
            !universal_read.contains("Predis"),
            "the generic convention must not contain a concrete library name: {universal_read}"
        );
        // The synthesized node has the right shape (it comes from the `Rule` template with holes).
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
        // by_name mode matches by wrapper-class naming convention (`*CacheAdapter::get`): for framework facades that
        // dispatch opaquely, where P7 cannot see the real internal call. The template also attaches the cross-language
        // common conventions (`*CacheService::get`, etc.).
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
        assert!(adapter, "expected a *CacheAdapter::get rule to be generated");
        let universal = fk.rules.iter().any(|r| match &r.selector {
            Selector::Call { callee, .. } => {
                callee.as_deref().unwrap_or("").contains("*CacheService::get")
            }
            _ => false,
        });
        assert!(universal, "the template generic *CacheService::get should exist");
    }

    #[test]
    fn expands_annotation_interfaces_into_rules() {
        // The kernel-standard recognizers (pii / table_criticality …) plus each stack's parameters are flattened into
        // concrete Annotate rules. "Which annotation kinds exist and how they are recognised" is a kernel standard;
        // a stack only fills in its own data (column names / source tables / thresholds).
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
        // 2 rules: pii + table_criticality.
        assert_eq!(fk.rules.len(), 2);
        // The pii rule matches schema column names and produces the kernel-standard kind `pii`.
        let pii = fk
            .rules
            .iter()
            .find(|r| r.id == "php-crmeb-ann-pii-0")
            .expect("the pii rule should exist");
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
        // The table_criticality rule produces the kernel-standard kind `data.criticality`, and its id does not clash.
        let crit = fk
            .rules
            .iter()
            .find(|r| r.id == "php-crmeb-ann-table_criticality-1")
            .expect("the criticality rule should exist");
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
        // Rule migration: common.yaml uses `capability_interfaces` to declare only PHP-specific types (Predis\Client);
        // the cross-language common conventions (`Cache` / `Redis` / `*CacheService` …) come from the core capability
        // template, and the two are unioned before being flattened into synthesis rules. The hand-written ConfigKey
        // rules still live under `rules:`.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fkb/php");
        let kb = YamlKnowledgeBase::load_dir(&root).expect("fkb/php should load");
        let fk = kb.by_id("php-common").expect("php-common should load successfully");
        // The hand-written ConfigKey rules live under rules:.
        assert!(
            fk.rules.iter().any(|r| r.id == "php-common-config"),
            "the ConfigKey rule should stay under rules:"
        );
        let joined: String = fk
            .rules
            .iter()
            .filter_map(|r| match &r.selector {
                Selector::Call { callee: Some(c), .. } => Some(c.clone()),
                _ => None,
            })
            .collect();
        // Cross-language common conventions (from the template, Wrapper): covers backend word roots + wrapper-class conventions.
        assert!(
            joined.contains("*CacheService::get"),
            "the template generic *CacheService::get should exist: {joined}"
        );
        assert!(joined.contains("*Cache::get"), "the template generic *Cache::get should exist: {joined}");
        assert!(joined.contains("*Redis::get"), "the template generic *Redis::get should exist: {joined}");
        // Per-language specific types (from common.yaml, Backend): libraries whose declaration does not fit the common convention.
        assert!(
            joined.contains("Predis\\Client::get"),
            "the PHP-specific Predis\\Client::get should exist: {joined}"
        );
        assert!(
            joined.contains("*Client::get"),
            "the PHP-specific *Client::get should exist: {joined}"
        );
    }
}
