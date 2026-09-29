# bagisto

Bagisto 电商系统（PHP/Laravel）

- 许可证：**MIT**
- 上游：https://github.com/bagisto/bagisto
- 样本路径（本地，不随仓库分发）：`samples/php-projects/laravel/bagisto`

## 图规模

| 节点类型 | 数量 |
| --- | --- |
| Cache | 2 |
| CallSite | 76366 |
| Class | 1352 |
| Column | 1124 |
| ConfigKey | 244 |
| Const | 208 |
| Enum | 19 |
| EnumCase | 134 |
| Event | 7 |
| File | 3066 |
| Function | 304 |
| HttpContract | 207 |
| Interface | 123 |
| Method | 6034 |
| Middleware | 10 |
| Namespace | 365 |
| Property | 797 |
| Queue | 83 |
| Table | 141 |
| Trait | 16 |

## 规则检测结果

> 共 304 条违规（critical 2、info 59、warning 243）。

| 规则 | 严重度 | 位置 | 信息 |
| --- | --- | --- | --- |
| `sql-injection-where-interp` | critical | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Http/Controllers/DataGrid/DataGridController.php:38 | where 条件 app->where 直接拼接了变量（位于 samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Http/Controllers/DataGrid/DataGridController.php:38）—— SQL 注入风险 |
| `sql-injection-where-interp` | critical | samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Http/Controllers/DataGridController.php:37 | where 条件 app->where 直接拼接了变量（位于 samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Http/Controllers/DataGridController.php:37）—— SQL 注入风险 |
| `ext-call-in-loop` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Core/src/Helpers/Exchange/FixerExchange.php:49 | 循环内发起外部调用 Http::get（位于 samples/php-projects/laravel/bagisto/packages/Webkul/Core/src/Helpers/Exchange/FixerExchange.php:49）—— N 条记录 = N 次串行网络往返 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Routes/api.php:57 | 契约 DELETE /all 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Routes/api.php:77 | 契约 DELETE /coupon 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/customers-routes.php:117 | 契约 DELETE /delete/{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/settings-routes.php:188 | 契约 DELETE /destroy/{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/appearance-routes.php:35 | 契约 DELETE /edit/{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/customers-routes.php:59 | 契约 DELETE /items 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Routes/api.php:69 | 契约 DELETE /selected 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/customers-routes.php:100 | 契约 DELETE /{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/customers-routes.php:51 | 契约 DELETE /{id}/compare-items 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/sales-routes.php:124 | 契约 DELETE /{id}/coupon 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/sales-routes.php:114 | 契约 DELETE /{id}/items 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/customers-routes.php:45 | 契约 DELETE /{id}/wishlist-items 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Routes/api.php:27 | 契约 GET /attributes 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Routes/api.php:29 | 契约 GET /attributes/{attribute_id}/options 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/SocialLogin/src/Http/routes.php:9 | 契约 GET /callback 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/sales-routes.php:100 | 契约 GET /config/{productId} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Routes/store-front-routes.php:39 | 契约 GET /confirmation/{uuid} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/marketing-routes.php:34 | 契约 GET /copy/{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Routes/api.php:17 | 契约 GET /countries 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/catalog-routes.php:27 | 契约 GET /create 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/sales-routes.php:46 | 契约 GET /create/{cartId} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Routes/api.php:79 | 契约 GET /cross-sell 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/settings-routes.php:220 | 契约 GET /download-error-report/{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/settings-routes.php:202 | 契约 GET /download-images-queued/{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/settings-routes.php:204 | 契约 GET /download-images-status/{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/settings-routes.php:216 | 契约 GET /download-sample-images-zip/{type?} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/settings-routes.php:214 | 契约 GET /download-sample/{type}/{format} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/settings-routes.php:218 | 契约 GET /download/{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Routes/store-front-routes.php:122 | 契约 GET /downloadable/download-sample/{type}/{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Routes/customer-routes.php:101 | 契约 GET /edit 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/catalog-routes.php:31 | 契约 GET /edit/{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/reporting-routes.php:20 | 契约 GET /export 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Razorpay/src/Routes/web.php:14 | 契约 GET /fail 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/sales-routes.php:130 | 契约 GET /get 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/sales-routes.php:157 | 契约 GET /get-messages 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/notification-routes.php:12 | 契约 GET /get-notifications 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/sales-routes.php:149 | 契约 GET /get-order-items/{orderId} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/sales-routes.php:151 | 契约 GET /get-resolution-reasons/{resolutionType} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Routes/customer-routes.php:121 | 契约 GET /html-view 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/settings-routes.php:190 | 契约 GET /import/{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/settings-routes.php:210 | 契约 GET /index/{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Installer/src/Routes/web.php:14 | 契约 GET /install 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/customers-routes.php:57 | 契约 GET /items 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/settings-routes.php:208 | 契约 GET /link/{id} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/rest-routes.php:26 | 契约 GET /look-up 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Routes/api.php:31 | 契约 GET /max-price/{id?} 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |
| `http-contract-without-handler` | warning | samples/php-projects/laravel/bagisto/packages/Webkul/Admin/src/Routes/notification-routes.php:10 | 契约 GET /notifications 既没有 handler 属性也没有 HandledBy 出边 —— 可能是真死链，也可能是图没识别出路由 |

> …共 304 条，仅展示前 50。
