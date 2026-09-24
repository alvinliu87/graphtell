//! P3 Prepare：装载 FKB 与权威符号表。
//!
//! * 用 FKB 的 `detectors` 识别每个子工程用了哪些框架
//! * 用 `root_rules` 解析 `AppRoot`（如从 `composer.json` 的 `autoload.psr-4`）
//! * 用 `loaders` 装载权威源：`schema` / `config_keys` / `i18n` / `facade_map`
//!   / `container_bindings` / `event_listeners` / `route_list` / `nginx`

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gt_domain::error::Result;
use gt_domain::model::{
    Action, Detector, FactValue, FrameworkKnowledge, GuardAttachSpec, KnowledgeScope,
    Language, NormalizeStep, Phase, PickStrategy, RouteCallSpec, RouteGuardSpec, RouteMatchBy, Rule,
    SubProjectId, SynthesizedKind,
};
use gt_domain::port::{FileSystem, KnowledgeProvider, LanguageParser, ParserRegistry};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::context::PipelineContext;
use crate::engine::{capture_locale, path_matches};
use crate::normalize::strip_prefixes;
use crate::workspace::{CallRecord, RouteGroup, RouteGuard, RouteGuardScope};

/// 执行 Prepare。
pub fn run(
    ctx: &mut PipelineContext,
    kb: &dyn KnowledgeProvider,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
) -> Result<()> {
    let phase = Phase(Phase::PREPARE.to_string());
    let subs = ctx.sub_projects.clone();
    let project_root = ctx.project.root_path.clone();
    // 全部子工程识别到的框架并集 —— 决定哪些框架规则有资格进入全局规则集。
    let mut detected_frameworks: std::collections::HashSet<String> = std::collections::HashSet::new();

    for sub in &subs {
        // 框架级 + 项目级知识分别识别：项目级仅在该子工程被识别为对应项目时加载，
        // 其规则只进 `rules_by_sub`（不进 `ctx.frameworks`、不进全局），绝不串味到其它工程。
        let frameworks = detect_frameworks(kb, fs, sub, &project_root, KnowledgeScope::Framework);
        info!("子工程 {} 识别到框架: {:?}", sub.name, frameworks);
        detected_frameworks.extend(frameworks.iter().cloned());
        ctx.frameworks.insert(sub.id.get(), frameworks.clone());

        let projects = detect_frameworks(kb, fs, sub, &project_root, KnowledgeScope::Project);
        if !projects.is_empty() {
            info!("子工程 {} 识别到项目知识: {:?}", sub.name, projects);
        }

        // 路由 handler 解析规则 + 消费入口方法名：框架级优先，其次项目级。
        // 都由 FKB 声明（每个框架怎么写 handler / 入口方法叫什么，是框架知识）。
        if let Some(spec) = frameworks
            .iter()
            .chain(projects.iter())
            .filter_map(|id| kb.by_id(id))
            .find_map(|fk| fk.handler.clone())
        {
            if ctx.handler_spec_default.is_none() {
                ctx.handler_spec_default = Some(spec.clone());
            }
            ctx.handler_specs.insert(sub.id.get(), spec);
        }
        // `@method` 魔法方法的转发目标：同样是 FKB 知识（转发给哪个属性是框架/项目约定）。
        if let Some(spec) = frameworks
            .iter()
            .chain(projects.iter())
            .filter_map(|id| kb.by_id(id))
            .find_map(|fk| fk.magic_delegation.clone())
            .filter(|s| !s.property.is_empty())
        {
            if ctx.magic_delegation_default.is_none() {
                ctx.magic_delegation_default = Some(spec.clone());
            }
            ctx.magic_delegation.insert(sub.id.get(), spec);
        }
        // 数据模型读 / 写动词：同样是 FKB 知识（框架的 Model/Query API 叫什么）。
        if let Some(spec) = frameworks
            .iter()
            .chain(projects.iter())
            .filter_map(|id| kb.by_id(id))
            .find_map(|fk| fk.db_verbs.clone())
            .filter(|s| !s.write.is_empty() || !s.read.is_empty())
        {
            if ctx.db_verbs_default.is_none() {
                ctx.db_verbs_default = Some(spec.clone());
            }
            ctx.db_verbs.insert(sub.id.get(), spec);
        }
        // 外部系统调用 / 事务标记：与 db_verbs 同理属于知识库内容，但这两份名单
        // **不含框架假设**（curl 是 PHP 内置、`transaction` 两家都这么写），所以取
        // **同语言的全部 FKB** 而不只是本子工程识别到的框架 —— 否则声明在
        // `fkb/php/common.yaml`（`apply_without_detection`、永不被"识别"）里的名单
        // 永远加载不到，实测 P12/P13 会静默变成 0 命中。
        // 不做子工程分桶：合并成一份全局集合即可。
        for fk in kb.all().iter().filter(|fk| fk.language == sub.language) {
            for c in &fk.external_calls {
                if !ctx.external_calls.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                    ctx.external_calls.push(c.clone());
                }
            }
            for c in &fk.tx_calls {
                if !ctx.tx_calls.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                    ctx.tx_calls.push(c.clone());
                }
            }
            // 「中间件类 → 能力」：名字是框架/项目约定（`AuthTokenMiddleware` 这种
            // 叫法内核不该认识），故同样由 FKB 声明、这里只做去重合并。
            for mc in &fk.middleware_capabilities {
                if !ctx
                    .middleware_capabilities
                    .iter()
                    .any(|x| x.capability == mc.capability && x.matches == mc.matches)
                {
                    ctx.middleware_capabilities.push(mc.clone());
                }
            }
        }
        if let Some(methods) = frameworks
            .iter()
            .chain(projects.iter())
            .filter_map(|id| kb.by_id(id))
            .find(|fk| !fk.entry_methods.is_empty())
            .map(|fk| fk.entry_methods.clone())
        {
            if ctx.entry_methods_default.is_empty() {
                ctx.entry_methods_default = methods.clone();
            }
            ctx.entry_methods.insert(sub.id.get(), methods);
        }

        // 框架 + 项目知识都应用 root_rules / loaders（项目知识通常无，但通道通用）。
        let mut facts = serde_json::Map::new();
        for id in frameworks.iter().chain(projects.iter()) {
            let Some(fk) = kb.by_id(id) else { continue };
            apply_root_rules(ctx, fk, sub, &mut facts, &phase, fs, parsers);
            // 自动探测表前缀：FKB 的 `db_prefix` root_rule 从工程配置读出，
            // 并入工程显式配置的前缀（去重），供 P3 装载与 P5 归一化使用。
            if let Some(v) = facts
                .get("db_prefix")
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_str())
            {
                let mut prefixes = ctx.ws.table_prefixes().to_vec();
                if !prefixes.iter().any(|p| p == v) {
                    prefixes.push(v.to_string());
                }
                ctx.ws.set_table_prefixes(prefixes);
            }
        }
        for id in frameworks.iter().chain(projects.iter()) {
            let Some(fk) = kb.by_id(id) else { continue };
            run_loaders(ctx, fk, sub, &project_root, fs, parsers, &phase);
        }

        let facts_value = Value::Object(facts);
        for (k, v) in facts_value.as_object().cloned().unwrap_or_default() {
            ctx.ws.set_fact(sub.id, &k, v);
        }

        // 规则按子工程装配：
        // ① 本子工程识别到的框架规则；
        // ② 同语言、未识别的框架级规则 —— **仅限显式声明 `apply_without_detection` 的**；
        // ③ 本子工程识别到的**项目级**规则（仅检测到的，不外溢）。
        //
        // ② 曾经是无条件的，后果是用 A 框架的知识解释 B 框架的代码（ThinkPHP 规则
        // 套到 Laravel 工程上凭空造出上百个 Table / HttpContract）。框架规则带强框架
        // 假设，未识别到该框架时不应生效。
        let mut rules: Vec<Rule> = frameworks
            .iter()
            .filter_map(|id| kb.by_id(id))
            .flat_map(|fk| fk.rules.iter().cloned())
            .collect();
        for fk in kb.all() {
            if fk.scope == KnowledgeScope::Framework
                && fk.language == sub.language
                && !frameworks.contains(&fk.id)
                && fk.apply_without_detection
            {
                rules.extend(fk.rules.iter().cloned());
            }
        }
        for id in &projects {
            if let Some(fk) = kb.by_id(id) {
                rules.extend(fk.rules.iter().cloned());
            }
        }
        ctx.rules_by_sub.insert(sub.id.get(), dedup_rules(rules));
    }

    // 全局规则：仅**框架级**规则去重后共享（合成节点可能跨工程汇聚）。
    // 项目级规则不进全局 —— 它们只在被识别为对应项目的子工程内生效。
    // 框架级同样要求「被任一子工程识别」或「显式声明无需识别」，理由同 `rules_by_sub`。
    let mut global: Vec<(Language, Rule)> = Vec::new();
    for fk in kb.all() {
        if fk.scope != KnowledgeScope::Framework {
            continue;
        }
        if !detected_frameworks.contains(&fk.id) && !fk.apply_without_detection {
            continue;
        }
        for r in &fk.rules {
            if !global.iter().any(|( _, g)| g.id == r.id) {
                global.push((fk.language.clone(), r.clone()));
            }
        }
    }
    ctx.rules_global = global;

    Ok(())
}

// ---------------------------------------------------------------- 框架识别

fn detect_frameworks(
    kb: &dyn KnowledgeProvider,
    fs: &dyn FileSystem,
    sub: &gt_domain::model::SubProject,
    project_root: &Path,
    scope: KnowledgeScope,
) -> Vec<String> {
    let mut hits: Vec<(String, f32)> = Vec::new();
    for fk in kb.all() {
        if fk.scope != scope {
            continue;
        }
        if fk.language != sub.language && sub.language.as_str() != Language::UNKNOWN {
            continue;
        }
        let mut best = 0f32;
        for d in &fk.detectors {
            let ok = match d {
                Detector::ManifestDependency { manifest, dependency, .. } => {
                    let path = sub.root_path.join(manifest);
                    manifest_has(&path, dependency, fs)
                        || manifest_has(&project_root.join(manifest), dependency, fs)
                }
                Detector::FileExists { path, .. } => {
                    sub.root_path.join(path).exists() || project_root.join(path).exists()
                }
            };
            if ok {
                let conf = match d {
                    Detector::ManifestDependency { confidence, .. } => *confidence,
                    Detector::FileExists { confidence, .. } => *confidence,
                };
                best = best.max(conf);
            }
        }
        if best > 0.0 {
            hits.push((fk.id.clone(), best));
        }
    }
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    hits.into_iter().map(|(id, _)| id).collect()
}

fn manifest_has(path: &Path, dependency: &str, fs: &dyn FileSystem) -> bool {
    if !fs.exists(path) {
        return false;
    }
    let Ok(text) = fs.read_to_string(path) else { return false };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        // composer.json 之外也可能是纯文本依赖清单
        return text.contains(dependency);
    };
    for section in ["require", "require-dev", "dependencies", "devDependencies"] {
        if let Some(map) = v.get(section).and_then(|s| s.as_object()) {
            for key in map.keys() {
                if key.eq_ignore_ascii_case(dependency) || key.contains(dependency) {
                    return true;
                }
            }
        }
    }
    text.contains(dependency)
}

// ---------------------------------------------------------------- AppRoot

fn apply_root_rules(
    ctx: &mut PipelineContext,
    fk: &FrameworkKnowledge,
    sub: &gt_domain::model::SubProject,
    facts: &mut serde_json::Map<String, Value>,
    phase: &Phase,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
) {
    for rule in &fk.root_rules {
        let resolved = match &rule.source {
            gt_domain::model::RootSource::ManifestJson { manifest, pointer, pick } => {
                let path = sub.root_path.join(manifest);
                let path = if path.exists() { path } else { ctx.project.root_path.join(manifest) };
                resolve_manifest_pointer(&path, pointer, *pick)
            }
            gt_domain::model::RootSource::DirectoryExists { path } => {
                resolve_directory_exists(&sub.root_path, path)
            }
            gt_domain::model::RootSource::ManifestPhp { manifest, pointer } => {
                resolve_manifest_php(sub, &ctx.project.root_path, manifest, pointer, fs, parsers)
            }
        };

        let (value, source, fallback_used, confidence) = match resolved {
            Some((v, src)) => (v, src, false, rule.confidence),
            None => {
                let fallback = rule
                    .fallbacks
                    .iter()
                    .find(|c| sub.root_path.join(c).exists());
                match fallback {
                    Some(c) => (
                        c.clone(),
                        format!("fallback: {}", c),
                        true,
                        rule.confidence * 0.7,
                    ),
                    None => {
                        ctx.ws.diagnose(
                            phase,
                            "RootRuleUnresolved",
                            gt_domain::model::Severity::Warning,
                            format!("框架 {} 的 {} 未能解析", fk.id, rule.key),
                            Some(sub.root_path.to_string_lossy().to_string()),
                        );
                        continue;
                    }
                }
            }
        };

        facts.insert(
            rule.key.clone(),
            json!({
                "value": value,
                "confidence": confidence,
                "source": source,
                "fallback_used": fallback_used,
            }),
        );
    }
}

/// 解析 `directory_exists` 根规则：在子工程内（含多级子目录，深度受限）查找目标相对路径
/// （如 `src/main/java`）。命中后返回其**父目录**作为源根（`app_root` 的语义应是源码根，而非
/// `src/main/java` 本身）。单模块工程在根目录直下命中时返回 `"."`；多模块工程返回首个命中模块
/// 的相对目录（如 `mall-admin`）。仅当整棵目录树都找不到时才返回 `None`（触发兜底 / 告警）。
fn resolve_directory_exists(root: &Path, rel: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    // 根目录直下命中：源根即子工程根。
    if root.join(rel).is_dir() {
        return Some((".".to_string(), format!("directory exists: {}", rel)));
    }
    // 多模块：广度优先（优先浅层），深度受限避免扫描整棵大目录树。
    let mut queue: std::collections::VecDeque<(PathBuf, u32)> =
        std::collections::VecDeque::from([(root.to_path_buf(), 0)]);
    let max_depth = 5;
    while let Some((dir, depth)) = queue.pop_front() {
        let target = parts.iter().fold(dir.clone(), |acc, p| acc.join(p));
        if target.is_dir() {
            let rel_root = dir.strip_prefix(root).unwrap_or_else(|_| Path::new(""));
            let value = if rel_root.as_os_str().is_empty() {
                ".".to_string()
            } else {
                rel_root.to_string_lossy().replace('\\', "/")
            };
            return Some((value, format!("directory exists (recursive): {}", rel)));
        }
        if depth < max_depth {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        queue.push_back((p, depth + 1));
                    }
                }
            }
        }
    }
    None
}

fn resolve_manifest_pointer(
    path: &Path,
    pointer: &str,
    pick: PickStrategy,
) -> Option<(String, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let mut cur = &v;
    for seg in pointer.split('.') {
        cur = cur.get(seg)?;
    }
    let map = cur.as_object()?;
    let mut candidates: Vec<(String, String)> = map
        .iter()
        .filter_map(|(ns, dir)| {
            dir.as_str().map(|d| (d.trim_matches('/').to_string(), ns.clone()))
        })
        .filter(|(d, _)| !d.is_empty())
        .collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by_key(|(d, _)| d.matches('/').count());
    let chosen = match pick {
        PickStrategy::ShallowestDir | PickStrategy::ByNamespaceKey | PickStrategy::FirstDir => {
            candidates.remove(0)
        }
    };
    Some((
        chosen.0.clone(),
        format!("{} {} (map_dir={}/)", path.display(), pointer, chosen.0),
    ))
}

/// 从 PHP 配置文件（如 ThinkPHP 的 `config/database.php`）按点分指针读取值。
///
/// 复用 PHP 解析器把 `return [...]` 展平成 `config_entries`，再按 `key_path`
/// 精确匹配 `pointer`（如 `connections.mysql.prefix`）。配置值若是
/// `env('KEY', 'default')` 这类无法静态求值的写法（解析器记为 `Unknown`），
/// 再用轻量正则提取其字面默认值作为兜底。
fn resolve_manifest_php(
    sub: &gt_domain::model::SubProject,
    project_root: &Path,
    manifest: &str,
    pointer: &str,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
) -> Option<(String, String)> {
    let abs = sub.root_path.join(manifest);
    let abs = if abs.exists() { abs } else { project_root.join(manifest) };
    if !abs.exists() {
        return None;
    }
    let text = fs.read_to_string(&abs).ok()?;
    let parser = parser_for_file(parsers, Some(sub), &abs)?;
    let rel = abs.to_string_lossy().replace('\\', "/");
    if let Ok(facts) = parser.parse(&rel, &text) {
        for entry in &facts.config_entries {
            if entry.key_path == pointer {
                if let Some(s) = entry.value.as_str() {
                    return Some((s.to_string(), format!("php config: {}", abs.display())));
                }
            }
        }
    }
    // 兜底：值可能是 `env('KEY', 'default')` → 从源文本提取叶子键对应的默认值。
    let leaf = pointer.rsplit('.').next().unwrap_or(pointer);
    extract_prefix_via_regex(&text, leaf).map(|s| {
        (s, format!("php config (env default): {}", abs.display()))
    })
}

/// 轻量兜底：从 `config/database.php` 源文本提取 `<leaf> => 'x'` 或
/// `<leaf> => env('K', 'x')` 中的字面默认值。仅用于解析器无法静态求值的场景。
fn extract_prefix_via_regex(text: &str, leaf: &str) -> Option<String> {
    let escaped = regex::escape(leaf);
    let re = regex::Regex::new(&format!(
        r#"(?i)(?:['"]){escaped}(?:['"])\s*=>\s*(?:env\(\s*['"][^'"]*['"]\s*,\s*['"]([^'"]*)['"]\s*\)|['"]([^'"]*)['"])"#
    ))
    .ok()?;
    re.captures(text)
        .and_then(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().to_string()))
}

// ---------------------------------------------------------------- 装载器

fn run_loaders(
    ctx: &mut PipelineContext,
    fk: &FrameworkKnowledge,
    sub: &gt_domain::model::SubProject,
    project_root: &Path,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    phase: &Phase,
) {
    // 契约路径的归一化链来自**本框架**的 http-contract 规则：`route_list` 的键必须与
    // P5 合成出来的 `HttpContract.name` 逐字一致，而路径怎么归一是框架知识
    // （ThinkPHP 要 `strip_prefix ["api/"] + param_wildcard`，Laravel 只要 `leading_slash`）。
    let contract_steps = contract_steps_of(fk);
    for loader in &fk.loaders {
        if let Err(e) = run_loader(
            ctx,
            loader,
            sub,
            project_root,
            fs,
            parsers,
            phase,
            &contract_steps,
            fk,
        ) {
            warn!("装载器 {} 失败: {e}", loader.id);
        }
    }
}

/// 从一份 FKB 里取契约路径的归一化步骤（第一条 `ContractId` 规则）。
///
/// 取不到时退回 `[LeadingSlash]`：至少保证前导斜杠口径一致，不会把键写成半成品。
fn contract_steps_of(fk: &FrameworkKnowledge) -> Vec<NormalizeStep> {
    for rule in &fk.rules {
        for action in &rule.binding {
            let Action::Synthesize(s) = action else { continue };
            if s.identity.kind.as_str() != SynthesizedKind::CONTRACT_ID {
                continue;
            }
            if let Some(steps) = s.identity.path.as_ref().and_then(|p| p.normalize.clone()) {
                if !steps.is_empty() {
                    return steps;
                }
            }
        }
    }
    vec![NormalizeStep::LeadingSlash]
}

/// 按**文件**选择解析器：优先用扩展名判定语言，取不到时回退到子工程语言。
///
/// 装载器处理的是具体文件（`config/database.php`、`app/event.php`、`lang/zh-cn/*.php`），
/// 语言应由文件本身决定。写死成某一种语言会让新增语言后所有装载器**静默失效**
/// （不报错、只是什么都不装载）—— 这是「内核不认识具体语言」在 P3 的落点。
fn parser_for_file<'a>(
    parsers: &'a dyn ParserRegistry,
    fallback_sub: Option<&gt_domain::model::SubProject>,
    path: &Path,
) -> Option<&'a dyn LanguageParser> {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if let Some(p) = parsers
            .language_for_extension(ext)
            .and_then(|lang| parsers.parser_for(&lang))
        {
            return Some(p);
        }
    }
    fallback_sub.and_then(|sub| parsers.parser_for(&sub.language))
}

fn run_loader(
    ctx: &mut PipelineContext,
    loader: &gt_domain::model::LoaderSpec,
    sub: &gt_domain::model::SubProject,
    project_root: &Path,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    phase: &Phase,
    contract_steps: &[NormalizeStep],
    fk: &FrameworkKnowledge,
) -> Result<()> {
    let app_root = ctx
        .ws
        .get_fact(sub.id, "app_root")
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("app")
        .to_string();

    match &loader.from {
        gt_domain::model::LoaderSource::File { path, key_path, .. } => {
            let rel = path.replace("{app_root}", &app_root);
            let abs = sub.root_path.join(&rel);
            let abs = if abs.exists() { abs } else { project_root.join(&rel) };
            if !abs.exists() {
                return Ok(());
            }
            let text = fs.read_to_string(&abs)?;
            let Some(parser) = parser_for_file(parsers, Some(sub), &abs) else {
                return Ok(());
            };
            let rel_display = abs
                .strip_prefix(project_root)
                .unwrap_or(&abs)
                .to_string_lossy()
                .replace('\\', "/");
            match parser.parse(&rel_display, &text) {
                Ok(facts) => {
                    for entry in &facts.config_entries {
                        let key = match key_path {
                            Some(prefix) => match entry.key_path.strip_prefix(&format!("{}.", prefix)) {
                                Some(rest) => rest.to_string(),
                                None => continue,
                            },
                            None => entry.key_path.clone(),
                        };
                        if key.is_empty() {
                            continue;
                        }
                        ctx.ws.put_symbol(
                            ctx.project.id,
                            &loader.table,
                            &key,
                            json!({
                                "value": entry.value,
                                "file": rel_display,
                                "line": entry.span.start_line,
                            }),
                        );
                    }
                }
                Err(e) => warn!("解析 {} 失败: {e}", rel_display),
            }
        }
        gt_domain::model::LoaderSource::Glob { pattern, locale_regex, .. } => {
            load_i18n(ctx, loader, pattern, project_root, fs, parsers, locale_regex.as_deref());
        }
        gt_domain::model::LoaderSource::Inline { rows } => {
            for row in rows {
                // 键字段名按表而定：`facade_map` 用 `facade`、通用表用 `key`、
                // 类清单表（如 `middleware_classes`）用 `class` —— 都是 FKB 说了算。
                let key = row
                    .get("facade")
                    .or_else(|| row.get("key"))
                    .or_else(|| row.get("class"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if key.is_empty() {
                    continue;
                }
                ctx.ws.put_symbol(ctx.project.id, &loader.table, &key, row.clone());
            }
        }
        gt_domain::model::LoaderSource::Builtin { name, params } => {
            run_builtin(ctx, name, params, sub, project_root, fs, phase, contract_steps, fk);
        }
    }
    Ok(())
}

fn load_i18n(
    ctx: &mut PipelineContext,
    loader: &gt_domain::model::LoaderSpec,
    pattern: &str,
    project_root: &Path,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    locale_regex: Option<&str>,
) {
    let files: Vec<gt_domain::model::SourceFile> = ctx.files.clone();

    for file in files {
        if !path_matches(pattern, &file.path) {
            continue;
        }
        let locale = match locale_regex {
            Some(re) => match regex::Regex::new(re) {
                Ok(re) => re
                    .captures(&file.path)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().to_string()),
                Err(_) => None,
            },
            None => capture_locale("lang/{locale}/*.php", &file.path),
        };
        let Some(locale) = locale else { continue };
        let abs = project_root.join(&file.path);
        let Ok(text) = fs.read_to_string(&abs) else { continue };
        let Some(parser) = parser_for_file(parsers, None, &abs) else {
            continue;
        };
        let Ok(facts) = parser.parse(&file.path, &text) else {
            continue;
        };
        let stem = std::path::Path::new(&file.path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        for entry in &facts.config_entries {
            let key = if stem.is_empty() {
                entry.key_path.clone()
            } else {
                format!("{}.{}", stem, entry.key_path)
            };
            let text_value = match &entry.value {
                gt_domain::model::FactValue::String(s) => s.clone(),
                other => other.to_string_lossy(),
            };
            let mut merged = ctx
                .ws
                .get_symbol(&loader.table, &key)
                .cloned()
                .unwrap_or_else(|| json!({ "texts": {} }));
            if let Some(texts) = merged.get_mut("texts").and_then(|t| t.as_object_mut()) {
                texts.insert(locale.clone(), json!(text_value));
            }
            merged["file"] = json!(file.path);
            ctx.ws.put_symbol(ctx.project.id, &loader.table, &key, merged);
        }
    }
}

// ---------------------------------------------------------------- 内置装载器

fn run_builtin(
    ctx: &mut PipelineContext,
    name: &str,
    params: &Value,
    sub: &gt_domain::model::SubProject,
    project_root: &Path,
    fs: &dyn FileSystem,
    phase: &Phase,
    contract_steps: &[NormalizeStep],
    fk: &FrameworkKnowledge,
) {
    match name {
        "php_db_schema" => load_schema(ctx, params, sub, project_root, phase),
        "php_migration_schema" => load_migration_schema(ctx, params, project_root),
        "php_config_keys" => load_config_keys(ctx, params, sub),
        // 通用别名装载器：文件 / 块标记 / 分隔符全由 FKB `params` 声明，不绑定任何语言。
        "middleware_aliases" => load_middleware_aliases(ctx, project_root, params),
        // 通用声明式中间件装载器：文件名 / 键 / 作用域全由 FKB `params` 声明，
        // 把声明类名单合并进 `route_list` 的 `guards`，由 P14 统一晋升节点 + 连边。
        "declared_middleware" => load_declared_middleware(ctx, project_root, params),
        // 通用路由守卫装载器：识别逻辑完全来自 FKB 的 `route_guards` 声明，不再写死任何框架。
        "routes" => {
            if let Some(spec) = fk.route_guards.as_ref() {
                load_routes(ctx, spec, sub, contract_steps);
            }
        }
        "nginx_config" => load_nginx(ctx, sub, project_root, fs),
        other => debug!("未知内置装载器: {other}"),
    }
}

fn load_schema(
    ctx: &mut PipelineContext,
    params: &Value,
    _sub: &gt_domain::model::SubProject,
    project_root: &Path,
    _phase: &Phase,
) {
    // 表前缀以工程探测到的为准（P3 从 config/database.php 自动读出并写入
    // `workspace.table_prefixes`）；YAML 仍可经 `params.prefixes` 追加额外前缀。
    let mut prefixes: Vec<String> = ctx.ws.table_prefixes().to_vec();
    if let Some(extra) = params
        .get("prefixes")
        .and_then(|p| serde_json::from_value::<Vec<String>>(p.clone()).ok())
    {
        for x in extra {
            if !prefixes.contains(&x) {
                prefixes.push(x);
            }
        }
    }

    // ① 从 SQL 安装脚本解析表结构
    for (path, text) in scan_text_files(project_root, &["sql"]) {
        for (table, columns) in parse_create_tables(&text) {
            let name = strip_prefixes(&table, &prefixes);
            merge_schema_columns(ctx, &name, columns, &path);
        }
    }

    // ② 从代码中的 Db::name('x') 收集表名（只收集最小的必要集合，避免整体克隆）
    // 表名识别的 receiver / method 由 FKB `params` 声明（缺省覆盖 ThinkPHP 的
    // `Db::name` / `Model` 与 Laravel 的 `DB::table` / `Query` / `Model`）。
    let table_receivers: Vec<String> = params
        .get("table_receivers")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_else(|| vec!["Db".into(), "\\Db".into(), "Query".into(), "Model".into()]);
    let table_methods: Vec<String> = params
        .get("table_methods")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_else(|| vec!["name".into(), "table".into()]);
    let mut found: Vec<(String, String)> = Vec::new();
    for call in ctx.ws.calls.iter() {
        // 只有 DB 语义的 name()/table() 才算表名：
        // `Route::name('xxx')` 是路由命名，不能当表。
        let is_db_receiver = call
            .receiver
            .as_deref()
            .map(|raw| {
                let r = raw.trim_start_matches('\\');
                table_receivers.iter().any(|p| {
                    if p.starts_with('\\') {
                        r.ends_with(p)
                    } else if p.eq_ignore_ascii_case("Db") {
                        r.eq_ignore_ascii_case(p)
                    } else {
                        r.contains(p)
                    }
                })
            })
            .unwrap_or(false);
        let is_table_call = call
            .method
            .as_deref()
            .map(|m| table_methods.iter().any(|t| m.eq_ignore_ascii_case(t)))
            .unwrap_or(false);
        if !is_table_call || !is_db_receiver {
            continue;
        }
        if let Some(gt_domain::model::FactValue::String(t)) = call.args.first() {
            let name = strip_prefixes(t, &prefixes);
            if name.is_empty() {
                continue;
            }
            found.push((name, format!("{}:{}", call.file, call.span.start_line)));
        }
    }
    for (name, loc) in found {
        let existing = ctx.ws.get_symbol("schema", &name).cloned();
        let mut value = existing.unwrap_or_else(|| json!({ "columns": [], "sources": [] }));
        if let Some(srcs) = value.get_mut("sources").and_then(|s| s.as_array_mut()) {
            if srcs.len() < 50 {
                srcs.push(json!(loc));
            }
        }
        ctx.ws.put_symbol(ctx.project.id, "schema", &name, value);
    }
}

/// Laravel migration：`Schema::create('users', function (Blueprint $table) { … })`。
///
/// 为什么必须有它：PHP ORM 的模型**不声明字段**，列只写在 migration 里；而
/// `$table->string('email')` 位于闭包内、调用点 owner 是闭包而非模型类，
/// FKB 拿不到"这一列属于哪张表"（这是 PHP 与 TypeORM `@Column` 的关键差别）。
/// 故由装载器直接把列写进 `schema` 符号表，P6 再沉淀成 `Column` 图节点。
fn load_migration_schema(ctx: &mut PipelineContext, params: &Value, project_root: &Path) {
    let mut prefixes: Vec<String> = ctx.ws.table_prefixes().to_vec();
    if let Some(extra) = params
        .get("prefixes")
        .and_then(|p| serde_json::from_value::<Vec<String>>(p.clone()).ok())
    {
        for x in extra {
            if !prefixes.contains(&x) {
                prefixes.push(x);
            }
        }
    }
    // migration 文件路径片段与扩展名由 FKB `params` 声明（缺省 Laravel/ThinkPHP 通用形态）。
    let paths: Vec<String> = params
        .get("paths")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_else(|| vec!["database/migrations".into()]);
    let exts: Vec<String> = params
        .get("extensions")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_else(|| vec!["php".into()]);
    for (path, text) in scan_migration_files(project_root, &paths, &exts) {
        for (table, columns) in parse_migration_tables(&text) {
            let name = strip_prefixes(&table, &prefixes);
            merge_schema_columns(ctx, &name, columns, &path);
        }
    }
}

/// 扫描 migration 文件（路径片段 + 扩展名由 FKB `params` 声明，缺省 `database/migrations` + `php`）。
/// 跳过依赖目录（通用，不绑定框架）。
fn scan_migration_files(root: &Path, paths: &[String], exts: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut walker = walkdir::WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(
                name.as_str(),
                "vendor" | "node_modules" | "target" | ".git" | "dist" | "build" | "runtime"
            ) {
                walker.skip_current_dir();
            }
            continue;
        }
        let p = entry.path();
        let is_php = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
            .unwrap_or(false);
        let norm = p.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
        let is_migration = paths.iter().any(|pat| norm.contains(&pat.to_ascii_lowercase()));
        if !is_php || !is_migration {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(p) {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, text));
        }
    }
    out
}

/// 解析 migration 的 `Schema::create` / `Schema::table`，取出「表名 → 列名」。
fn parse_migration_tables(src: &str) -> Vec<(String, Vec<String>)> {
    let Ok(re) = regex::Regex::new(
        r#"Schema\s*::\s*(?:create|table)\s*\(\s*['"]([\w]+)['"]"#,
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for cap in re.captures_iter(src) {
        let Some(m0) = cap.get(0) else { continue };
        let table = cap[1].to_string();
        // 配对 `Schema::create(` 的右括号（跳过字符串里的括号），取出整个调用块
        let Some(rel) = m0.as_str().find('(') else { continue };
        let open = m0.start() + rel;
        let Some(close) = matching_paren(src, open) else { continue };
        if close <= open + 1 {
            continue;
        }
        out.push((table, columns_of_blueprint(&src[open + 1..close])));
    }
    out
}

/// 取 Blueprint 闭包体里的列声明：`$table->string('email')` → `email`。
///
/// **只认列声明方法（白名单）** —— 不能简单"取第一个字符串实参"：
/// `->comment('说明')` / `->after('col')` / `->default('x')` 这类**修饰符**也带
/// 字符串实参，会被误当成列名。
fn columns_of_blueprint(body: &str) -> Vec<String> {
    let b = body.as_bytes();
    let mut cols: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let Some(rel) = body[i..].find("$table") else { break };
        let at = i + rel;
        let mut p = at + "$table".len();
        p = skip_ws(b, p);
        if !body[p..].starts_with("->") {
            i = at + 1;
            continue;
        }
        p = skip_ws(b, p + 2);
        let ms = p;
        while p < b.len() && (b[p].is_ascii_alphanumeric() || b[p] == b'_') {
            p += 1;
        }
        let method = &body[ms..p];
        // 跳到实参列表的左括号
        while p < b.len() && b[p] != b'(' && b[p] != b';' && b[p] != b'{' {
            p += 1;
        }
        if p >= b.len() || b[p] != b'(' {
            i = at + 1;
            continue;
        }
        if !is_column_method(method) {
            i = at + 1;
            continue;
        }
        let q = skip_ws(b, p + 1);
        if q < b.len() && (b[q] == b'\'' || b[q] == b'"') {
            if let Some(col) = read_quoted(b, q) {
                push_col(&mut cols, &col);
            }
        } else if q < b.len() && b[q] == b')' {
            // 无实参的列声明（`$table->id()` / `->timestamps()`）：按 Laravel 约定补列名
            for c in implicit_columns(method) {
                push_col(&mut cols, c);
            }
        }
        i = at + 1;
    }
    cols
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// 读 `i` 处的字符串字面量内容（不含引号）。
fn read_quoted(b: &[u8], i: usize) -> Option<String> {
    let quote = *b.get(i)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let mut e = i + 1;
    while e < b.len() && b[e] != quote {
        if b[e] == b'\\' {
            e += 1;
        }
        e += 1;
    }
    if e >= b.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&b[i + 1..e]).to_string())
}

fn push_col(cols: &mut Vec<String>, c: &str) {
    if !c.is_empty() && !cols.iter().any(|x| x == c) {
        cols.push(c.to_string());
    }
}

/// Blueprint 里**真正声明列**的方法（白名单，见 [`columns_of_blueprint`]）。
fn is_column_method(m: &str) -> bool {
    matches!(
        m,
        "bigIncrements" | "bigInteger" | "binary" | "boolean" | "char" | "date" | "dateTime"
        | "dateTimeTz" | "decimal" | "double" | "enum" | "float" | "foreignId" | "foreignUuid"
        | "geography" | "geometry" | "id" | "increments" | "integer" | "ipAddress" | "json"
        | "jsonb" | "longText" | "macAddress" | "mediumIncrements" | "mediumInteger"
        | "mediumText" | "set" | "smallIncrements" | "smallInteger" | "string" | "text"
        | "time" | "timeTz" | "timestamp" | "timestampTz" | "tinyIncrements" | "tinyInteger"
        | "tinyText" | "unsignedBigInteger" | "unsignedDecimal" | "unsignedDouble"
        | "unsignedFloat" | "unsignedInteger" | "unsignedMediumInteger" | "unsignedSmallInteger"
        | "unsignedTinyInteger" | "ulid" | "uuid" | "year"
        // 无实参、按约定补列名的方法
        | "rememberToken" | "softDeletes" | "softDeletesTz" | "timestamps" | "timestampsTz"
        | "nullableTimestamps" | "morphs" | "nullableMorphs" | "nullableUuidMorphs"
        | "nullableUlidMorphs"
    )
}

/// 无实参列声明的**隐含列名**（Laravel 约定）。
///
/// 刻意不含裸 `uuid()` / `ulid()`：它们默认列名就叫 `uuid` / `ulid`，
/// 但代码里更常见的是 `$table->uuid('id')`（带实参已覆盖）—— 不猜。
fn implicit_columns(m: &str) -> Vec<&'static str> {
    match m {
        "id" | "increments" | "bigIncrements" | "mediumIncrements" | "smallIncrements"
        | "tinyIncrements" => vec!["id"],
        "timestamps" | "timestampsTz" | "nullableTimestamps" => vec!["created_at", "updated_at"],
        "softDeletes" | "softDeletesTz" => vec!["deleted_at"],
        "rememberToken" => vec!["remember_token"],
        _ => Vec::new(),
    }
}

/// 找到与 `open` 处 `(` 配对的 `)`（跳过字符串字面量内的括号）。
fn matching_paren(src: &str, open: usize) -> Option<usize> {
    let b = src.as_bytes();
    if open >= b.len() || b[open] != b'(' {
        return None;
    }
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => {
                let quote = b[i];
                i += 1;
                while i < b.len() && b[i] != quote {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// 把一批列并入 `schema` 符号表（**并集去重**，不整份覆盖）。
///
/// 同一张表可能有两个来源：SQL 安装脚本与 Laravel migration。两个装载器都写 `schema`，
/// 若各自 `put_symbol` 整份覆盖，后跑的会抹掉先跑的（**装载顺序不保证**）——
/// 故统一走这里做并集。
fn merge_schema_columns(
    ctx: &mut PipelineContext,
    table: &str,
    columns: Vec<String>,
    source: &str,
) {
    if table.is_empty() || columns.is_empty() {
        return;
    }
    let mut value = ctx
        .ws
        .get_symbol("schema", table)
        .cloned()
        .unwrap_or_else(|| json!({ "columns": [], "sources": [] }));
    let mut cols: Vec<String> = value
        .get("columns")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    for c in columns {
        if !cols.iter().any(|x| x == &c) {
            cols.push(c);
        }
    }
    value["columns"] = json!(cols);
    if let Some(srcs) = value.get_mut("sources").and_then(|s| s.as_array_mut()) {
        if srcs.len() < 50 && !srcs.iter().any(|s| s.as_str() == Some(source)) {
            srcs.push(json!(source));
        }
    }
    ctx.ws.put_symbol(ctx.project.id, "schema", table, value);
}

fn load_config_keys(ctx: &mut PipelineContext, params: &Value, _sub: &gt_domain::model::SubProject) {
    // 配置访问器全部由 FKB `params` 声明（不同框架写法不同：
    // ThinkPHP 用 `sys_config` / `config`，Laravel 用 `config` / `env`）。
    // `accessors` 为精确 callee（大小写不敏感），`suffixes` 为后缀匹配
    // （如 `::get` 覆盖任意 `Xxx::get`）。缺省沿用当前跨框架通用写法。
    let accessors: Vec<String> = params
        .get("accessors")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_else(|| {
            vec![
                "sys_config".into(),
                "sys_config_all".into(),
                "config".into(),
                "env".into(),
                "Env::get".into(),
            ]
        });
    let suffixes: Vec<String> = params
        .get("suffixes")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_else(|| vec!["::get".into()]);
    let mut found: Vec<(String, String, u32)> = Vec::new();
    for call in ctx.ws.calls.iter() {
        let is_config = accessors
            .iter()
            .any(|a| call.callee.eq_ignore_ascii_case(a))
            || suffixes.iter().any(|s| call.callee.ends_with(s));
        if !is_config {
            continue;
        }
        let Some(gt_domain::model::FactValue::String(key)) = call.args.first() else {
            continue;
        };
        if key.is_empty() || key.contains(' ') {
            continue;
        }
        found.push((key.clone(), call.file.clone(), call.span.start_line));
    }
    for (key, file, line) in found {
        ctx.ws.put_symbol(
            ctx.project.id,
            "config_keys",
            &key,
            json!({
                "storage": "Database",
                "mutable": "RuntimeMutable",
                "value_type": "string",
                "file": file,
                "line": line,
            }),
        );
    }
}

/// 归一化类名：把连续的反斜杠收成一个、去掉首尾空段。
///
/// 别名表 / 路由守卫里的类名经过 JSON 与文本扫描的多轮转义，可能出现
/// `Illuminate\\Session\\Middleware\\X`（多个反斜杠）。比对前先归一，
/// 否则 FKB 声明的已知中间件永远对不上（实测 laravel10 整表落空）。
pub fn norm_class(s: &str) -> String {
    s.split('\\')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\\")
}

/// 把 `callee`（可能带命名空间 `org.x.Y` / `x\Y`）折成短名，便于与声明里的装饰器 / 注解名比对。
fn short_callee(callee: &str) -> &str {
    callee.rsplit(['\\', '.']).next().unwrap_or(callee)
}

/// 一个调用的 `receiver` 是否匹配某条 `route_calls` 模式的 `receiver` 字段。
fn receiver_matches(rc: &RouteCallSpec, recv: &str) -> bool {
    let lower = recv.to_ascii_lowercase();
    let target = rc.receiver.to_ascii_lowercase();
    if rc.receiver_ends_with {
        lower.trim_start_matches('\\').ends_with(&target)
    } else {
        lower.contains(&target)
    }
}

/// 取一个实参的「中间件名」。
///
/// 只认**字面量**（`String` / `ClassConst`）——PHP 的中间件恒为 `X::class`。
/// 标识符（`Unknown(Some(name))`，即 JS / Python 的函数引用）**仅当 FKB 显式声明
/// `accept_identifier`** 才收：PHP 的动态实参 `->middleware($v)` 若被收进来就是编造。
fn guard_arg_name(v: &FactValue, accept_identifier: bool) -> Option<String> {
    match v {
        FactValue::String(s) | FactValue::ClassConst(s) if !s.trim().is_empty() => {
            Some(s.trim().to_string())
        }
        FactValue::Unknown(Some(name)) if accept_identifier => {
            // JS / Python 的中间件实参可能是**整条调用表达式**
            // （`lusca({ csrf: true })`、`passport.authenticate('google', {...})`）。
            // 取调用名（`(` 之前的部分）作为中间件名：既可读，也让同一中间件的不同
            // 实参写法自然汇聚成一个节点，而不是每种策略一个。
            let base = name.split('(').next().unwrap_or(name).trim().to_string();
            if base.is_empty() {
                None
            } else {
                Some(base)
            }
        }
        _ => None,
    }
}

/// 判断一个调用点是否「路由定义」，命中则返回 `(verb, path, handler)`。
fn match_route_call(call: &CallRecord, rc: &RouteCallSpec) -> Option<(String, String, Option<String>)> {
    let verb = match rc.by {
        RouteMatchBy::Receiver => {
            let recv = call.receiver.as_deref()?;
            if !receiver_matches(rc, recv) {
                return None;
            }
            let m = call.method.as_deref()?.to_ascii_lowercase();
            rc.verb_methods.get(&m).cloned()?
        }
        RouteMatchBy::Callee => {
            let callee = short_callee(&call.callee).to_ascii_lowercase();
            rc.verb_methods.get(&callee).cloned()?
        }
    };
    let path = call
        .args
        .get(rc.path_arg)
        .and_then(|a| a.as_str())
        .unwrap_or_default()
        .to_string();
    if path.is_empty() {
        return None;
    }
    let handler = rc
        .handler_arg
        .and_then(|i| call.args.get(i))
        .and_then(|a| a.as_str())
        .map(|s| s.to_string());
    Some((verb, path, handler))
}

/// 收集「哪些中间件守着哪一段路由」——**完全按 FKB `route_guards` 声明识别**，
/// 不再写死任何框架的 receiver / 方法名 / 动词表。
///
/// 三种挂载模型：
/// * `chain`：`Route::get(path)->middleware(X[, arg])`（ThinkPHP / Laravel）；
/// * `positional`：`app.get(path, mw1, mw2, handler)`（Express / Koa）；
/// * `decorator`：`@UseGuards(X)` / `@login_required` / `@PreAuthorize` 落在与被修饰
///   路由**同一方法**上，按 `owner_fqn` 关联（NestJS / Python / Spring）。
fn collect_route_guards(spec: &RouteGuardSpec, calls: &[CallRecord]) -> Vec<RouteGuardScope> {
    // 可声明**多个**挂载模型（同一框架常有几种写法），结果取并集。
    let mut out = Vec::new();
    for attach in spec.guard_attach.specs() {
        let mut scopes = match attach {
            GuardAttachSpec::Chain(chain) => extract_chain_guards(spec, chain, calls),
            GuardAttachSpec::Positional => extract_positional_guards(spec, calls),
            GuardAttachSpec::Decorator(d) => extract_decorator_guards(spec, d, calls),
            GuardAttachSpec::Consumer(cg) => extract_consumer_guards(spec, cg, calls),
        };
        out.append(&mut scopes);
    }
    out
}

/// 链式守卫：`Route::get(path)->middleware(X[, arg])`。
///
/// 难点在**链式调用**：`Route::group('pc', fn)->middleware(A::class)` 在图上落成一串调用点，
/// 根节点（`group`）与每个 `->middleware()` **共享同一个 `start_byte`**
/// （`member_call_expression` 从根那一段源码开始）。按 `(file, start_byte)` 配对即可把
/// 中间件挂回它真正修饰的那段区间；链上候选里**优先取 `group` 调用**（它才包住闭包区间），
/// 其余取 `end_byte` 最小者作为根。
fn extract_chain_guards(
    spec: &RouteGuardSpec,
    chain: &gt_domain::model::ChainGuardSpec,
    calls: &[CallRecord],
) -> Vec<RouteGuardScope> {
    let is_route_receiver = |c: &CallRecord| -> bool {
        c.receiver
            .as_deref()
            .map(|r| spec.route_calls.iter().any(|rc| receiver_matches(rc, r)))
            .unwrap_or(false)
    };
    let is_verb = |m: Option<&str>| -> bool {
        m.map(|mm| {
            spec.route_calls
                .iter()
                .any(|rc| rc.verb_methods.keys().any(|k| k.eq_ignore_ascii_case(mm)))
        })
        .unwrap_or(false)
    };
    let is_group = |m: Option<&str>| -> bool {
        m.map(|mm| spec.route_calls.iter().any(|rc| rc.group_method.as_deref() == Some(mm)))
            .unwrap_or(false)
    };

    // ① 每条链的根：`(file, start_byte)` → 根调用。优先 `group`，否则 `end_byte` 最小者。
    let mut roots: HashMap<(String, u32), &CallRecord> = HashMap::new();
    for call in calls {
        if !is_route_receiver(call) {
            continue;
        }
        let m = call.method.as_deref();
        if !is_group(m) && !is_verb(m) {
            continue;
        }
        let key = (call.file.clone(), call.span.start_byte);
        let prev_is_group = roots
            .get(&key)
            .and_then(|p| p.method.as_deref())
            .map(|mm| is_group(Some(mm)))
            .unwrap_or(false);
        let call_is_group = is_group(m);
        let shorter = roots
            .get(&key)
            .map_or(true, |p| (p.span.end_byte - p.span.start_byte) > (call.span.end_byte - call.span.start_byte));
        if call_is_group || (!prev_is_group && shorter) {
            roots.insert(key, call);
        }
    }

    // ② 把每个 `->middleware(X[, arg])` 归并到它的根。实参可单值可数组。
    let mut scopes: HashMap<(String, u32), RouteGuardScope> = HashMap::new();
    for call in calls {
        if call.method.as_deref() != Some(chain.method.as_str()) || !is_route_receiver(call) {
            continue;
        }
        // `accept_identifier` 由**命中的那条 route_calls 模式**给出（同框架的其它模式可能不同）。
        let accept_identifier = spec
            .route_calls
            .iter()
            .any(|rc| receiver_matches(rc, call.receiver.as_deref().unwrap_or("")) && rc.accept_identifier);
        let mut classes: Vec<(String, Option<String>)> = Vec::new();
        let arg2 = chain.arg2_index.and_then(|i| call.args.get(i));
        match call.args.get(chain.arg_index) {
            Some(a @ (FactValue::String(_) | FactValue::ClassConst(_) | FactValue::Unknown(_))) => {
                if let Some(name) = guard_arg_name(a, accept_identifier) {
                    classes.push((name, arg2.and_then(guard_arg_text)));
                }
            }
            Some(FactValue::Array(items)) => {
                for (_, v) in items {
                    if let Some(name) = guard_arg_name(v, accept_identifier) {
                        classes.push((name, arg2.and_then(guard_arg_text)));
                    }
                }
            }
            _ => {}
        }
        if classes.is_empty() {
            continue;
        }
        let Some(root) = roots.get(&(call.file.clone(), call.span.start_byte)) else {
            continue;
        };
        let scope_key = (root.file.clone(), root.span.start_byte);
        let scope = scopes.entry(scope_key).or_insert_with(|| RouteGuardScope {
            file: root.file.clone(),
            start_line: root.span.start_line,
            end_line: root.span.end_line,
            guards: Vec::new(),
        });
        for (class, arg) in classes {
            if !scope.guards.iter().any(|g| g.class == class) {
                scope.guards.push(RouteGuard { class, arg });
            }
        }
    }

    let mut out: Vec<RouteGuardScope> = scopes.into_values().collect();
    out.sort_by_key(|s| (s.file.clone(), s.start_line, s.end_line));
    out
}

/// 位置参数守卫：`app.get(path, mw1, mw2, handler)`——`path_arg` 之后到 `handler_arg`
/// （不含）之间的实参都是中间件；`handler_arg` 缺省时把**最后一个**实参当 handler 丢弃。
fn extract_positional_guards(spec: &RouteGuardSpec, calls: &[CallRecord]) -> Vec<RouteGuardScope> {
    let mut scopes: HashMap<(String, u32), RouteGuardScope> = HashMap::new();
    for call in calls {
        let Some(rc) = spec.route_calls.iter().find(|rc| match_route_call(call, rc).is_some())
        else {
            continue;
        };
        let path_arg = rc.path_arg;
        let start = path_arg + 1;
        if start >= call.args.len() {
            continue;
        }
        // 有 handler_arg 时取 [start, handler_arg)，否则取 [start, len-1)（丢最后一个当 handler）。
        let end = if rc.handler_arg.is_some() {
            rc.handler_arg.unwrap()
        } else {
            call.args.len().saturating_sub(1)
        };
        if start >= end {
            continue;
        }
        let mut guards: Vec<RouteGuard> = Vec::new();
        for a in &call.args[start..end] {
            if let Some(name) = guard_arg_name(a, rc.accept_identifier) {
                guards.push(RouteGuard {
                    class: name,
                    arg: None,
                });
            }
        }
        if guards.is_empty() {
            continue;
        }
        let key = (call.file.clone(), call.span.start_byte);
        let scope = scopes.entry(key).or_insert_with(|| RouteGuardScope {
            file: call.file.clone(),
            start_line: call.span.start_line,
            end_line: call.span.end_line,
            guards: Vec::new(),
        });
        for g in guards {
            if !scope.guards.iter().any(|x| x.class == g.class) {
                scope.guards.push(g);
            }
        }
    }
    let mut out: Vec<RouteGuardScope> = scopes.into_values().collect();
    out.sort_by_key(|s| (s.file.clone(), s.start_line, s.end_line));
    out
}

/// 消费者式挂载（NestJS `MiddlewareConsumer`）：
/// ```ts
/// // user.module.ts
/// consumer.apply(AuthMiddleware).forRoutes({ path: '*', method: RequestMethod.ALL });
/// ```
/// 中间件在**模块**里声明，作用在**控制器**文件的路由上 —— 两者既不同 owner、
/// 也没有 handler 实参可循，故：
/// * 挂载点：`receiver == consumer` 且 `method == apply` 的调用，守卫名取自实参；
/// * 作用范围：同一条链上 `forRoutes` 的实参。显式路径按路径匹配；命中通配时按
///   FKB 声明的 [`gt_domain::model::ConsumerScope`] 展开（默认"与模块同目录"）。
fn extract_consumer_guards(
    spec: &RouteGuardSpec,
    cg: &gt_domain::model::ConsumerGuardSpec,
    calls: &[CallRecord],
) -> Vec<RouteGuardScope> {
    // 路由定义调用（@Get/@Post…），附带解析出的 (动词, 路径)，供下方按路径 + 动词匹配。
    let route_entries: Vec<(&CallRecord, String, String)> = calls
        .iter()
        .filter_map(|c| {
            spec.route_calls
                .iter()
                .find_map(|rc| match_route_call(c, rc).map(|(v, p, _)| (c, v, p)))
        })
        .collect();
    if route_entries.is_empty() {
        return Vec::new();
    }
    let is_consumer = |c: &CallRecord| {
        c.receiver
            .as_deref()
            .map(|r| r.to_ascii_lowercase().contains(&cg.receiver.to_ascii_lowercase()))
            .unwrap_or(false)
    };

    let mut out = Vec::new();
    for call in calls {
        if !is_consumer(call) || call.method.as_deref() != Some(cg.apply_method.as_str()) {
            continue;
        }
        // 守卫名：`apply(X)` / `apply(A, B)` —— 与位置参数写法同款取值。
        let mut guards: Vec<RouteGuard> = Vec::new();
        for a in &call.args {
            if let Some(name) = guard_arg_name(a, true) {
                guards.push(RouteGuard { class: name, arg: None });
            }
        }
        if guards.is_empty() {
            continue;
        }
        // 同一条链（同 file + start_byte）上的 `forRoutes(...)` 决定作用范围。
        // 实参可以是：
        //   * 字符串：`forRoutes('user')` —— 只给路径，动词不限；
        //   * 对象字面量：`forRoutes({ path: 'user', method: RequestMethod.GET })` —— 解析出路径与动词；
        //   * 通配：`forRoutes('*')` —— 按 `scope` 展开（默认"与模块同目录"）。
        // 解析器把对象字面量落成 `FactValue::Array` 的 (key, value) 序列，
        // 故这里也认 `Array`（实测 NestJS realworld 全用的对象字面量写法）。
        let targets: Vec<(String, Option<String>)> = calls
            .iter()
            .filter(|c| {
                c.file == call.file
                    && c.span.start_byte == call.span.start_byte
                    && c.method.as_deref() == Some(cg.for_routes_method.as_str())
            })
            .flat_map(|c| c.args.iter().filter_map(parse_for_route_target))
            .filter(|(p, _)| !p.is_empty())
            .collect();
        let wildcard = targets.is_empty()
            || targets
                .iter()
                .any(|(p, _)| cg.wildcards.iter().any(|w| p == w));

        let matched: Vec<&CallRecord> = if wildcard {
            match cg.scope {
                gt_domain::model::ConsumerScope::ExplicitOnly => Vec::new(),
                gt_domain::model::ConsumerScope::All => {
                    route_entries.iter().map(|(rc, _, _)| *rc).collect()
                }
                gt_domain::model::ConsumerScope::Directory => {
                    let dir = match call.file.rfind('/') {
                        Some(i) => call.file[..i].to_string(),
                        None => String::new(),
                    };
                    route_entries
                        .iter()
                        .filter(|(rc, _, _)| match rc.file.rfind('/') {
                            Some(i) => rc.file[..i] == dir,
                            None => dir.is_empty(),
                        })
                        .map(|(rc, _, _)| *rc)
                        .collect()
                }
            }
        } else {
            // 显式路径：按 (路径前缀 + 动词) 匹配。
            // `forRoutes({path:'articles', method:POST})` 只覆盖 POST 版，
            // 不会误挂到同路径的 GET 路由（动词维度由 `RequestMethod.*` 还原）。
            route_entries
                .iter()
                .filter(|(_, verb, path)| {
                    targets.iter().any(|(tp, tv)| {
                        let tp = tp.trim().trim_start_matches('/');
                        let path_ok = !tp.is_empty() && (*path == tp || path.starts_with(&format!("{}/", tp)));
                        let verb_ok = match tv {
                            None => true,
                            Some(v) => v.eq_ignore_ascii_case(verb),
                        };
                        path_ok && verb_ok
                    })
                })
                .map(|(rc, _, _)| *rc)
                .collect()
        };

        for rc in matched {
            out.push(RouteGuardScope {
                file: rc.file.clone(),
                start_line: rc.span.start_line,
                end_line: rc.span.end_line,
                guards: guards.clone(),
            });
        }
    }
    out.sort_by_key(|s| (s.file.clone(), s.start_line, s.end_line));
    out
}

/// 解析 `forRoutes` 的一个实参，取出 `(路径, 动词)`。
///
/// * 字符串实参：`forRoutes('user')` → `("user", None)`（动词不限）；
/// * 对象字面量：`forRoutes({ path: 'user', method: RequestMethod.GET })` →
///   `("user", Some("GET"))`；`RequestMethod.ALL` 视为动词不限（`None`）；
/// * 其余（`Unknown` 文本）尽力按 `path: '...'` 形式抽取，抽不到则返回 `None`。
fn parse_for_route_target(a: &FactValue) -> Option<(String, Option<String>)> {
    match a {
        FactValue::String(s) => Some((s.trim().to_string(), None)),
        FactValue::Array(items) => {
            let mut path = None;
            let mut verb = None;
            for (k, v) in items {
                match k.to_ascii_lowercase().as_str() {
                    "path" => path = v.as_str().map(|s| s.trim().to_string()),
                    "method" => verb = request_method_verb(v),
                    _ => {}
                }
            }
            path.map(|p| (p, verb))
        }
        FactValue::Unknown(Some(n)) if n.contains("path") => {
            // 兜底：从 `{path: 'x'}` 形式的文本里抽路径（不引入正则依赖）。
            let after = n.split_once("path")?.1;
            let quote = after.find('\'').or_else(|| after.find('"'))?;
            let rest = &after[quote + 1..];
            let end = rest.find(rest.chars().next()?)?;
            let p = rest[..end].trim().to_string();
            if p.is_empty() {
                None
            } else {
                Some((p, None))
            }
        }
        _ => None,
    }
}

/// 把 `RequestMethod.GET` / `'GET'` / `ALL` 这类写法归一成 HTTP 动词（`GET`/`POST`…）。
///
/// 返回 `None` 表示"不限动词"（`RequestMethod.ALL` / 空），由调用方按路径维度匹配。
fn request_method_verb(v: &FactValue) -> Option<String> {
    let s = match v {
        FactValue::String(s) | FactValue::Unknown(Some(s)) => s.clone(),
        _ => return None,
    };
    let last = s.trim().rsplit('.').next().unwrap_or(s.trim()).trim().to_ascii_uppercase();
    if last.is_empty() || last == "ALL" {
        return None;
    }
    Some(last)
}

/// 装饰器 / 注解守卫：`@UseGuards(X)` / `@login_required` / `@PreAuthorize` 落在与被修饰
/// 路由**同一方法**上（parser 已把装饰器捕捉成调用、以被修饰方法的 `owner_fqn` 作为 owner），
/// 故按 `owner_fqn` 关联：某 owner 上的所有守卫装饰器，挂到该 owner 上的每条路由装饰器。
///
/// 「哪些调用算路由定义」两条路都认，覆盖两种框架形态：
/// * `route_decorators` 名单（按 callee）—— NestJS 的 `@Get`、Spring 的 `@GetMapping`；
/// * `route_calls` 模式（按 receiver + 动词，返回非空即算）—— Python 的
///   `app.route(...)` / `router.get(...)` 这类"装饰器写法但实际是普通调用"的路由，
///   守卫却挂在同一函数的另一个装饰器上，故守卫仍按 owner 关联。
fn extract_decorator_guards(
    spec: &RouteGuardSpec,
    d: &gt_domain::model::DecoratorGuardSpec,
    calls: &[CallRecord],
) -> Vec<RouteGuardScope> {
    // 形态约束：只认"看起来像装饰器"的调用点，避免把普通方法调用当成守卫。
    let shape_ok = |c: &CallRecord| -> bool {
        if d.require_at_prefix && !c.callee.starts_with('@') {
            return false;
        }
        if d.require_no_receiver && c.receiver.is_some() {
            return false;
        }
        true
    };
    // 只把 `@` 前缀约束作用于路由（TS 装饰器都有 `@`）；`require_no_receiver`
    // **不能**用于路由 —— Python 的路由是 `app.route(...)`，本来就有接收者。
    let route_shape_ok = |c: &CallRecord| -> bool {
        !d.require_at_prefix || c.callee.starts_with('@')
    };
    let is_route = |c: &CallRecord| {
        !c.owner_fqn.is_empty()
            && route_shape_ok(c)
            && (d
                .route_decorators
                .iter()
                .any(|x| short_callee(&c.callee).eq_ignore_ascii_case(x))
                || spec.route_calls.iter().any(|rc| match_route_call(c, rc).is_some()))
    };
    // 守卫名正则（FKB 声明）：JS / Python 的守卫常是项目自写的装饰器，穷举名字不现实，
    // 故按"什么样的名字算守卫"匹配。编译一次复用；非法正则跳过（不因此崩掉整条装载）。
    let patterns: Vec<regex::Regex> = d
        .guard_name_patterns
        .iter()
        .filter_map(|p| regex::Regex::new(&format!("(?i){}", p)).ok())
        .collect();
    // 排除模式（deny）**优先于**包含模式：宁可漏，不可把文档装饰器当成鉴权守卫。
    let excludes: Vec<regex::Regex> = d
        .guard_exclude_patterns
        .iter()
        .filter_map(|p| regex::Regex::new(&format!("(?i){}", p)).ok())
        .collect();
    let is_guard = |c: &CallRecord| {
        if c.owner_fqn.is_empty() {
            return false;
        }
        // 形态约束：必须是"看起来像装饰器 / 注解"的调用点，
        // 否则 `this.userService.generateJWT(...)` 这类普通业务方法会被当成守卫。
        if !shape_ok(c) {
            return false;
        }
        let name = short_callee(&c.callee);
        if excludes.iter().any(|re| re.is_match(name)) {
            return false;
        }
        d.guard_decorators
            .iter()
            .any(|x| name.eq_ignore_ascii_case(x))
            || patterns.iter().any(|re| re.is_match(name))
    };
    // 一个守卫装饰器 → 它的守卫名（可能多个：数组形式 `@UseGuards(A, B)`）。
    let guards_of = |c: &CallRecord| -> Vec<RouteGuard> {
        let decorator_name = || RouteGuard {
            class: short_callee(&c.callee).to_string(),
            arg: None,
        };
        // Spring 的 `@PreAuthorize("hasRole('ADMIN')")`：实参是 SpEL 表达式，
        // 真正的守卫是注解本身 —— 由 `name_from_args: false` 声明。
        if !d.name_from_args {
            return vec![decorator_name()];
        }
        // 排除模式同样作用于**推导出来的守卫名**：实参里可能是局部变量
        // （实测 `_user` 被当成中间件），deny 优先。
        let keep = |name: &str| !excludes.iter().any(|re| re.is_match(name));
        let from_args: Vec<RouteGuard> = c
            .args
            .iter()
            .filter_map(|a| match a {
                FactValue::String(s) | FactValue::ClassConst(s) if !s.trim().is_empty() => {
                    Some(s.trim().to_string())
                }
                FactValue::Unknown(Some(n)) => {
                    // 标识符实参：取调用名（`(` 之前），如 `AuthGuard('jwt')` → `AuthGuard`
                    let base = n.split('(').next().unwrap_or(n).trim().to_string();
                    if base.is_empty() { None } else { Some(base) }
                }
                _ => None,
            })
            .filter(|n| keep(n))
            .map(|n| RouteGuard { class: n, arg: None })
            .collect();
        // 无参装饰器（`@login_required`）：装饰器名本身就是守卫。
        if from_args.is_empty() {
            let name = decorator_name();
            if keep(&name.class) { vec![name] } else { vec![] }
        } else {
            from_args
        }
    };

    let route_calls: Vec<&CallRecord> = calls.iter().filter(|c| is_route(c)).collect();
    let guard_calls: Vec<&CallRecord> = calls.iter().filter(|c| is_guard(c)).collect();
    if route_calls.is_empty() || guard_calls.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for rc in route_calls {
        // 守卫来源①：**同一方法 / 函数**（owner_fqn 相同）
        let mut guards: Vec<RouteGuard> = guard_calls
            .iter()
            .filter(|g| g.owner_fqn == rc.owner_fqn)
            .flat_map(|g| guards_of(g))
            .collect();
        // 守卫来源②：**类级**装饰器 / 注解（打在类上，作用于该类所有路由方法）。
        //
        // 两个候选类名都要试：
        //   * `owner_class`（解析器透传的类 FQN）—— 有就最准；
        //   * 从方法 owner 推导：`CatsController.findOne` → `CatsController`、
        //     `com.x.Ctrl.method` → `com.x.Ctrl`。TS / Java 的装饰器调用点常常
        //     **没有**回填 `owner_class`，只靠它会漏掉类级 `@UseGuards` / `@PreAuthorize`
        //     （实测 NestJS `@Controller` 类上的守卫整段丢失）。
        if d.include_class_level {
            let mut class_names: Vec<&str> = Vec::new();
            if let Some(cls) = rc.owner_class.as_deref() {
                class_names.push(cls);
            }
            if let Some(pos) = rc.owner_fqn.rfind(['.', ':', '\\']) {
                let derived = &rc.owner_fqn[..pos];
                if !derived.is_empty() {
                    class_names.push(derived);
                }
            }
            for cls in class_names {
                let from_class: Vec<RouteGuard> = guard_calls
                    .iter()
                    .filter(|g| g.owner_fqn == cls)
                    .flat_map(|g| guards_of(g))
                    .collect();
                for g in from_class {
                    if !guards.iter().any(|x| x.class == g.class) {
                        guards.push(g);
                    }
                }
            }
        }
        // 守卫来源③：**按 handler 实参跨文件关联**（Django：路由在 `urls.py`、
        // 守卫装饰在 `views.py` 的视图函数上，两者 owner 不同）。
        if let Some(arg_idx) = d.link_via_handler_arg {
            // handler 可能是字面量（`path("x", "pkg.mod.view")`）也可能是**标识符**
            // （`path("x", views.profile)` → `Unknown(Some("views.profile"))`），两种都要取。
            let handler = rc
                .args
                .get(arg_idx)
                .and_then(|a| match a {
                    FactValue::String(s) | FactValue::ClassConst(s) => Some(s.as_str()),
                    FactValue::Unknown(Some(n)) => Some(n.as_str()),
                    _ => None,
                })
                .unwrap_or_default()
                .trim()
                .trim_start_matches('.')
                .to_string();
            if !handler.is_empty() {
                let hl = handler.to_ascii_lowercase();
                let from_handler: Vec<RouteGuard> = guard_calls
                    .iter()
                    .filter(|g| {
                        let gl = g.owner_fqn.to_ascii_lowercase();
                        gl == hl || gl.ends_with(&format!(".{}", hl))
                    })
                    .flat_map(|g| guards_of(g))
                    .collect();
                for g in from_handler {
                    if !guards.iter().any(|x| x.class == g.class) {
                        guards.push(g);
                    }
                }
            }
        }
        if guards.is_empty() {
            continue;
        }
        out.push(RouteGuardScope {
            file: rc.file.clone(),
            start_line: rc.span.start_line,
            end_line: rc.span.end_line,
            guards,
        });
    }
    out.sort_by_key(|s| (s.file.clone(), s.start_line, s.end_line));
    out
}

/// 中间件别名表：**文件名、块标记、键值分隔符全部由 FKB 声明**（`params`）。
///
/// 路由上写的常常是**别名**（Laravel 的 `->middleware('auth')`），真实类名在别处登记。
/// 不还原就只知道"过了一个叫 auth 的东西"，连不上真实类节点。
///
/// # 为什么必须由 FKB 声明
///
/// 这三样**全是框架 / 版本约定**，内核不该认识其中任何一个：
/// * 文件名：Laravel 是 `app/Http/Kernel.php`，别的框架可能根本没有这个文件；
/// * 块标记：Laravel ≤9 叫 `$routeMiddleware`，**10.x 改叫 `$middlewareAliases`**
///   （实测 10.x 骨架就是后者 —— 硬编码前者会整表落空）；
/// * 分隔符：PHP 是 `=>`，别的语言可能是 `:` / `=`。
///
/// 故 FKB 给 `paths` / `markers`（**可多个，逐个尝试**）/ `end` / `separator` / `extensions`，
/// 内核只做"按声明扫描固定写法"这一件事 —— 与 `load_nginx` 同款：
/// **宁可只认固定写法，也不引入一整套解析**。
fn load_middleware_aliases(ctx: &mut PipelineContext, project_root: &Path, params: &Value) {
    let strs = |k: &str| -> Vec<String> {
        params
            .get(k)
            .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
            .unwrap_or_default()
    };
    let paths = strs("paths");
    if paths.is_empty() {
        return;
    }
    let markers = strs("markers");
    let end = params
        .get("end")
        .and_then(|v| v.as_str())
        .unwrap_or("];")
        .to_string();
    let separator = params
        .get("separator")
        .and_then(|v| v.as_str())
        .unwrap_or("=>")
        .to_string();
    let mut exts = strs("extensions");
    if exts.is_empty() {
        exts.push("php".into());
    }
    let exts: Vec<&str> = exts.iter().map(|s| s.as_str()).collect();

    for (path, text) in scan_text_files(project_root, &exts) {
        // `**/X` 与 `X` 都按"路径以此结尾"匹配。
        let hit = paths.iter().any(|p| path.ends_with(p.trim_start_matches("**/")));
        if !hit {
            continue;
        }
        // 只取指定块：同一个文件里还有 `$middleware` / `$middlewareGroups`，
        // 它们的键含义完全不同（`$middlewareGroups` 的 `web` / `api` 是组名不是中间件名）。
        // markers 为空时按整份文件扫。
        let block: String = if markers.is_empty() {
            text.clone()
        } else {
            match markers.iter().find_map(|m| text.split(m.as_str()).nth(1)) {
                Some(b) => b.to_string(),
                None => continue,
            }
        };
        let block = block.split(&end).next().unwrap_or(&block);
        for line in block.lines() {
            let line = line.trim();
            let Some((key, rest)) = line.split_once(&separator) else {
                continue;
            };
            let key = key.trim().trim_matches('\'').trim_matches('"').trim();
            let class = rest
                .trim()
                .trim_end_matches(',')
                .trim()
                .trim_end_matches("::class")
                .trim()
                .trim_start_matches('\\')
                .trim();
            if key.is_empty() || class.is_empty() || !class.contains('\\') {
                continue;
            }
            ctx.ws.put_symbol(
                ctx.project.id,
                MIDDLEWARE_ALIASES,
                key,
                json!({ "class": class, "file": path }),
            );
        }
    }
}

/// 中间件别名表的符号表名。
const MIDDLEWARE_ALIASES: &str = "middleware_aliases";

/// 把挂载实参里的**别名**还原成类名（`auth` → `App\Http\Middleware\Authenticate`）。
///
/// 带命名空间分隔符的视为已经类名（`app\api\middleware\AuthToken` / `AuthToken::class`），
/// 原样返回；否则查别名表。查不到时**返回原名**（`throttle:60` 这类带参数写法的别名
/// 表里存的是 `throttle`，这里按冒号前的部分再查一次）。
fn resolve_guard_class(ctx: &PipelineContext, raw: &str, alias_table: Option<&str>) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.contains('\\') || raw.contains('/') {
        return Some(raw.trim_start_matches('\\').to_string());
    }
    let base = raw.split(':').next().unwrap_or(raw).trim();
    let table = alias_table.unwrap_or(MIDDLEWARE_ALIASES);
    if let Some(entry) = ctx
        .ws
        .symbols
        .get(table)
        .and_then(|t| t.get(base))
        .and_then(|v| v.get("class"))
        .and_then(|c| c.as_str())
    {
        return Some(entry.to_string());
    }
    // 查不到就不编造：返回 None，由调用方跳过（宁可缺不可猜）。
    if raw.contains('\\') {
        Some(raw.to_string())
    } else {
        None
    }
}

/// 取挂载实参的可读文本（`true` / `60` / `'auth:api'`…）；不可静态求值时返回 `None`。
fn guard_arg_text(v: &FactValue) -> Option<String> {
    match v {
        FactValue::String(s) | FactValue::ClassConst(s) => Some(s.clone()),
        FactValue::Bool(b) => Some(b.to_string()),
        FactValue::Int(i) => Some(i.to_string()),
        FactValue::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

fn load_routes(
    ctx: &mut PipelineContext,
    spec: &RouteGuardSpec,
    _sub: &gt_domain::model::SubProject,
    contract_steps: &[NormalizeStep],
) {
    // ① 路由组前缀（仅 `chain` 模型有 `group_method`）：`Route::group('v2', fn)` 的前缀
    // 要拼到组内每条路由路径上，否则契约 ID 会丢掉 `v2`、与真实请求路径及前端调用对不上。
    // 收集时遍历的是**全量**调用点，只需登记一次（后续子工程重复调用时跳过）。
    if ctx.ws.route_groups.is_empty() {
        let mut groups: Vec<RouteGroup> = Vec::new();
        for rc in &spec.route_calls {
            let Some(gm) = &rc.group_method else { continue };
            for call in ctx.ws.calls.iter() {
                if !receiver_matches(rc, call.receiver.as_deref().unwrap_or("")) {
                    continue;
                }
                if call.method.as_deref() != Some(gm.as_str()) {
                    continue;
                }
                // 无前缀写法：`group(fn)` 的 arg0 是闭包不是字符串，跳过。
                let Some(gt_domain::model::FactValue::String(prefix)) = call.args.first() else {
                    continue;
                };
                let prefix = prefix.trim().trim_matches('/').trim().to_string();
                if prefix.is_empty() {
                    continue;
                }
                groups.push(RouteGroup {
                    file: call.file.clone(),
                    start_line: call.span.start_line,
                    end_line: call.span.end_line,
                    prefix,
                });
            }
        }
        if !groups.is_empty() {
            info!("P3 路由组：{} 个带前缀的路由组", groups.len());
            ctx.ws.add_route_groups(groups);
        }
    }

    // FKB 授权「查不到节点的守卫也建成 Middleware 节点」时登记到工作区，供 P14 读取。
    // （JS / Python 的中间件是函数值、解析器不为它建语法节点，故需要这条授权。）
    if spec.synthesize_unresolved {
        ctx.ws.synthesize_unresolved_guards = true;
    }

    // ② 中间件挂载：**必须在 prefixes 之后登记**（下面建 key 时要查），且同样只登记一次。
    if ctx.ws.route_guard_scopes().is_empty() {
        let scopes = collect_route_guards(spec, &ctx.ws.calls);
        if !scopes.is_empty() {
            let total: usize = scopes.iter().map(|s| s.guards.len()).sum();
            info!("P3 路由守卫：{} 段区间 / {} 处中间件挂载", scopes.len(), total);
        }
        ctx.ws.add_route_guard_scopes(scopes);
    }

    // ③ 建 `route_list`：键 = `METHOD /归一化路径`，与 P5 合成的 `HttpContract.name` 逐字一致。
    let alias_table = spec.alias_table.as_deref();
    let mut found: Vec<(String, String, String, u32, Vec<RouteGuard>)> = Vec::new();
    for call in ctx.ws.calls.iter() {
        // 找匹配的路由定义（含 decorator 模型）：逐一试 `route_calls` 模式。
        let Some((method, path, handler)) =
            spec.route_calls.iter().find_map(|rc| match_route_call(call, rc))
        else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        // 键归一化照搬 P5 的 CONTRACT_ID 三步：① FKB http-contract 的 normalize；
        // ② 路由组前缀插到前导斜杠之后；③ 再整体过一遍同一套 normalize（幂等）。
        let steps = contract_steps;
        let normed = crate::normalize::apply_normalize(&path, steps);
        let prefix = ctx.ws.route_group_prefix(&call.file, call.span.start_line);
        let with_prefix = if prefix.is_empty() {
            normed
        } else {
            format!("/{}/{}", prefix, normed.trim_start_matches('/'))
        };
        let key = format!(
            "{} {}",
            method.to_uppercase(),
            crate::normalize::apply_normalize(&with_prefix, steps)
        );
        let guards = ctx.ws.route_guards(&call.file, call.span.start_line);
        found.push((key, handler.unwrap_or_default(), call.file.clone(), call.span.start_line, guards));
    }
    for (key, handler, file, line, guards) in found {
        let mut value = json!({ "handler": handler, "file": file, "line": line });
        // 别名 → 类：Laravel 在路由上写的是 `'auth'`，类名在 `Kernel::$routeMiddleware`。
        // 还原不了时**保留源码里写的那个名字**（`web` / `auth` 也可能是中间件**组名**，
        // 它确实是路由文件里写下的事实），只是连不到类节点 —— 结论区照样能读，
        // P14 建边时查不到节点就跳过。整条丢掉会让 Laravel 工程一点守卫信息都不剩。
        let guards: Vec<RouteGuard> = guards
            .into_iter()
            .map(|g| RouteGuard {
                class: resolve_guard_class(ctx, &g.class, alias_table)
                    .unwrap_or_else(|| g.class.clone()),
                arg: g.arg,
            })
            .collect();
        if !guards.is_empty() {
            value["guards"] = json!(guards
                .iter()
                .map(|g| json!({ "class": g.class, "arg": g.arg }))
                .collect::<Vec<_>>());
        }
        ctx.ws.put_symbol(ctx.project.id, "route_list", &key, value);
    }
}

fn load_nginx(
    ctx: &mut PipelineContext,
    _sub: &gt_domain::model::SubProject,
    project_root: &Path,
    _fs: &dyn FileSystem,
) {
    for (path, text) in scan_text_files(project_root, &["conf"]) {
        let mut server_name = String::new();
        let mut root_dir = String::new();
        let mut locations: Vec<Value> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("server_name") {
                server_name = rest.trim_end_matches(';').trim().to_string();
            } else if let Some(rest) = line.strip_prefix("root") {
                root_dir = rest.trim_end_matches(';').trim().to_string();
            } else if line.starts_with("location") {
                locations.push(json!({ "raw": line }));
            }
        }
        if server_name.is_empty() && root_dir.is_empty() && locations.is_empty() {
            continue;
        }
        ctx.ws.put_symbol(
            ctx.project.id,
            "nginx",
            &path,
            json!({
                "server_name": server_name,
                "root": root_dir,
                "locations": locations,
            }),
        );
    }
}

/// 声明式中间件列表：文件名 / 键 / 作用域**全由 FKB `params` 声明**——与 `middleware_aliases`、
/// `nginx_config` 同款思路，内核只做"按声明扫固定写法"，不认任何框架 / 语言。
///
/// 中间件常在**文件**里登记，而非路由调用 `->middleware()`：
/// * ThinkPHP 全局：`app/middleware.php` 返回裸类数组 `return [A::class, B::class];`；
/// * ThinkPHP 多应用：`app/<app>/config/route.php` 的 `'middleware' => [A::class, ...]`。
/// 这些写法不产生路由调用链，故 `guard_attach` 识别不到、`route_list` 的 `guards` 整段落空，
/// 图里一个中间件都没有（likeadmin 就是典型）。
///
/// 本装载器把"声明类名单 + 作用域"存进工作区 `declared_middleware` 符号表；**不直接建节点**
/// ——因为 HttpContract 节点（尤其是 likeadmin 这类自动路由 / 由 `frontend-http-contract`
/// 规则合成的契约）在 prepare 阶段还不存在。真正的节点 / `PassesThrough` 边由建图完成后的
/// 后置步骤（见 `gt-application::PipelineService::attach_declared_middleware`）在 HttpContract
/// 节点已齐全时按作用域挂出来，复用 P14 同款机理，内核零框架字符串。
///
/// # params
/// * `paths`：要扫的文件（后缀匹配，支持 `**/X`）。`per_app` 时支持单个 `*` 通配段
///   （如 `app/*/config/route.php`），匹配到的 `*` 段即应用名。
/// * `key`：可选。声明数组所在键；省略时按整份文件取第一个 `[...]` 数组
///   （即 `return [A::class, ...];` 形态）。
/// * `scope`：`global`（挂到全部路由，默认）或 `per_app`（只挂到名字含 `/<app>` 前缀的路由，
///   `prefix` 即 `/<app>`，从 `paths` 的 `*` 通配段抽取）。
fn load_declared_middleware(ctx: &mut PipelineContext, project_root: &Path, params: &Value) {
    let paths = params
        .get("paths")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_default();
    if paths.is_empty() {
        return;
    }
    let key = params.get("key").and_then(Value::as_str).map(|s| s.to_string());
    let scope = params
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("global");
    let mut exts = params
        .get("extensions")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_default();
    if exts.is_empty() {
        exts.push("php".into());
    }
    let exts: Vec<&str> = exts.iter().map(|s| s.as_str()).collect();

    let mut idx: u32 = 0;
    for (path, text) in scan_text_files(project_root, &exts) {
        let hit = paths
            .iter()
            .any(|p| declared_mw_path_matches(p, &path));
        if !hit {
            continue;
        }
        let classes = extract_middleware_classes(&text, key.as_deref());
        if classes.is_empty() {
            continue;
        }
        // `per_app` 时从 `paths` 里形如 `app/*/config/route.php` 的 `*` 段抽应用名
        // 作为路由前缀（如 `/adminapi`）—— 对齐结尾，不受 project_root 下多几层目录影响。
        let prefix: Option<String> = if scope == "per_app" {
            paths.iter().find_map(|p| {
                let dseg: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
                let aseg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
                if aseg.len() < dseg.len() {
                    return None;
                }
                let suffix = &aseg[aseg.len() - dseg.len()..];
                dseg.iter().zip(suffix).find_map(|(d, a)| {
                    if *d == "*" {
                        Some(format!("/{}", a))
                    } else {
                        None
                    }
                })
            })
        } else {
            None
        };
        // 存进工作区，等建图完成后的后置步骤（见
        // `gt-application::PipelineService::attach_declared_middleware`）按作用域挂链。
        // key 加 `scope` 前缀 + 文件序号，保证同一工程内多个声明文件互不覆盖
        // （`idx` 在每个 loader 调用内从 0 起算，不同 scope 必须靠前缀区分）。
        ctx.ws.put_symbol(
            ctx.project.id,
            "declared_middleware",
            &format!("{}_{}", scope, idx),
            json!({ "classes": classes, "scope": scope, "prefix": prefix }),
        );
        idx += 1;
        info!(
            "P3 声明式中间件：{} 个类（作用域 {:?}{}）",
            classes.len(),
            scope,
            prefix.map(|p| format!(", 前缀 {}", p)).unwrap_or_default()
        );
    }
}

/// 从文件文本抽取中间件类名列表。
///
/// * `key` 给出时（如 `middleware`）：取 `key => [ ... ]` 数组块；
///   注意 `key` 可能出现在命名空间里（如 `app\...\middleware\Foo`），故要求 key 后紧跟 `=>`
///   （跳过引号 / 空白）才认作赋值键，避免命中命名空间。
/// * 否则：取整份文件的第一个 `[ ... ]` 数组（即 `return [A::class, ...];` 形态）。
/// 每行取 `X::class`（剥 `//` 注释、尾逗号、前导 `\`），得到归一后的 FQN。
fn extract_middleware_classes(text: &str, key: Option<&str>) -> Vec<String> {
    let block = match key {
        Some(k) => {
            // 找「key => [」：遍历所有命中，取其后紧跟 `=>` 的那一处。
            let mut from = 0;
            let mut found: Option<usize> = None;
            while let Some(rel) = text[from..].find(k) {
                let abs = from + rel;
                let rest = &text[abs + k.len()..];
                let after = rest
                    .trim_start_matches(|c: char| c == '\'' || c == '"' || c.is_whitespace());
                if after.starts_with("=>") {
                    found = Some(abs);
                    break;
                }
                from = abs + k.len();
            }
            match found {
                Some(idx) => {
                    let after = &text[idx + k.len()..];
                    let after = after.trim_start().strip_prefix("=>").unwrap_or(after);
                    match after.find('[') {
                        Some(open) => {
                            let rest = &after[open..];
                            match rest.find(']') {
                                Some(close) => rest[1..close].to_string(),
                                None => return Vec::new(),
                            }
                        }
                        None => return Vec::new(),
                    }
                }
                None => return Vec::new(),
            }
        }
        None => match text.find('[') {
            Some(open) => {
                let bytes = text.as_bytes();
                let mut depth = 0i32;
                let mut close = None;
                for i in open..text.len() {
                    if bytes[i] == b'[' {
                        depth += 1;
                    } else if bytes[i] == b']' {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(i);
                            break;
                        }
                    }
                }
                match close {
                    Some(c) => text[open + 1..c].to_string(),
                    None => return Vec::new(),
                }
            }
            None => return Vec::new(),
        },
    };
    let mut out = Vec::new();
    for line in block.lines() {
        let line = line.split("//").next().unwrap_or(line).trim();
        let line = line.trim_end_matches(',').trim();
        if line.is_empty() {
            continue;
        }
        if let Some(c) = line.strip_suffix("::class") {
            let c = c.trim().trim_start_matches('\\').trim();
            if !c.is_empty() {
                out.push(c.to_string());
            }
        }
    }
    out
}

/// 路径后缀匹配（对齐结尾），支持 `*` 通配段（匹配恰好一个路径段）。
///
/// 用于 `declared_middleware` 的 `paths`：声明 `app/*/config/route.php` 能命中
/// `server/app/adminapi/config/route.php`（无论 project_root 下多几层目录），而
/// 不像 `engine::path_matches` 那样要求 pattern 从开头匹配 —— 自动路由项目的应用目录
/// 常藏在 `server/app/<app>` 这类子目录里。无 `*` 时退化为普通后缀相等。
fn declared_mw_path_matches(pattern: &str, path: &str) -> bool {
    let pseg: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let aseg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if aseg.len() < pseg.len() {
        return false;
    }
    let suffix = &aseg[aseg.len() - pseg.len()..];
    pseg.iter()
        .zip(suffix)
        .all(|(p, a)| *p == "*" || p == a)
}

/// 扫描指定扩展名的文本文件（跳过依赖目录）。
fn scan_text_files(root: &Path, exts: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut walker = walkdir::WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(
                name.as_str(),
                "vendor" | "node_modules" | "target" | ".git" | "dist" | "build" | "runtime"
            ) {
                walker.skip_current_dir();
            }
            continue;
        }
        let ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !exts.contains(&ext.as_str()) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            let rel = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, text));
        }
        if out.len() > 400 {
            break;
        }
    }
    out
}

/// 解析 `CREATE TABLE`：取表名与列名（按括号配对，避免被 `int(11)` 截断）。
fn parse_create_tables(sql: &str) -> Vec<(String, Vec<String>)> {
    let re = regex::Regex::new(
        r#"(?is)CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?[`"]?([\w.]+)[`"]?\s*\("#,
    )
    .unwrap();
    let mut out = Vec::new();
    for m in re.find_iter(sql) {
        let caps = re.captures(&sql[m.start()..]).unwrap();
        let table = match caps.get(1) {
            Some(t) => t.as_str().trim_matches(|c| c == '`' || c == '"').to_string(),
            None => continue,
        };
        // 从 `(` 起做括号配对
        let bytes = sql.as_bytes();
        let open = m.start() + m.as_str().len() - 1;
        let mut depth = 0i32;
        let mut close = None;
        let mut i = open;
        let mut in_quote: Option<u8> = None;
        while i < bytes.len() {
            let b = bytes[i];
            match in_quote {
                Some(q) => {
                    if b == q {
                        in_quote = None;
                    }
                }
                None => match b {
                    b'`' | b'"' | b'\'' => in_quote = Some(b),
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(i);
                            break;
                        }
                    }
                    _ => {}
                },
            }
            i += 1;
        }
        let Some(close) = close else { continue };
        let body = &sql[open + 1..close];
        out.push((table, split_columns(body)));
    }
    out
}

/// 拆分 `CREATE TABLE` 字段列表的第一层逗号。
fn split_columns(body: &str) -> Vec<String> {
    let mut columns = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut in_quote: Option<u8> = None;
    for b in body.bytes() {
        match in_quote {
            Some(q) => {
                current.push(b as char);
                if b == q {
                    in_quote = None;
                }
            }
            None => match b {
                b'`' | b'"' | b'\'' => {
                    current.push(b as char);
                    in_quote = Some(b);
                }
                b'(' => {
                    depth += 1;
                    current.push('(');
                }
                b')' => {
                    depth -= 1;
                    current.push(')');
                }
                b',' if depth == 0 => {
                    push_column(&mut columns, &current);
                    current.clear();
                }
                _ => current.push(b as char),
            },
        }
    }
    push_column(&mut columns, &current);
    columns
}

fn push_column(out: &mut Vec<String>, raw: &str) {
    let line = raw.trim().trim_end_matches(',');
    if line.is_empty() || line.starts_with("--") || line.starts_with('#') {
        return;
    }
    let upper = line.to_ascii_uppercase();
    for kw in [
        "PRIMARY KEY",
        "KEY ",
        "UNIQUE",
        "INDEX",
        "CONSTRAINT",
        "FOREIGN",
        "FULLTEXT",
        "CHECK",
        "SPATIAL",
    ] {
        if upper.starts_with(kw) {
            return;
        }
    }
    if let Some(first) = line.split_whitespace().next() {
        let col = first.trim_matches(|c| c == '`' || c == '"' || c == '[' || c == ']');
        if !col.is_empty() && col.len() < 64 && col.chars().all(|c| c.is_alphanumeric() || c == '_') {
            out.push(col.to_string());
        }
    }
}

/// 供 P5 判断配置条目文件是否匹配（含 `{app_root}` 占位展开）。
pub fn expand(path: &str, app_root: &str) -> String {
    path.replace("{app_root}", app_root)
}

/// 供测试使用。
pub fn _sub_id(id: SubProjectId) -> SubProjectId {
    id
}

/// `FactValue` 的文本化。
trait LossyText {
    fn to_string_lossy(&self) -> String;
}

impl LossyText for gt_domain::model::FactValue {
    fn to_string_lossy(&self) -> String {
        match self {
            gt_domain::model::FactValue::String(s) | gt_domain::model::FactValue::ClassConst(s) => s.clone(),
            other => format!("{other:?}"),
        }
    }
}

/// 规则去重（按 id）。
fn dedup_rules(rules: Vec<Rule>) -> Vec<Rule> {
    let mut out: Vec<Rule> = Vec::new();
    for r in rules {
        if !out.iter().any(|x| x.id == r.id) {
            out.push(r);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{collect_route_guards, guard_arg_text};
    use crate::workspace::{CallRecord, GraphWorkspace};
    use gt_adapter_fs::StdFileSystem;
    use gt_adapter_parser::DefaultParserRegistry;
    use gt_domain::model::{
        ChainGuardSpec, ConsumerGuardSpec, ConsumerScope, FactValue, GuardAttach, GuardAttachSpec,
        Language, NodeId, ProjectId, RouteCallSpec, RouteGuardSpec, RouteMatchBy, Span, SubProject,
        SubProjectId,
    };

    /// ThinkPHP 6 的 `route_guards` 声明（与 `fkb/php/thinkphp6.yaml` 等价的最小集），供测试复用。
    fn tp6_spec() -> RouteGuardSpec {
        RouteGuardSpec {
            route_calls: vec![RouteCallSpec {
                receiver: "Route".into(),
                by: RouteMatchBy::Receiver,
                verb_methods: [
                    ("get".into(), "GET".into()),
                    ("post".into(), "POST".into()),
                    ("put".into(), "PUT".into()),
                    ("delete".into(), "DELETE".into()),
                    ("patch".into(), "PATCH".into()),
                    ("any".into(), "ANY".into()),
                    ("rule".into(), "ANY".into()),
                ]
                .into_iter()
                .collect(),
                path_arg: 0,
                handler_arg: Some(1),
                group_method: Some("group".into()),
                receiver_ends_with: false,
                accept_identifier: false,
            }],
            guard_attach: GuardAttach::One(GuardAttachSpec::Chain(ChainGuardSpec {
                method: "middleware".into(),
                arg_index: 0,
                arg2_index: Some(1),
            })),
            alias_table: None,
            synthesize_unresolved: false,
        }
    }

    fn make_sub(root: PathBuf) -> SubProject {
        SubProject {
            id: SubProjectId::new(1),
            project_id: ProjectId::new(1),
            name: "test".into(),
            root_path: root,
            language: Language::new(Language::PHP),
            role: "backend".into(),
            detected_by: "composer.json".into(),
            frameworks: vec!["thinkphp6".into()],
            facts: serde_json::Value::Null,
        }
    }

    /// 造一个调用点：`Route::group('pc', fn)` / `Route::get('x','C@m')` / `->middleware(...)`。
    ///
    /// `byte` 是这条调用在源码里的**起始字节**：同一条链上的所有调用点共享它
    /// （`member_call_expression` 从根那一段源码开始），这是配对唯一的依据。
    fn call(
        file: &str,
        receiver: &str,
        method: &str,
        args: Vec<FactValue>,
        start_line: u32,
        end_line: u32,
        byte: u32,
        end_byte: u32,
    ) -> CallRecord {
        CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: "<file>".into(),
            owner_class: None,
            callee: format!("{}::{}", receiver, method),
            receiver: Some(receiver.into()),
            method: Some(method.into()),
            args,
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span {
                start_line,
                end_line,
                start_byte: byte,
                end_byte,
            },
            file: file.into(),
            sub: None,
            language: Language::new(Language::PHP),
        }
    }

    fn class(name: &str) -> FactValue {
        FactValue::ClassConst(name.into())
    }

    /// CRMEB 的真实写法：`Route::group('pc', fn){ ... })->middleware(A)->middleware(B, true)`，
    /// 组内有多条路由。中间件必须落到**每条**路由上。
    #[test]
    fn group_level_guard_covers_inner_routes() {
        let f = "app/api/route/pc.php";
        let calls = vec![
            // 根：`Route::group(function(){...})`（无前缀写法，跨 1..3 行），byte=10
            call(f, "Route", "group", vec![], 1, 3, 10, 200),
            call(f, "Route", "get", vec![FactValue::String("a".into())], 2, 2, 40, 90),
            // 链上的两次 `->middleware`：与根共享 start_byte=10，span 更长
            call(
                f,
                "Route",
                "middleware",
                vec![class(r"app\http\middleware\AllowOriginMiddleware")],
                1,
                4,
                10,
                230,
            ),
            call(
                f,
                "Route",
                "middleware",
                vec![
                    class(r"app\api\middleware\AuthTokenMiddleware"),
                    FactValue::Bool(true),
                ],
                1,
                5,
                10,
                260,
            ),
        ];
        let scopes = collect_route_guards(&tp6_spec(), &calls);
        assert_eq!(scopes.len(), 1, "一条链只应产出一段区间");
        assert_eq!((scopes[0].start_line, scopes[0].end_line), (1, 3), "区间必须取根的 span");
        assert_eq!(scopes[0].guards.len(), 2, "两个中间件都要收");

        let mut ws = GraphWorkspace::new(ProjectId::new(1));
        ws.add_route_guard_scopes(scopes);
        let got = ws.route_guards(f, 2);
        let names: Vec<&str> = got.iter().map(|g| g.class.as_str()).collect();
        assert_eq!(
            names,
            vec![
                r"app\http\middleware\AllowOriginMiddleware",
                r"app\api\middleware\AuthTokenMiddleware",
            ]
        );
        assert_eq!(got[1].arg.as_deref(), Some("true"), "`AuthToken::class, true` 的第二参数要留下");
        // 区间外的路由不该被它守着（这正是"全局 vs 组级"的分界）
        assert!(ws.route_guards(f, 40).is_empty());
    }

    /// 路由自带的 `->middleware(X::class, false)`：粒度细到单条路由，第二参数才会出现。
    #[test]
    fn route_level_guard_keeps_force_flag() {
        let f = "app/api/route/user.php";
        let calls = vec![
            call(
                f,
                "Route",
                "get",
                vec![FactValue::String("info".into())],
                7,
                7,
                10,
                60,
            ),
            call(
                f,
                "Route",
                "middleware",
                vec![
                    class(r"app\api\middleware\AuthTokenMiddleware"),
                    FactValue::Bool(false),
                ],
                7,
                8,
                10,
                90,
            ),
        ];
        let scopes = collect_route_guards(&tp6_spec(), &calls);
        assert_eq!(scopes.len(), 1);
        let mut ws = GraphWorkspace::new(ProjectId::new(1));
        ws.add_route_guard_scopes(scopes);
        let got = ws.route_guards(f, 7);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].arg.as_deref(), Some("false"));
    }

    /// Laravel 的**修饰符前置**写法：`Route::middleware('auth')->group(fn){...}`。
    /// 关键在根节点必须选 `group`（它才包着路由区间），而不是链首的 `middleware('auth')`。
    #[test]
    fn laravel_prefix_form_binds_to_group() {
        let f = "routes/web.php";
        let calls = vec![
            // 链首：`Route::middleware('auth')`，span 很短（只有这一小段）
            call(
                f,
                "Route",
                "middleware",
                vec![FactValue::String("auth".into())],
                1,
                1,
                10,
                40,
            ),
            // 链尾：`->group(fn){...}`，span 覆盖整个闭包（第 1~9 行）
            call(f, "Route", "group", vec![], 1, 9, 10, 400),
            // 组内的一条路由
            call(
                f,
                "Route",
                "get",
                vec![FactValue::String("profile".into())],
                3,
                3,
                90,
                130,
            ),
        ];
        let scopes = collect_route_guards(&tp6_spec(), &calls);
        assert_eq!(scopes.len(), 1, "整条链只应产出一段区间");
        assert_eq!(
            (scopes[0].start_line, scopes[0].end_line),
            (1, 9),
            "区间必须取 group 的 span（否则包不住组内路由）"
        );
        let mut ws = GraphWorkspace::new(ProjectId::new(1));
        ws.add_route_guard_scopes(scopes);
        let got = ws.route_guards(f, 3);
        assert_eq!(got.len(), 1, "组内路由应当被这个别名守卫覆盖");
        assert_eq!(got[0].class, "auth");
    }

    /// 数组形式的多个别名（`->middleware(['auth', 'throttle:60'])`）要分别收。
    #[test]
    fn collects_array_form_guards() {
        let f = "routes/web.php";
        let calls = vec![
            call(f, "Route", "get", vec![FactValue::String("x".into())], 1, 1, 10, 60),
            call(
                f,
                "Route",
                "middleware",
                vec![FactValue::Array(vec![
                    ("0".into(), FactValue::String("auth".into())),
                    ("1".into(), FactValue::String("throttle:60".into())),
                ])],
                1,
                2,
                10,
                90,
            ),
        ];
        let scopes = collect_route_guards(&tp6_spec(), &calls);
        assert_eq!(scopes[0].guards.len(), 2, "数组里每一项都算一次挂载");
    }

    /// 非 Route 接收者上的同名 `->middleware()` 不能误配；
    /// 非标量实参（`->middleware($v)`）不该产出伪事实。
    #[test]
    fn ignores_foreign_and_dynamic_middleware() {
        let f = "app/Service.php";
        let calls = vec![
            call(f, "$router", "middleware", vec![class("app\\A")], 1, 2, 10, 30),
            call(f, "Route", "get", vec![FactValue::String("x".into())], 3, 3, 50, 80),
            call(
                f,
                "Route",
                "middleware",
                vec![FactValue::Unknown(Some("$mw".into()))],
                3,
                4,
                50,
                100,
            ),
        ];
        assert!(collect_route_guards(&tp6_spec(), &calls).is_empty());
        assert_eq!(guard_arg_text(&FactValue::Int(60)), Some("60".into()));
        assert_eq!(guard_arg_text(&FactValue::Null), None);
    }

    /// Express 的位置参数写法：`app.get('/x', mw1, mw2, handler)`——path 之后的实参都是中间件。
    #[test]
    fn express_positional_guards() {
        let f = "routes/index.js";
        let rc = RouteCallSpec {
            receiver: "app".into(),
            by: RouteMatchBy::Receiver,
            verb_methods: [("get".into(), "GET".into()), ("post".into(), "POST".into())]
                .into_iter()
                .collect(),
            path_arg: 0,
            handler_arg: None, // 最后一个实参当作 handler 丢弃
            group_method: None,
            receiver_ends_with: false,
            accept_identifier: true,
        };
        let spec = RouteGuardSpec {
            route_calls: vec![rc],
            guard_attach: GuardAttach::One(GuardAttachSpec::Positional),
            alias_table: None,
            synthesize_unresolved: false,
        };
        let calls = vec![
            call(f, "app", "get", vec![
                FactValue::String("/cart".into()),
                class("AuthMiddleware"),
                class("CartOwnerMiddleware"),
                FactValue::String("handler".into()),
            ], 1, 1, 10, 80),
        ];
        let scopes = collect_route_guards(&spec, &calls);
        assert_eq!(scopes.len(), 1);
        let names: Vec<&str> = scopes[0].guards.iter().map(|g| g.class.as_str()).collect();
        assert_eq!(names, vec!["AuthMiddleware", "CartOwnerMiddleware"]);
        // 只有 `app.get` 收，裸 `router.use` 不带路径的不应误当路由守卫
        let calls2 = vec![call(f, "router", "use", vec![class("X")], 2, 2, 90, 120)];
        assert!(collect_route_guards(&spec, &calls2).is_empty());
    }

    /// 装饰器 / 注解写法：`@UseGuards(X)` 与被修饰路由 `@Get` 同 owner_fqn，按 owner 关联。
    #[test]
    fn decorator_guards_group_by_owner() {
        let f = "user.controller.ts";
        let d = gt_domain::model::DecoratorGuardSpec {
            route_decorators: vec!["Get".into(), "Post".into()],
            guard_decorators: vec!["UseGuards".into()],
            guard_name_patterns: vec![],
            guard_exclude_patterns: vec![],
            require_at_prefix: false,
            require_no_receiver: false,
            name_from_args: true,
            include_class_level: true,
            link_via_handler_arg: None,
        };
        let spec = RouteGuardSpec {
            route_calls: vec![RouteCallSpec {
                receiver: "Get".into(),
                by: RouteMatchBy::Callee,
                verb_methods: [("Get".into(), "GET".into()), ("Post".into(), "POST".into())]
                    .into_iter()
                    .collect(),
                path_arg: 0,
                handler_arg: None,
                group_method: None,
                receiver_ends_with: false,
                accept_identifier: false,
            }],
            guard_attach: GuardAttach::One(GuardAttachSpec::Decorator(d)),
            alias_table: None,
            synthesize_unresolved: false,
        };
        let mk = |callee: &str, owner: &str, arg: FactValue, line: u32| CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: owner.into(),
            owner_class: None,
            callee: callee.into(),
            receiver: None,
            method: None,
            args: vec![arg],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span { start_line: line, end_line: line, start_byte: line * 10, end_byte: line * 10 + 5 },
            file: f.into(),
            sub: None,
            language: Language::new(Language::PHP), // 仅占位，不影响装饰器匹配
        };
        let calls = vec![
            mk("Get", "UserController.profile", FactValue::String("/profile".into()), 10),
            mk("UseGuards", "UserController.profile", class("JwtAuthGuard"), 9),
            mk("Post", "UserController.update", FactValue::String("/update".into()), 20),
            mk("UseGuards", "UserController.update", class("AdminGuard"), 19),
        ];
        let scopes = collect_route_guards(&spec, &calls);
        assert_eq!(scopes.len(), 2, "每个被修饰的路由方法各出一段");
        let mut by_line: std::collections::HashMap<u32, Vec<String>> = scopes
            .iter()
            .map(|s| (s.start_line, s.guards.iter().map(|g| g.class.clone()).collect()))
            .collect();
        assert_eq!(by_line.remove(&10).unwrap(), vec!["JwtAuthGuard".to_string()]);
        assert_eq!(by_line.remove(&20).unwrap(), vec!["AdminGuard".to_string()]);
    }

    /// 一条「装饰器路由 + 消费者式中间件」的最小声明构造器（避免重复样板）。
    fn rc_callee(name: &str, verb: &str) -> RouteCallSpec {
        RouteCallSpec {
            receiver: name.into(),
            by: RouteMatchBy::Callee,
            verb_methods: [(name.to_ascii_lowercase(), verb.to_string())]
                .into_iter()
                .collect(),
            path_arg: 0,
            handler_arg: None,
            group_method: None,
            receiver_ends_with: false,
            accept_identifier: true,
        }
    }

    /// NestJS `MiddlewareConsumer`：`consumer.apply(AuthMiddleware).forRoutes({path, method})`
    /// 的**对象字面量**实参必须被解析成 (路径, 动词)，并按「路径 + 动词」落到对应路由。
    ///
    /// 复现 realworld 工程 `user.module.ts` 的写法 —— 此前 `forRoutes` 只认字符串 /
    /// `Unknown` 名，对象字面量被当成无名实参，整个模块 0 命中守卫。
    #[test]
    fn nestjs_consumer_object_literal_routes() {
        let consumer_spec = ConsumerGuardSpec {
            receiver: "consumer".into(),
            apply_method: "apply".into(),
            for_routes_method: "forRoutes".into(),
            wildcards: vec!["*".into()],
            scope: ConsumerScope::Directory,
        };
        let spec = RouteGuardSpec {
            route_calls: vec![
                rc_callee("@Get", "GET"),
                rc_callee("@Put", "PUT"),
                rc_callee("@Post", "POST"),
                rc_callee("@Delete", "DELETE"),
            ],
            guard_attach: GuardAttach::One(GuardAttachSpec::Consumer(consumer_spec)),
            alias_table: None,
            synthesize_unresolved: true,
        };
        let ctrl = "src/user/user.controller.ts";
        let module = "src/user/user.module.ts";
        let mk_route = |callee: &str, path: &str, line: u32| CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: "UserController".into(),
            owner_class: None,
            callee: callee.to_string(),
            receiver: None,
            method: Some(callee.trim_start_matches('@').to_string()),
            args: vec![FactValue::String(path.into())],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span {
                start_line: line,
                end_line: line,
                start_byte: line * 10,
                end_byte: line * 10 + 5,
            },
            file: ctrl.into(),
            sub: None,
            language: Language::new(Language::JAVASCRIPT),
        };
        // 整条链 `consumer.apply(AuthMiddleware).forRoutes(...)` 共享 start_byte=100。
        let apply = CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: "UserModule".into(),
            owner_class: None,
            callee: "consumer.apply".into(),
            receiver: Some("consumer".into()),
            method: Some("apply".into()),
            args: vec![FactValue::Unknown(Some("AuthMiddleware".into()))],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span {
                start_line: 17,
                end_line: 21,
                start_byte: 100,
                end_byte: 800,
            },
            file: module.into(),
            sub: None,
            language: Language::new(Language::JAVASCRIPT),
        };
        let for_routes = CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: "UserModule".into(),
            owner_class: None,
            callee: "consumer.apply.forRoutes".into(),
            receiver: Some("consumer.apply(AuthMiddleware)".into()),
            method: Some("forRoutes".into()),
            args: vec![
                FactValue::Array(vec![
                    ("path".into(), FactValue::String("user".into())),
                    ("method".into(), FactValue::Unknown(Some("RequestMethod.GET".into()))),
                ]),
                FactValue::Array(vec![
                    ("path".into(), FactValue::String("user".into())),
                    ("method".into(), FactValue::Unknown(Some("RequestMethod.PUT".into()))),
                ]),
            ],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span {
                start_line: 18,
                end_line: 21,
                start_byte: 100,
                end_byte: 800,
            },
            file: module.into(),
            sub: None,
            language: Language::new(Language::JAVASCRIPT),
        };
        let calls = vec![
            mk_route("@Get", "user", 21),
            mk_route("@Put", "user", 26),
            mk_route("@Post", "users", 32),
            mk_route("@Delete", "users/:slug", 37),
            apply,
            for_routes,
        ];
        let scopes = collect_route_guards(&spec, &calls);
        // 只应命中 'user' GET 与 'user' PUT 两条（动词维度筛掉了 POST / DELETE）。
        assert_eq!(scopes.len(), 2, "只应命中显式路径 + 动词匹配到的两条路由");
        let mut by_line: std::collections::HashMap<u32, Vec<String>> = scopes
            .iter()
            .map(|s| (s.start_line, s.guards.iter().map(|g| g.class.clone()).collect()))
            .collect();
        assert_eq!(
            by_line.remove(&21).unwrap(),
            vec!["AuthMiddleware".to_string()],
            "GET 'user'"
        );
        assert_eq!(
            by_line.remove(&26).unwrap(),
            vec!["AuthMiddleware".to_string()],
            "PUT 'user'"
        );
    }

    /// Django：`path('profile', views.profile)` 的路由与视图函数**分处两个文件**，
    /// 按 owner 关联必然落空 —— 靠 `link_via_handler_arg` 按 handler 实参跨文件接上。
    #[test]
    fn django_handler_link_guards() {
        let d = gt_domain::model::DecoratorGuardSpec {
            route_decorators: vec![],
            guard_decorators: vec!["login_required".into()],
            guard_name_patterns: vec![],
            guard_exclude_patterns: vec![],
            require_at_prefix: false,
            require_no_receiver: true,
            name_from_args: true,
            include_class_level: true,
            link_via_handler_arg: Some(1),
        };
        let spec = RouteGuardSpec {
            route_calls: vec![RouteCallSpec {
                receiver: "path".into(),
                by: RouteMatchBy::Callee,
                verb_methods: [("path".into(), "ANY".into())].into_iter().collect(),
                path_arg: 0,
                handler_arg: Some(1),
                group_method: None,
                receiver_ends_with: false,
                accept_identifier: true,
            }],
            guard_attach: GuardAttach::One(GuardAttachSpec::Decorator(d)),
            alias_table: None,
            synthesize_unresolved: false,
        };
        let mk = |callee: &str, owner: &str, args: Vec<FactValue>, file: &str, line: u32| CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: owner.into(),
            owner_class: None,
            callee: callee.into(),
            receiver: None,
            method: None,
            args,
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span { start_line: line, end_line: line, start_byte: line * 10, end_byte: line * 10 + 5 },
            file: file.into(),
            sub: None,
            language: Language::new(Language::PHP),
        };
        let calls = vec![
            // urls.py：路由，handler 是标识符 `views.profile`
            mk("path", "urls", vec![FactValue::String("profile".into()), FactValue::Unknown(Some("views.profile".into()))], "urls.py", 5),
            // views.py：守卫装饰器挂在视图函数上（owner 是 myapp.views.profile）
            mk("login_required", "myapp.views.profile", vec![], "views.py", 12),
            // 另一个视图的守卫不该串到 profile 上
            mk("login_required", "myapp.views.other", vec![], "views.py", 20),
        ];
        let scopes = collect_route_guards(&spec, &calls);
        assert_eq!(scopes.len(), 1);
        assert_eq!(
            scopes[0].guards.iter().map(|g| g.class.as_str()).collect::<Vec<_>>(),
            vec!["login_required"]
        );
    }

    /// 宽泛的包含模式不能把**文档装饰器**和**普通成员调用**当成守卫（deny + 形态约束）。
    #[test]
    fn decorator_guards_reject_docs_and_member_calls() {
        let d = gt_domain::model::DecoratorGuardSpec {
            route_decorators: vec!["@Get".into()],
            guard_decorators: vec!["@UseGuards".into()],
            // 宽泛模式（会命中 ApiBearerAuth 与 generateJWT）
            guard_name_patterns: vec!["(auth|jwt)".into()],
            guard_exclude_patterns: vec!["^@?api".into(), "^_".into()],
            require_at_prefix: true,
            require_no_receiver: false,
            name_from_args: true,
            include_class_level: true,
            link_via_handler_arg: None,
        };
        let spec = RouteGuardSpec {
            route_calls: vec![RouteCallSpec {
                receiver: "@Get".into(),
                by: RouteMatchBy::Callee,
                verb_methods: [("@get".into(), "GET".into())].into_iter().collect(),
                path_arg: 0,
                handler_arg: None,
                group_method: None,
                receiver_ends_with: false,
                accept_identifier: true,
            }],
            guard_attach: GuardAttach::One(GuardAttachSpec::Decorator(d)),
            alias_table: None,
            synthesize_unresolved: false,
        };
        let mk = |callee: &str, owner: &str, recv: Option<&str>, line: u32| CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: owner.into(),
            owner_class: None,
            callee: callee.into(),
            receiver: recv.map(|r| r.into()),
            method: None,
            args: vec![],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span { start_line: line, end_line: line, start_byte: line * 10, end_byte: line * 10 + 5 },
            file: "c.ts".into(),
            sub: None,
            language: Language::new(Language::PHP),
        };
        let calls = vec![
            mk("@Get", "C.f", None, 10),
            mk("@UseGuards", "C.f", None, 9),   // 真守卫（带 @）
            mk("@ApiBearerAuth", "C.f", None, 8), // 文档装饰器 → 排除
            mk("generateJWT", "C.f", Some("this.userService"), 11), // 成员调用 → 排除
        ];
        let scopes = collect_route_guards(&spec, &calls);
        assert_eq!(scopes.len(), 1);
        assert_eq!(
            scopes[0].guards.iter().map(|g| g.class.as_str()).collect::<Vec<_>>(),
            vec!["@UseGuards"],
            "只剩真守卫：文档装饰器与成员调用都不该算"
        );
    }

    /// 内圈覆盖外圈：组上 `AuthToken(true)`、路由上 `AuthToken(false)` 时生效的应是后者。
    #[test]
    fn inner_guard_overrides_outer() {
        let f = "app/api/route/pc.php";
        let mut ws = GraphWorkspace::new(ProjectId::new(1));
        ws.add_route_guard_scopes(vec![crate::workspace::RouteGuardScope {
            file: f.into(),
            start_line: 1,
            end_line: 30,
            guards: vec![crate::workspace::RouteGuard {
                class: r"app\Auth".into(),
                arg: Some("true".into()),
            }],
        }]);
        ws.add_route_guard_scopes(vec![crate::workspace::RouteGuardScope {
            file: f.into(),
            start_line: 5,
            end_line: 6,
            guards: vec![crate::workspace::RouteGuard {
                class: r"app\Auth".into(),
                arg: Some("false".into()),
            }],
        }]);
        let got = ws.route_guards(f, 5);
        assert_eq!(got.len(), 1, "同名中间件只算一个");
        assert_eq!(got[0].arg.as_deref(), Some("false"), "内层实参覆盖外层");
    }

    #[test]
    fn detects_table_prefix_from_php_config() {
        let dir = std::env::temp_dir().join(format!("em_test_prefix_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("config"));
        std::fs::write(
            dir.join("config/database.php"),
            "<?php\nreturn [\n    'default' => 'mysql',\n    'connections' => [\n        'mysql' => [\n            'prefix' => 'eb_',\n        ],\n    ],\n];",
        )
        .unwrap();
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let sub = make_sub(dir.clone());
        let got = super::resolve_manifest_php(
            &sub,
            &dir,
            "config/database.php",
            "connections.mysql.prefix",
            &fs,
            &parsers,
        );
        assert_eq!(
            got,
            Some((
                "eb_".to_string(),
                format!("php config: {}", dir.join("config/database.php").display())
            )),
            "应从 config/database.php 的 connections.mysql.prefix 读出表前缀"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_prefix_when_config_missing() {
        let dir = std::env::temp_dir().join(format!("em_test_noprefix_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let sub = make_sub(dir.clone());
        let got = super::resolve_manifest_php(
            &sub,
            &dir,
            "config/database.php",
            "connections.mysql.prefix",
            &fs,
            &parsers,
        );
        assert!(got.is_none(), "配置文件缺失时不应探测到前缀");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_prefix_from_env_default() {
        let dir = std::env::temp_dir().join(format!("em_test_envprefix_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("config"));
        std::fs::write(
            dir.join("config/database.php"),
            "<?php\nreturn [\n    'connections' => [\n        'mysql' => [\n            'prefix' => env('DB_PREFIX', 'eb_'),\n        ],\n    ],\n];",
        )
        .unwrap();
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let sub = make_sub(dir.clone());
        let got = super::resolve_manifest_php(
            &sub,
            &dir,
            "config/database.php",
            "connections.mysql.prefix",
            &fs,
            &parsers,
        );
        assert_eq!(
            got.map(|(v, _)| v),
            Some("eb_".to_string()),
            "应从 env('DB_PREFIX', 'eb_') 的默认值读出表前缀"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
