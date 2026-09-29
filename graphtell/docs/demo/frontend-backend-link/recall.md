### 「订单创建流程」

# 召回上下文：订单创建流程

- 工程：#1
- 查询词：订单创建流程, 订单, 创建, 流程
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `Order` Class（得分 130.0）
- `orderInvoiceDetail` Function（得分 105.0）
- `delete` Method（得分 52.9）
- `order-status` Cache（得分 49.4）
- `GET /api/order/invoice_detail/:*` HttpContract（得分 35.4）

## 相关代码

### 1. Class `Order`

- 位置：`samples/frontend-backend-link/backend/app/controller/Order.php:6`
- 得分：130.0 · 跳数 0 · 来源种子 `Order` · 直接命中

```
use think\facade\Cache;

class Order
{
    // 处理删除（被 route/api.php 的 Route::post('/api/delete') 指向）
    public function delete()
```

### 2. Function `orderInvoiceDetail`

- 位置：`samples/frontend-backend-link/frontend/src/api.js:26`
- 得分：105.0 · 跳数 0 · 来源种子 `orderInvoiceDetail` · 直接命中
- 图上关系：→ CallsHttp，→ HasCallSite

```

// **模板串** URL：`` `...${id}` `` 同理（CRMEB uni-app 的真实写法）。
export function orderInvoiceDetail(id) {
  request.get(`/api/order/invoice_detail/${id}`);
}

```

### 3. File `backend/app/controller/Order.php`

- 位置：`samples/frontend-backend-link/backend/app/controller/Order.php`
- 得分：65.0 · 跳数 1 · 来源种子 `Order`

### 4. Method `delete`

- 位置：`samples/frontend-backend-link/backend/app/controller/Order.php:9`
- 得分：65.0 · 跳数 1 · 来源种子 `Order`
- 图上关系：← HandledBy，→ HasCallSite，→ ReadsCache

```
{
    // 处理删除（被 route/api.php 的 Route::post('/api/delete') 指向）
    public function delete()
    {
        // 后端缓存读取：应经 `fkb/php/common.yaml` 通用缓存规则合成
        // `Cache` 语义节点并标注 `side = backend`（与前端 `side = frontend` 对称）。
```

### 5. CallSite `request.get`

- 位置：`samples/frontend-backend-link/frontend/src/api.js:27`
- 得分：52.5 · 跳数 1 · 来源种子 `orderInvoiceDetail`

```
// **模板串** URL：`` `...${id}` `` 同理（CRMEB uni-app 的真实写法）。
export function orderInvoiceDetail(id) {
  request.get(`/api/order/invoice_detail/${id}`);
}

// 前端**语义节点**示例（与后端 `Cache::set('k')` → `Cache` 同构）：
```



### 「用户登录入口」

# 召回上下文：用户登录入口

> ⚠️ **召回质量：中（置信度 0.65）** — 部分特征词未命中（登录），结果可能不完整
>
> 建议改用以下特征词自行检索：`登录`、`login`、`auth`、`signin`

- 工程：#1
- 查询词：用户登录入口, 用户, 登录, 入口
- 跳数上限：2，命中 4 条

## 种子（直接命中关键词）

- `rememberToken` Function（得分 60.0）

## 相关代码

### 1. Function `rememberToken`

- 位置：`samples/frontend-backend-link/frontend/src/api.js:32`
- 得分：60.0 · 跳数 0 · 来源种子 `rememberToken` · 直接命中
- 图上关系：→ HasCallSite，→ WritesCache

```
// 前端**语义节点**示例（与后端 `Cache::set('k')` → `Cache` 同构）：
// `uni.setStorageSync('k', v)` / `localStorage.getItem('k')` 合成 `Cache` 节点。
export function rememberToken(token) {
  uni.setStorageSync('token', token);
}
export function readToken() {
```

### 2. CallSite `uni.setStorageSync`

- 位置：`samples/frontend-backend-link/frontend/src/api.js:33`
- 得分：30.0 · 跳数 1 · 来源种子 `rememberToken`

```
// `uni.setStorageSync('k', v)` / `localStorage.getItem('k')` 合成 `Cache` 节点。
export function rememberToken(token) {
  uni.setStorageSync('token', token);
}
export function readToken() {
  return uni.getStorageSync('token');
```

### 3. Cache `token`

- 位置：`samples/frontend-backend-link/frontend/src/api.js:33`
- 得分：30.0 · 跳数 1 · 来源种子 `rememberToken`
- 图上关系：← ReadsCache，← WritesCache

```
// `uni.setStorageSync('k', v)` / `localStorage.getItem('k')` 合成 `Cache` 节点。
export function rememberToken(token) {
  uni.setStorageSync('token', token);
}
export function readToken() {
  return uni.getStorageSync('token');
```

### 4. Function `readToken`

- 位置：`samples/frontend-backend-link/frontend/src/api.js:35`
- 得分：15.0 · 跳数 2 · 来源种子 `rememberToken`
- 图上关系：→ HasCallSite，→ ReadsCache

```
  uni.setStorageSync('token', token);
}
export function readToken() {
  return uni.getStorageSync('token');
}

```



