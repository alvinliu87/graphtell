//! P3 Prepare：装载 FKB 与权威符号表。
//!
//! * 用 FKB 的 `detectors` 识别每个子工程用了哪些框架
//! * 用 `root_rules` 解析 `AppRoot`（如从 `composer.json` 的 `autoload.psr-4`）
//! * 用 `loaders` 装载权威源：`schema` / `config_keys` / `i18n` / `facade_map`
//!   / `container_bindings` / `event_listeners` / `route_list` / `nginx`

use std::path::Path;

use em_domain::error::Result;
use em_domain::model::{
    Detector, FrameworkKnowledge, KnowledgeScope, Language, Phase, PickStrategy, Rule,
    SubProjectId,
};
use em_domain::port::{FileSystem, KnowledgeProvider, ParserRegistry};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::context::PipelineContext;
use crate::engine::{capture_locale, path_matches};
use crate::normalize::strip_prefixes;
use crate::workspace::RouteGroup;

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

    for sub in &subs {
        // 框架级 + 项目级知识分别识别：项目级仅在该子工程被识别为对应项目时加载，
        // 其规则只进 `rules_by_sub`（不进 `ctx.frameworks`、不进全局），绝不串味到其它工程。
        let frameworks = detect_frameworks(kb, fs, sub, &project_root, KnowledgeScope::Framework);
        info!("子工程 {} 识别到框架: {:?}", sub.name, frameworks);
        ctx.frameworks.insert(sub.id.get(), frameworks.clone());

        let projects = detect_frameworks(kb, fs, sub, &project_root, KnowledgeScope::Project);
        if !projects.is_empty() {
            info!("子工程 {} 识别到项目知识: {:?}", sub.name, projects);
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
        // ② 同语言、未显式识别的**框架级**规则（只加"同语言"，避免把 Uni-app 规则套到 PHP）；
        // ③ 本子工程识别到的**项目级**规则（仅检测到的，不外溢）。
        // （只加"同语言"而不是全部，避免把 Uni-app 规则套到 PHP 调用上）
        let mut rules: Vec<Rule> = frameworks
            .iter()
            .filter_map(|id| kb.by_id(id))
            .flat_map(|fk| fk.rules.iter().cloned())
            .collect();
        for fk in kb.all() {
            if fk.scope == KnowledgeScope::Framework
                && fk.language == sub.language
                && !frameworks.contains(&fk.id)
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
    let mut global: Vec<Rule> = Vec::new();
    for fk in kb.all() {
        if fk.scope != KnowledgeScope::Framework {
            continue;
        }
        for r in &fk.rules {
            if !global.iter().any(|g| g.id == r.id) {
                global.push(r.clone());
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
    sub: &em_domain::model::SubProject,
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
    sub: &em_domain::model::SubProject,
    facts: &mut serde_json::Map<String, Value>,
    phase: &Phase,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
) {
    for rule in &fk.root_rules {
        let resolved = match &rule.source {
            em_domain::model::RootSource::ManifestJson { manifest, pointer, pick } => {
                let path = sub.root_path.join(manifest);
                let path = if path.exists() { path } else { ctx.project.root_path.join(manifest) };
                resolve_manifest_pointer(&path, pointer, *pick)
            }
            em_domain::model::RootSource::DirectoryExists { path } => {
                if sub.root_path.join(path).exists() {
                    Some((path.clone(), format!("directory exists: {}", path)))
                } else {
                    None
                }
            }
            em_domain::model::RootSource::ManifestPhp { manifest, pointer } => {
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
                            em_domain::model::Severity::Warning,
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
    sub: &em_domain::model::SubProject,
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
    let parser = parsers.parser_for(&Language::new(Language::PHP))?;
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
    sub: &em_domain::model::SubProject,
    project_root: &Path,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    phase: &Phase,
) {
    for loader in &fk.loaders {
        if let Err(e) = run_loader(ctx, loader, sub, project_root, fs, parsers, phase) {
            warn!("装载器 {} 失败: {e}", loader.id);
        }
    }
}

fn run_loader(
    ctx: &mut PipelineContext,
    loader: &em_domain::model::LoaderSpec,
    sub: &em_domain::model::SubProject,
    project_root: &Path,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    phase: &Phase,
) -> Result<()> {
    let app_root = ctx
        .ws
        .get_fact(sub.id, "app_root")
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("app")
        .to_string();

    match &loader.from {
        em_domain::model::LoaderSource::File { path, key_path, .. } => {
            let rel = path.replace("{app_root}", &app_root);
            let abs = sub.root_path.join(&rel);
            let abs = if abs.exists() { abs } else { project_root.join(&rel) };
            if !abs.exists() {
                return Ok(());
            }
            let text = fs.read_to_string(&abs)?;
            let Some(parser) = parsers.parser_for(&Language::new(Language::PHP)) else {
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
        em_domain::model::LoaderSource::Glob { pattern, locale_regex, .. } => {
            load_i18n(ctx, loader, pattern, project_root, fs, parsers, locale_regex.as_deref());
        }
        em_domain::model::LoaderSource::Inline { rows } => {
            for row in rows {
                let key = row
                    .get("facade")
                    .or_else(|| row.get("key"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if key.is_empty() {
                    continue;
                }
                ctx.ws.put_symbol(ctx.project.id, &loader.table, &key, row.clone());
            }
        }
        em_domain::model::LoaderSource::Builtin { name, params } => {
            run_builtin(ctx, name, params, sub, project_root, fs, phase);
        }
    }
    Ok(())
}

fn load_i18n(
    ctx: &mut PipelineContext,
    loader: &em_domain::model::LoaderSpec,
    pattern: &str,
    project_root: &Path,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    locale_regex: Option<&str>,
) {
    let files: Vec<em_domain::model::SourceFile> = ctx.files.clone();

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
        let Some(parser) = parsers.parser_for(&Language::new(Language::PHP)) else {
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
                em_domain::model::FactValue::String(s) => s.clone(),
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
    sub: &em_domain::model::SubProject,
    project_root: &Path,
    fs: &dyn FileSystem,
    phase: &Phase,
) {
    match name {
        "php_db_schema" => load_schema(ctx, params, sub, project_root, phase),
        "php_config_keys" => load_config_keys(ctx, sub),
        "php_routes" => load_routes(ctx, sub),
        "nginx_config" => load_nginx(ctx, sub, project_root, fs),
        other => debug!("未知内置装载器: {other}"),
    }
}

fn load_schema(
    ctx: &mut PipelineContext,
    params: &Value,
    _sub: &em_domain::model::SubProject,
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
            ctx.ws.put_symbol(
                ctx.project.id,
                "schema",
                &name,
                json!({ "columns": columns, "sources": [path], "raw_table": table }),
            );
        }
    }

    // ② 从代码中的 Db::name('x') 收集表名（只收集最小的必要集合，避免整体克隆）
    let mut found: Vec<(String, String)> = Vec::new();
    for call in ctx.ws.calls.iter() {
        // 只有 DB 语义的 name()/table() 才算表名：
        // `Route::name('xxx')` 是路由命名，不能当表。
        let is_db_receiver = call
            .receiver
            .as_deref()
            .map(|r| {
                let r = r.trim_start_matches('\\');
                r.eq_ignore_ascii_case("Db")
                    || r.ends_with("\\Db")
                    || r.contains("Query")
                    || r.contains("Model")
            })
            .unwrap_or(false);
        let is_table_call = call
            .method
            .as_deref()
            .map(|m| m.eq_ignore_ascii_case("name") || m.eq_ignore_ascii_case("table"))
            .unwrap_or(false);
        if !is_table_call || !is_db_receiver {
            continue;
        }
        if let Some(em_domain::model::FactValue::String(t)) = call.args.first() {
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

fn load_config_keys(ctx: &mut PipelineContext, _sub: &em_domain::model::SubProject) {
    let mut found: Vec<(String, String, u32)> = Vec::new();
    for call in ctx.ws.calls.iter() {
        let is_config = matches!(
            call.callee.as_str(),
            "sys_config" | "sys_config_all" | "config" | "env" | "Env::get"
        ) || call.callee.ends_with("::get");
        if !is_config {
            continue;
        }
        let Some(em_domain::model::FactValue::String(key)) = call.args.first() else {
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

fn load_routes(ctx: &mut PipelineContext, _sub: &em_domain::model::SubProject) {
    // 先收集路由组区间：`Route::group('v2', function(){...})` 的前缀要拼到组内每条
    // 路由的路径上，否则契约 ID 会丢掉 `v2`、与真实请求路径及前端调用对不上。
    // 收集时遍历的是**全量**调用点，因此只需登记一次（后续子工程重复调用时跳过）。
    if ctx.ws.route_groups.is_empty() {
        let mut groups: Vec<RouteGroup> = Vec::new();
        for call in ctx.ws.calls.iter() {
            let is_route = call
                .receiver
                .as_deref()
                .map(|r| r.eq_ignore_ascii_case("Route") || r.ends_with("\\Route"))
                .unwrap_or(false);
            if !is_route {
                continue;
            }
            if !call
                .method
                .as_deref()
                .map(|m| m.eq_ignore_ascii_case("group"))
                .unwrap_or(false)
            {
                continue;
            }
            // 无前缀写法 `Route::group(function(){...})`：arg0 是闭包不是字符串，跳过。
            let Some(em_domain::model::FactValue::String(prefix)) = call.args.first() else {
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
        if !groups.is_empty() {
            info!("P3 路由组：{} 个带前缀的 Route::group", groups.len());
            ctx.ws.add_route_groups(groups);
        }
    }

    let mut found: Vec<(String, String, String, u32)> = Vec::new();
    for call in ctx.ws.calls.iter() {
        let is_route = call
            .receiver
            .as_deref()
            .map(|r| r.eq_ignore_ascii_case("Route") || r.ends_with("\\Route"))
            .unwrap_or(false);
        if !is_route {
            continue;
        }
        let Some(method) = call.method.clone() else { continue };
        if !["post", "get", "any", "rule", "put", "delete", "patch"]
            .iter()
            .any(|m| m.eq_ignore_ascii_case(&method))
        {
            continue;
        }
        let path = call
            .args
            .first()
            .and_then(|a| a.as_str())
            .unwrap_or_default()
            .to_string();
        if path.is_empty() {
            continue;
        }
        let handler = call
            .args
            .get(1)
            .and_then(|a| a.as_str())
            .unwrap_or_default()
            .to_string();
        let key = format!("{} {}", method.to_uppercase(), path);
        found.push((key, handler, call.file.clone(), call.span.start_line));
    }
    for (key, handler, file, line) in found {
        ctx.ws.put_symbol(
            ctx.project.id,
            "route_list",
            &key,
            json!({ "handler": handler, "file": file, "line": line }),
        );
    }
}

fn load_nginx(
    ctx: &mut PipelineContext,
    _sub: &em_domain::model::SubProject,
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

impl LossyText for em_domain::model::FactValue {
    fn to_string_lossy(&self) -> String {
        match self {
            em_domain::model::FactValue::String(s) | em_domain::model::FactValue::ClassConst(s) => s.clone(),
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

    use em_adapter_fs::StdFileSystem;
    use em_adapter_parser::DefaultParserRegistry;
    use em_domain::model::{Language, ProjectId, SubProject, SubProjectId};

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
