//! Parser tests against the real Python sample corpus (the Django framework source).
//!
//! Mirrors `sample_project_php.rs` / `sample_project_java.rs` / `sample_project_js.rs`: it pins the *facts*
//! tree-sitter extracts from real source, not the semantics. The `python_smoke.rs` file already pins the
//! same contracts against tiny synthesized snippets; this file pins them on a real, oversized checkout
//! where the interesting failures live — Django's `django/contrib/auth/models.py` mixes class inheritance,
//! ORM field declarations, `@property` decorators, and FQN-restoring imports:
//!
//! * class FQN derivation from the **file path** (`django/contrib/auth/models.py` -> `django.contrib.auth.models`)
//! * class inheritance (`Permission(models.Model)`, `User(AbstractUser)`)
//! * class-body ORM field declarations (`name = models.CharField(...)`) -> call site owned by `class.field`
//! * a `ForeignKey` whose target class is resolved via an import into an FQN (`entity`)
//! * a `@property` decorator landing as a call site on the decorated method
//! * `from django.db import models` recorded as the qualified symbol `django.db.models`

use gt_adapter_parser::DefaultParserRegistry;
use gt_domain::model::{Language, SyntaxFacts};
use gt_domain::port::ParserRegistry;
use gt_sample_support::{missing_hint_named, python_sample_inner_dir, python_sample_root};

/// Parse `rel` from the Python sample, but hand the parser a **controlled path** so the derived module FQN is
/// deterministic — the Python parser builds the module name from the file path, not from any in-source declaration.
fn parse_python(rel: &str) -> Option<SyntaxFacts> {
    let root = python_sample_root()?;
    let real = root.join(python_sample_inner_dir()).join(rel);
    let content = std::fs::read_to_string(&real).ok()?;
    let registry = DefaultParserRegistry::new();
    let parser = registry
        .parser_for(&Language::new(Language::PYTHON))
        .expect("the python parser is registered");
    // `rel` itself (e.g. `django/contrib/auth/models.py`) drives the module FQN.
    parser.parse(rel, &content).ok()
}

#[test]
#[ignore = "needs the python sample (Django source), which is not committed (too large to ship with the repo)"]
fn parses_class_fqn_inheritance_and_method() {
    let Some(facts) = parse_python("django/contrib/auth/models.py") else {
        panic!("{}", missing_hint_named("python"));
    };
    let permission = facts
        .declarations
        .iter()
        .find(|d| d.kind.is("Class") && d.name == "Permission")
        .expect("expected the Permission class to be parsed");
    assert_eq!(
        permission.fqn, "django.contrib.auth.models.Permission",
        "the class FQN must be module + class (module is derived from the path we pass)"
    );

    // `class Permission(models.Model)` -> a single positional base `models.Model`.
    let bases: Vec<&str> = facts
        .inheritances
        .iter()
        .filter(|i| i.child_fqn == "django.contrib.auth.models.Permission")
        .map(|i| i.base_name.as_str())
        .collect();
    assert_eq!(bases, vec!["models.Model"], "only the positional base counts");

    // A nested class (`class Meta:` inside `Permission`) is NOT a base — only the positional base arguments of
    // the `class ...(...)` clause are collected (src/python.rs:250). Collecting nested classes would attach
    // every Django `Meta` to its outer model as a spurious inheritance edge.
    assert!(
        !facts.inheritances.iter().any(|i| i.child_fqn
            == "django.contrib.auth.models.Permission"
            && i.base_name == "Meta"),
        "a nested class must not be collected as a base: {bases:?}"
    );

    // `class User(AbstractUser)` -> base `AbstractUser`.
    assert!(
        facts.inheritances.iter().any(|i| i.child_fqn == "django.contrib.auth.models.User"
            && i.base_name == "AbstractUser"),
        "User must inherit AbstractUser"
    );

    // A real method inside a real class must be recorded with a fully qualified FQN.
    let method = facts
        .declarations
        .iter()
        .find(|d| {
            d.kind.is("Method")
                && d.name == "get_by_natural_key"
                && d.parent_fqn.as_deref() == Some("django.contrib.auth.models.PermissionManager")
        })
        .expect("expected PermissionManager.get_by_natural_key to be parsed");
    assert_eq!(
        method.fqn, "django.contrib.auth.models.PermissionManager.get_by_natural_key"
    );

    // Python's member separator is `.` (src/python.rs:110), NOT PHP's `::`. A cross-language "unify the
    // separator" regression would silently change every Python member node identity — this pins it
    // independently of the positive assertion above.
    assert_ne!(
        method.fqn, "django.contrib.auth.models.PermissionManager::get_by_natural_key",
        "Python member FQNs must use `.`, never PHP's `::`"
    );
}

/// The parser's signature Python fact: a class-body field assignment to a callable (`models.CharField(...)`)
/// is a call site whose owner is precise to `class.field` — FKB then reads `owner_class.owner_member` as the
/// column identity, exactly like the JS field decorator / Java field annotation.
#[test]
#[ignore = "needs the python sample (Django source), which is not committed (too large to ship with the repo)"]
fn parses_orm_field_declaration_as_class_field_call_site() {
    let Some(facts) = parse_python("django/contrib/auth/models.py") else {
        panic!("{}", missing_hint_named("python"));
    };
    let char = facts
        .call_sites
        .iter()
        .find(|c| {
            c.callee_text == "models.CharField"
                && c.owner_fqn == "django.contrib.auth.models.Permission.name"
        })
        .expect("`name = models.CharField(...)` must become a call site on class.field");
    assert_eq!(
        char.owner_class.as_deref(),
        Some("django.contrib.auth.models.Permission")
    );

    // The owner must be precise to `class.field`, never the class itself: FKB reads `owner_class.owner_member`
    // as the column identity, so an over-attribution to the class would collapse every column of a model onto
    // one node and `HasColumn` would lose the column.
    assert_ne!(
        char.owner_fqn, "django.contrib.auth.models.Permission",
        "an ORM field call site must be owned by class.field, not over-attributed to the class"
    );
}

/// A relation field (`content_type = models.ForeignKey(ContentType, ...)`) must carry the target entity as an
/// FQN, resolved through the file's imports — so `References` can link to the target model class.
#[test]
#[ignore = "needs the python sample (Django source), which is not committed (too large to ship with the repo)"]
fn parses_foreign_key_entity_resolved_via_import() {
    let Some(facts) = parse_python("django/contrib/auth/models.py") else {
        panic!("{}", missing_hint_named("python"));
    };
    let fk = facts
        .call_sites
        .iter()
        .find(|c| {
            c.callee_text == "models.ForeignKey"
                && c.owner_fqn == "django.contrib.auth.models.Permission.content_type"
        })
        .expect("`content_type = models.ForeignKey(...)` must become a call site on class.field");
    assert_eq!(
        fk.entity.as_deref(),
        Some("django.contrib.contenttypes.models.ContentType"),
        "the ForeignKey target must resolve to an FQN via `from django.contrib.contenttypes.models import ContentType`"
    );

    // The target must be an FQN, never the bare short name: P7 resolves `References` by FQN, and a short name
    // finds no target node (the same failure mode as Java's short-name `MapsTo` lookup).
    assert_ne!(
        fk.entity.as_deref(),
        Some("ContentType"),
        "the ForeignKey target must be resolved to an FQN, not left as the bare short name"
    );
}

/// A `@property` decorator landing as a call site on the decorated method — the Python version of "an annotation
/// is a call site". This is the precondition for Flask/FastAPI `@app.get` routes being matched by FKB.
#[test]
#[ignore = "needs the python sample (Django source), which is not committed (too large to ship with the repo)"]
fn parses_property_decorator_as_call_site_on_method() {
    let Some(facts) = parse_python("django/contrib/auth/models.py") else {
        panic!("{}", missing_hint_named("python"));
    };
    let prop = facts
        .call_sites
        .iter()
        .find(|c| {
            c.callee_text == "property"
                && c.owner_fqn == "django.contrib.auth.models.AnonymousUser.groups"
        })
        .expect("`@property def groups` must become a call site owned by the method");
    assert_eq!(
        prop.owner_class.as_deref(),
        Some("django.contrib.auth.models.AnonymousUser")
    );

    // A decorator belongs to **what it decorates** (src/python.rs:23): it must not be over-attributed to the
    // class. This is the precondition for Flask/FastAPI `@app.get` routes matching the handler function rather
    // than the module / class.
    assert_ne!(
        prop.owner_fqn, "django.contrib.auth.models.AnonymousUser",
        "a @property decorator must be owned by the decorated method, not the class"
    );
}

/// `from django.db import models` must be recorded as the qualified symbol `django.db.models` so short names
/// (e.g. `models.CharField`) can be restored to FQNs elsewhere.
#[test]
#[ignore = "needs the python sample (Django source), which is not committed (too large to ship with the repo)"]
fn parses_from_import_as_qualified_symbol() {
    let Some(facts) = parse_python("django/contrib/auth/models.py") else {
        panic!("{}", missing_hint_named("python"));
    };
    let names: Vec<&str> = facts.imports.iter().map(|i| i.name.as_str()).collect();
    assert!(
        names.contains(&"django.db.models"),
        "from django.db import models -> django.db.models, got: {names:?}"
    );
    assert!(
        names.contains(&"django.contrib.contenttypes.models.ContentType"),
        "the ContentType import must be recorded so the ForeignKey target resolves"
    );

    // A from-import must be recorded as the **qualified symbol only** — never the bare short name `models`
    // (src/python.rs:677). P3 restores short names to FQNs through these entries; a bare name carries nothing
    // to restore from.
    assert!(
        !names.contains(&"models"),
        "a from-import must be recorded as the qualified symbol, not the bare short name: {names:?}"
    );

    // A class-body assignment to a **literal** becomes a Property declaration, never a call site
    // (src/python.rs:336) — otherwise every `verbose_name = "x"` would fabricate a phantom ORM column.
    assert!(
        !facts.call_sites.iter().any(|c| c.callee_text.is_empty()),
        "no literal class-body assignment may become a call site: {:?}",
        facts.call_sites.iter().map(|c| &c.callee_text).collect::<Vec<_>>()
    );
}

/// NEGATIVE: a class-body field declaration must produce exactly **one** call site. The class-body assignment
/// handler reports "handled completely" so the caller does not recurse again (src/python.rs:301) — otherwise the
/// call inside `name = models.CharField(...)` would be captured twice by recursive traversal and FKB would see
/// two `HasColumn` facts for the same column.
#[test]
#[ignore = "needs the python sample (Django source), which is not committed (too large to ship with the repo)"]
fn orm_field_declaration_is_not_captured_twice() {
    let Some(facts) = parse_python("django/contrib/auth/models.py") else {
        panic!("{}", missing_hint_named("python"));
    };
    let count = facts
        .call_sites
        .iter()
        .filter(|c| {
            c.callee_text == "models.CharField"
                && c.owner_fqn == "django.contrib.auth.models.Permission.name"
        })
        .count();
    assert_eq!(
        count, 1,
        "the field declaration call site must be captured exactly once, got {count}"
    );
}
