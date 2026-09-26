# 项目级意图别名配置（`.graphtell/aliases.json`）

graphtell 的召回依赖「中文查询词 → 英文标识符」的桥接。内置的
[`INTENT_ALIASES`](../../crates/gt-application/src/recall_service.rs) 只收
**换个项目也大概率成立**的通用词（CRUD 动词、order/pay/user、sms/merge/spec…），
刻意不放任何具体业务的专属黑话（如「秒杀」「拼团」「团购」）。

业务黑话的正确归处是**每个代码库自己的** `.graphtell/aliases.json`：
它随工程走、不污染通用工具源码，且换项目零改动。

## 位置

放在**工程根目录**（即 `graphtell list` 显示的 `root_path`）下：

```
<项目根>/.graphtell/aliases.json
```

召回时工具会按 `root_path` 读取该文件，与内置表**合并**后用于展开查询词。
（注意：若多个工程共用同一 `root_path`，它们会共享同一份配置。）

## 格式

一个 JSON 对象，键是中文意图词，值是它对应的英文 token 数组
（token 会做**子串匹配**节点名 / fqn）：

```json
{
  "秒杀": ["seckill"],
  "拼团": ["combination", "pink"],
  "佣金": ["brokerage", "commission"]
}
```

## 合并语义

- 与内置 `INTENT_ALIASES` **合并**：项目配置里的词会追加进召回词典。
- **同名键追加而非覆盖**：若项目写 `"订单": ["seckill_order"]`，则最终
  `订单 → [order, seckill_order]`，内置的 `order` 不会被丢掉。
- **文件不存在 / 解析失败**：静默回退到内置表（仅打一条 `warning` 日志），
  绝不阻断召回。

## 如何找到该填的 token

1. 在代码库里 grep 领域名词，看类名 / 方法名实际用了什么英文
   （如 CRMEB 的 `StoreSeckillServices` → `seckill`；litemall 的
   `LitemallGroupon` → `groupon`）。
2. 跑一次 `graphtell recall --query "你的中文问题" --markdown`，
   看质量评估里 **「未命中概念」** 列出的中文词与英文展开——那些就是该补的。

## 示例

仓库内已附带几份可参考的示例：

- CRMEB：`samples/php-projects/thinkphp/CRMEB/.graphtell/aliases.json`
- Bagisto：`samples/php-projects/laravel/bagisto/.graphtell/aliases.json`
- litemall：`samples/java-projects/litemall/.graphtell/aliases.json`
