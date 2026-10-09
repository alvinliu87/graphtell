//! Django `settings.py` → `db_connections` integration test (public surface only).
//!
//! Guards the *enumeration* path for Django: a project declares its database connections in
//! `settings.py` as `DATABASES = { 'default': {...}, 'read': {...} }`, and the connection **name**
//! is user-chosen — not always `default` (read/write splitting, several databases). The default
//! PHP adapter reads `connections.*`; Django's shape is the same *enumeration* problem, solved with
//! the same `root.<name>.<field>` scheme, so the kernel's `db_prefix` attribution works unchanged.
//!
//! Mirrors `tests/db_connections.rs` (PHP), with the Django-specific differences called out inline:
//! Django has no per-connection table `prefix`, and no separate "which connection is default" pointer
//! (the connection literally keyed `default` is the default).

use std::path::PathBuf;

use gt_adapter_fs::StdFileSystem;
use gt_adapter_parser::DefaultParserRegistry;
use gt_adapter_techstack::PythonTechStackAdapter;
use gt_domain::model::{EntryField, EntryFieldFrom, Language, ProjectId, SubProject, SubProjectId};
use gt_domain::port::{ManifestEntries, TechStackAdapter};

fn sub(root: PathBuf) -> SubProject {
    SubProject {
        id: SubProjectId::new(1),
        project_id: ProjectId::new(1),
        name: "app".into(),
        root_path: root,
        language: Language::new(Language::PYTHON),
        role: "backend".into(),
        detected_by: "pyproject.toml".into(),
        frameworks: vec!["django".into()],
        facts: serde_json::Value::Null,
    }
}

/// Django connection fields. `name` comes from the dict key; the rest are read from the dict keys
/// `ENGINE` / `NAME` / `HOST` / `PORT`. `prefix` is **deliberately** declared though Django has no
/// such key — it exercises the "missing field must not be fabricated" invariant.
fn fields() -> Vec<EntryField> {
    vec![
        EntryField { name: "name".into(), pointer: None, from: Some(EntryFieldFrom::Key) },
        EntryField { name: "engine".into(), pointer: Some("{key}.ENGINE".into()), from: None },
        EntryField { name: "database".into(), pointer: Some("{key}.NAME".into()), from: None },
        EntryField { name: "host".into(), pointer: Some("{key}.HOST".into()), from: None },
        EntryField { name: "port".into(), pointer: Some("{key}.PORT".into()), from: None },
        EntryField { name: "prefix".into(), pointer: Some("{key}.PREFIX".into()), from: None },
    ]
}

fn write_config(root: &PathBuf, body: &str) {
    std::fs::write(root.join("settings.py"), body).unwrap();
}

/// Django has no `default` pointer, so we pass `Some("default")` to mirror the PHP call; the resolver
/// finds no such scalar and falls back to the "connection literally named `default`" convention.
fn read(root: &PathBuf) -> Option<ManifestEntries> {
    let adapter = PythonTechStackAdapter::new();
    adapter.read_manifest_entries(
        &sub(root.clone()),
        root,
        "settings.py",
        "DATABASES",
        &fields(),
        Some("default"),
        &StdFileSystem::new(),
        &DefaultParserRegistry::new(),
    )
}

fn find<'a>(entries: &'a [gt_domain::port::ManifestEntry], name: &str) -> &'a gt_domain::port::ManifestEntry {
    entries
        .iter()
        .find(|e| e.key == name)
        .unwrap_or_else(|| panic!("connection `{name}` missing, got {:?}", entries.iter().map(|e| &e.key).collect::<Vec<_>>()))
}

fn field_of<'a>(entry: &'a gt_domain::port::ManifestEntry, name: &str) -> &'a str {
    entry
        .fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("field `{name}` missing on `{}`: {:?}", entry.key, entry.fields))
}

/// Like [`field_of`], but returns `None` instead of panicking — for the negative cases where the whole
/// point is that a field must **not** be produced.
fn field_opt<'a>(entry: &'a gt_domain::port::ManifestEntry, name: &str) -> Option<&'a str> {
    entry
        .fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("pyad_conn_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// The classic single-connection case must keep working exactly as expected: the `default` connection
/// resolves to `engine` / `database` / `host` / `port`, and the absent `prefix` stays absent.
#[test]
fn single_default_connection_resolves_engine_and_name() {
    let root = tmp("single");
    std::fs::create_dir_all(&root).unwrap();
    write_config(
        &root,
        "DATABASES = {\n    'default': {\n        'ENGINE': 'django.db.backends.mysql',\n        'NAME': 'myproject',\n        'HOST': '127.0.0.1',\n        'PORT': '3306',\n    },\n}",
    );

    let out = read(&root).expect("default connection must resolve");
    assert_eq!(out.default.as_deref(), Some("default"));
    assert_eq!(out.entries.len(), 1);
    let e = find(&out.entries, "default");
    assert_eq!(field_of(e, "name"), "default");
    assert_eq!(field_of(e, "engine"), "django.db.backends.mysql");
    assert_eq!(field_of(e, "database"), "myproject");
    assert_eq!(field_of(e, "host"), "127.0.0.1");
    assert_eq!(field_of(e, "port"), "3306");
    // Django has no table prefix: the field must be omitted, never fabricated.
    assert_eq!(field_opt(e, "prefix"), None, "Django has no prefix key, so it must stay absent");
    let _ = std::fs::remove_dir_all(&root);
}

/// Read/write splitting + a second database: every connection keeps **its own** fields, and none is
/// cross-attributed.
#[test]
fn multiple_connections_keep_their_own_fields() {
    let root = tmp("multi");
    std::fs::create_dir_all(&root).unwrap();
    write_config(
        &root,
        "DATABASES = {\n    'default': {\n        'ENGINE': 'django.db.backends.mysql',\n        'NAME': 'myproject',\n        'HOST': '127.0.0.1',\n    },\n    'read': {\n        'ENGINE': 'django.db.backends.mysql',\n        'NAME': 'myproject_read',\n        'HOST': '127.0.0.2',\n    },\n}",
    );

    let out = read(&root).expect("two connections must resolve");
    assert_eq!(out.entries.len(), 2);
    assert_eq!(out.default.as_deref(), Some("default"));
    assert_eq!(field_of(find(&out.entries, "default"), "database"), "myproject");
    assert_eq!(field_of(find(&out.entries, "default"), "host"), "127.0.0.1");
    assert_eq!(field_of(find(&out.entries, "read"), "database"), "myproject_read");
    assert_eq!(field_of(find(&out.entries, "read"), "host"), "127.0.0.2");
    assert_eq!(field_of(find(&out.entries, "read"), "engine"), "django.db.backends.mysql");
    let _ = std::fs::remove_dir_all(&root);
}

/// The regression the single-pointer design could never cover: a project whose only connection is
/// named after its role, with no `default` key anywhere.
#[test]
fn connection_not_named_default_is_still_resolved() {
    let root = tmp("replica");
    std::fs::create_dir_all(&root).unwrap();
    write_config(
        &root,
        "DATABASES = {\n    'replica': {\n        'ENGINE': 'django.db.backends.postgresql',\n        'NAME': 'myproject_rep',\n    },\n}",
    );

    let out = read(&root).expect("replica-only project must resolve");
    // No connection is literally `default`, so `default` must be `None`.
    assert_eq!(out.default, None);
    assert_eq!(out.entries.len(), 1);
    assert_eq!(field_of(find(&out.entries, "replica"), "engine"), "django.db.backends.postgresql");
    assert_eq!(field_of(find(&out.entries, "replica"), "database"), "myproject_rep");
    let _ = std::fs::remove_dir_all(&root);
}

/// NEGATIVE: a connection that does not declare a requested key must simply **omit** that field — no
/// empty string, no value borrowed from a sibling connection. Fabricating a prefix would silently
/// attribute tables to the wrong connection.
#[test]
fn missing_field_is_not_fabricated() {
    let root = tmp("missing_field");
    std::fs::create_dir_all(&root).unwrap();
    write_config(
        &root,
        "DATABASES = {\n    'default': {\n        'ENGINE': 'django.db.backends.mysql',\n        'NAME': 'myproject',\n    },\n    'read': {\n        'ENGINE': 'django.db.backends.mysql',\n    },\n}",
    );

    let out = read(&root).expect("connections must resolve");
    assert_eq!(out.entries.len(), 2);
    let read = find(&out.entries, "read");
    // `read` declares no `NAME` (and Django has no `PREFIX`) — both must be absent.
    assert_eq!(field_opt(read, "database"), None, "a connection without a NAME must not gain one");
    assert_eq!(field_opt(read, "prefix"), None, "Django has no prefix; it must stay absent");
    // The fields it does declare are unaffected, and the sibling keeps its own.
    assert_eq!(field_of(read, "engine"), "django.db.backends.mysql");
    assert_eq!(field_of(find(&out.entries, "default"), "database"), "myproject");
    let _ = std::fs::remove_dir_all(&root);
}

/// NEGATIVE: every child **key** of a connection (`ENGINE`, `NAME`, `HOST`, `PORT`, `USER`,
/// `PASSWORD`) must not become an entry of its own — the enumeration collapses `DATABASES.<name>.…`
/// to the distinct `<name>`. A nested dict (`OPTIONS`) is also skipped, so its inner keys
/// (`init_command`, `charset`) are not mistaken for connection-level fields.
#[test]
fn sub_keys_do_not_become_extra_entries() {
    let root = tmp("subkeys");
    std::fs::create_dir_all(&root).unwrap();
    write_config(
        &root,
        "DATABASES = {\n    'default': {\n        'ENGINE': 'django.db.backends.mysql',\n        'NAME': 'myproject',\n        'USER': 'root',\n        'PASSWORD': 'secret',\n        'HOST': '127.0.0.1',\n        'PORT': '3306',\n        'OPTIONS': {\n            'init_command': \"SET sql_mode='STRICT'\",\n            'charset': 'utf8mb4',\n        },\n    },\n}",
    );

    let out = read(&root).expect("connections must resolve");
    assert_eq!(
        out.entries.len(),
        1,
        "one connection with six settings + a nested dict must yield one entry, got {:?}",
        out.entries.iter().map(|e| &e.key).collect::<Vec<_>>()
    );
    let default = find(&out.entries, "default");
    assert_eq!(field_of(default, "engine"), "django.db.backends.mysql");
    assert_eq!(field_of(default, "database"), "myproject");
    assert_eq!(field_of(default, "host"), "127.0.0.1");
    assert_eq!(field_of(default, "port"), "3306");
    // The nested `OPTIONS` keys must not leak as connection-level fields.
    assert_eq!(field_opt(default, "init_command"), None);
    assert_eq!(field_opt(default, "charset"), None);
    let _ = std::fs::remove_dir_all(&root);
}

/// NEGATIVE: a `settings.py` with no `DATABASES` key resolves to nothing rather than guessing.
#[test]
fn missing_databases_key_resolves_to_nothing() {
    let root = tmp("none");
    std::fs::create_dir_all(&root).unwrap();
    write_config(&root, "DEBUG = True\nINSTALLED_APPS = []\n");

    assert!(read(&root).is_none());
    let _ = std::fs::remove_dir_all(&root);
}

/// NEGATIVE: a manifest that does not exist resolves to nothing rather than panicking or inventing a
/// connection. Complements `missing_databases_key_resolves_to_nothing`, which covers a file that
/// exists but has no `DATABASES` key.
#[test]
fn absent_manifest_file_resolves_to_nothing() {
    let root = tmp("no_file");
    // Deliberately no `write_config`: the file must not exist.

    assert!(
        read(&root).is_none(),
        "a missing settings.py must resolve to nothing"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The real `django-admin startproject` layout keeps `settings.py` one directory below the project
/// root (`<root>/<project>/settings.py`), where the `requirements.txt` marker — and thus the
/// sub-project root — does not sit. The adapter must probe for it rather than silently resolving to
/// nothing (the failure mode the bounded `settings.py` search was added to prevent).
#[test]
fn nested_settings_below_root_is_discovered() {
    let root = tmp("nested");
    std::fs::create_dir_all(&root.join("myproject")).unwrap();
    std::fs::write(
        root.join("myproject/settings.py"),
        "DATABASES = {\n    'default': {\n        'ENGINE': 'django.db.backends.mysql',\n        'NAME': 'myproject',\n        'HOST': '127.0.0.1',\n    },\n}",
    )
    .unwrap();

    // `sub.root_path` is `root`, but the manifest lives at `root/myproject/settings.py`.
    let out = read(&root).expect("nested settings.py must be discovered");
    assert_eq!(out.default.as_deref(), Some("default"));
    assert_eq!(out.entries.len(), 1);
    let e = find(&out.entries, "default");
    assert_eq!(field_of(e, "name"), "default");
    assert_eq!(field_of(e, "engine"), "django.db.backends.mysql");
    assert_eq!(field_of(e, "database"), "myproject");
    assert_eq!(field_of(e, "host"), "127.0.0.1");
    // Still no `prefix` key in Django.
    assert_eq!(field_opt(e, "prefix"), None);
    let _ = std::fs::remove_dir_all(&root);
}
