### 「购物车结算」

# 召回上下文：购物车结算

> ⚠️ **召回质量：中（置信度 0.39）** — 部分特征词未命中（结算），结果可能不完整
>
> 建议改用以下特征词自行检索：`结算`、`settle`、`settlement`

- 工程：#4
- 查询词：购物车结算, 购物车, 结算
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `cart` Table（得分 244.4）
- `cart` Queue（得分 242.0）
- `settle` Method（得分 121.0）
- `Cart` Class（得分 194.7）
- `Cart` Class（得分 200.2）
- `cart.discount_amount` Column（得分 144.5）
- `refreshCart` Method（得分 134.6）

## 相关代码

### 1. Table `cart`

- 位置：`samples/php-projects/laravel/bagisto/packages/Webkul/Checkout/src/Database/Migrations/2024_04_23_133154_add_incl_tax_columns_in_cart_table.php:26`
- 得分：244.4 · 跳数 0 · 来源种子 `cart` · 直接命中
- 图上关系：→ HasColumn ×36，← MapsTo ×11，← ReadsDb ×3，← WritesDb ×2

```
        });

        DB::table('cart')->update([
            'sub_total_incl_tax' => DB::raw('sub_total + tax_total'),
            'base_sub_total_incl_tax' => DB::raw('base_sub_total + base_tax_total'),
        ]);
```

### 2. Queue `cart`

- 位置：`samples/php-projects/laravel/bagisto/packages/Webkul/Checkout/src/Cart.php:265`
- 得分：242.0 · 跳数 0 · 来源种子 `cart` · 直接命中
- 图上关系：← PublishesTo ×10

```
        }

        Event::dispatch('checkout.cart.add.before', $product->id);

        if (! $this->cart) {
            $this->createCart([]);
```

### 3. Class `Cart`

- 位置：`samples/php-projects/laravel/bagisto/packages/Webkul/Checkout/src/Cart.php:23`
- 得分：200.2 · 跳数 0 · 来源种子 `Cart` · 直接命中
- 图上关系：← Calls ×18，← ResolvesTo

```
use Webkul\Tax\Repositories\TaxCategoryRepository;

class Cart
{
    /**
     * The cart instance.
```

### 4. Class `Cart`

- 位置：`samples/php-projects/laravel/bagisto/packages/Webkul/Checkout/src/Facades/Cart.php:8`
- 得分：187.9 · 跳数 0 · 来源种子 `Cart` · 直接命中
- 图上关系：← Calls ×70，→ ResolvesTo

```
use Webkul\Checkout\Cart as BaseCart;

class Cart extends Facade
{
    /**
     * Get the registered name of the component.
```

### 5. Column `cart.discount_amount`

- 位置：（无位置信息）
- 得分：144.5 · 跳数 0 · 来源种子 `cart.discount_amount` · 直接命中
- 图上关系：← HasColumn



### 「商品分类查询」

# 召回上下文：商品分类查询

- 工程：#4
- 查询词：商品分类查询, 商品, 分类
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `getCategoryUrls` Method（得分 779.9）
- `getCategoryJsonLd` Method（得分 799.6）
- `getTaxCategory` Method（得分 669.6）
- `getProductMaxPrice` Method（得分 669.6）
- `getEffectiveQuery` Method（得分 558.6）
- `getProductOffers` Method（得分 492.9）
- `getProductAggregateRating` Method（得分 558.1）
- `getChannels` Method（得分 503.0）

## 相关代码

### 1. Method `getCategoryJsonLd`

- 位置：`samples/php-projects/laravel/bagisto/packages/Webkul/Product/src/Helpers/SEO.php:159`
- 得分：799.6 · 跳数 0 · 来源种子 `getCategoryJsonLd` · 直接命中
- 图上关系：→ HasCallSite ×5，→ ReadsConfig

```
     * @return string
     */
    public function getCategoryJsonLd($category)
    {
        $data = [
            '@type' => 'WebSite',
```

### 2. Method `getCategoryUrls`

- 位置：`samples/php-projects/laravel/bagisto/packages/Webkul/FPC/src/Listeners/Product.php:96`
- 得分：779.9 · 跳数 0 · 来源种子 `getCategoryUrls` · 直接命中
- 图上关系：→ HasCallSite ×2，← Calls

```
     * @param  \Webkul\Product\Contracts\Product  $product
     */
    protected function getCategoryUrls($product): array
    {
        $urls = [];

```

### 3. Method `getTaxCategory`

- 位置：`samples/php-projects/laravel/bagisto/packages/Webkul/Product/src/Type/AbstractType.php:675`
- 得分：669.6 · 跳数 0 · 来源种子 `getTaxCategory` · 直接命中
- 图上关系：→ HasCallSite ×2

```
     * @return TaxCategory
     */
    public function getTaxCategory()
    {
        $taxCategoryId = $this->product->parent?->tax_category_id ?? $this->product->tax_category_id;

```

### 4. Method `getProductMaxPrice`

- 位置：`samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Http/Controllers/API/CategoryController.php:123`
- 得分：669.6 · 跳数 0 · 来源种子 `getProductMaxPrice` · 直接命中
- 图上关系：→ HasCallSite ×10

```
     * Get product maximum price.
     */
    public function getProductMaxPrice($categoryId = null): JsonResource
    {
        if (core()->getConfigData('catalog.products.search.engine') == 'elastic') {
            $searchEngine = core()->getConfigData('catalog.products.search.storefront_mode');
```

### 5. Method `getEffectiveQuery`

- 位置：`samples/php-projects/laravel/bagisto/packages/Webkul/Shop/src/Http/Controllers/API/ProductController.php:115`
- 得分：558.6 · 跳数 0 · 来源种子 `getEffectiveQuery` · 直接命中
- 图上关系：→ HasCallSite ×2，← Calls

```
     * It will return the effective query based on the search engine.
     */
    protected function getEffectiveQuery(string $originalQuery, string $searchEngine): ?string
    {
        $effectiveQuery = $this->productRepository->setSearchEngine($searchEngine)->getSuggestions($originalQuery);

```



