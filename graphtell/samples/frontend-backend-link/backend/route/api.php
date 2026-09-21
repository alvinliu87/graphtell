<?php
use think\facade\Route;

// 后端契约：POST /api/delete
Route::post('/api/delete', 'app\controller\Order@delete');

// 后端契约：GET /api/ping —— 与前端**成员式**调用 `request.get('/api/ping')` 汇聚
Route::get('/api/ping', 'app\controller\Ping@ping');

// 带路径参数的路由：与前端**拼接式 / 模板串** URL 按形状汇聚
// （前端 `:param`、后端 `:id`，参数名不同但形状相同）
Route::get('/api/invoice/detail/:id', 'app\controller\Ping@detail');
Route::get('/api/order/invoice_detail/:uni', 'app\controller\Ping@orderDetail');

// REST **资源路由**：一条语句 = 7 条契约（index / create / save / read / edit /
// update / delete）。`->except(['read'])` 表示 `read` 动作不注册，
// 因此 `GET /api/tags/:id` **不应**出现在图里 —— 否则就是凭空造路由。
Route::resource('/api/items', 'app\controller\Item');
Route::resource('/api/tags', 'app\controller\Tag')->except(['read']);
