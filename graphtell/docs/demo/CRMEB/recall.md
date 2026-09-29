### 「订单支付流程」

# 召回上下文：订单支付流程

- 工程：#5
- 查询词：订单支付流程, 订单, 支付, 流程
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `paymentOrder` Method（得分 1032.7）
- `paymentOrder` Method（得分 1040.4）
- `zeroYuanPayment` Method（得分 779.8）
- `zeroYuanPayment` Method（得分 758.8）
- `createorder` Method（得分 709.8）
- `paymentService` Method（得分 669.7）
- `configForPayment` Method（得分 531.0）

## 相关代码

### 1. Method `paymentOrder`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/crmeb/services/app/WechatService.php:401`
- 得分：1040.4 · 跳数 0 · 来源种子 `paymentOrder` · 直接命中
- 图上关系：→ HasCallSite ×5，← Calls ×2

```
     * @return Order
     */
    protected static function paymentOrder($openid, $out_trade_no, $total_fee, $attach, $body, $detail = '', $trade_type = 'JSAPI', $options = [])
    {
        $total_fee = bcmul($total_fee, 100, 0);
        $order = array_merge(compact('out_trade_no', 'total_fee', 'attach', 'body', 'detail', 'trade_type'), $options);
```

### 2. Method `paymentOrder`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/crmeb/services/app/MiniProgramService.php:371`
- 得分：1032.7 · 跳数 0 · 来源种子 `paymentOrder` · 直接命中
- 图上关系：→ HasCallSite ×4，← Calls ×2

```
     * @return Order
     */
    protected static function paymentOrder($openid, $out_trade_no, $total_fee, $attach, $body, $detail = '', $trade_type = 'JSAPI', $options = [])
    {
        $total_fee = bcmul($total_fee, 100, 0);
        $order = array_merge(compact('openid', 'out_trade_no', 'total_fee', 'attach', 'body', 'detail', 'trade_type'), $options);
```

### 3. Method `zeroYuanPayment`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/app/services/order/OtherOrderServices.php:268`
- 得分：779.8 · 跳数 0 · 来源种子 `zeroYuanPayment` · 直接命中
- 图上关系：→ ReadsConfig ×7，→ HasCallSite ×3，→ Calls ×2，→ PublishesTo，→ WritesDb

```
     * @return bool
     */
    public function zeroYuanPayment($orderInfo)
    {
        if ($orderInfo['paid']) {
            throw new ApiException('订单已支付');
```

### 4. Method `zeroYuanPayment`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/app/services/order/StoreOrderSuccessServices.php:49`
- 得分：758.8 · 跳数 0 · 来源种子 `zeroYuanPayment` · 直接命中
- 图上关系：→ Calls ×2，→ HasCallSite ×2，→ WritesDb

```
     * @throws \think\exception\DbException
     */
    public function zeroYuanPayment(array $orderInfo, int $uid, string $payType = PayServices::YUE_PAY)
    {
        if ($orderInfo['paid']) {
            throw new ApiException('该订单已支付');
```

### 5. Method `createorder`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/crmeb/services/easywechat/miniPayment/WeChatClient.php:68`
- 得分：709.8 · 跳数 0 · 来源种子 `createorder` · 直接命中
- 图上关系：→ HasCallSite ×3，→ Calls

```
     * @throws \GuzzleHttp\Exception\GuzzleException
     */
    public function createorder($order)
    {
        $params = [
            'openid' => $order['openid'],    // 支付者的openid
```



### 「商品库存扣减」

# 召回上下文：商品库存扣减

- 工程：#5
- 查询词：商品库存扣减, 商品, 库存, 扣减
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `decGoodsStock` Method（得分 746.7）
- `decGoodsStock` Method（得分 746.7）
- `decCombinationStock` Method（得分 596.7）
- `decSeckillStock` Method（得分 596.7）
- `decProductStock` Method（得分 727.5）
- `decProductAttrStock` Method（得分 738.6）
- `getSeckillAttrStock` Method（得分 609.1）
- `getProductAttrStock` Method（得分 485.2）

## 相关代码

### 1. Method `decGoodsStock`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/app/services/activity/integral/StoreIntegralOrderServices.php:247`
- 得分：746.7 · 跳数 0 · 来源种子 `decGoodsStock` · 直接命中
- 图上关系：→ HasCallSite ×5，← Calls，→ Calls，→ ResolvesTo

```
     * @param int $bargainId
     */
    public function decGoodsStock(array $productInfo, int $num)
    {
        $res5 = true;
        /** @var StoreIntegralServices $StoreIntegralServices */
```

### 2. Method `decGoodsStock`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/app/services/order/StoreOrderCreateServices.php:391`
- 得分：746.7 · 跳数 0 · 来源种子 `decGoodsStock` · 直接命中
- 图上关系：→ HasCallSite ×22，← Calls，→ Calls

```
     * @param int $bargainId
     */
    public function decGoodsStock(array $cartInfo, int $combinationId, int $seckillId, int $bargainId, int $advanceId)
    {
        $res5 = true;
        /** @var StoreProductServices $services */
```

### 3. Method `decProductAttrStock`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/app/services/product/sku/StoreProductAttrValueServices.php:147`
- 得分：738.6 · 跳数 0 · 来源种子 `decProductAttrStock` · 直接命中
- 图上关系：→ Calls ×4，→ HasCallSite ×2，→ ReadsConfig，→ ReadsDb

```
     * @return mixed
     */
    public function decProductAttrStock($productId, $unique, $num, $type = 0)
    {
        $res = $this->dao->decStockIncSales([
            'product_id' => $productId,
```

### 4. Method `decProductStock`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/app/services/product/product/StoreProductServices.php:1901`
- 得分：727.5 · 跳数 0 · 来源种子 `decProductStock` · 直接命中
- 图上关系：→ HasCallSite ×4，→ Calls ×3

```
     * @return bool
     */
    public function decProductStock(int $num, int $productId, string $unique = '')
    {
        $res = true;
        if ($unique) {
```

### 5. Method `getSeckillAttrStock`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/app/services/product/sku/StoreProductAttrValueServices.php:205`
- 得分：609.1 · 跳数 0 · 来源种子 `getSeckillAttrStock` · 直接命中
- 图上关系：→ Calls ×4，→ HasCallSite ×4，→ ReadsCache，→ WritesCache

```
     * @throws \think\db\exception\ModelNotFoundException
     */
    public function getSeckillAttrStock(int $productId, string $unique, bool $isNew = false)
    {
        $key = md5('seclkill_attr_stock_' . $productId . '_' . $unique);
        $stock = CacheService::get($key);
```



### 「用户优惠券」

# 召回上下文：用户优惠券

- 工程：#5
- 查询词：用户优惠券, 用户, 优惠券
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `GET /user/member/coupons/list` HttpContract（得分 590.4）
- `memberCouponUserGroupBymonth` Method（得分 589.1）
- `memberIssueUserCoupon` Method（得分 572.1）
- `memberCouponUserGroupBymonth` Method（得分 588.3）
- `memberCouponList` Method（得分 534.1）
- `sendMemberCoupon` Method（得分 402.7）
- `getDiscount` Method（得分 359.1）
- `getMemberCoupon` Method（得分 499.4）
- `memberCouponIsFail` Method（得分 550.4）

## 相关代码

### 1. HttpContract `GET /user/member/coupons/list`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/app/api/route/v1.php:307`
- 得分：590.4 · 跳数 0 · 来源种子 `GET /user/member/coupons/list` · 直接命中
- 图上关系：→ PassesThrough ×3，← CallsHttp，→ HandledBy

```
        Route::post('user/member/card/draw', 'v1.user.MemberCardController/draw_member_card')->name('userMemberCardDraw')->option(['real_name' => '卡密领取会员卡']);//卡密领取会员卡
        Route::post('user/member/card/create', 'v1.order.OtherOrderController/create')->name('userMemberCardCreate')->option(['real_name' => '购买卡创建订单']);//购买卡创建订单
        Route::get('user/member/coupons/list', 'v1.user.MemberCardController/memberCouponList')->name('userMemberCouponsList')->option(['real_name' => '会员券列表']);//会员券列表
        Route::get('user/member/overdue/time', 'v1.user.MemberCardController/getOverdueTime')->name('userMemberOverdueTime')->option(['real_name' => '会员时间']);//会员时间
    })->option(['parent' => 'user', 'cate_name' => '会员卡']);

```

### 2. Method `memberCouponUserGroupBymonth`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/app/dao/activity/coupon/StoreCouponUserDao.php:173`
- 得分：589.1 · 跳数 0 · 来源种子 `memberCouponUserGroupBymonth` · 直接命中
- 图上关系：→ HasCallSite ×7，→ Calls ×2，← Calls，→ ReadsDb

```
     * @throws \think\db\exception\ModelNotFoundException
     */
    public function memberCouponUserGroupBymonth(array $where)
    {
        return $this->search($where, false)
            ->whereMonth('add_time')
```

### 3. Method `memberCouponUserGroupBymonth`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/app/services/activity/coupon/StoreCouponUserServices.php:399`
- 得分：588.3 · 跳数 0 · 来源种子 `memberCouponUserGroupBymonth` · 直接命中
- 图上关系：→ Calls ×2，→ HasCallSite，→ ReadsDb

```
     * @throws \think\db\exception\ModelNotFoundException
     */
    public function memberCouponUserGroupBymonth(array $where)
    {
        return $this->dao->memberCouponUserGroupBymonth($where);
    }
```

### 4. Method `memberIssueUserCoupon`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/app/services/activity/coupon/StoreCouponIssueServices.php:433`
- 得分：572.1 · 跳数 0 · 来源种子 `memberIssueUserCoupon` · 直接命中
- 图上关系：→ HasCallSite ×8，→ Calls ×5，← Calls，→ ReadsDb

```
     * @throws \think\db\exception\ModelNotFoundException
     */
    public function memberIssueUserCoupon($id, $uid)
    {
        $issueCouponInfo = $this->dao->getInfo((int)$id);
        if ($issueCouponInfo) {
```

### 5. Method `memberCouponIsFail`

- 位置：`samples/php-projects/thinkphp/CRMEB/crmeb/app/services/activity/coupon/StoreCouponUserServices.php:408`
- 得分：550.4 · 跳数 0 · 来源种子 `memberCouponIsFail` · 直接命中
- 图上关系：→ Calls ×2，→ HasCallSite，→ WritesDb

```
     * @return bool|mixed
     */
    public function memberCouponIsFail($coupon_user)
    {
        if (!$coupon_user) return false;
        if ($coupon_user['use_time'] == 0) {
```



