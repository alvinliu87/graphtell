# frontend-backend-link

前端 + 后端小示例：演示跨端调用链路与表读写

源码样本：[`samples/frontend-backend-link`](../../samples/frontend-backend-link)

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

## 提示词增强（召回）示例

> 以下为中文问句经 GraphTell 召回出的相关代码上下文包（Markdown）。

### 「订单创建流程」

# 召回上下文：订单创建流程

- 工程：#1
- 查询词：订单创建流程, 订单, 创建, 流程
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `Order` Class（得分 266.9）
- `orderInvoiceDetail` Function（得分 269.6）
- `delete` Method（得分 183.5）
- `order-status` Cache（得分 119.3）
- `GET /api/order/invoice_detail/:*` HttpContract（得分 107.1）
- `invoiceDetail` Function（得分 155.2）
- `goDetail` Function（得分 150.8）
- `goList` Function（得分 145.9）

## 相关代码

### 1. Function `orderInvoiceDetail`

- 位置：`samples/frontend-backend-link/frontend/src/api.js:26`
- 得分：269.6 · 跳数 0 · 来源种子 `orderInvoiceDetail` · 直接命中
- 图上关系：→ CallsHttp，→ HasCallSite

```

// **模板串** URL：`` `...${id}` `` 同理（CRMEB uni-app 的真实写法）。
export function orderInvoiceDetail(id) {
  request.get(`/api/order/invoice_detail/${id}`);
}

```

### 2. Class `Order`

- 位置：`samples/frontend-backend-link/backend/app/controller/Order.php:6`
- 得分：266.9 · 跳数 0 · 来源种子 `Order` · 直接命中

```
use think\facade\Cache;

class Order
{
    // 处理删除（被 route/api.php 的 Route::post('/api/delete') 指向）
    public function delete()
```

### 3. Method `delete`

- 位置：`samples/frontend-backend-link/backend/app/controller/Order.php:9`
- 得分：183.5 · 跳数 0 · 来源种子 `delete` · 直接命中
- 图上关系：← HandledBy，→ HasCallSite，→ ReadsCache

```
{
    // 处理删除（被 route/api.php 的 Route::post('/api/delete') 指向）
    public function delete()
    {
        // 后端缓存读取：应经 `fkb/php/common.yaml` 通用缓存规则合成
        // `Cache` 语义节点并标注 `side = backend`（与前端 `side = frontend` 对称）。
```

### 4. Function `invoiceDetail`

- 位置：`samples/frontend-backend-link/frontend/src/api.js:21`
- 得分：155.2 · 跳数 0 · 来源种子 `invoiceDetail` · 直接命中
- 图上关系：→ CallsHttp，→ HasCallSite

```
// **拼接式** URL：`'...' + id` 的形状可静态确定（非字面量段 → `:param`），
// 与后端 `Route::get('/api/invoice/detail/:id')` 按形状汇聚到同一契约。
export function invoiceDetail(id) {
  request.get('/api/invoice/detail/' + id);
}

```

### 5. Function `goDetail`

- 位置：`samples/frontend-backend-link/frontend/src/RouteView.jsx:3`
- 得分：150.8 · 跳数 0 · 来源种子 `goDetail` · 直接命中
- 图上关系：→ HasCallSite

```
// 页面跳转示例：验证前端 `Page` 语义节点（来自 pages.json）与 `NavigatesTo`
// 语义边（来自 uni.navigateTo）能像后端 Route 一样建进图。
export function goDetail(id) {
  uni.navigateTo({ url: '/pages/detail/detail?id=' + id });
}

```



### 「用户登录入口」

# 召回上下文：用户登录入口

> ⚠️ **召回质量：中（置信度 0.53）** — 部分特征词未命中（登录），结果可能不完整
>
> 建议改用以下特征词自行检索：`登录`、`login`、`auth`、`signin`

- 工程：#1
- 查询词：用户登录入口, 用户, 登录, 入口
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `rememberToken` Function（得分 179.3）
- `readToken` Function（得分 98.1）
- `counter/inc` Store（得分 97.9）
- `TIMEOUT` ConfigKey（得分 97.8）

## 相关代码

### 1. Function `rememberToken`

- 位置：`samples/frontend-backend-link/frontend/src/api.js:32`
- 得分：179.3 · 跳数 0 · 来源种子 `rememberToken` · 直接命中
- 图上关系：→ HasCallSite，→ WritesCache

```
// 前端**语义节点**示例（与后端 `Cache::set('k')` → `Cache` 同构）：
// `uni.setStorageSync('k', v)` / `localStorage.getItem('k')` 合成 `Cache` 节点。
export function rememberToken(token) {
  uni.setStorageSync('token', token);
}
export function readToken() {
```

### 2. Function `readToken`

- 位置：`samples/frontend-backend-link/frontend/src/api.js:35`
- 得分：98.1 · 跳数 0 · 来源种子 `readToken` · 直接命中
- 图上关系：→ HasCallSite，→ ReadsCache

```
  uni.setStorageSync('token', token);
}
export function readToken() {
  return uni.getStorageSync('token');
}

```

### 3. Store `counter/inc`

- 位置：`samples/frontend-backend-link/frontend/src/api.js:46`
- 得分：97.9 · 跳数 0 · 来源种子 `counter/inc` · 直接命中
- 图上关系：← Mutates

```
// 前端**语义节点**之三：Vuex / Pinia → `Store`（本 FKB 用 `semantic_kinds` 声明的新种类）
export function bumpCounter() {
  return $store.dispatch('counter/inc');
}

// 反例（必须**不**产生契约）：同名 getter，接收者不是 HTTP 客户端。
```

### 4. ConfigKey `TIMEOUT`

- 位置：`samples/frontend-backend-link/frontend/config/app.js:5`
- 得分：97.8 · 跳数 0 · 来源种子 `TIMEOUT` · 直接命中

```
export default {
  HTTP_REQUEST_URL: 'https://demo.example.com',
  TIMEOUT: 60000,
};
```

### 5. CallSite `uni.setStorageSync`

- 位置：`samples/frontend-backend-link/frontend/src/api.js:33`
- 得分：89.6 · 跳数 1 · 来源种子 `rememberToken`

```
// `uni.setStorageSync('k', v)` / `localStorage.getItem('k')` 合成 `Cache` 节点。
export function rememberToken(token) {
  uni.setStorageSync('token', token);
}
export function readToken() {
  return uni.getStorageSync('token');
```



