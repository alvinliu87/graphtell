# nestjs-realworld-example-app

NestJS + TypeScript 真实项目

源码样本：[`samples/nestjs-realworld-example-app`](../../samples/nestjs-realworld-example-app)

## 图规模

| 节点类型 | 数量 |
| --- | --- |
| CallSite | 308 |
| Class | 28 |
| Column | 22 |
| File | 42 |
| Function | 2 |
| HttpContract | 17 |
| Method | 70 |
| Middleware | 1 |
| Table | 4 |

## 规则检测结果

> 未发现违规。

## 提示词增强（召回）示例

> 以下为中文问句经 GraphTell 召回出的相关代码上下文包（Markdown）。

### 「用户注册与鉴权」

# 召回上下文：用户注册与鉴权

> ⚠️ **召回质量：低（置信度 0.56）** — 各概念分别被互不相关的节点命中（无任何命中同时覆盖两个概念），多为泛词各自撞名
>
> 建议改用以下特征词自行检索：`注册`、`register`、`signup`

- 工程：#2
- 查询词：用户注册与鉴权, 用户, 注册, 鉴权
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `AuthMiddleware` Middleware（得分 217.2）
- `constructor` Method（得分 157.5）
- `use` Method（得分 157.5）
- `user` Table（得分 140.8）
- `POST /users/login` HttpContract（得分 84.0）

## 相关代码

### 1. Middleware `AuthMiddleware`

- 位置：`samples/nestjs-realworld-example-app/src/user/auth.middleware.ts:10`
- 得分：217.2 · 跳数 0 · 来源种子 `AuthMiddleware` · 直接命中
- 图上关系：← PassesThrough ×2，→ HasCallSite ×2，→ DependsOn

```

@Injectable()
export class AuthMiddleware implements NestMiddleware {
  constructor(private readonly userService: UserService) {}

  async use(req: Request, res: Response, next: NextFunction) {
```

### 2. Method `constructor`

- 位置：`samples/nestjs-realworld-example-app/src/user/auth.middleware.ts:11`
- 得分：157.5 · 跳数 0 · 来源种子 `constructor` · 直接命中

```
@Injectable()
export class AuthMiddleware implements NestMiddleware {
  constructor(private readonly userService: UserService) {}

  async use(req: Request, res: Response, next: NextFunction) {
    const authHeaders = req.headers.authorization;
```

### 3. Method `use`

- 位置：`samples/nestjs-realworld-example-app/src/user/auth.middleware.ts:13`
- 得分：157.5 · 跳数 0 · 来源种子 `use` · 直接命中
- 图上关系：→ HasCallSite ×6

```
  constructor(private readonly userService: UserService) {}

  async use(req: Request, res: Response, next: NextFunction) {
    const authHeaders = req.headers.authorization;
    if (authHeaders && (authHeaders as string).split(' ')[1]) {
      const token = (authHeaders as string).split(' ')[1];
```

### 4. Table `user`

- 位置：`samples/nestjs-realworld-example-app/src/user/user.entity.ts:6`
- 得分：140.8 · 跳数 0 · 来源种子 `user` · 直接命中
- 图上关系：← ForeignKey，← MapsTo，→ ForeignKey

```
import { ArticleEntity } from '../article/article.entity';

@Entity('user')
export class UserEntity {

  @PrimaryGeneratedColumn()
```

### 5. File `src/user/auth.middleware.ts`

- 位置：`samples/nestjs-realworld-example-app/src/user/auth.middleware.ts`
- 得分：108.6 · 跳数 1 · 来源种子 `AuthMiddleware`



### 「文章发布的接口」

# 召回上下文：文章发布的接口

> ⚠️ **召回质量：中（置信度 0.38）** — 部分特征词未命中（接口），结果可能不完整
>
> 建议改用以下特征词自行检索：`接口`、`api`、`interface`

- 工程：#2
- 查询词：文章发布的, 文章, 发布, 布的
- 结构提示：HttpContract
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `POST /:slug/comments` HttpContract（得分 192.0）
- `article` Table（得分 169.0）
- `POST /:slug/favorite` HttpContract（得分 153.6）
- `POST /:username/follow` HttpContract（得分 153.6）
- `POST /users` HttpContract（得分 153.6）

## 相关代码

### 1. HttpContract `POST /:slug/comments`

- 位置：`samples/nestjs-realworld-example-app/src/article/article.controller.ts:76`
- 得分：192.0 · 跳数 0 · 来源种子 `POST /:slug/comments` · 直接命中
- 图上关系：→ HandledBy

```
  @ApiResponse({ status: 201, description: 'The comment has been successfully created.'})
  @ApiResponse({ status: 403, description: 'Forbidden.' })
  @Post(':slug/comments')
  async createComment(@Param('slug') slug, @Body('comment') commentData: CreateCommentDto) {
    return await this.articleService.addComment(slug, commentData);
  }
```

### 2. Table `article`

- 位置：`samples/nestjs-realworld-example-app/src/article/article.entity.ts:5`
- 得分：169.0 · 跳数 0 · 来源种子 `article` · 直接命中
- 图上关系：← ForeignKey，← MapsTo，→ ForeignKey

```
import { Comment } from './comment.entity';

@Entity('article')
export class ArticleEntity {

  @PrimaryGeneratedColumn()
```

### 3. HttpContract `POST /:slug/favorite`

- 位置：`samples/nestjs-realworld-example-app/src/article/article.controller.ts:93`
- 得分：153.6 · 跳数 0 · 来源种子 `POST /:slug/favorite` · 直接命中
- 图上关系：→ HandledBy

```
  @ApiResponse({ status: 201, description: 'The article has been successfully favorited.'})
  @ApiResponse({ status: 403, description: 'Forbidden.' })
  @Post(':slug/favorite')
  async favorite(@User('id') userId: number, @Param('slug') slug) {
    return await this.articleService.favorite(userId, slug);
  }
```

### 4. HttpContract `POST /:username/follow`

- 位置：`samples/nestjs-realworld-example-app/src/profile/profile.controller.ts:23`
- 得分：153.6 · 跳数 0 · 来源种子 `POST /:username/follow` · 直接命中
- 图上关系：→ HandledBy

```
  }

  @Post(':username/follow')
  async follow(@User('email') email: string, @Param('username') username: string): Promise<ProfileRO> {
    return await this.profileService.follow(email, username);
  }
```

### 5. HttpContract `POST /users`

- 位置：`samples/nestjs-realworld-example-app/src/user/user.controller.ts:32`
- 得分：153.6 · 跳数 0 · 来源种子 `POST /users` · 直接命中
- 图上关系：→ HandledBy

```

  @UsePipes(new ValidationPipe())
  @Post('users')
  async create(@Body('user') userData: CreateUserDto) {
    return this.userService.create(userData);
  }
```



