//! The perspective-registry port.

use crate::model::ViewRegistry;

/// The perspective provider port.
///
/// Like `KnowledgeProvider`, this is an **outbound port**: the kernel only requires "perspective declarations can be
/// fetched" and does not care whether they come from YAML, a database or remote config.
pub trait ViewRegistryProvider: Send + Sync {
    fn registry(&self) -> &ViewRegistry;
}
