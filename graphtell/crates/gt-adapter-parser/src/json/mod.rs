//! 极简 JSON 解析器：专门把 uni-app 的 `pages.json` 翻成 `配置条目` 事实。
//!
//! 之所以单开一个 JSON 语言而不是让 JS 解析器硬吞 `.json`：
//! * tree-sitter-typescript 解析不了 JSON（会吐半截垃圾事实）；
//! * 我们只关心**路由声明**这一种 JSON——其余 `.json`（`package.json` / `tsconfig.json`）
//!   若是逐条采成配置条目，会把语义节点节点污染掉。于是本解析器**只在顶层出现
//!   `pages` 数组时才产出事实**，其它 JSON 一律返回空，从而零负担接入现有
//!   `kind: config_entry` 选择器与 `frontend-page` 规则。
//!
//! 产出与后端 `config/*.php` 的 `return [...]` 同构，因此前端 `Page` 语义节点
//! 与后端 `Route` 走同一套 Synthesize 机制，路由视角里前后端「页面 ↔ 接口」直接对看。

use gt_domain::error::Result;
use gt_domain::model::{ConfigEntryFact, FactValue, Language, Span, SyntaxFacts};
use gt_domain::port::LanguageParser;

/// JSON 前端配置解析器（服务 `json` 语言，扩展名 `.json`）。
pub struct JsonParser;

impl JsonParser {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

impl LanguageParser for JsonParser {
    fn language(&self) -> Language {
        Language::new("json")
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["json"]
    }

    fn manifest_files(&self) -> &'static [&'static str] {
        &[]
    }

    fn exclude_dirs(&self) -> &'static [&'static str] {
        &[]
    }

    fn namespace_separator(&self) -> &'static [char] {
        &['.']
    }

    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts> {
        let value: serde_json::Value = match serde_json::from_str(source) {
            Ok(v) => v,
            Err(_) => return Ok(SyntaxFacts::default()),
        };
        let mut facts = SyntaxFacts::default();

        // 只处理 uni-app 的 pages.json：顶层带 `pages` 数组。
        let Some(obj) = value.as_object() else {
            return Ok(facts);
        };

        // 主包页面：`pages: ["pages/index/index", ...]`
        if let Some(pages) = obj.get("pages").and_then(|v| v.as_array()) {
            collect_pages(&mut facts, path, source, pages, None);
        }

        // 分包页面：`subPackages: [{ root: "pagesA", pages: ["list/list"] }]`
        // root 前缀要拼到每个子页面身份上，否则 `pagesA/list/list` 与
        // `pages.json` 声明对不上、跳转边也连不到节点。
        if let Some(subs) = obj.get("subPackages").and_then(|v| v.as_array()) {
            for (i, sub) in subs.iter().enumerate() {
                let Some(sub_obj) = sub.as_object() else {
                    continue;
                };
                let root = sub_obj.get("root").and_then(|v| v.as_str()).unwrap_or("");
                let Some(arr) = sub_obj.get("pages").and_then(|v| v.as_array()) else {
                    continue;
                };
                collect_pages(&mut facts, path, source, arr, Some((i, root)));
            }
        }

        Ok(facts)
    }
}

/// 把一组页面路由翻成配置条目；`sub` 为 `Some((i, root))` 时按子包处理。
fn collect_pages(
    facts: &mut SyntaxFacts,
    path: &str,
    source: &str,
    pages: &[serde_json::Value],
    sub: Option<(usize, &str)>,
) {
    let _ = path;
    for (j, p) in pages.iter().enumerate() {
        let Some(page) = p.as_str() else {
            continue;
        };
        let route = match sub {
            Some((_, root)) if !root.is_empty() => format!(
                "{}/{}",
                root.trim_end_matches('/'),
                page.trim_start_matches('/')
            ),
            _ => page.to_string(),
        };
        let route = ensure_leading_slash(&route);
        let key_path = match sub {
            Some((i, _)) => format!("subPackages.{i}.pages.{j}"),
            None => format!("pages.{j}"),
        };
        let line = line_of(source, page);
        facts.config_entries.push(ConfigEntryFact {
            key_path,
            value: FactValue::String(route),
            span: Span {
                start_line: line,
                end_line: line,
                start_byte: 0,
                end_byte: 0,
            },
        });
    }
}

/// 路由身份统一以 `/` 开头，与导航调用归一化后的身份对齐（保证 `pages.json`
/// 与 `uni.navigateTo({ url })` 汇聚到同一个 `Page` 节点）。
fn ensure_leading_slash(s: &str) -> String {
    if s.starts_with('/') {
        s.to_string()
    } else {
        format!("/{s}")
    }
}

/// 在源文本里定位某页面串首次出现的行号（供节点给出可点击的出处）。
fn line_of(source: &str, needle: &str) -> u32 {
    let mut line = 1u32;
    for (i, ch) in source.char_indices() {
        if ch == '\n' {
            line += 1;
        }
        if source[i..].starts_with(needle) {
            return line;
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_src(src: &str) -> SyntaxFacts {
        JsonParser::new()
            .unwrap()
            .parse("pages.json", src)
            .expect("解析应成功")
    }

    #[test]
    fn emits_main_package_pages() {
        let facts = parse_src(r#"{ "pages": ["pages/index/index", "pages/detail/detail"] }"#);
        assert_eq!(facts.config_entries.len(), 2);
        assert_eq!(facts.config_entries[0].key_path, "pages.0");
        assert_eq!(
            facts.config_entries[0].value,
            FactValue::String("/pages/index/index".into())
        );
        assert_eq!(facts.config_entries[1].key_path, "pages.1");
        assert_eq!(
            facts.config_entries[1].value,
            FactValue::String("/pages/detail/detail".into())
        );
    }

    #[test]
    fn subpackage_root_is_prepended() {
        let facts = parse_src(
            r#"{ "pages": ["pages/home/home"], "subPackages": [ { "root": "pagesA", "pages": ["list/list"] } ] }"#,
        );
        // 主包 1 条 + 子包 1 条
        assert_eq!(facts.config_entries.len(), 2);
        let sub = facts
            .config_entries
            .iter()
            .find(|e| e.key_path == "subPackages.0.pages.0")
            .expect("应产出子包页面条目");
        assert_eq!(sub.value, FactValue::String("/pagesA/list/list".into()));
    }

    #[test]
    fn non_pages_json_is_ignored() {
        // package.json 之类不应污染语义节点
        let facts = parse_src(r#"{ "name": "x", "dependencies": { "axios": "^1" } }"#);
        assert!(
            facts.config_entries.is_empty(),
            "非 pages.json 应不产出配置条目，实际：{:?}",
            facts.config_entries
        );
    }

    #[test]
    fn malformed_json_is_safe() {
        let facts = parse_src("{ not valid json");
        assert!(facts.config_entries.is_empty());
    }
}
