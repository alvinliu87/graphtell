# litemall

litemall 电商系统（Java/SpringBoot）

- 许可证：**MIT**
- 上游：https://github.com/linlinjava/litemall
- 样本路径（本地，不随仓库分发）：`samples/java-projects/litemall`

## 图规模

| 节点类型 | 数量 |
| --- | --- |
| CallSite | 33980 |
| Class | 412 |
| Enum | 68 |
| File | 937 |
| Function | 425 |
| HttpContract | 274 |
| Interface | 141 |
| Method | 11847 |
| Schedule | 4 |
| Store | 18 |
| Table | 34 |

## 规则检测结果

> 共 274 条违规（info 151、warning 123）。

| 规则 | 严重度 | 位置 | 信息 |
| --- | --- | --- | --- |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/ad.js:4 | 前端调用了 GET /ad/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/ad.js:20 | 前端调用了 GET /ad/read，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/user.js:28 | 前端调用了 GET /address/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/admin.js:4 | 前端调用了 GET /admin/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/admin.js:20 | 前端调用了 GET /admin/readmin，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/aftersale.js:4 | 前端调用了 GET /aftersale/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/login.js:24 | 前端调用了 GET /auth/info，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/login.js:32 | 前端调用了 GET /auth/kaptcha，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/brand.js:4 | 前端调用了 GET /brand/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/brand.js:20 | 前端调用了 GET /brand/read，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/category.js:12 | 前端调用了 GET /category/l1，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/category.js:4 | 前端调用了 GET /category/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/category.js:27 | 前端调用了 GET /category/read，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/user.js:36 | 前端调用了 GET /collect/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/comment.js:4 | 前端调用了 GET /comment/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/config.js:19 | 前端调用了 GET /config/express，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/config.js:4 | 前端调用了 GET /config/mall，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/config.js:34 | 前端调用了 GET /config/order，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/config.js:49 | 前端调用了 GET /config/wx，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/coupon.js:4 | 前端调用了 GET /coupon/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/coupon.js:44 | 前端调用了 GET /coupon/listuser，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/coupon.js:20 | 前端调用了 GET /coupon/read，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/dashboard.js:4 | 前端调用了 GET /dashboard，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/user.js:44 | 前端调用了 GET /feedback/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/user.js:52 | 前端调用了 GET /footprint/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/goods.js:44 | 前端调用了 GET /goods/catAndBrand，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/goods.js:28 | 前端调用了 GET /goods/detail，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/goods.js:4 | 前端调用了 GET /goods/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/groupon.js:12 | 前端调用了 GET /groupon/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/groupon.js:4 | 前端调用了 GET /groupon/listRecord，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/user.js:60 | 前端调用了 GET /history/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/issue.js:4 | 前端调用了 GET /issue/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/issue.js:20 | 前端调用了 GET /issue/read，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/keyword.js:4 | 前端调用了 GET /keyword/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/keyword.js:20 | 前端调用了 GET /keyword/read，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/log.js:4 | 前端调用了 GET /log/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/notice.js:4 | 前端调用了 GET /notice/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/notice.js:20 | 前端调用了 GET /notice/read，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/order.js:64 | 前端调用了 GET /order/channel，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/order.js:16 | 前端调用了 GET /order/detail，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/order.js:5 | 前端调用了 GET /order/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/profile.js:19 | 前端调用了 GET /profile/lsnotice，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/profile.js:12 | 前端调用了 GET /profile/nnotice，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/region.js:11 | 前端调用了 GET /region/clist，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/region.js:4 | 前端调用了 GET /region/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/role.js:4 | 前端调用了 GET /role/list，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/role.js:60 | 前端调用了 GET /role/options，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/role.js:44 | 前端调用了 GET /role/permissions，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/role.js:20 | 前端调用了 GET /role/read，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |
| `frontend-calls-missing-backend` | warning | samples/java-projects/litemall/litemall-admin/src/api/stat.js:20 | 前端调用了 GET /stat/goods，图上没有对应后端路由（可能是真幽灵调用，也可能是前端 baseURL 前缀未参与归一） |

> …共 274 条，仅展示前 50。
