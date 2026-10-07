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

use gt_domain::model::Language;
use gt_domain::port::TechStackAdapter;

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

#[cfg(test)]
mod tests {
    use super::*;

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
