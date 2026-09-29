# frontend-backend-link

自造合成夹具：前端 ↔ 后端跨端链路

- 许可证：**本项目自有**
- 样本路径（本地，不随仓库分发）：`samples/frontend-backend-link`

## 图规模

| 节点类型 | 数量 |
| --- | --- |
| Cache | 2 |
| CallSite | 24 |
| Class | 2 |
| ConfigKey | 2 |
| EventBus | 1 |
| File | 11 |
| Function | 14 |
| HttpContract | 19 |
| Method | 2 |
| Namespace | 1 |
| Page | 3 |
| Store | 1 |

## 规则检测结果

> 共 25 条违规（info 4、warning 21）。

| 规则 | 严重度 | 位置 | 信息 |
| --- | --- | --- | --- |
| `frontend-calls-missing-backend` | warning | samples/frontend-backend-link/backend/route/api.php:12 | 前端调用了 GET /api/invoice/detail/:*，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/frontend-backend-link/backend/route/api.php:13 | 前端调用了 GET /api/order/invoice_detail/:*，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/frontend-backend-link/backend/route/api.php:8 | 前端调用了 GET /api/ping，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/frontend-backend-link/backend/route/api.php:5 | 前端调用了 POST /api/delete，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:18 | 契约 DELETE /api/items/:id 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:19 | 契约 DELETE /api/tags/:id 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:12 | 契约 GET /api/invoice/detail/:* 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:18 | 契约 GET /api/items 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:18 | 契约 GET /api/items/:id 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:18 | 契约 GET /api/items/:id/edit 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:18 | 契约 GET /api/items/create 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:13 | 契约 GET /api/order/invoice_detail/:* 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:8 | 契约 GET /api/ping 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:19 | 契约 GET /api/tags 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:19 | 契约 GET /api/tags/:id/edit 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:19 | 契约 GET /api/tags/create 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:5 | 契约 POST /api/delete 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:18 | 契约 POST /api/items 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:19 | 契约 POST /api/tags 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:18 | 契约 PUT /api/items/:id 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `http-contract-handler-unresolved` | warning | samples/frontend-backend-link/backend/route/api.php:19 | 契约 PUT /api/tags/:id 注册了 handler 但图没解析到方法（无 HandledBy 出边）—— 路由多半是好的，是图的解析有缺口 |
| `backend-endpoint-never-called` | info | samples/frontend-backend-link/backend/app/controller/Order.php:9 | 端点 ANY /order/delete 有 handler 但没有任何前端调用方，可能是死端点 |
| `backend-endpoint-never-called` | info | samples/frontend-backend-link/backend/app/controller/Ping.php:8 | 端点 ANY /ping/ping 有 handler 但没有任何前端调用方，可能是死端点 |
| `cache-never-written` | info | samples/frontend-backend-link/backend/app/controller/Order.php:13 | 缓存 order-status 被读取但图上没有任何写入 —— 读到的可能是空 / 旧的，或写路径未被图识别 |
| `cache-never-written` | info | samples/frontend-backend-link/frontend/src/api.js:33 | 缓存 token 被读取但图上没有任何写入 —— 读到的可能是空 / 旧的，或写路径未被图识别 |
