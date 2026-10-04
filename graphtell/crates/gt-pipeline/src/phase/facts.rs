//! Resolving facts from a project's **own configuration** — no graph required.
//!
//! Shared by the two phases that need framework knowledge without having parsed any code:
//! * P0 ([`crate::phase::exclude`]): exclusion globs must be known *before* the scan runs, so the
//!   scan never ingests framework caches / generated files in the first place;
//! * P3 ([`crate::phase::prepare`]): `root_rules` resolution (`app_root`, `db_prefix` …).
//!
//! Everything here reads a manifest or probes a directory. None of it depends on parsed code, which is
//! exactly what makes P0 use possible — P0 is the phase that decides *which files exist at all*.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use gt_domain::model::{
    Detector, Language, PickStrategy, ProjectId, RootSource, SubProject, SubProjectId,
};
use gt_domain::port::{
    FileSystem, KnowledgeProvider, ManifestEntries, ParserRegistry, TechStackRegistry,
};
use serde_json::Value;

/// Whether `dependency` is declared by the manifest at `path`.
///
/// Recognises the common dependency sections (`require` / `dependencies` …); anything else degrades to
/// a plain-text probe rather than failing to match — missing a framework is worse than mistaking one.
pub fn manifest_has(path: &Path, dependency: &str, fs: &dyn FileSystem) -> bool {
    if !fs.exists(path) {
        return false;
    }
    let Ok(text) = fs.read_to_string(path) else { return false };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        // Besides composer.json there may also be a plain-text dependency manifest
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

/// Whether `dependency` appears in a **lock file**: the resolved dependency closure, so it covers packages
/// the project never declared itself.
///
/// Unlike [`manifest_has`], which only ever looks at hand-written declarations, this reads what was actually
/// installed — the only manifest-shaped signal that can see a package pulled in transitively. Layouts are
/// recognised per ecosystem; anything else (yarn.lock, poetry.lock …) falls back to a plain-text probe
/// rather than failing to match, the same conservative direction `manifest_has` takes.
pub fn lock_has(path: &Path, dependency: &str, fs: &dyn FileSystem) -> bool {
    if !fs.exists(path) {
        return false;
    }
    let Ok(text) = fs.read_to_string(path) else { return false };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return text.contains(dependency);
    };
    let needle = dependency.to_ascii_lowercase();
    let mut names: Vec<String> = Vec::new();
    // composer.lock: `packages` / `packages-dev` are arrays of `{ "name": ... }`.
    for section in ["packages", "packages-dev"] {
        if let Some(arr) = v.get(section).and_then(|s| s.as_array()) {
            for p in arr {
                if let Some(n) = p.get("name").and_then(|n| n.as_str()) {
                    names.push(n.to_string());
                }
            }
        }
    }
    // package-lock.json v2+: `packages` is an object keyed by install path (`node_modules/foo`).
    if let Some(map) = v.get("packages").and_then(|s| s.as_object()) {
        for (key, entry) in map {
            let n = entry.get("name").and_then(|n| n.as_str()).unwrap_or(key);
            names.push(n.trim_start_matches("node_modules/").to_string());
        }
    }
    // package-lock.json v1: `dependencies` is an object keyed by package name.
    if let Some(map) = v.get("dependencies").and_then(|s| s.as_object()) {
        names.extend(map.keys().cloned());
    }
    if names
        .iter()
        .any(|n| n.to_ascii_lowercase() == needle || n.to_ascii_lowercase().contains(&needle))
    {
        return true;
    }
    text.contains(dependency)
}

/// Resolve one [`RootSource`] to `(value, provenance)`.
///
/// Each kind deliberately only needs a manifest or a directory: that is what lets P0 use this too.
/// A manifest is looked up under the sub-project root first and the project root second (a single-module
/// project keeps its `composer.json` at the root, which *is* the sub-project root).
pub fn resolve_root_source(
    source: &RootSource,
    sub_root: &Path,
    project_root: &Path,
    sub: &SubProject,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    techstack: &dyn TechStackRegistry,
) -> Option<(String, String)> {
    match source {
        RootSource::ManifestJson { manifest, pointer, pick } => {
            let path = sub_root.join(manifest);
            let path = if fs.exists(&path) { path } else { project_root.join(manifest) };
            resolve_manifest_pointer(&path, pointer, *pick, fs)
        }
        RootSource::DirectoryExists { path } => resolve_directory_exists(sub_root, path),
        // The *interpretation* of the file format is the tech-stack adapter's job (PHP `return [...]`,
        // YAML, TOML …), so the kernel stays language-agnostic.
        RootSource::Manifest { manifest, pointer } => techstack
            .adapter_for(&sub.language)
            .and_then(|a| a.read_manifest(sub, project_root, manifest, pointer, fs, parsers)),
        // A collection resolves to **several** records, so it cannot fill a single placeholder;
        // list-shaped rules go through [`resolve_root_entries`] instead.
        RootSource::ManifestEntries { .. } => None,
    }
}

/// Resolve an entry-collection [`RootSource`] (e.g. every `connections.*` in `config/database.php`).
///
/// Separate from [`resolve_root_source`] because the result is a **list** of records, not one value:
/// a project may configure several database connections (read/write splitting, multiple databases) and
/// each carries its own driver and table prefix.
pub fn resolve_root_entries(
    source: &RootSource,
    project_root: &Path,
    sub: &SubProject,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    techstack: &dyn TechStackRegistry,
) -> Option<ManifestEntries> {
    let RootSource::ManifestEntries { manifest, root, fields, default_from } = source else {
        return None;
    };
    techstack.adapter_for(&sub.language).and_then(|a| {
        a.read_manifest_entries(
            sub,
            project_root,
            manifest,
            root,
            fields,
            default_from.as_deref(),
            fs,
            parsers,
        )
    })
}

/// A stand-in `SubProject` for callers that have not persisted sub-projects yet (P0).
///
/// Adapters only ever read `root_path` / `language` to locate the manifest, so an id of `0` is safe —
/// it is never written anywhere.
pub fn provisional_sub(sub_root: &Path, language: &Language) -> SubProject {
    SubProject {
        id: SubProjectId(0),
        project_id: ProjectId(0),
        name: String::new(),
        root_path: sub_root.to_path_buf(),
        language: language.clone(),
        role: "unknown".to_string(),
        detected_by: String::new(),
        frameworks: Vec::new(),
        facts: Value::Null,
    }
}

/// Probe whether `rel` exists inside `root`; on a hit return its **parent dir** as the value (the
/// semantics `app_root` needs: the source root, not `src/main/java` itself).
///
/// A single-module project hitting directly under the root returns `"."`; a multi-module project returns
/// the first hit module's relative dir (e.g. `mall-admin`). Only when the whole tree cannot be found do we
/// return `None` (triggering fallback / a diagnostic).
pub fn resolve_directory_exists(root: &Path, rel: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    // Hit directly under the root: source root is the sub-project root.
    if root.join(rel).is_dir() {
        return Some((".".to_string(), format!("directory exists: {}", rel)));
    }
    // Multi-module: breadth-first (prefer shallow), depth-limited to avoid scanning the whole large tree.
    let mut queue: VecDeque<(PathBuf, u32)> = VecDeque::from([(root.to_path_buf(), 0)]);
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

/// Read a value out of a JSON manifest by a dotted pointer.
///
/// Two shapes are accepted at the pointer:
/// * an object (Composer's `autoload.psr-4`: `{ "app\\": "app/" }`) — the mapped directories become
///   candidates and `pick` decides which one wins;
/// * a plain string (`extra.public-dir: "web"`) — taken as-is.
pub fn resolve_manifest_pointer(
    path: &Path,
    pointer: &str,
    pick: PickStrategy,
    fs: &dyn FileSystem,
) -> Option<(String, String)> {
    let text = fs.read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let mut cur = &v;
    for seg in pointer.split('.') {
        cur = cur.get(seg)?;
    }
    if let Some(s) = cur.as_str() {
        let value = s.trim().trim_matches('/').to_string();
        if value.is_empty() {
            return None;
        }
        return Some((value, format!("{} {} (string)", path.display(), pointer)));
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

/// Framework recognition **without code evidence** — the P0 subset of P3's detection.
///
/// Only detectors that read a manifest or probe a file can fire this early (`manifest_dependency` /
/// `lock_dependency` / `file_exists`). `import_exists` / `call_exists` need parsed code, so they are
/// skipped here; a framework that is only recognisable from code is therefore detected in P3, not P0.
/// That asymmetry is acceptable precisely because P0 uses the result only for *exclusions*, which are
/// always fallible in the safe direction (see [`crate::phase::exclude`]).
pub fn detect_without_code(
    kb: &dyn KnowledgeProvider,
    fs: &dyn FileSystem,
    sub_root: &Path,
    project_root: &Path,
    language: &Language,
) -> Vec<String> {
    let mut hits: Vec<(String, f32)> = Vec::new();
    for fk in kb.all() {
        if fk.language != *language && language.as_str() != Language::UNKNOWN && fk.language.as_str() != "*"
        {
            continue;
        }
        let mut best = 0f32;
        for d in &fk.detectors {
            let ok = match d {
                Detector::ManifestDependency { manifest, dependency, .. } => {
                    manifest_has(&sub_root.join(manifest), dependency, fs)
                        || manifest_has(&project_root.join(manifest), dependency, fs)
                }
                Detector::LockDependency { lock, dependency, .. } => {
                    lock_has(&sub_root.join(lock), dependency, fs)
                        || lock_has(&project_root.join(lock), dependency, fs)
                }
                Detector::FileExists { path, .. } => {
                    sub_root.join(path).exists() || project_root.join(path).exists()
                }
                // No parsed code yet — cannot fire in P0.
                Detector::ImportExists { .. } | Detector::CallExists { .. } => false,
            };
            if ok {
                best = best.max(d.confidence());
            }
        }
        // Knowledge that applies unconditionally to the language (the cross-framework generic layer)
        // contributes even with no detector of its own.
        if best == 0.0 && fk.apply_without_detection {
            hits.push((fk.id.clone(), 0.0));
            continue;
        }
        if best > 0.0 {
            hits.push((fk.id.clone(), best));
        }
    }
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    expand_provided(hits, kb).into_iter().map(|(id, _)| id).collect()
}

/// Expand `provides`: recognising a framework recognises the component knowledge it bundles.
///
/// Breadth-first and cycle-safe (`seen`), and **directly detected knowledge always precedes provided
/// knowledge** regardless of confidence — a component may declare a lower confidence than some other
/// framework's direct hit, but "the code / manifest said so" outranks "something else said so". That
/// ordering is what keeps a framework's own declaration of a single-valued field (`db_verbs`,
/// `method_ref` …, all taken by `find_map`) ahead of a component's.
pub fn expand_provided(
    hits: Vec<(String, f32)>,
    kb: &dyn KnowledgeProvider,
) -> Vec<(String, f32)> {
    let mut out = hits;
    let mut seen: HashSet<String> = out.iter().map(|(id, _)| id.clone()).collect();
    let mut i = 0;
    while i < out.len() {
        let (id, conf) = out[i].clone();
        if let Some(fk) = kb.by_id(&id) {
            for p in &fk.provides {
                if seen.insert(p.clone()) {
                    out.push((p.clone(), conf * PROVIDED_CONFIDENCE_DECAY));
                }
            }
        }
        i += 1;
    }
    out
}

/// Confidence granted to knowledge that was merely *provided* rather than detected.
const PROVIDED_CONFIDENCE_DECAY: f32 = 0.9;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_pointer_reads_a_plain_string() {
        // A pointer that lands on a scalar (e.g. `extra.public-dir: "web"`) must be usable, not just
        // namespace maps like `autoload.psr-4`.
        let dir = std::env::temp_dir().join(format!("gt-resolve-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("composer.json");
        std::fs::write(&path, r#"{"extra": {"public-dir": "web/"}}"#).unwrap();
        let fs = gt_adapter_fs::StdFileSystem::new();
        let got = resolve_manifest_pointer(&path, "extra.public-dir", PickStrategy::FirstDir, &fs);
        assert_eq!(got.map(|(v, _)| v), Some("web".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
