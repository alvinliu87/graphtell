//! `gt-adapter-php` — the PHP tech-stack adapter (implementation of the outbound port `TechStackAdapter`).
//!
//! This crate is the **only** place that knows PHP's concrete conventions:
//! * `composer.json` PSR-4 autoload → namespace → directory roots;
//! * PHP `return [...]` config files (read by dotted pointer, with an `env('K', 'default')` fallback);
//! * Laravel / ThinkPHP migration syntax (`Schema::create('users', ...)` / `$table->string('email')`).
//!
//! The kernel's prepare phase stays language-agnostic and only depends on the `TechStackAdapter` port.

use std::path::{Path, PathBuf};

use gt_domain::error::Result;
use gt_domain::model::{Language, MethodRefSpec, SubProject};
use gt_domain::port::{AdapterFact, FileSystem, ParserRegistry, TechStackAdapter};

/// The PHP tech-stack adapter.
pub struct PhpTechStackAdapter;

impl PhpTechStackAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PhpTechStackAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl TechStackAdapter for PhpTechStackAdapter {
    fn language(&self) -> Language {
        Language::new(Language::PHP)
    }

    fn manifest_namespaces(&self, sub_root: &Path, project_root: &Path) -> Vec<(String, PathBuf)> {
        psr4_roots(sub_root, project_root)
    }

    fn read_manifest(
        &self,
        sub: &SubProject,
        project_root: &Path,
        manifest: &str,
        pointer: &str,
        fs: &dyn FileSystem,
        parsers: &dyn ParserRegistry,
    ) -> Option<(String, String)> {
        resolve_manifest_php(sub, project_root, manifest, pointer, fs, parsers)
    }

    fn enrich_method_ref(&self, spec: &mut MethodRefSpec, sub_root: &Path, project_root: &Path) {
        enrich_method_ref_spec(spec, sub_root, project_root)
    }

    fn load(
        &self,
        loader_id: &str,
        params: &serde_json::Value,
        _sub: &SubProject,
        project_root: &Path,
        _fs: &dyn FileSystem,
        _parsers: &dyn ParserRegistry,
        table_prefixes: &[String],
    ) -> Result<Vec<AdapterFact>> {
        match loader_id {
            // Parse Laravel / ThinkPHP `.php` migration files into schema facts.
            "php_migration_schema" => load_migration_schema(params, project_root, table_prefixes),
            // Unknown to PHP: let the kernel's generic (params-driven) built-in loaders handle it.
            _ => Ok(Vec::new()),
        }
    }
}

/// Derive `psr4_namespaces` and `app_segments` for a `method_ref` spec from `composer.json`'s PSR-4
/// autoload map, so the FKB never has to hard-code a controller directory name or an app-module allow-list.
///
/// * `psr4_namespaces` — every PSR-4 root namespace (e.g. `app\`, `App\`). The resolver matches handlers
///   against the real class FQNs under these namespaces, learning the controller directory name from the
///   graph itself (no `controller` / `Http/Controllers` assumption).
/// * `app_segments` — app modules, discovered by walking each PSR-4 root for subdirectories that themselves
///   contain a subdirectory (namespace containers such as `app/api`, `app/admin` — **not** assuming the
///   name `controller`). Only an extra allow-list on top of the module inferred from the route file path.
fn enrich_method_ref_spec(spec: &mut MethodRefSpec, sub_root: &Path, project_root: &Path) {
    let roots = psr4_roots(sub_root, project_root);
    if roots.is_empty() {
        return;
    }
    spec.psr4_namespaces = roots
        .iter()
        .map(|(ns, _)| ns.trim_end_matches('\\').to_string())
        .collect();

    let mut modules: Vec<String> = Vec::new();
    for (_ns, dir) in &roots {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    // A module is a directory that itself contains at least one subdirectory
                    // (a namespace container), independent of any `controller` naming convention.
                    let is_module = std::fs::read_dir(&p)
                        .map(|it| it.flatten().any(|c| c.path().is_dir()))
                        .unwrap_or(false);
                    if is_module {
                        let m = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                        if !m.is_empty() && !modules.contains(&m) {
                            modules.push(m);
                        }
                    }
                }
            }
        }
    }
    if !modules.is_empty() {
        spec.app_segments = modules;
    }
}

/// Read `composer.json`'s `autoload.psr-4` (preferring the sub-project root, falling back to the project root)
/// into `(namespace, dir)` pairs.
fn psr4_roots(sub_root: &Path, project_root: &Path) -> Vec<(String, PathBuf)> {
    let bases = [sub_root.to_path_buf(), project_root.to_path_buf()];
    let mut out = Vec::new();
    for base in bases {
        let path = base.join("composer.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(map) = v.pointer("/autoload/psr-4").and_then(|m| m.as_object()) else {
            continue;
        };
        for (ns, dir) in map {
            if let Some(d) = dir.as_str() {
                let ns = ns.trim_end_matches('\\').to_string();
                let dir = base.join(d.trim_matches('/'));
                out.push((ns, dir));
            }
        }
        // Found a composer.json with psr-4; prefer the sub-project's own mapping over the project root.
        if !out.is_empty() {
            break;
        }
    }
    out
}

/// Read a value from a PHP config file (e.g. ThinkPHP's `config/database.php`) by dot-separated pointer.
///
/// Reuse the PHP parser to flatten `return [...]` into `config_entries`, then precisely match `pointer`
/// (e.g. `connections.mysql.prefix`). If the config value is a form that can't be statically evaluated
/// like `env('KEY', 'default')` (parser records as `Unknown`), use a lightweight regex to extract its
/// literal default as a fallback.
fn resolve_manifest_php(
    sub: &SubProject,
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
    // Fallback: the value may be `env('KEY', 'default')` → extract the default for the leaf key from source text.
    let leaf = pointer.rsplit('.').next().unwrap_or(pointer);
    extract_php_config_default(&text, leaf).map(|s| {
        (s, format!("php config (env default): {}", abs.display()))
    })
}

/// Lightweight fallback: extract the literal default from `config/database.php` source text `<leaf> => 'x'` or
/// `<leaf> => env('K', 'x')`. Only for cases the parser can't statically evaluate.
fn extract_php_config_default(text: &str, leaf: &str) -> Option<String> {
    let escaped = regex::escape(leaf);
    let re = regex::Regex::new(&format!(
        r#"(?i)(?:['"]){escaped}(?:['"])\s*=>\s*(?:env\(\s*['"][^'"]*['"]\s*,\s*['"]([^'"]*)['"]\s*\)|['"]([^'"]*)['"])"#
    ))
    .ok()?;
    re.captures(text)
        .and_then(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().to_string()))
}

/// Laravel migration: `Schema::create('users', function (Blueprint $table) { … })`.
///
/// Why it must exist: PHP ORM models **don't declare fields**; columns are only written in migrations; and
/// `$table->string('email')` is inside a closure, the call site owner is the closure not the model class,
/// so FKB can't get "which table this column belongs to" (this is the key difference from TypeORM `@Column`).
/// Hence the loader writes columns straight into the `schema` symbol table, and P6 settles them into `Column` graph nodes.
fn load_migration_schema(
    params: &serde_json::Value,
    project_root: &Path,
    table_prefixes: &[String],
) -> Result<Vec<AdapterFact>> {
    // Table prefix follows what the project detected (P3 auto-reads from config/database.php) merged with
    // the prefix from the FKB `params`.
    let mut prefixes: Vec<String> = table_prefixes.to_vec();
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
    // migration file path fragment and extension declared by FKB `params` (default Laravel/ThinkPHP common shape).
    let paths: Vec<String> = params
        .get("paths")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_else(|| vec!["database/migrations".into()]);
    let exts: Vec<String> = params
        .get("extensions")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_else(|| vec!["php".into()]);
    let mut facts = Vec::new();
    for (path, text) in scan_migration_files(project_root, &paths, &exts) {
        for (table, columns) in parse_migration_tables(&text) {
            let name = strip_prefixes(&table, &prefixes);
            if name.is_empty() || columns.is_empty() {
                continue;
            }
            facts.push(AdapterFact::Schema {
                table: name,
                columns,
                source: path.clone(),
            });
        }
    }
    Ok(facts)
}

/// Scan migration files (path fragment + extension declared by FKB `params`, default `database/migrations` + `php`).
/// Skip dependency dirs (generic, framework-independent).
fn scan_migration_files(
    root: &Path,
    paths: &[String],
    exts: &[String],
) -> Vec<(String, String)> {
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

/// Parse migration's `Schema::create` / `Schema::table`, extract "table name → column names".
fn parse_migration_tables(src: &str) -> Vec<(String, Vec<String>)> {
    let Ok(re) = regex::Regex::new(r#"Schema\s*::\s*(?:create|table)\s*\(\s*['"]([\w]+)['"]"#) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for cap in re.captures_iter(src) {
        let Some(m0) = cap.get(0) else { continue };
        let table = cap[1].to_string();
        // Pair the `(` of `Schema::create(` with its `)` (skip parentheses inside strings), take the whole call block
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

/// Take column declarations in the Blueprint closure body: `$table->string('email')` → `email`.
///
/// **Only recognize column-declaration methods (whitelist)** — can't simply "take the first string arg":
/// `->comment('说明')` / `->after('col')` / `->default('x')` modifiers also carry
/// string args, would be mistaken as column names.
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
        // Jump to the arg list's left paren
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
            // Column declarations with no args (`$table->id()` / `->timestamps()`): fill column name by Laravel convention
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

/// Read the string-literal content at `i` (without quotes).
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

/// Methods in Blueprint that **really declare columns** (whitelist, see [`columns_of_blueprint`]).
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
            // Methods with no args, fill column name by convention
            | "rememberToken" | "softDeletes" | "softDeletesTz" | "timestamps" | "timestampsTz"
            | "nullableTimestamps" | "morphs" | "nullableMorphs" | "nullableUuidMorphs"
            | "nullableUlidMorphs"
    )
}

/// **Implicit column names** for argument-less column declarations (Laravel convention).
///
/// Deliberately exclude bare `uuid()` / `ulid()`: their default column name is `uuid` / `ulid`,
/// but more commonly code writes `$table->uuid('id')` (covered by the arg form) — don't guess.
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

/// Find the `)` pairing the `(` at `open` (skip parentheses inside string literals).
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

/// Strip prefixes repeatedly (to handle a doubled prefix like `eb_eb_store_order`). Pure string util,
/// local copy of the kernel's `normalize::strip_prefixes` so this adapter depends only on `gt-domain`.
fn strip_prefixes(s: &str, prefixes: &[String]) -> String {
    let mut s = s.to_string();
    let mut changed = true;
    while changed {
        changed = false;
        for p in prefixes {
            if !p.is_empty() && s.starts_with(p.as_str()) && s.len() > p.len() {
                s = s[p.len()..].to_string();
                changed = true;
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_adapter_fs::StdFileSystem;
    use gt_adapter_parser::DefaultParserRegistry;
    use gt_domain::model::{ProjectId, SubProjectId};

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

    fn adapter() -> PhpTechStackAdapter {
        PhpTechStackAdapter::new()
    }

    #[test]
    fn reads_table_prefix_from_php_config() {
        let dir = std::env::temp_dir().join(format!("phpad_test_prefix_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("config"));
        std::fs::write(
            dir.join("config/database.php"),
            "<?php\nreturn [\n    'default' => 'mysql',\n    'connections' => [\n        'mysql' => [\n            'prefix' => 'eb_',\n        ],\n    ],\n];",
        )
        .unwrap();
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let sub = make_sub(dir.clone());
        let got = adapter().read_manifest(
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
        let dir = std::env::temp_dir().join(format!("phpad_test_noprefix_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let sub = make_sub(dir.clone());
        let got = adapter().read_manifest(
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
    fn reads_prefix_from_env_default() {
        let dir = std::env::temp_dir().join(format!("phpad_test_envprefix_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("config"));
        std::fs::write(
            dir.join("config/database.php"),
            "<?php\nreturn [\n    'connections' => [\n        'mysql' => [\n            'prefix' => env('DB_PREFIX', 'eb_'),\n        ],\n    ],\n];",
        )
        .unwrap();
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let sub = make_sub(dir.clone());
        let got = adapter().read_manifest(
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
