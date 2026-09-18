//! `gt-adapter-fkb` —— 框架知识库（FKB）适配器。
//!
//! 把 YAML 描述的框架知识装载为领域模型 [`FrameworkKnowledge`]，
//! 并实现 [`KnowledgeProvider`] 出站端口。
//!
//! 内核不认识任何框架；新增框架支持 = 增加一份 YAML。

pub mod loader;

pub use loader::YamlKnowledgeBase;
