<?php
namespace app\controller;

class Ping
{
    // 处理心跳（被 route/api.php 的 Route::get('/api/ping') 指向），
    // 对应前端**成员式**调用 `request.get('/api/ping')`。
    public function ping()
    {
        // ...
    }
}
