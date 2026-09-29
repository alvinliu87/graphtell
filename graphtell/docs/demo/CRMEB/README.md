# CRMEB

CRMEB 商城（PHP/ThinkPHP）

- 许可证：**Apache-2.0**
- 上游：https://github.com/crmeb/CRMEB
- 样本路径（本地，不随仓库分发）：`samples/php-projects/thinkphp/CRMEB`

## 图规模

| 节点类型 | 数量 |
| --- | --- |
| Cache | 42 |
| CallSite | 80263 |
| Class | 1043 |
| ConfigKey | 278 |
| Const | 209 |
| Event | 20 |
| EventBus | 28 |
| File | 2189 |
| Function | 1907 |
| HttpContract | 1603 |
| Interface | 6 |
| Method | 6724 |
| Middleware | 10 |
| Namespace | 229 |
| Page | 29 |
| Property | 1024 |
| Queue | 27 |
| Schedule | 17 |
| Store | 75 |
| Table | 156 |
| Trait | 4 |

## 规则检测结果

> 共 1036 条违规（critical 1、error 3、info 692、warning 340）。

| 规则 | 严重度 | 位置 | 信息 |
| --- | --- | --- | --- |
| `sql-injection-raw` | critical | samples/php-projects/thinkphp/CRMEB/crmeb/app/adminapi/controller/v1/setting/SystemCrud.php:599 | 原始 SQL 执行点 Db::query 的 SQL 直接拼接了变量（位于 samples/php-projects/thinkphp/CRMEB/crmeb/app/adminapi/controller/v1/setting/SystemCrud.php:599）—— 高危 SQL 注入 |
| `cors-reflect-origin` | error | samples/php-projects/thinkphp/CRMEB/crmeb/app/adminapi/route/route.php:65 | Access-Control-Allow-Origin 被设为请求 Origin（位于 samples/php-projects/thinkphp/CRMEB/crmeb/app/adminapi/route/route.php:65）—— 反射源站，凭证级跨域泄露 |
| `cors-reflect-origin` | error | samples/php-projects/thinkphp/CRMEB/crmeb/app/kefuapi/route/route.php:110 | Access-Control-Allow-Origin 被设为请求 Origin（位于 samples/php-projects/thinkphp/CRMEB/crmeb/app/kefuapi/route/route.php:110）—— 反射源站，凭证级跨域泄露 |
| `cors-reflect-origin` | error | samples/php-projects/thinkphp/CRMEB/crmeb/app/outapi/route/route.php:107 | Access-Control-Allow-Origin 被设为请求 Origin（位于 samples/php-projects/thinkphp/CRMEB/crmeb/app/outapi/route/route.php:107）—— 反射源站，凭证级跨域泄露 |
| `ext-call-in-loop` | warning | samples/php-projects/thinkphp/CRMEB/crmeb/app/services/diy/ThemeServices.php:748 | 循环内发起外部调用 curl_exec（位于 samples/php-projects/thinkphp/CRMEB/crmeb/app/services/diy/ThemeServices.php:748）—— N 条记录 = N 次串行网络往返 |
| `ext-call-in-loop` | warning | samples/php-projects/thinkphp/CRMEB/crmeb/app/services/diy/ThemeServices.php:736 | 循环内发起外部调用 curl_init（位于 samples/php-projects/thinkphp/CRMEB/crmeb/app/services/diy/ThemeServices.php:736）—— N 条记录 = N 次串行网络往返 |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/uploadPictures.js:52 | 前端调用了 DELETE /file/category/:*，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/product.js:60 | 前端调用了 DELETE /product/cache，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/systemAdmin.js:51 | 前端调用了 DELETE /setting/admin/:*，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/systemBackendRouting.js:102 | 前端调用了 DELETE /system/route_cate/:*，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/uni-app/api/admin.js:347 | 前端调用了 GET /admin/manage/user/label/:*，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/uni-app/api/order.js:189 | 前端调用了 GET /admin/order/express/:*，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/uni-app/api/admin.js:298 | 前端调用了 GET /admin/order/split_cart_info/:*，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/user.js:575 | 前端调用了 GET /agent/division/agent_agreement/info，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/uni-app/api/order.js:311 | 前端调用了 GET /ali_pay，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/setting.js:229 | 前端调用了 GET /app/feedback/:*/edit，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:18 | 前端调用了 GET /app/routine，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:61 | 前端调用了 GET /app/routine/:*/edit，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:50 | 前端调用了 GET /app/routine/create，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:426 | 前端调用了 GET /app/wechat/action，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:395 | 前端调用了 GET /app/wechat/group，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:416 | 前端调用了 GET /app/wechat/group/:*/edit，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:405 | 前端调用了 GET /app/wechat/group/create，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/setting.js:198 | 前端调用了 GET /app/wechat/speechcraft/:*/edit，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/setting.js:482 | 前端调用了 GET /app/wechat/speechcraftcate/:*/edit，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:364 | 前端调用了 GET /app/wechat/tag，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:385 | 前端调用了 GET /app/wechat/tag/:*/edit，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:374 | 前端调用了 GET /app/wechat/tag/create，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:106 | 前端调用了 GET /app/wechat/template，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:128 | 前端调用了 GET /app/wechat/template/:*/edit，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:117 | 前端调用了 GET /app/wechat/template/create，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:332 | 前端调用了 GET /app/wechat/user，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/app.js:343 | 前端调用了 GET /app/wechat/user/tag_group，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/cms.js:85 | 前端调用了 GET /cms/category/:*/edit，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/cms.js:42 | 前端调用了 GET /cms/cms/:*，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/system.js:609 | 前端调用了 GET /crmeb_product，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/setting.js:1279 | 前端调用了 GET /diy/link/category/form/:*/:*，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/order.js:466 | 前端调用了 GET /export/batchOrderDelivery/:*/:*/:*，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/order.js:499 | 前端调用了 GET /export/expressList，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/user.js:478 | 前端调用了 GET /export/memberCard/:*，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/marketing.js:480 | 前端调用了 GET /export/storeBargain，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/marketing.js:491 | 前端调用了 GET /export/storeCombination，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/order.js:475 | 前端调用了 GET /export/storeIntegralOrder，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/order.js:387 | 前端调用了 GET /export/storeOrder，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/product.js:362 | 前端调用了 GET /export/storeProduct，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/marketing.js:502 | 前端调用了 GET /export/storeSeckill，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/uploadPictures.js:41 | 前端调用了 GET /file/category/:*/edit，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/setting.js:1130 | 前端调用了 GET /file/scan_upload/qrcode?pid=:param，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/setting.js:413 | 前端调用了 GET /freight/express/:*/edit，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/php-projects/thinkphp/CRMEB/template/admin/src/api/product.js:28 | 前端调用了 GET /goods/goods_category，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |

> …共 1036 条，仅展示前 50。
