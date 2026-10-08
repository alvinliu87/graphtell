//! `gt-adapter-techstack` — the PHP tech-stack adapter (implementation of the outbound port `TechStackAdapter`).
//!
//! This module is the **only** place that knows PHP's concrete conventions:
//! * `composer.json` PSR-4 autoload → namespace → directory roots;
//! * PHP `return [...]` config files (read by dotted pointer, with an `env('K', 'default')` fallback);
//! * Laravel / ThinkPHP migration syntax (`Schema::create('users', ...)` / `$table->string('email')`).
//!
//! The kernel's prepare phase stays language-agnostic and only depends on the `TechStackAdapter` port.

use std::path::{Path, PathBuf};

use gt_domain::error::Result;
use gt_domain::model::{EntryField, EntryFieldFrom, Language, MethodRefSpec, SubProject};
use gt_domain::port::{
    AdapterFact, FileSystem, ManifestEntries, ManifestEntry, ParserRegistry, TechStackAdapter,
};

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

    fn i18n_path_patterns(&self) -> Vec<String> {
        // `lang/` is ThinkPHP's (and Laravel 9+'s) location; Laravel ≤8 keeps them under
        // `resources/lang/`. Which directory a stack uses is stack knowledge, so it lives here.
        vec!["lang/{locale}/".into(), "resources/lang/{locale}/".into()]
    }

    fn manifest_dependencies(&self, file_name: &str, text: &str) -> Option<Vec<String>> {
        if file_name != "composer.json" {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        Some(keys_of(&v, &["require", "require-dev"]))
    }

    fn lock_dependencies(&self, file_name: &str, text: &str) -> Option<Vec<String>> {
        if file_name != "composer.lock" {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let mut out = Vec::new();
        // `packages` / `packages-dev` are arrays of `{ "name": ... }`.
        for section in ["packages", "packages-dev"] {
            if let Some(arr) = v.get(section).and_then(|s| s.as_array()) {
                for p in arr {
                    if let Some(n) = p.get("name").and_then(|n| n.as_str()) {
                        out.push(n.to_string());
                    }
                }
            }
        }
        Some(out)
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

    fn read_manifest_entries(
        &self,
        sub: &SubProject,
        project_root: &Path,
        manifest: &str,
        root: &str,
        fields: &[EntryField],
        default_from: Option<&str>,
        fs: &dyn FileSystem,
        parsers: &dyn ParserRegistry,
    ) -> Option<ManifestEntries> {
        resolve_manifest_entries_php(sub, project_root, manifest, root, fields, default_from, fs, parsers)
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
            // Generic, language-agnostic loader id: parse the project's DB-migration files into schema facts.
            // Each tech-stack adapter owns the format-specific parsing, so a Java/Flyway or Node/TypeORM
            // migration loader reuses this same id and is dispatched by `sub.language` — no PHP-specific name
            // leaks into the kernel or the FKB.
            "migration_schema" => load_migration_schema(params, project_root, table_prefixes),
            // Unknown to PHP: let the kernel's generic (params-driven) built-in loaders handle it.
            _ => Ok(Vec::new()),
        }
    }
}

/// Derive `root_namespaces` and `app_segments` for a `method_ref` spec from `composer.json`'s PSR-4
/// autoload map, so the FKB never has to hard-code a controller directory name or an app-module allow-list.
///
/// * `root_namespaces` — every PSR-4 root namespace (e.g. `app\`, `App\`). The resolver matches handlers
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
    spec.root_namespaces = roots
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

/// Collect the keys of several object sections of a JSON value (a manifest's dependency sections).
fn keys_of(v: &serde_json::Value, sections: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for section in sections {
        if let Some(obj) = v.get(section).and_then(|s| s.as_object()) {
            out.extend(obj.keys().cloned());
        }
    }
    out
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

/// Enumerate one family of config entries (e.g. every connection under `connections.*`) and read the
/// declared fields from each.
///
/// Reuses the same tree-sitter `config_entries` flattening as [`resolve_manifest_php`]; only the
/// selection differs — every child key of `root` becomes one entry instead of matching a single pointer.
///
/// The `env()`-default regex fallback runs **only when a single entry exists**: it scans raw text for the
/// first `'<leaf>' => …` occurrence and therefore cannot be attributed to a specific entry, so with
/// several connections it would silently copy one connection's value onto all of them.
fn resolve_manifest_entries_php(
    sub: &SubProject,
    project_root: &Path,
    manifest: &str,
    root: &str,
    fields: &[EntryField],
    default_from: Option<&str>,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
) -> Option<ManifestEntries> {
    let abs = sub.root_path.join(manifest);
    let abs = if abs.exists() { abs } else { project_root.join(manifest) };
    if !abs.exists() {
        return None;
    }
    let text = fs.read_to_string(&abs).ok()?;
    let parser = parsers.parser_for(&Language::new(Language::PHP))?;
    let rel = abs.to_string_lossy().replace('\\', "/");
    let facts = parser.parse(&rel, &text).ok()?;

    // The literal value at `pointer`, when the parser could evaluate it statically.
    let literal_at = |pointer: &str| -> Option<String> {
        facts
            .config_entries
            .iter()
            .find(|e| e.key_path == pointer)
            .and_then(|e| e.value.as_str())
            .map(|s| s.to_string())
    };

    // Children of `root`: `connections.<name>.…` → distinct `<name>`, source order preserved.
    let child_prefix = format!("{}.", root);
    let mut names: Vec<String> = Vec::new();
    for entry in &facts.config_entries {
        let Some(rest) = entry.key_path.strip_prefix(&child_prefix) else {
            continue;
        };
        let name = rest.split('.').next().unwrap_or("");
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    if names.is_empty() {
        return None;
    }

    let single = names.len() == 1;
    let mut entries = Vec::new();
    for name in &names {
        let mut pairs = Vec::new();
        for field in fields {
            let value = match (&field.from, &field.pointer) {
                (Some(EntryFieldFrom::Key), _) => Some(name.clone()),
                (_, Some(pointer)) => {
                    let full = format!("{}.{}", root, pointer.replace("{key}", name));
                    literal_at(&full).or_else(|| {
                        if single {
                            let leaf = full.rsplit('.').next().unwrap_or(&full);
                            extract_php_config_default(&text, leaf)
                        } else {
                            None
                        }
                    })
                }
                _ => None,
            };
            if let Some(v) = value {
                pairs.push((field.name.clone(), v));
            }
        }
        entries.push(ManifestEntry { key: name.clone(), fields: pairs });
    }

    let default = default_from.and_then(|pointer| {
        literal_at(pointer).or_else(|| {
            let leaf = pointer.rsplit('.').next().unwrap_or(pointer);
            extract_php_config_default(&text, leaf)
        })
    });

    Some(ManifestEntries { entries, default })
}

/// Lightweight fallback: extract the literal default from `config/database.php` source text `<leaf> => 'x'`,
/// `<leaf> => env('K', 'x')` or `<leaf> => Env::get('K', 'x')`. Only for cases the parser can't statically
/// evaluate.
///
/// Both env spellings must be accepted: Laravel / newer ThinkPHP apps call the helper `env('DB_PREFIX', 'eb_')`,
/// while ThinkPHP's own config files (sample_project among them) use the facade `Env::get('database.prefix', 'eb_')`.
/// Recognising only the first silently cost those projects their `db_prefix` fact — and with it every
/// column-level fact: the DDL in the install script keeps the prefix (`eb_user`) while the code writes
/// `Db::name('user')`, so without the prefix the schema symbol table never matches a `Table` node and no
/// `Column` node is ever produced (measured on sample_project: 0 columns, 0 PII annotations).
fn extract_php_config_default(text: &str, leaf: &str) -> Option<String> {
    let escaped = regex::escape(leaf);
    let re = regex::Regex::new(&format!(
        r#"(?i)(?:['"]){escaped}(?:['"])\s*=>\s*(?:(?:env\s*\(\s*|Env\s*::\s*get\s*\(\s*)['"][^'"]*['"]\s*,\s*['"]([^'"]*)['"]\s*\)|['"]([^'"]*)['"])"#
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
/// `->comment('note')` / `->after('col')` / `->default('x')` modifiers also carry
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
            frameworks: vec!["thinkphp".into()],
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
            "the table prefix must be read from connections.mysql.prefix in config/database.php"
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
        assert!(got.is_none(), "with the config file missing, no prefix must be detected");
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
            "the table prefix must be read from the default of env('DB_PREFIX', 'eb_')"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ThinkPHP's own spelling (`Env::get`) — sample_project's `config/database.php` writes
    /// `'prefix' => Env::get('database.prefix', 'eb_')`. Missing it means no `db_prefix` fact, so the
    /// install script's `eb_*` tables never match the `Table` nodes and no column is ever materialised.
    #[test]
    fn reads_prefix_from_env_get_default() {
        let dir = std::env::temp_dir().join(format!("phpad_test_envget_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("config"));
        std::fs::write(
            dir.join("config/database.php"),
            "<?php\nreturn [\n    'connections' => [\n        'mysql' => [\n            'prefix' => Env::get('database.prefix', 'eb_'),\n        ],\n    ],\n];",
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
            "the table prefix must be read from the default of Env::get('database.prefix', 'eb_')"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- pure / filesystem-light helpers (private surface) ----

    #[test]
    fn manifest_namespaces_from_psr4() {
        let dir = std::env::temp_dir().join(format!("phpad_test_psr4_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("composer.json"),
            r#"{ "autoload": { "psr-4": { "app\\": "app/", "App\\": "src/" } } }"#,
        )
        .unwrap();
        let ns = adapter().manifest_namespaces(&dir, &dir);
        let names: Vec<&str> = ns.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"app"), "PSR-4 must export the app namespace: {names:?}");
        assert!(names.contains(&"App"), "PSR-4 must export the App namespace: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only whitelisted column-declaration methods are captured; modifiers (`comment` / `default` / `after`)
    /// carry string args but must NOT be mistaken for column names.
    #[test]
    fn migration_columns_exclude_modifiers() {
        let body = r#"
            $table->id();
            $table->string('email');
            $table->comment('Email');
            $table->default('x');
            $table->after('name');
            $table->timestamps();
        "#;
        let cols = columns_of_blueprint(body);
        assert!(cols.contains(&"email".to_string()), "string('email') must be recognised as a column: {cols:?}");
        assert!(cols.contains(&"id".to_string()), "the implicit column name of id() must be id: {cols:?}");
        assert!(
            cols.contains(&"created_at".to_string()) && cols.contains(&"updated_at".to_string()),
            "timestamps() must expand to created_at/updated_at: {cols:?}"
        );
        assert!(!cols.contains(&"Email".to_string()), "the string argument of comment must not be misjudged as a column");
        assert!(!cols.contains(&"name".to_string()), "the string argument of after must not be misjudged as a column");
    }

    #[test]
    fn migration_table_and_columns_parsed() {
        let src = r#"
            Schema::create('users', function (Blueprint $table) {
                $table->id();
                $table->string('name');
                $table->string('email');
            });
            Schema::create('orders', function (Blueprint $table) {
                $table->id();
                $table->unsignedBigInteger('user_id');
            });
        "#;
        let tables = parse_migration_tables(src);
        let users = tables.iter().find(|(t, _)| t == "users").expect("the users table");
        assert_eq!(users.1, vec!["id", "name", "email"]);
        let orders = tables.iter().find(|(t, _)| t == "orders").expect("the orders table");
        assert_eq!(orders.1, vec!["id", "user_id"]);
    }

    /// A `)` inside a string literal must not terminate the Blueprint body early.
    #[test]
    fn migration_paren_in_string_is_balanced() {
        let src = "Schema::create('items', function (Blueprint $table) {\n    $table->string('note')->default(')');\n    $table->string('name');\n});";
        let tables = parse_migration_tables(src);
        let (_, cols) = tables.iter().find(|(t, _)| t == "items").expect("the items table");
        assert!(
            cols.contains(&"name".to_string()),
            "parentheses inside a string must not cut the parse short: {cols:?}"
        );
        assert!(!cols.contains(&")".to_string()));
    }

    #[test]
    fn implicit_columns_by_convention() {
        assert_eq!(implicit_columns("id"), vec!["id"]);
        assert_eq!(
            implicit_columns("timestamps"),
            vec!["created_at", "updated_at"]
        );
        assert_eq!(implicit_columns("softDeletes"), vec!["deleted_at"]);
        assert_eq!(implicit_columns("rememberToken"), vec!["remember_token"]);
    }

    /// Both `env('K', 'x')` (Laravel / newer ThinkPHP) and `Env::get('K', 'x')` (ThinkPHP / sample_project) spellings,
    /// plus a plain literal, must be recognised.
    #[test]
    fn config_default_handles_both_env_spellings() {
        let src = r#"return [
            'prefix' => env('DB_PREFIX', 'eb_'),
            'other' => Env::get('database.prefix', 'xx_'),
            'plain' => 'yy_',
        ];"#;
        assert_eq!(extract_php_config_default(src, "prefix"), Some("eb_".to_string()));
        assert_eq!(extract_php_config_default(src, "other"), Some("xx_".to_string()));
        assert_eq!(extract_php_config_default(src, "plain"), Some("yy_".to_string()));
        assert_eq!(extract_php_config_default(src, "missing"), None);
    }

    #[test]
    fn strip_prefixes_handles_doubled_prefix() {
        let p = vec!["eb_".to_string()];
        assert_eq!(strip_prefixes("eb_user", &p), "user");
        assert_eq!(strip_prefixes("eb_eb_store_order", &p), "store_order");
        assert_eq!(strip_prefixes("no_prefix_here", &p), "no_prefix_here");
    }

    /// The kernel routes any unmatched built-in loader id to the tech-stack adapter by `sub.language`. The
    /// `migration_schema` id must therefore be stack-agnostic — this test pins that the PHP adapter handles the
    /// generic name (there is no PHP-only alias left), so a FKB can declare `migration_schema` for
    /// any stack and let that stack's adapter own the format-specific parsing.
    #[test]
    fn migration_schema_generic_loader_id_routes_to_parser() {
        let dir =
            std::env::temp_dir().join(format!("phpad_test_migschema_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("database/migrations"));
        std::fs::write(
            dir.join("database/migrations/2019_01_01_create_users.php"),
            "<?php\nSchema::create('users', function (Blueprint $table) {\n    $table->id();\n    $table->string('name');\n});",
        )
        .unwrap();
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let sub = make_sub(dir.clone());
        let params = serde_json::json!({});
        let facts = adapter()
            .load("migration_schema", &params, &sub, &dir, &fs, &parsers, &[])
            .unwrap();
        let table = facts.iter().find_map(|f| match f {
            AdapterFact::Schema { table, .. } => Some(table.clone()),
            _ => None,
        });
        assert_eq!(
            table.as_deref(),
            Some("users"),
            "generic `migration_schema` id must reach the PHP migration parser"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- dependency detection (the port that lets FKB ask "does this project use X?") ----

    #[test]
    fn manifest_dependencies_reads_composer_require_sections() {
        let json = r#"{
            "require": { "topthink/framework": "^6.0", "php": ">=7.2" },
            "require-dev": { "phpunit/phpunit": "^9" }
        }"#;
        let got = adapter().manifest_dependencies("composer.json", json);
        let deps = got.expect("composer.json must yield dependencies");
        assert!(deps.contains(&"topthink/framework".to_string()), "{deps:?}");
        assert!(deps.contains(&"phpunit/phpunit".to_string()), "require-dev must count as well: {deps:?}");
        // A non-manifest file must return None so the kernel falls back to whole-file text probing.
        assert!(adapter().manifest_dependencies("package.json", json).is_none());
        assert!(
            adapter().manifest_dependencies("composer.json", "{ not json").is_none(),
            "bad JSON must return None (left to the fallback) rather than panic"
        );
    }

    #[test]
    fn lock_dependencies_reads_composer_lock_packages() {
        let json = r#"{
            "packages": [ { "name": "topthink/framework" } ],
            "packages-dev": [ { "name": "phpunit/phpunit" } ]
        }"#;
        let got = adapter().lock_dependencies("composer.lock", json);
        let deps = got.expect("composer.lock must yield dependencies");
        assert_eq!(deps, vec!["topthink/framework".to_string(), "phpunit/phpunit".to_string()]);
        assert!(adapter().lock_dependencies("composer.json", json).is_none());
        assert!(adapter().lock_dependencies("composer.lock", "{ not json").is_none());
    }

    /// Which directory a stack keeps translations in is stack knowledge, so it lives here — and **directory
    /// forms must precede flat forms**, otherwise `en/translation` would be read as a locale.
    #[test]
    fn i18n_path_patterns_cover_thinkphp_and_legacy_laravel() {
        let pats = adapter().i18n_path_patterns();
        assert_eq!(
            pats,
            vec!["lang/{locale}/".to_string(), "resources/lang/{locale}/".to_string()],
            "lang/ is ThinkPHP (and Laravel 9+), resources/lang/ is Laravel ≤8"
        );
    }

    // ---- migration column helpers ----

    /// A column repeated in one blueprint must not be pushed twice (a duplicate would double-count in the schema).
    #[test]
    fn push_col_deduplicates_and_skips_empty() {
        let mut cols = Vec::new();
        push_col(&mut cols, "id");
        push_col(&mut cols, "id");
        push_col(&mut cols, "");
        push_col(&mut cols, "name");
        assert_eq!(cols, vec!["id".to_string(), "name".to_string()]);
    }

    /// String literals inside a migration may carry escaped quotes; an unterminated one must yield `None`
    /// rather than a truncated column name.
    #[test]
    fn read_quoted_handles_both_quotes_and_escapes() {
        let b = b"'a\\'b' rest";
        assert_eq!(read_quoted(b, 0).as_deref(), Some("a\\'b"), "an escaped quote must not end it early");
        let b2 = b"\"plain\"";
        assert_eq!(read_quoted(b2, 0).as_deref(), Some("plain"));
        let b3 = b"'unterminated";
        assert!(read_quoted(b3, 0).is_none(), "an unterminated string must return None");
        let b4 = b"not-a-quote";
        assert!(read_quoted(b4, 0).is_none(), "not starting with a quote must return None");
    }

    // ---- port surface that the kernel depends on but wasn't pinned yet ----

    /// `psr4_roots` only accepts a **string** directory value (`if let Some(d) = dir.as_str()`). PSR-4 also
    /// allows an array of directories (`"Other\\": ["src/", "lib/"]`), and such an entry is currently dropped
    /// silently — a project declaring **only** the array form yields no namespaces at all, which leaves
    /// `enrich_method_ref_spec` with an empty `root_namespaces`. Pinned as the current contract; if array
    /// support is added, this is the assertion to update.
    #[test]
    fn psr4_roots_skips_entries_whose_directory_is_not_a_string() {
        let dir = std::env::temp_dir().join(format!("phpad_test_psr4_arr_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A mixed map: the string entry is kept, the array one is dropped.
        std::fs::write(
            dir.join("composer.json"),
            r#"{ "autoload": { "psr-4": { "app\\": "app/", "Other\\": ["src/", "lib/"] } } }"#,
        )
        .unwrap();
        let roots = psr4_roots(&dir, &dir);
        let namespaces: Vec<&str> = roots.iter().map(|(ns, _)| ns.as_str()).collect();
        assert_eq!(namespaces, vec!["app"], "only the string-valued entry is kept: {namespaces:?}");
        assert_eq!(
            roots[0].1,
            dir.join("app"),
            "the directory is resolved against the composer.json's own base"
        );

        // Array-only: the whole namespace map is lost, not just that one entry.
        std::fs::write(
            dir.join("composer.json"),
            r#"{ "autoload": { "psr-4": { "Only\\": ["src/"] } } }"#,
        )
        .unwrap();
        assert!(
            psr4_roots(&dir, &dir).is_empty(),
            "an array-only psr-4 map currently yields no namespaces"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Migration scanning must not pick up dependency copies: a `vendor/` (or `node_modules/`, `target/`, …)
    /// directory can contain a path fragment identical to the real migrations dir, and scanning it would add
    /// phantom tables / columns from third-party packages to the schema symbol table.
    #[test]
    fn scan_migration_files_skips_dependency_directories() {
        let root = std::env::temp_dir().join(format!("phpad_test_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let real = root.join("database/migrations");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(
            real.join("create_users.php"),
            "<?php Schema::create('users', function ($t) { $t->string('email'); });",
        )
        .unwrap();

        // The same path fragment inside dependency dirs — must be skipped.
        for dep in ["vendor/pkg/database/migrations", "node_modules/x/database/migrations"] {
            let d = root.join(dep);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("create_dep.php"),
                "<?php Schema::create('dep', function ($t) { $t->string('x'); });",
            )
            .unwrap();
        }

        let files =
            scan_migration_files(&root, &["database/migrations".to_string()], &["php".to_string()]);
        let rel: Vec<&str> = files.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(
            rel,
            vec!["database/migrations/create_users.php"],
            "only the project's own migration is scanned: {rel:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn language_is_php() {
        assert_eq!(adapter().language().as_str(), "php");
    }

    /// The default `serves` matches only the adapter's own language; a PHP sub-project must keep its
    /// adapter (otherwise dependency detection would silently degrade to a whole-file text probe), and a
    /// TypeScript / JavaScript / Python sub-project must NOT be handed the PHP adapter. Pins the same
    /// "one ecosystem, several language tags" guard the JS adapter has.
    #[test]
    fn php_serves_only_php() {
        let a = adapter();
        assert!(a.serves(&Language::new(Language::PHP)));
        assert!(
            !a.serves(&Language::new(Language::TYPESCRIPT)),
            "a TS sub-project must not be served by the PHP adapter"
        );
        assert!(!a.serves(&Language::new(Language::JAVASCRIPT)));
        assert!(!a.serves(&Language::new("python")));
    }

    /// `enrich_method_ref` turns `composer.json`'s PSR-4 into `root_namespaces` and walks each root for
    /// namespace-container subdirectories (modules) — none of this is hard-coded by the kernel.
    #[test]
    fn enrich_method_ref_discovers_namespaces_and_modules() {
        let dir =
            std::env::temp_dir().join(format!("phpad_test_enrich_{}", std::process::id()));
        // `app/api` / `app/admin` contain a subdir → modules; `app/plain` has only a file → not a module.
        let _ = std::fs::create_dir_all(dir.join("app/api/Controllers"));
        let _ = std::fs::create_dir_all(dir.join("app/admin/Controllers"));
        let _ = std::fs::create_dir_all(dir.join("app/plain"));
        std::fs::write(dir.join("app/plain/readme.txt"), "x").unwrap();
        std::fs::write(
            dir.join("composer.json"),
            r#"{ "autoload": { "psr-4": { "app\\": "app/" } } }"#,
        )
        .unwrap();
        let mut spec = MethodRefSpec {
            root_namespaces: Vec::new(),
            app_segments: Vec::new(),
            ..Default::default()
        };
        adapter().enrich_method_ref(&mut spec, &dir, &dir);
        assert!(
            spec.root_namespaces.contains(&"app".to_string()),
            "the app namespace must be discovered (trailing slash removed): {:?}",
            spec.root_namespaces
        );
        assert!(
            spec.app_segments.contains(&"api".to_string()),
            "app/api must be a module: {:?}",
            spec.app_segments
        );
        assert!(
            spec.app_segments.contains(&"admin".to_string()),
            "app/admin must be a module: {:?}",
            spec.app_segments
        );
        assert!(
            !spec.app_segments.contains(&"plain".to_string()),
            "app/plain, which has no sub-directory, is not a module"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no PSR-4 map the adapter must not touch the spec (the early return, so a project without
    /// composer.json keeps whatever the FKB/kernel already assumed).
    #[test]
    fn enrich_method_ref_noop_when_no_psr4() {
        let dir =
            std::env::temp_dir().join(format!("phpad_test_enrich_noop_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut spec = MethodRefSpec {
            root_namespaces: vec!["preset".into()],
            app_segments: vec!["preset".into()],
            ..Default::default()
        };
        adapter().enrich_method_ref(&mut spec, &dir, &dir);
        assert_eq!(spec.root_namespaces, vec!["preset".to_string()]);
        assert_eq!(spec.app_segments, vec!["preset".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `psr4_roots` prefers the sub-project's own `composer.json` but falls back to the project root.
    #[test]
    fn namespaces_fall_back_to_project_root() {
        let proj =
            std::env::temp_dir().join(format!("phpad_test_nsfb_proj_{}", std::process::id()));
        let sub = std::env::temp_dir().join(format!("phpad_test_nsfb_sub_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&proj);
        let _ = std::fs::create_dir_all(&sub);
        std::fs::write(
            proj.join("composer.json"),
            r#"{ "autoload": { "psr-4": { "App\\": "src/" } } }"#,
        )
        .unwrap();
        let ns = adapter().manifest_namespaces(&sub, &proj);
        let names: Vec<&str> = ns.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"App"),
            "a sub-project without composer.json must fall back to the project root: {names:?}"
        );
        let _ = std::fs::remove_dir_all(&proj);
        let _ = std::fs::remove_dir_all(&sub);
    }

    /// `read_manifest` looks in `sub.root_path` first, then the project root — a manifest that only lives
    /// at the project root must still resolve.
    #[test]
    fn read_manifest_falls_back_to_project_root() {
        let dir =
            std::env::temp_dir().join(format!("phpad_test_rmfb_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("config"));
        std::fs::write(
            dir.join("config/database.php"),
            "<?php\nreturn ['connections' => ['mysql' => ['prefix' => 'fb_']]];",
        )
        .unwrap();
        let subdir = dir.join("app");
        let _ = std::fs::create_dir_all(&subdir);
        let sub = make_sub(subdir);
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
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
            Some("fb_".to_string()),
            "when the manifest exists only at the project root the fallback must hit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `read_manifest_entries` enumerates every child of `root`, fills `EntryFieldFrom::Key` with the entry
    /// name, resolves `{key}`-substituted pointers, and picks up a top-level `default` entry.
    #[test]
    fn read_manifest_entries_enumerates_connections() {
        let dir =
            std::env::temp_dir().join(format!("phpad_test_entries_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("config"));
        std::fs::write(
            dir.join("config/database.php"),
            "<?php\nreturn [\n    'connections' => [\n        'mysql' => ['prefix' => 'eb_', 'driver' => 'mysql'],\n        'sqlite' => ['prefix' => 'sq_', 'driver' => 'sqlite'],\n    ],\n    'default' => 'mysql',\n];",
        )
        .unwrap();
        let sub = make_sub(dir.clone());
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let fields = vec![
            EntryField { name: "name".into(), from: Some(EntryFieldFrom::Key), pointer: None },
            EntryField { name: "prefix".into(), from: None, pointer: Some("{key}.prefix".into()) },
        ];
        let got = adapter().read_manifest_entries(
            &sub,
            &dir,
            "config/database.php",
            "connections",
            &fields,
            Some("default"),
            &fs,
            &parsers,
        );
        let me = got.expect("the connections must be enumerated");
        let by_name: std::collections::HashMap<String, Vec<(String, String)>> =
            me.entries.iter().map(|e| (e.key.clone(), e.fields.clone())).collect();
        assert_eq!(by_name.get("mysql").unwrap().clone(), vec![
            ("name".to_string(), "mysql".to_string()),
            ("prefix".to_string(), "eb_".to_string()),
        ]);
        assert_eq!(by_name.get("sqlite").unwrap().clone(), vec![
            ("name".to_string(), "sqlite".to_string()),
            ("prefix".to_string(), "sq_".to_string()),
        ]);
        assert_eq!(me.default.as_deref(), Some("mysql"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `env()`-default regex fallback may only run for a **single** entry: with several connections it
    /// cannot be attributed, so it must stay `None` rather than silently copied onto every entry.
    #[test]
    fn read_manifest_entries_env_default_only_for_single() {
        let dir = std::env::temp_dir()
            .join(format!("phpad_test_entries_env_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("config"));
        // two connections: the env-valued one must NOT fall back (single == false).
        std::fs::write(
            dir.join("config/database.php"),
            "<?php\nreturn [\n    'connections' => [\n        'mysql' => ['prefix' => env('DB_PREFIX', 'yy_')],\n        'sqlite' => ['prefix' => 'sq_'],\n    ],\n];",
        )
        .unwrap();
        let sub = make_sub(dir.clone());
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let fields = vec![EntryField {
            name: "prefix".into(),
            from: None,
            pointer: Some("{key}.prefix".into()),
        }];
        let me = adapter()
            .read_manifest_entries(
                &sub,
                &dir,
                "config/database.php",
                "connections",
                &fields,
                None,
                &fs,
                &parsers,
            )
            .expect("the connections must be enumerated");
        let by_name: std::collections::HashMap<String, Vec<(String, String)>> =
            me.entries.iter().map(|e| (e.key.clone(), e.fields.clone())).collect();
        assert!(
            by_name.get("mysql").map(|f| f.iter().any(|(n, _)| n == "prefix")).unwrap_or(false) == false,
            "with several connections the env default must not fall back (it would be copied to the other connections): {:?}",
            by_name
        );
        let sqlite_prefix = by_name
            .get("sqlite")
            .and_then(|f| f.iter().find(|(n, _)| n == "prefix").map(|(_, v)| v.clone()));
        assert_eq!(sqlite_prefix.as_deref(), Some("sq_"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// When `root` has no children in the manifest, the adapter returns `None` (not an empty entries list).
    #[test]
    fn read_manifest_entries_none_when_root_absent() {
        let dir =
            std::env::temp_dir().join(format!("phpad_test_entries_none_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("config"));
        std::fs::write(
            dir.join("config/database.php"),
            "<?php\nreturn ['foo' => ['bar' => 'x']];",
        )
        .unwrap();
        let sub = make_sub(dir.clone());
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let fields = vec![EntryField {
            name: "prefix".into(),
            from: None,
            pointer: Some("{key}.prefix".into()),
        }];
        let got = adapter().read_manifest_entries(
            &sub,
            &dir,
            "config/database.php",
            "connections",
            &fields,
            None,
            &fs,
            &parsers,
        );
        assert!(got.is_none(), "a missing root must return None");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unknown loader ids must yield an empty fact list (so the kernel can fall through to its generic
    /// built-in loaders), never an error.
    #[test]
    fn load_unknown_loader_id_returns_empty() {
        let dir =
            std::env::temp_dir().join(format!("phpad_test_loadunk_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let sub = make_sub(dir.clone());
        let facts = adapter()
            .load(
                "nonexistent_loader",
                &serde_json::json!({}),
                &sub,
                &dir,
                &fs,
                &parsers,
                &[],
            )
            .unwrap();
        assert!(facts.is_empty(), "an unknown loader id must return an empty vector rather than error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The table prefix passed from the kernel (`table_prefixes`) must be stripped from migration table names.
    #[test]
    fn migration_schema_strips_table_prefix() {
        let dir =
            std::env::temp_dir().join(format!("phpad_test_migpref_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("database/migrations"));
        std::fs::write(
            dir.join("database/migrations/2019_create_eb_users.php"),
            "<?php\nSchema::create('eb_users', function (Blueprint $table) {\n    $table->id();\n    $table->string('name');\n});",
        )
        .unwrap();
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let sub = make_sub(dir.clone());
        let facts = adapter()
            .load(
                "migration_schema",
                &serde_json::json!({}),
                &sub,
                &dir,
                &fs,
                &parsers,
                &["eb_".to_string()],
            )
            .unwrap();
        let table = facts.iter().find_map(|f| match f {
            AdapterFact::Schema { table, .. } => Some(table.clone()),
            _ => None,
        });
        assert_eq!(
            table.as_deref(),
            Some("users"),
            "the eb_ table prefix must be stripped: {:?}",
            table
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FKB may relocate migrations via the `paths` / `extensions` params; the loader must honour them.
    #[test]
    fn migration_schema_custom_paths_param() {
        let dir =
            std::env::temp_dir().join(format!("phpad_test_migpaths_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("custom/mig"));
        std::fs::write(
            dir.join("custom/mig/2019_create_posts.php"),
            "<?php\nSchema::create('posts', function (Blueprint $table) { $table->id(); });",
        )
        .unwrap();
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let sub = make_sub(dir.clone());
        let params = serde_json::json!({ "paths": ["custom/mig"], "extensions": ["php"] });
        let facts = adapter()
            .load("migration_schema", &params, &sub, &dir, &fs, &parsers, &[])
            .unwrap();
        let table = facts.iter().find_map(|f| match f {
            AdapterFact::Schema { table, .. } => Some(table.clone()),
            _ => None,
        });
        assert_eq!(
            table.as_deref(),
            Some("posts"),
            "a custom paths argument must locate the migration files"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn matching_paren_finds_closing_and_none_when_unterminated() {
        assert_eq!(matching_paren("(a(b)c)", 0), Some(6));
        assert_eq!(matching_paren("()", 0), Some(1));
        assert!(
            matching_paren("(unterminated", 0).is_none(),
            "an unclosed parenthesis must return None"
        );
        assert!(
            matching_paren("no paren", 0).is_none(),
            "not starting with a left parenthesis must return None"
        );
    }

    #[test]
    fn is_column_method_whitelist() {
        assert!(is_column_method("string"));
        assert!(is_column_method("id"));
        assert!(is_column_method("timestamps"));
        // modifiers that carry string args must NOT be mistaken for column declarations
        assert!(!is_column_method("comment"));
        assert!(!is_column_method("default"));
        assert!(!is_column_method("after"));
    }
}
