//! The framework knowledge base port.

use crate::model::{FrameworkKnowledge, Language, Phase, Rule};

/// The FKB provider port.
///
/// The kernel does not care whether FKB comes from a YAML directory, a database or the network — only that it can be fetched.
pub trait KnowledgeProvider: Send + Sync {
    /// Every known framework.
    fn all(&self) -> Vec<&FrameworkKnowledge>;
    fn by_id(&self, id: &str) -> Option<&FrameworkKnowledge>;

    /// Take all rules of a language for a phase (filtered by the framework id list).
    fn rules_for(&self, framework_ids: &[String], phase: &Phase) -> Vec<Rule> {
        let mut out = Vec::new();
        for id in framework_ids {
            if let Some(fk) = self.by_id(id) {
                out.extend(fk.rules.iter().filter(|r| r.phase == *phase).cloned());
            }
        }
        out
    }

    /// Frameworks supporting a language.
    fn for_language(&self, language: &Language) -> Vec<&FrameworkKnowledge> {
        self.all()
            .into_iter()
            .filter(|fk| fk.language == *language)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FrameworkKnowledge, Language, Phase, Rule};
    use serde_json::json;

    /// A minimal in-memory `KnowledgeProvider` so the default `rules_for` / `for_language` methods — which
    /// live on the trait — can be exercised without a YAML loader or a DB.
    struct StubProvider {
        fks: Vec<FrameworkKnowledge>,
    }

    impl KnowledgeProvider for StubProvider {
        fn all(&self) -> Vec<&FrameworkKnowledge> {
            self.fks.iter().collect()
        }
        fn by_id(&self, id: &str) -> Option<&FrameworkKnowledge> {
            self.fks.iter().find(|f| f.id == id)
        }
    }

    fn fk(id: &str, lang: &str, rules: Vec<Rule>) -> FrameworkKnowledge {
        FrameworkKnowledge {
            id: id.into(),
            language: Language::new(lang),
            rules,
            ..Default::default()
        }
    }

    /// A rule needs a `selector` + `binding`, both heavy enums; build one from JSON so the test stays terse.
    fn rule(id: &str, phase: &str) -> Rule {
        serde_json::from_value(json!({
            "id": id,
            "phase": phase,
            "selector": { "kind": "call" },
            "binding": []
        }))
        .unwrap_or_else(|e| panic!("cannot build rule {id}: {e}"))
    }

    /// `rules_for` keeps only rules of the requested phase, across every named framework, in framework-id order.
    #[test]
    fn rules_for_filters_by_framework_and_phase() {
        let p = StubProvider {
            fks: vec![
                fk("laravel", "php", vec![rule("r1", "scan"), rule("r2", "taint")]),
                fk("thinkphp", "php", vec![rule("r3", "scan"), rule("r4", "link")]),
            ],
        };
        let scan = p.rules_for(&["laravel".into(), "thinkphp".into()], &Phase::new("scan"));
        let ids: Vec<&str> = scan.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["r1", "r3"], "仅保留 scan 阶段规则，且跨两个框架按 id 顺序合并");

        let taint = p.rules_for(&["laravel".into()], &Phase::new("taint"));
        assert_eq!(taint.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["r2"]);
    }

    /// An unrecognised framework id is skipped, not turned into an error; a list of only-unknown ids yields nothing.
    #[test]
    fn rules_for_skips_unknown_framework_ids() {
        let p = StubProvider {
            fks: vec![fk("laravel", "php", vec![rule("r1", "scan")])],
        };
        let got = p.rules_for(&["missing".into(), "laravel".into()], &Phase::new("scan"));
        assert_eq!(
            got.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["r1"],
            "未知 id 被跳过，已知 id 仍命中"
        );

        assert!(
            p.rules_for(&["missing".into()], &Phase::new("scan")).is_empty(),
            "全是未知 id 时返回空"
        );
    }

    /// `for_language` filters `all()` by language; a language with no framework yields an empty list.
    #[test]
    fn for_language_filters_by_language() {
        let p = StubProvider {
            fks: vec![
                fk("laravel", "php", vec![]),
                fk("thinkphp", "php", vec![]),
                fk("react", "javascript", vec![]),
            ],
        };
        let php: Vec<&str> = p
            .for_language(&Language::new("php"))
            .iter()
            .map(|f| f.id.as_str())
            .collect();
        assert_eq!(php, vec!["laravel", "thinkphp"]);

        let js = p.for_language(&Language::new("javascript"));
        assert_eq!(js.len(), 1);
        assert_eq!(js[0].id, "react");

        assert!(
            p.for_language(&Language::new("go")).is_empty(),
            "无对应语言的框架时返回空"
        );
    }
}
