<?php
namespace app\controller;

use think\facade\Cache;

class Order
{
    // 处理删除（被 route/api.php 的 Route::post('/api/delete') 指向）
    public function delete()
    {
        // 后端缓存读取：应经 `fkb/php/common.yaml` 通用缓存规则合成
        // `Cache` 语义节点并标注 `side = backend`（与前端 `side = frontend` 对称）。
        $status = Cache::get('order-status');
        // ...
    }
}
