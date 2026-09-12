//! 视角注册表端口。

use crate::model::ViewRegistry;

/// 视角供给端口。
///
/// 与 `KnowledgeProvider` 一样是**出站端口**：内核只要求"能取到视角声明"，
/// 不关心它来自 YAML、数据库还是远端配置。
pub trait ViewRegistryProvider: Send + Sync {
    fn registry(&self) -> &ViewRegistry;
}
