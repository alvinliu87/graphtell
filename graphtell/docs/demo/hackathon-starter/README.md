# hackathon-starter

Node/Express 起步项目

源码样本：[`samples/hackathon-starter`](../../samples/hackathon-starter)

## 图规模

| 节点类型 | 数量 |
| --- | --- |
| CallSite | 6775 |
| File | 83 |
| Function | 98 |
| HttpContract | 123 |
| Middleware | 10 |

## 规则检测结果

> 共 1 条违规（warning 1）。

| 规则 | 严重度 | 位置 | 信息 |
| --- | --- | --- | --- |
| `get-with-write-verb` | warning | samples/hackathon-starter/app.js:266 | 契约 GET /reset/:token 是 GET 却包含写动词，违反 REST 只读语义（可能被重试 / 预取误触发写操作） |

## 提示词增强（召回）示例

> 以下为中文问句经 GraphTell 召回出的相关代码上下文包（Markdown）。

### 「用户登录的实现」

# 召回上下文：用户登录的实现

- 工程：#3
- 查询词：用户登录的实现, 用户, 登录, 的实, 实现
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `saveOAuth2UserTokens` Function（得分 440.3）
- `handleAuthLogin` Function（得分 439.5）
- `getUserByLogin` Function（得分 351.6）
- `sendPasswordlessLoginLinkIfUserExists` Function（得分 351.6）
- `POST /login/webauthn-start` HttpContract（得分 204.8）

## 相关代码

### 1. Function `saveOAuth2UserTokens`

- 位置：`samples/hackathon-starter/config/passport.js:196`
- 得分：440.3 · 跳数 0 · 来源种子 `saveOAuth2UserTokens` · 直接命中
- 图上关系：→ HasCallSite ×18，← Calls ×2

```
 *    - Creates new token entry with provided tokens and expirations
 */
async function saveOAuth2UserTokens(req, accessToken, refreshToken, accessTokenExpiration, refreshTokenExpiration, providerName, tokenConfig = {}) {
  try {
    let user = await User.findById(req.user._id);
    if (!user) {
```

### 2. Function `handleAuthLogin`

- 位置：`samples/hackathon-starter/config/passport.js:78`
- 得分：439.5 · 跳数 0 · 来源种子 `handleAuthLogin` · 直接命中
- 图上关系：→ HasCallSite ×18，← Calls，→ Calls

```
 * Returns User (new or updated) on success or throws Error on failure.
 */
async function handleAuthLogin(req, accessToken, refreshToken, providerName, params, providerProfile, sessionAlreadyLoggedIn, tokenSecret, oauth2provider, tokenConfig = {}, refreshTokenExpiration = null) {
  if (sessionAlreadyLoggedIn) {
    const existingUser = await User.findOne({
      [providerName]: { $eq: providerProfile.id },
```

### 3. Function `getUserByLogin`

- 位置：`samples/hackathon-starter/controllers/api.js:628`
- 得分：351.6 · 跳数 0 · 来源种子 `getUserByLogin` · 直接命中
- 图上关系：→ HasCallSite ×3，← Calls，→ CallsHttp

```
  };

  const getUserByLogin = async (loginID) => {
    const response = await fetch(`https://api.twitch.tv/helix/users?login=${loginID}`, {
      headers: {
        Authorization: `Bearer ${token.accessToken}`,
```

### 4. Function `sendPasswordlessLoginLinkIfUserExists`

- 位置：`samples/hackathon-starter/controllers/user.js:166`
- 得分：351.6 · 跳数 0 · 来源种子 `sendPasswordlessLoginLinkIfUserExists` · 直接命中
- 图上关系：→ HasCallSite ×5，← Calls

```
 * mitigate account enumeration attacks.
 */
async function sendPasswordlessLoginLinkIfUserExists(user, req) {
  const token = await User.generateToken();
  user.loginToken = token;
  user.loginExpires = Date.now() + 900000; // 15 min
```

### 5. File `config/passport.js`

- 位置：`samples/hackathon-starter/config/passport.js`
- 得分：220.2 · 跳数 1 · 来源种子 `saveOAuth2UserTokens`



### 「发送邮件的逻辑」

# 召回上下文：发送邮件的逻辑

- 工程：#3
- 查询词：发送邮件的逻辑, 发送, 邮件, 的逻, 逻辑
- 跳数上限：2，命中 5 条

## 种子（直接命中关键词）

- `sendContactEmail` Function（得分 746.7）
- `sendTwoFactorEmail` Function（得分 746.7）
- `POST /account/2fa/email/enable` HttpContract（得分 157.5）
- `POST /account/2fa/email/remove` HttpContract（得分 157.5）
- `sendSSE` Function（得分 105.4）

## 相关代码

### 1. Function `sendContactEmail`

- 位置：`samples/hackathon-starter/controllers/contact.js:90`
- 得分：746.7 · 跳数 0 · 来源种子 `sendContactEmail` · 直接命中
- 图上关系：← Calls，→ HasCallSite

```
  }

  const sendContactEmail = async () => {
    const mailOptions = {
      to: process.env.SITE_CONTACT_EMAIL,
      from: `${fromName} <${fromEmail}>`,
```

### 2. Function `sendTwoFactorEmail`

- 位置：`samples/hackathon-starter/controllers/user.js:788`
- 得分：746.7 · 跳数 0 · 来源种子 `sendTwoFactorEmail` · 直接命中
- 图上关系：← Calls，→ HasCallSite

```
 * between first send and resend.
 */
async function sendTwoFactorEmail(email, code, req, successMsg = 'A verification code has been sent to your email.') {
  const mailOptions = {
    to: email,
    from: process.env.SITE_CONTACT_EMAIL,
```

### 3. File `controllers/contact.js`

- 位置：`samples/hackathon-starter/controllers/contact.js`
- 得分：373.3 · 跳数 1 · 来源种子 `sendContactEmail`

### 4. CallSite `nodemailerConfig.sendMail`

- 位置：`samples/hackathon-starter/controllers/contact.js:108`
- 得分：373.3 · 跳数 1 · 来源种子 `sendContactEmail`

```
    };

    return nodemailerConfig.sendMail(mailSettings);
  };

  try {
```

### 5. HttpContract `POST /account/2fa/email/enable`

- 位置：`samples/hackathon-starter/app.js:277`
- 得分：157.5 · 跳数 0 · 来源种子 `POST /account/2fa/email/enable` · 直接命中
- 图上关系：→ PassesThrough

```
app.post('/account/profile', passportConfig.isAuthenticated, userController.postUpdateProfile);
app.post('/account/password', passportConfig.isAuthenticated, userController.postUpdatePassword);
app.post('/account/2fa/email/enable', passportConfig.isAuthenticated, userController.postEnable2FA);
app.post('/account/2fa/email/remove', passportConfig.isAuthenticated, userController.postRemoveEmail2FA);
app.get('/account/2fa/totp/setup', passportConfig.isAuthenticated, userController.getTotpSetup);
app.post('/account/2fa/totp/setup', passportConfig.isAuthenticated, userController.postTotpSetup);
```



