//! 强类型标识符。
//!
//! 用 newtype 区分不同实体的主键，避免"把 FileId 当 NodeId 传"这类错误
//! （编译期即可拦截）。所有 ID 底层为 `i64`，直接对应 SQLite 的 `INTEGER PRIMARY KEY`。

/// 由宏生成：`i64` newtype + 常用 trait。
macro_rules! declare_ids {
    ($( $(#[$meta:meta])* $name:ident => $doc:literal ),* $(,)?) => {
        $(
            $(#[$meta])*
            #[doc = $doc]
            #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize, Default)]
            pub struct $name(pub i64);

            impl $name {
                /// 空值哨兵（未持久化）。
                pub const NONE: $name = $name(0);

                pub fn new(v: i64) -> Self { Self(v) }
                pub fn get(self) -> i64 { self.0 }
                pub fn is_none(self) -> bool { self.0 == 0 }
            }

            impl std::fmt::Display for $name {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    write!(f, "{}", self.0)
                }
            }

            impl From<i64> for $name {
                fn from(v: i64) -> Self { Self(v) }
            }

            impl From<$name> for i64 {
                fn from(v: $name) -> Self { v.0 }
            }
        )*
    };
}

declare_ids! {
    ProjectId    => "工程（被分析的顶层代码库）",
    SubProjectId => "子工程（一个工程内的独立可分析单元，如 backend / uni-app 前端）",
    FileId       => "源文件",
    NodeId       => "图节点",
    EdgeId       => "图边",
}
