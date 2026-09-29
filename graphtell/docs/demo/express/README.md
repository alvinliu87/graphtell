# express

Express 起步项目（Node）

- 许可证：**MIT**
- 上游：https://github.com/expressjs/express
- 样本路径（本地，不随仓库分发）：`samples/node-projects/express`

## 图规模

| 节点类型 | 数量 |
| --- | --- |
| CallSite | 6775 |
| File | 83 |
| Function | 98 |
| HttpContract | 123 |
| Middleware | 10 |

## 规则检测结果

> 共 1 条违规（warning 1）。

| 规则 | 严重度 | 位置 | 信息 |
| --- | --- | --- | --- |
| `get-with-write-verb` | warning | samples/node-projects/express/hackathon-starter/app.js:266 | 契约 GET /reset/:token 是 GET 却包含写动词，违反 REST 只读语义（可能被重试 / 预取误触发写操作） |
