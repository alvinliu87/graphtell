//! 规则供给端口。
//!
//! 内核不认识任何具体规则：规则由 YAML 声明，由出站适配器装载后从这里注入。
//! 新增一条规则 = 加一份 YAML，不需要改内核（开闭原则）。

use crate::model::CheckRule;

/// 规则供给。
pub trait RuleProvider: Send + Sync {
    /// 全部已装载的规则（保持 YAML 中的声明顺序）。
    fn rules(&self) -> &[CheckRule];
    /// 按 id 查找。
    fn rule(&self, id: &str) -> Option<&CheckRule> {
        self.rules().iter().find(|r| r.id == id)
    }
    /// 启用的规则。
    fn enabled_rules(&self) -> Vec<&CheckRule> {
        self.rules().iter().filter(|r| r.enabled).collect()
    }
    /// 已装载规则数。
    fn len(&self) -> usize {
        self.rules().len()
    }
    fn is_empty(&self) -> bool {
        self.rules().is_empty()
    }
}
