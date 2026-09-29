### 「订单支付流程」

# 召回上下文：订单支付流程

- 工程：#3
- 查询词：订单支付流程, 订单, 支付, 流程
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `pay` Method（得分 488.0）
- `pay` Method（得分 395.4）
- `payOrder` Function（得分 445.4）
- `orderPrepay` Function（得分 351.5）
- `orderH5pay` Function（得分 351.5）
- `parseHeaders` Function（得分 93.8）
- `parseHeaders` Function（得分 93.8）

## 相关代码

### 1. Function `payOrder`

- 位置：`samples/java-projects/litemall/litemall-admin/src/api/order.js:39`
- 得分：445.4 · 跳数 0 · 来源种子 `payOrder` · 直接命中
- 图上关系：← Calls，→ Calls，→ CallsHttp，→ HasCallSite

```
}

export function payOrder(data) {
  return request({
    url: '/order/pay',
    method: 'post',
```

### 2. Method `pay`

- 位置：`samples/java-projects/litemall/litemall-admin-api/src/main/java/org/linlinjava/litemall/admin/web/AdminOrderController.java:107`
- 得分：395.4 · 跳数 0 · 来源种子 `pay` · 直接命中
- 图上关系：→ HasCallSite ×4，→ Calls ×2，← HandledBy

```
    }

    @RequiresPermissions("admin:order:pay")
    @RequiresPermissionsDesc(menu = {"商场管理", "订单管理"}, button = "订单收款")
    @PostMapping("/pay")
    public Object pay(@RequestBody String body) {
```

### 3. Function `orderPrepay`

- 位置：`samples/java-projects/litemall/litemall-vue/src/api/api.js:303`
- 得分：351.5 · 跳数 0 · 来源种子 `orderPrepay` · 直接命中
- 图上关系：← Calls，→ Calls，→ HasCallSite

```
}
const OrderPrepay='/order/prepay'; // 订单的预支付会话
export function orderPrepay(data) {
  return request({
    url: OrderPrepay,
    method: 'post',
```

### 4. Function `orderH5pay`

- 位置：`samples/java-projects/litemall/litemall-vue/src/api/api.js:311`
- 得分：351.5 · 跳数 0 · 来源种子 `orderH5pay` · 直接命中
- 图上关系：← Calls，→ Calls，→ HasCallSite

```
}
const OrderH5pay = '/order/h5pay'; // h5支付
export function orderH5pay(data) {
  return request({
    url: OrderH5pay,
    method: 'post',
```

### 5. Method `pay`

- 位置：`samples/java-projects/litemall/litemall-admin-api/src/main/java/org/linlinjava/litemall/admin/service/AdminOrderService.java:293`
- 得分：488.0 · 跳数 0 · 来源种子 `pay` · 直接命中
- 图上关系：→ HasCallSite ×14，→ Calls ×2，← Calls

```
    }

    public Object pay(String body) {
        Integer orderId = JacksonUtil.parseInteger(body, "orderId");
        String newMoney = JacksonUtil.parseString(body, "newMoney");

```



### 「商品库存扣减」

# 召回上下文：商品库存扣减

- 工程：#3
- 查询词：商品库存扣减, 商品, 库存, 扣减
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `reduceStock` Method（得分 1216.9）
- `reduceStock` Method（得分 1351.2）
- `addStock` Method（得分 681.6）
- `addStock` Method（得分 680.1）
- `LitemallGoodsProductExample` Method（得分 337.5）

## 相关代码

### 1. Method `reduceStock`

- 位置：`samples/java-projects/litemall/litemall-db/src/main/java/org/linlinjava/litemall/db/service/LitemallGoodsProductService.java:57`
- 得分：1351.2 · 跳数 0 · 来源种子 `reduceStock` · 直接命中
- 图上关系：→ Calls ×2，→ HasCallSite，→ WritesDb

```
    }

    public int reduceStock(Integer id, Short num){
        return goodsProductMapper.reduceStock(id, num);
    }

```

### 2. Method `reduceStock`

- 位置：`samples/java-projects/litemall/litemall-db/src/main/java/org/linlinjava/litemall/db/dao/GoodsProductMapper.java:7`
- 得分：1216.9 · 跳数 0 · 来源种子 `reduceStock` · 直接命中
- 图上关系：← Calls ×2，→ HasCallSite ×2，→ WritesDb

```
public interface GoodsProductMapper {
    int addStock(@Param("id") Integer id, @Param("num") Short num);
    int reduceStock(@Param("id") Integer id, @Param("num") Short num);
}
```

### 3. Method `addStock`

- 位置：`samples/java-projects/litemall/litemall-db/src/main/java/org/linlinjava/litemall/db/dao/GoodsProductMapper.java:6`
- 得分：681.6 · 跳数 0 · 来源种子 `addStock` · 直接命中
- 图上关系：← Calls ×2，→ HasCallSite ×2，→ WritesDb

```

public interface GoodsProductMapper {
    int addStock(@Param("id") Integer id, @Param("num") Short num);
    int reduceStock(@Param("id") Integer id, @Param("num") Short num);
}
```

### 4. Method `addStock`

- 位置：`samples/java-projects/litemall/litemall-db/src/main/java/org/linlinjava/litemall/db/service/LitemallGoodsProductService.java:53`
- 得分：680.1 · 跳数 0 · 来源种子 `addStock` · 直接命中
- 图上关系：→ Calls ×2，→ HasCallSite，→ WritesDb

```
    }

    public int addStock(Integer id, Short num){
        return goodsProductMapper.addStock(id, num);
    }

```

### 5. Class `LitemallGoodsProductService`

- 位置：`samples/java-projects/litemall/litemall-db/src/main/java/org/linlinjava/litemall/db/service/LitemallGoodsProductService.java:14`
- 得分：675.6 · 跳数 1 · 来源种子 `reduceStock`
- 图上关系：→ HasCallSite ×3，← Calls

```
import java.util.List;

@Service
public class LitemallGoodsProductService {
    @Resource
    private LitemallGoodsProductMapper litemallGoodsProductMapper;
```



