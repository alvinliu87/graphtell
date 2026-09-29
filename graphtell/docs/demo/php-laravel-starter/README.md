# php-laravel-starter

Laravel 起步项目

源码样本：[`samples/php-projects/laravel-starter`](../../samples/php-projects/laravel-starter)

## 图规模

| 节点类型 | 数量 |
| --- | --- |
| CallSite | 337 |
| Class | 18 |
| ConfigKey | 128 |
| File | 31 |
| HttpContract | 2 |
| Method | 9 |
| Middleware | 2 |
| Namespace | 9 |
| Property | 1 |

## 规则检测结果

> 未发现违规。

## 提示词增强（召回）示例

> 以下为中文问句经 GraphTell 召回出的相关代码上下文包（Markdown）。

### 「用户注册与登录」

# 召回上下文：用户注册与登录

> ⚠️ **召回质量：中（置信度 0.60）** — 部分特征词未命中（注册），结果可能不完整
>
> 建议改用以下特征词自行检索：`注册`、`register`、`signup`

- 工程：#5
- 查询词：用户注册与登录, 用户, 注册, 登录
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `SLACK_BOT_USER_OAUTH_TOKEN` ConfigKey（得分 351.8）
- `User` Class（得分 347.5）
- `AUTH_GUARD` ConfigKey（得分 126.6）
- `AUTH_PASSWORD_RESET_TOKEN_TABLE` ConfigKey（得分 126.6）
- `User` Class（得分 110.4）

## 相关代码

### 1. ConfigKey `SLACK_BOT_USER_OAUTH_TOKEN`

- 位置：`samples/php-projects/laravel-starter/config/services.php:33`
- 得分：351.8 · 跳数 0 · 来源种子 `SLACK_BOT_USER_OAUTH_TOKEN` · 直接命中
- 图上关系：← ReadsConfig

```
    'slack' => [
        'notifications' => [
            'bot_user_oauth_token' => env('SLACK_BOT_USER_OAUTH_TOKEN'),
            'channel' => env('SLACK_BOT_USER_DEFAULT_CHANNEL'),
        ],
    ],
```

### 2. Class `User`

- 位置：（无位置信息）
- 得分：347.5 · 跳数 0 · 来源种子 `User` · 直接命中

### 3. File `config/services.php`

- 位置：`samples/php-projects/laravel-starter/config/services.php`
- 得分：175.9 · 跳数 1 · 来源种子 `SLACK_BOT_USER_OAUTH_TOKEN`

### 4. Class `User`

- 位置：`samples/php-projects/laravel-starter/app/Models/User.php:13`
- 得分：173.7 · 跳数 1 · 来源种子 `User`
- 图上关系：← Calls

```
use Illuminate\Notifications\Notifiable;

#[Fillable(['name', 'email', 'password'])]
#[Hidden(['password', 'remember_token'])]
class User extends Authenticatable
{
```

### 5. ConfigKey `AUTH_GUARD`

- 位置：`samples/php-projects/laravel-starter/config/auth.php:19`
- 得分：126.6 · 跳数 0 · 来源种子 `AUTH_GUARD` · 直接命中
- 图上关系：← ReadsConfig

```

    'defaults' => [
        'guard' => env('AUTH_GUARD', 'web'),
        'passwords' => env('AUTH_PASSWORD_BROKER', 'users'),
    ],

```



### 「路由定义」

# 召回上下文：路由定义

> ⚠️ **召回质量：中（置信度 1.00）** — 查询未包含可评估的意图概念（多为领域专有词 / 生僻说法），无法确认召回质量，结果需自行判断

- 工程：#5
- 查询词：定义
- 结构提示：HttpContract
- 跳数上限：2，命中 0 条

## 相关代码



