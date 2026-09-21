//! `gt-adapter-rules` —— 合规规则（Check）适配器。
//!
//! 把 YAML 声明的检查规则装载为领域模型 [`CheckRule`]，并实现 [`RuleProvider`]
//! 出站端口。
//!
//! 内核不认识任何具体规则；新增一条规则 = 在 `rules/` 加一份 YAML。

pub mod loader;

pub use loader::YamlRuleSet;
