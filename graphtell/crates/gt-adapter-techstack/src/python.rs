//! `gt-adapter-techstack` — the Python tech-stack adapter.
//!
//! Resolves Python build manifests into the kernel's language-agnostic dependency list, plus Django's
//! locale directory convention:
//! * `requirements.txt` / `pyproject.toml` / `Pipfile` / `setup.py` → dependency names;
//! * `poetry.lock` / `Pipfile.lock` → the resolved dependency closure;
//! * `locale/{locale}/` (and `conf/locale/{locale}/`) → i18n path template.
//!
//! What Python does NOT have (left at the neutral default, matching the JVM side):
//! * a namespace manifest — Python packages are directories (the parser derives the module FQN from the
//!   file path), so no `manifest_namespaces`;
//! * a `read_manifest` config reader — Django/Flask config is Python code, and framework config keys
//!   (`os.environ.get(...)`, `@Value`-style) are captured by the parser + FKB, not by a manifest pointer;
//! * a migration-schema loader — SQLAlchemy / Django ORM fields are extracted by the parser as Properties /
//!   field-declaration call sites, so `Table` / `Column` come from the syntax layer.

use std::path::{Path, PathBuf};

use gt_domain::model::{EntryField, EntryFieldFrom, Language, SubProject};
use gt_domain::port::{
    FileSystem, ManifestEntries, ManifestEntry, ParserRegistry, TechStackAdapter,
};

/// The Python tech-stack adapter.
pub struct PythonTechStackAdapter;

impl PythonTechStackAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PythonTechStackAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl TechStackAdapter for PythonTechStackAdapter {
    fn language(&self) -> Language {
        Language::new(Language::PYTHON)
    }

    fn manifest_dependencies(&self, file_name: &str, text: &str) -> Option<Vec<String>> {
        match file_name {
            "requirements.txt" => Some(deps_from_requirements(text)),
            "pyproject.toml" => Some(deps_from_pyproject(text)),
            "Pipfile" => Some(deps_from_pipfile(text)),
            "setup.py" => Some(deps_from_setup_py(text)),
            _ => None,
        }
    }

    fn lock_dependencies(&self, file_name: &str, text: &str) -> Option<Vec<String>> {
        match file_name {
            "poetry.lock" => Some(deps_from_poetry_lock(text)),
            "Pipfile.lock" => deps_from_pipfile_lock(text),
            _ => None,
        }
    }

    /// Django keeps translations under `locale/{locale}/LC_MESSAGES/` (or `conf/locale/{locale}/`).
    /// Directory form first, matching the port contract (a flat `*.po` form is not used here).
    fn i18n_path_patterns(&self) -> Vec<String> {
        vec!["locale/{locale}/".into(), "conf/locale/{locale}/".into()]
    }

    /// Enumerate Django's `settings.py` `DATABASES` dict into per-connection entries.
    ///
    /// Django (unlike PHP's `config/database.php`) exposes its connections as a Python dict literal
    /// `DATABASES = { 'default': {...}, 'read': {...} }`, so the Python parser (which only captures
    /// ORM `Column` facts) never flattens it into `config_entries`. The adapter therefore owns the
    /// flattening here — the same language-shaped work the PHP adapter delegates to its parser.
    ///
    /// The returned entries reuse the **same** `root.<name>.<field>` key-path scheme as PHP, so the
    /// kernel's downstream `db_prefix` attribution works for both stacks unchanged. Django has no
    /// separate "which connection is the default" pointer — the connection literally keyed `default`
    /// is the default by convention — so `default` resolves to that name when present.
    fn read_manifest_entries(
        &self,
        sub: &SubProject,
        project_root: &Path,
        manifest: &str,
        root: &str,
        fields: &[EntryField],
        default_from: Option<&str>,
        fs: &dyn FileSystem,
        _parsers: &dyn ParserRegistry,
    ) -> Option<ManifestEntries> {
        resolve_manifest_entries_python(sub, project_root, manifest, root, fields, default_from, fs)
    }
}

/// Collect every quoted string inside a bracketed list body (`dependencies = [ "a", "b" ]`).
/// Strips extras (`flask[dotenv]`) and version constraints (`flask>=2`) so only the bare package name remains.
fn collect_quoted(body: &str, out: &mut Vec<String>) {
    if let Ok(re) = regex::Regex::new(r#"["']([^"']+)["']"#) {
        for c in re.captures_iter(body) {
            if let Some(m) = c.get(1) {
                let raw = m.as_str();
                // strip extras like `flask[dotenv]`
                let base = raw.split('[').next().unwrap_or(raw).trim();
                // strip version constraints like `flask>=2`, `flask==1.0`
                let name = base
                    .split(|c| matches!(c, '=' | '>' | '<' | '~' | '!' | ' ' | '@' | ';'))
                    .next()
                    .unwrap_or(base)
                    .trim();
                if !name.is_empty() && !out.iter().any(|x: &String| x == name) {
                    out.push(name.to_string());
                }
            }
        }
    }
}

/// `flask==2.0` / `sqlalchemy>=1.4` / `requests[security]` → bare package name.
fn deps_from_requirements(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue; // comments and `-r`/`-c` references
        }
        let name = line
            .split(|c| {
                matches!(
                    c,
                    '=' | '>' | '<' | '~' | '!' | ' ' | '@' | ';' | '[' | '('
                )
            })
            .next()
            .unwrap_or("")
            .trim();
        if !name.is_empty() && !out.iter().any(|x: &String| x == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// `[project] dependencies = [...]` plus `[tool.poetry.dependencies]` / `[tool.pdm.dependencies]` tables.
fn deps_from_pyproject(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(re) = regex::Regex::new(r"(?i)dependencies\s*=\s*\[([^\]]*)\]") {
        if let Some(c) = re.captures(text) {
            collect_quoted(&c[1], &mut out);
        }
    }
    // `name = "..."` or `name = { ... }` table entries. Skip the project's own metadata keys.
    if let Ok(re) = regex::Regex::new(r#"(?im)^\s*([A-Za-z0-9_.\-]+)\s*=\s*(?:\{|["'])"#) {
        for c in re.captures_iter(text) {
            if let Some(m) = c.get(1) {
                let n = m.as_str();
                if n.eq_ignore_ascii_case("name")
                    || n.eq_ignore_ascii_case("version")
                    || n.eq_ignore_ascii_case("description")
                    || n.eq_ignore_ascii_case("readme")
                    || n.eq_ignore_ascii_case("python")
                    || n.eq_ignore_ascii_case("requires-python")
                {
                    continue;
                }
                if !out.iter().any(|x: &String| x == n) {
                    out.push(n.to_string());
                }
            }
        }
    }
    out
}

/// `[packages]` / `[dev-packages]` tables: `name = "*"` or `name = { version = "*" }`.
fn deps_from_pipfile(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(re) = regex::Regex::new(r#"(?im)^\s*([A-Za-z0-9_.\-]+)\s*=\s*(?:\*|\{|["'])"#) {
        for c in re.captures_iter(text) {
            if let Some(m) = c.get(1) {
                let n = m.as_str();
                if n.eq_ignore_ascii_case("python_version")
                    || n.eq_ignore_ascii_case("source")
                    || n.eq_ignore_ascii_case("requires")
                {
                    continue;
                }
                if !out.iter().any(|x: &String| x == n) {
                    out.push(n.to_string());
                }
            }
        }
    }
    out
}

/// `install_requires = [ "a", "b" ]` inside `setup.py`.
fn deps_from_setup_py(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(re) = regex::Regex::new(r"(?i)install_requires\s*=\s*\[([^\]]*)\]") {
        if let Some(c) = re.captures(text) {
            collect_quoted(&c[1], &mut out);
        }
    }
    out
}

/// `[[package]]` blocks: `name = "x"`.
fn deps_from_poetry_lock(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(re) = regex::Regex::new(r#"(?im)^\s*name\s*=\s*"([^"]+)""#) {
        for c in re.captures_iter(text) {
            if let Some(m) = c.get(1) {
                let n = m.as_str();
                if !out.iter().any(|x: &String| x == n) {
                    out.push(n.to_string());
                }
            }
        }
    }
    out
}

/// Pipfile.lock is JSON: `default` / `develop` objects keyed by package name.
fn deps_from_pipfile_lock(text: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut out = Vec::new();
    for section in ["default", "develop"] {
        if let Some(obj) = v.get(section).and_then(|s| s.as_object()) {
            for k in obj.keys() {
                if !out.iter().any(|x: &String| x == k) {
                    out.push(k.clone());
                }
            }
        }
    }
    Some(out)
}

/// Locate a manifest file for the Python adapter.
///
/// Mirrors the PHP adapter's two-shot resolution (`sub.root_path` then `project_root`), then adds a
/// Django-specific fallback: `settings.py` in a real `django-admin startproject` checkout usually
/// lives one directory below the project root (`<root>/<project>/settings.py`), not at the root where
/// the `requirements.txt` marker sits. Without this probe the `db_connections` rule would resolve to
/// nothing for the most common Django layout. The probe is bounded (depth ≤ 3, skips venv/cache dirs)
/// and only fires for a `settings.py` manifest whose content actually declares `DATABASES`, so it
/// never wanders a whole repository.
fn resolve_manifest_path(sub: &SubProject, project_root: &Path, manifest: &str) -> Option<PathBuf> {
    let direct = sub.root_path.join(manifest);
    if direct.exists() {
        return Some(direct);
    }
    let alt = project_root.join(manifest);
    if alt.exists() {
        return Some(alt);
    }
    if Path::new(manifest)
        .file_name()
        .map(|n| n == "settings.py")
        .unwrap_or(false)
    {
        for base in [sub.root_path.clone(), project_root.to_path_buf()] {
            if let Some(found) = find_settings_with_databases(&base, 3) {
                return Some(found);
            }
        }
    }
    None
}

/// Bounded search for a `settings.py` that declares `DATABASES`, returning the first hit.
fn find_settings_with_databases(base: &Path, max_depth: usize) -> Option<PathBuf> {
    if max_depth == 0 {
        return None;
    }
    let Ok(entries) = std::fs::read_dir(base) else {
        return None;
    };
    let mut deeper: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.is_file() {
            if p.file_name().map(|n| n == "settings.py").unwrap_or(false)
                && std::fs::read_to_string(&p)
                    .map(|t| t.contains("DATABASES"))
                    .unwrap_or(false)
            {
                return Some(p);
            }
        } else if p.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Skip heavy / generated directories so the probe stays cheap.
            if !name.starts_with('.')
                && name != "__pycache__"
                && name != "node_modules"
                && name != "venv"
                && name != ".venv"
            {
                deeper.push(p);
            }
        }
    }
    for d in deeper {
        if let Some(f) = find_settings_with_databases(&d, max_depth - 1) {
            return Some(f);
        }
    }
    None
}

/// Resolve `read_manifest_entries` for a Django `settings.py` `DATABASES` dict.
///
/// Mirrors the PHP `resolve_manifest_entries_php` enumeration, only the flattened `key_path → value`
/// table comes from [`django_databases_entries`] instead of the PHP parser's `config_entries` (the
/// Python parser does not flatten `settings.py`). The `env()`-default regex fallback is intentionally
/// absent: Django settings are Python code and `os.environ.get(...)` interpolation is out of scope here.
fn resolve_manifest_entries_python(
    sub: &SubProject,
    project_root: &Path,
    manifest: &str,
    root: &str,
    fields: &[EntryField],
    default_from: Option<&str>,
    fs: &dyn FileSystem,
) -> Option<ManifestEntries> {
    let abs = resolve_manifest_path(sub, project_root, manifest)?;
    let text = fs.read_to_string(&abs).ok()?;
    let flattened = django_databases_entries(&text)?;

    let literal_at = |pointer: &str| -> Option<String> {
        flattened
            .iter()
            .find(|(k, _)| k == pointer)
            .map(|(_, v)| v.clone())
    };

    // Children of `root`: `DATABASES.<name>.…` → distinct `<name>`, source order preserved.
    let child_prefix = format!("{}.", root);
    let mut names: Vec<String> = Vec::new();
    for (k, _) in &flattened {
        if let Some(rest) = k.strip_prefix(&child_prefix) {
            let name = rest.split('.').next().unwrap_or("");
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    if names.is_empty() {
        return None;
    }

    let mut entries = Vec::new();
    for name in &names {
        let mut pairs = Vec::new();
        for field in fields {
            let value = match (&field.from, &field.pointer) {
                (Some(EntryFieldFrom::Key), _) => Some(name.clone()),
                (_, Some(pointer)) => {
                    let full = format!("{}.{}", root, pointer.replace("{key}", name));
                    literal_at(&full)
                }
                _ => None,
            };
            // A field that cannot be resolved is omitted, never guessed.
            if let Some(v) = value {
                pairs.push((field.name.clone(), v));
            }
        }
        entries.push(ManifestEntry {
            key: name.clone(),
            fields: pairs,
        });
    }

    // Django has no "default connection" pointer; the connection literally keyed `default` is the
    // default by convention. `default_from` (a PHP-style pointer) is honoured when it resolves, else
    // we fall back to that convention.
    let default = default_from
        .and_then(|p| literal_at(p))
        .or_else(|| {
            if names.iter().any(|n| n == "default") {
                Some("default".to_string())
            } else {
                None
            }
        });

    Some(ManifestEntries { entries, default })
}

/// Flatten a Django `settings.py` `DATABASES = { ... }` literal into `DATABASES.<conn>.<KEY>` → `value`
/// pairs.
///
/// Only **top-level scalar string** values of each connection dict are kept (`ENGINE`, `NAME`, `HOST`,
/// `PORT`, `USER`, `PASSWORD`, …); nested dicts (`OPTIONS`, `TEST`, `ATOMIC_REQUESTS`) are skipped so
/// their inner keys are never mistaken for connection-level settings. Returns `None` when no
/// `DATABASES = {...}` assignment is found or it yields nothing.
fn django_databases_entries(text: &str) -> Option<Vec<(String, String)>> {
    let block = django_databases_block(text)?;
    let chars: Vec<char> = block.chars().collect();
    let n = chars.len();
    let mut out: Vec<(String, String)> = Vec::new();

    let mut depth = 0i32;
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c == '{' {
            // A connection dict opens at depth 1 (just inside `DATABASES`).
            if depth == 1 {
                if let Some(name) = conn_name_before(&chars, i) {
                    if let Some(conn_end) = match_brace(&chars, i) {
                        let conn_block: String = chars[i..=conn_end].iter().collect();
                        for (k, v) in conn_scalar_pairs(&conn_block) {
                            out.push((format!("DATABASES.{}.{}", name, k), v));
                        }
                        // Jump past the whole connection; we're back at `DATABASES` level (depth 1).
                        i = conn_end + 1;
                        depth = 1;
                        continue;
                    }
                }
            }
            depth += 1;
        } else if c == '}' {
            depth -= 1;
        }
        i += 1;
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Return the `DATABASES = { ... }` block (including its outer braces) from a `settings.py` body, or
/// `None` if no `DATABASES` assignment exists.
fn django_databases_block(text: &str) -> Option<String> {
    let kw = "DATABASES";
    let pos = text.find(kw)?;
    let after_kw = pos + kw.len();
    let eq = text[after_kw..].find('=')? + after_kw;
    let rest = &text[eq + 1..];
    let open_rel = rest.find('{')?;
    let open = eq + 1 + open_rel;
    let tail: Vec<char> = text[open..].chars().collect();
    let end_rel = match_brace(&tail, 0)?;
    Some(text[open..=open + end_rel].to_string())
}

/// Find the matching `}` for the `{` at `open` in `chars`, skipping string literals. Returns the index
/// of the closing brace, or `None` if unterminated.
fn match_brace(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str: Option<char> = None;
    let mut i = open;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = in_str {
            if c == q {
                in_str = None;
            }
        } else {
            match c {
                '\'' | '"' => in_str = Some(c),
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Given the index of a `{` that opens a connection dict, recover the connection name from the
/// `'name':` immediately preceding it. Returns `None` when the shape does not match.
fn conn_name_before(chars: &[char], brace_idx: usize) -> Option<String> {
    // Walk back over whitespace, expect `:`, then over whitespace, then a quoted name.
    let mut i = brace_idx;
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    if i == 0 || chars[i - 1] != ':' {
        return None;
    }
    i -= 1;
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return None;
    }
    let q = chars[i - 1];
    if q != '\'' && q != '"' {
        return None;
    }
    let end = i - 1;
    let mut start = end;
    while start > 0 && chars[start - 1] != q {
        start -= 1;
    }
    if start == 0 && chars[0] != q {
        return None;
    }
    let name: String = chars[start..end].iter().collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Extract the top-level scalar string assignments of a connection dict literal (including its outer
/// braces). Nested dict values (`{ ... }`) are skipped so their inner keys are not returned. Only
/// quoted string values are captured (Django's `ENGINE`/`NAME`/… are strings).
fn conn_scalar_pairs(block: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = block.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c == '{' {
            depth += 1;
            i += 1;
            continue;
        }
        if c == '}' {
            depth -= 1;
            i += 1;
            continue;
        }
        if depth == 1 && (c == '\'' || c == '"') {
            let q = c;
            let mut j = i + 1;
            while j < n && chars[j] != q {
                j += 1;
            }
            let key: String = chars[i + 1..j].iter().collect();
            let mut k = j + 1;
            while k < n && chars[k].is_whitespace() {
                k += 1;
            }
            if k < n && chars[k] == ':' {
                let mut m = k + 1;
                while m < n && chars[m].is_whitespace() {
                    m += 1;
                }
                if m < n && (chars[m] == '\'' || chars[m] == '"') {
                    let vq = chars[m];
                    let vs = m + 1;
                    let mut ve = vs;
                    while ve < n && chars[ve] != vq {
                        ve += 1;
                    }
                    if !key.is_empty() {
                        out.push((key, chars[vs..ve].iter().collect()));
                    }
                    i = ve + 1;
                    continue;
                }
                // Non-string value (`{`, number, …): let the loop handle `{` via depth tracking.
                if m < n && chars[m] == '{' {
                    i = m;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;


    /// `setup.py` is one of the four recognised manifests (see `manifest_dependencies`) but had **no test at
    /// all** — a project that never adopted `pyproject.toml` would silently contribute no dependencies.
    #[test]
    fn setup_py_yields_install_requires() {
        let a = PythonTechStackAdapter::new();
        let mut deps = a
            .manifest_dependencies("setup.py", "setup(name='x', install_requires=['flask>=2', 'django'])")
            .unwrap();
        deps.sort();
        assert!(deps.contains(&"flask".to_string()), "a version constraint must be stripped: {deps:?}");
        assert!(deps.contains(&"django".to_string()));
        assert!(
            !deps.contains(&"x".to_string()),
            "the project's own name comes from `name=`, not from install_requires: {deps:?}"
        );

        // A `setup.py` with no `install_requires` is still *recognised*: `Some(empty)`, not `None` (which
        // would hand the file to the kernel's plain-text probe).
        assert_eq!(a.manifest_dependencies("setup.py", "print(1)"), Some(Vec::new()));

        // Pinned: an **extras** marker truncates the list. `install_requires` is matched with `[^\]]*`, so the
        // first `]` — the one inside `requests[security]` — ends the body and every later entry is lost.
        let truncated = a
            .manifest_dependencies("setup.py", "install_requires=['flask', 'requests[security]', 'django']")
            .unwrap();
        assert!(truncated.contains(&"flask".to_string()), "{truncated:?}");
        assert!(
            !truncated.contains(&"django".to_string()),
            "entries after an extras marker are currently lost: {truncated:?}"
        );
    }

    /// "Recognised but empty" must stay distinct from "not recognised": `None` means "this stack does not own
    /// the file" and the kernel then falls back to a whole-file text probe, while `Some(empty)` means the stack
    /// owns it and it declares nothing.
    #[test]
    fn recognised_manifests_return_some_empty_not_none() {
        let a = PythonTechStackAdapter::new();
        for f in ["requirements.txt", "pyproject.toml", "Pipfile", "setup.py"] {
            assert_eq!(
                a.manifest_dependencies(f, ""),
                Some(Vec::new()),
                "`{f}` is a recognised Python manifest, so an empty body must be Some(empty)"
            );
        }
        assert_eq!(a.manifest_dependencies("pom.xml", ""), None, "a non-Python manifest must stay None");
    }

    /// The extractors are **deliberately broad** — the port contract accepts over-detection rather than silence
    /// ("missing a framework is worse than mistaking one"), so the skip-lists only exclude the project's own
    /// metadata keys. Pinned so the behaviour is visible instead of silently relied upon; if the skip-lists are
    /// extended, these are the assertions to update.
    ///
    /// * `pyproject.toml` — any `key = {…}` / `key = "…"` at line start counts, so `license = { text = … }` is
    ///   collected although it is project metadata, not a package;
    /// * `Pipfile` — the same rule reaches `[scripts]`, so a script name becomes a "dependency";
    /// * `requirements.txt` — a VCS / URL requirement has no version separator, so the whole spec is taken as
    ///   the package name.
    #[test]
    fn dependency_extraction_is_deliberately_broad() {
        assert_eq!(
            deps_from_pyproject("[project]\nname = 'x'\nlicense = { text = 'MIT' }\n"),
            vec!["license".to_string()],
            "only name / version / description / readme / python / requires-python are skipped"
        );
        assert_eq!(
            deps_from_pipfile("[packages]\nflask = \"*\"\n[scripts]\nserve = \"python app.py\"\n"),
            vec!["flask".to_string(), "serve".to_string()],
            "only python_version / source / requires are skipped, so a script name is collected"
        );
        assert_eq!(
            deps_from_requirements("git+https://github.com/x/y.git\n"),
            vec!["git+https://github.com/x/y.git".to_string()],
            "a VCS requirement carries no version separator, so the whole spec becomes the name"
        );
    }

    #[test]
    fn language_is_python() {
        assert_eq!(PythonTechStackAdapter::new().language().as_str(), "python");
    }

    #[test]
    fn requirements_yield_package_names() {
        let req = "# comment\nflask==2.0.3\nsqlalchemy>=1.4\nrequests[security]>=2\ndjango\n-r other.txt\n";
        let mut deps = PythonTechStackAdapter::new()
            .manifest_dependencies("requirements.txt", req)
            .unwrap();
        deps.sort();
        assert!(deps.contains(&"flask".to_string()));
        assert!(deps.contains(&"sqlalchemy".to_string()));
        assert!(deps.contains(&"requests".to_string()));
        assert!(deps.contains(&"django".to_string()));
        assert!(!deps.contains(&"other.txt".to_string()), "-r references must be skipped");
        assert!(PythonTechStackAdapter::new()
            .manifest_dependencies("pom.xml", req)
            .is_none());
    }

    #[test]
    fn pyproject_yields_dependencies() {
        let toml = r#"
[project]
name = "myproject"
dependencies = ["flask>=2", "sqlalchemy"]

[tool.poetry.dependencies]
python = "^3.11"
fastapi = "^0.100"
"#;
        let mut deps = PythonTechStackAdapter::new()
            .manifest_dependencies("pyproject.toml", toml)
            .unwrap();
        deps.sort();
        assert!(deps.contains(&"flask".to_string()));
        assert!(deps.contains(&"sqlalchemy".to_string()));
        assert!(deps.contains(&"fastapi".to_string()));
        assert!(!deps.contains(&"myproject".to_string()), "the project's own name must not leak");
        assert!(!deps.contains(&"python".to_string()), "python version key must not leak");
    }

    #[test]
    fn pipfile_yields_packages() {
        let pf = "[packages]\nflask = \"*\"\nrequests = {version = \"*\"}\n[dev-packages]\npytest = \"*\"\n[requires]\npython_version = \"3.11\"\n";
        let mut deps = PythonTechStackAdapter::new()
            .manifest_dependencies("Pipfile", pf)
            .unwrap();
        deps.sort();
        assert!(deps.contains(&"flask".to_string()));
        assert!(deps.contains(&"requests".to_string()));
        assert!(deps.contains(&"pytest".to_string()));
        assert!(!deps.contains(&"python_version".to_string()));
    }

    #[test]
    fn poetry_lock_yields_names() {
        let lock = "[[package]]\nname = \"flask\"\nversion = \"2.0\"\n[[package]]\nname = \"sqlalchemy\"\n";
        let mut deps = PythonTechStackAdapter::new()
            .lock_dependencies("poetry.lock", lock)
            .unwrap();
        deps.sort();
        assert!(deps.contains(&"flask".to_string()));
        assert!(deps.contains(&"sqlalchemy".to_string()));
        assert!(PythonTechStackAdapter::new()
            .lock_dependencies("Pipfile.lock", lock)
            .is_none());
    }

    #[test]
    fn pipfile_lock_yields_names() {
        let lock = r#"{"default": {"flask": {"version": "*"}}, "develop": {"pytest": {"version": "*"}}}"#;
        let mut deps = PythonTechStackAdapter::new()
            .lock_dependencies("Pipfile.lock", lock)
            .unwrap();
        deps.sort();
        assert!(deps.contains(&"flask".to_string()));
        assert!(deps.contains(&"pytest".to_string()));
        // poetry.lock is TOML, not JSON: feeding JSON to it must not panic and yields no names.
        let empty = PythonTechStackAdapter::new()
            .lock_dependencies("poetry.lock", lock)
            .unwrap_or_default();
        assert!(empty.is_empty());
    }

    /// Django keeps translations under `locale/{locale}/LC_MESSAGES/` — a directory prefix, so every pattern
    /// must end in `/` (there is no flat-file `*.po` form to order after it, unlike JS).
    #[test]
    fn i18n_patterns_are_directory_forms() {
        let pats = PythonTechStackAdapter::new().i18n_path_patterns();
        assert_eq!(
            pats,
            vec!["locale/{locale}/".to_string(), "conf/locale/{locale}/".to_string()]
        );
        assert!(
            pats.iter().all(|p| p.ends_with('/')),
            "all Django locale patterns must be directory forms: {pats:?}"
        );
    }
}
