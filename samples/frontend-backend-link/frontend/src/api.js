import axios from 'axios';
import request from './request';

// 前端调用后端契约：POST /api/delete
// 包成具名函数，使其成为前端第一类语义节点（而非 File 节点）。
export function deleteItem() {
  axios.post('/api/delete');
}

// 同上，但走 **uni-app 形态**的成员式封装：`request.get('/api/ping')`
// —— 成员名即 HTTP method，URL 是首个实参。
// template/uni-app 子工程（`utils/request.js` 里把 uni.request
// 包成 request.get / request.post）正是这种写法；真正的 uni.request 只有一处
// 且 URL 是动态拼串，能被静态确定 location 的就是这层。
export function pingItem() {
  request.get('/api/ping');
}

// **拼接式** URL：`'...' + id` 的形状可静态确定（非字面量段 → `:param`），
// 与后端 `Route::get('/api/invoice/detail/:id')` 按形状汇聚到同一契约。
export function invoiceDetail(id) {
  request.get('/api/invoice/detail/' + id);
}

// **模板串** URL：`` `...${id}` `` 同理（uni-app 的真实写法）。
export function orderInvoiceDetail(id) {
  request.get(`/api/order/invoice_detail/${id}`);
}

// 前端**语义节点**示例（与后端 `Cache::set('k')` → `Cache` 同构）：
// `uni.setStorageSync('k', v)` / `localStorage.getItem('k')` 合成 `Cache` 节点。
export function rememberToken(token) {
  uni.setStorageSync('token', token);
}
export function readToken() {
  return uni.getStorageSync('token');
}

// 前端**语义节点**之二：i18n 文案 → `I18nKey`（与后端 `lang/*.php` 同类）
export function sayHi() {
  return i18n.t('hello');
}

// 前端**语义节点**之三：Vuex / Pinia → `Store`（本 FKB 用 `semantic_kinds` 声明的新种类）
export function bumpCounter() {
  return $store.dispatch('counter/inc');
}

// 反例（必须**不**产生契约）：同名 getter，接收者不是 HTTP 客户端。
// 若它出现在路由视角里，说明成员式识别扩大过度了。
export function readCache() {
  cache.get('/api/ping');
}
