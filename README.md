# vrc-utilities

把 VRCX 的数据采集能力搬到 Cloudflare Workers：不用开电脑，云端常驻登录 VRChat 账号，
持续记录**自己与好友的状态、位置变化和通知**，随时用浏览器查看。

Rust + `workers-rs`，跑在 Cloudflare 的 Workers / Durable Objects / D1 上。

---

## 一、能力边界（先看这节，避免预期错位）

| 想要的数据 | 云端能不能拿到 | 说明 |
|---|---|---|
| 自己的资料、头像、信任等级、状态、所在地 | ✅ | `GET /auth/user` |
| 好友列表 + 每人的在线状态、位置、状态描述 | ✅ | `GET /users/{id}/friends` |
| 好友上线/下线/**换房间**的实时推送 | ✅ | pipeline 长连接 |
| 邀请、好友请求、群公告、通知、经济事件 | ✅ | pipeline |
| **公共实例的人数与成员名单** | ⚠️ 部分 | 公开实例可从 API 读到 occupants；私密/好友+/群组实例按设计就不可枚举 |
| **"某房间里所有玩家的进出"（含非好友）** | ❌ | 必须有客户端**身处该实例**才有这个视野。VRCX 是游戏内轮询实现的，云端没有等价物 |

所以第 6 行做不到，第 5 行是"好友进出 + 公开实例成员"的上限。这也是 90% 实际使用价值的所在。

**副作用**：账号挂上 pipeline 后，它在 VRChat 里会显示为在线/活跃，好友能看到你在线。

---

## 二、架构

```
                    ┌──────────────── Worker（无状态门面）─────────────────┐
   浏览器 ─────────▶│  /                   看板静态页（编进 wasm）          │
   看板             │  /api/session        账号 + pipeline 健康度           │
                    │  /api/events         事件流（增量游标 since_id）      │
                    │  /api/friends        好友列表                        │
                    │  /api/login|verify   转发给 DO                       │
                    │  /ws                 升级为 WebSocket → DO           │
                    └───────────────┬─────────────────────────────────────┘
                                    │ stub.fetch()
                    ┌───────────────▼─────── AccountSession（每账号一个 DO）─┐
                    │  出站 WebSocket → wss://pipeline.vrchat.cloud          │
                    │  alarm（2 分钟）→ 续期 + 僵死检测 + 重连                │
                    │  accept_web_socket → 向浏览器广播事件                   │
                    └───────────────┬───────────────────────────────────────┘
                                    │ 增量写
                    ┌───────────────▼───────────────┐
                    │  D1：events / friends / users  │
                    │      worlds / instances / ...  │
                    └────────────────────────────────┘
```

### 三个绕不过去的平台约束

1. **出站 WebSocket 不支持 hibernation。**
   没有 `accept_web_socket` 的 DO 在 70~140 秒无事件后被驱逐，长连接随之中断；
   且单个出站操作最多只阻止驱逐 15 分钟。→ 用 `setAlarm` 每 2 分钟续期。
   重排 alarm 时必须带 `allowConcurrency: true`，否则会把正在执行的 alarm 处理器当成取消。

2. **auth cookie 与登录出口 IP 绑定。**
   pipeline 在 IP 不匹配时直接回 `authToken doesn't correspond with an active session` 并断开。
   所以 cookie 只能存在 DO 里（单点、可寻址），不能放在无状态 Worker。

3. **`State` / `Storage` 都不实现 `Clone`**，而 `spawn_local` 要求 `'static` future。
   所以后台循环只捕获 owned 的 `Env`（可克隆）和一个 `JsValue` 形式的状态句柄，
   WebSocket 列表按需通过 `Reflect` 调 `getWebSockets()` 取。

---

## 三、部署

需要 Node 18+、Rust + `wasm32-unknown-unknown` target、以及一个 Cloudflare 账号。

```bash
# 1. 装构建工具
cargo install -q "worker-build@^0.8"
rustup target add wasm32-unknown-unknown

# 2. 建 D1 库，把打印出来的 database_id 填进 wrangler.toml
npx wrangler d1 create vrc-utilities

# 3. 应用表结构
npx wrangler d1 migrations apply vrc-utilities --remote

# 4. 设置看板令牌（强烈建议用 secret，不要留在 wrangler.toml 里）
npx wrangler secret put DASHBOARD_TOKEN

# 5. 部署
npx wrangler deploy
```

部署后打开 `https://<你的域名>/?token=<你的令牌>`，在页面里填账号密码登录。
`?token=` 只用于首次进站，之后会存进 localStorage 并从地址栏移除。

**首次登录带 2FA 的账号**：提交账号密码后页面会切换成验证码输入框，
输入 TOTP 动态码即可（也支持 `method: "email"` 走邮箱验证码）。

---

## 四、主号登录的代价（重要）

会话与登录 IP 绑定，意味着：

- 云端 Worker 登录后，**本机 VRCX 的登录会被挤掉**；反之本机先登录，云端的 pipeline 会立刻断开。
- Cloudflare 出口 IP 会因为 DO 迁移而漂移，届时需要重新登录。
- 想两边同时在线，只能用**专用小号**在云端常驻 —— 但小号只能看到它自己的好友动态。

既然你选了主号，实际用法就是：**云端常驻采集，本机 VRCX 不再登录**，
需要看数据时访问网页，或者把云端的导出拉回本地。

---

## 五、费用

DO 因为有出站长连接，24 小时不休眠，会持续计费（按分配的 128 MB 计）：

| 项目 | 用量 | 免费额度 | 结论 |
|---|---|---|---|
| DO duration | 86,400 s/天 × 0.125 GB ≈ **10,368 GB-s/天** | 13,000 GB-s/天 | 占 80%，**免费**但偏紧 |
| DO 写入 | alarm 720 次/天 + 好友差异行 | 100,000 行/天 | 宽裕 |
| D1 写入 | 事件行 ≈ 账号活跃度决定 | 100,000 行/天 | 宽裕 |
| DO 存储 | 少量 KV | 5 GB | 宽裕 |

付费版（$5/月最低消费）下：compute duration 约 0.33M GB-s/月，落在 400,000 GB-s 的
包含额度内，所以**总成本就是 $5/月**。

> 已实现的写入优化：`replace_friends` 先读一次现状，只写真正变化的行。
> 否则 300 好友 × 每 2 分钟全量重写 ≈ 每月 1300 万行，单这一项就打爆写入额度。

---

## 六、数据模型

`schema/0001_init.sql`，7 张表：

- `accounts` / `sessions` —— 账号自身信息与 pipeline 健康度（看板顶栏读这两张）
- `users` / `worlds` / `instances` —— 维度表，只存展示必需的列
- `friends` —— 每个好友一行当前状态，历史走 `events`
- `events` —— 核心事件流，保留 `raw_json`

所有时间列是**毫秒整数**。`events.raw_json` 在落库前会经过 `sanitize_raw()`
递归删除 `authToken`/`password`/`totp` 等字段并做 16KB 截断 —— 这张表会长期保存，
不能把凭据写进去。`schema/_validate.py` 可用本地 SQLite 快速校验表结构。

---

## 七、开发

```bash
# 编译（wasm）
cargo build --release --target wasm32-unknown-unknown

# 单元测试（跑在宿主平台，覆盖事件解析 / 位置解析 / 脱敏 / base64）
cargo test --lib

# 前端语法与 HTML 结构校验
node webcheck.cjs

# 完整构建 + 本地调试
worker-build --release --panic-unwind
npx wrangler dev
```

> 本机编译 `syn` 时若遇到 `STATUS_STACK_BUFFER_OVERRUN`，用
> `RUST_MIN_STACK=33554432` 并降低并行度（`-j 2`）即可绕过。

### 代码结构

| 文件 | 职责 |
|---|---|
| `src/lib.rs` | 路由、看板鉴权、静态页、D1 只读查询、cron |
| `src/session.rs` | Durable Object：登录、pipeline 循环、alarm 保活、广播 |
| `src/vrchat.rs` | VRChat API 客户端（含手写 base64，避免额外依赖） |
| `src/event.rs` | pipeline 事件解析、location 解析、脱敏 |
| `src/store.rs` | D1 落库（差异写入） |
| `src/db.rs` | D1/JsValue 绑定辅助 |
| `web/` | 看板前端，构建时用 `include_str!` 编进 wasm |

---

## 八、排障

### 网页把 HTML 当纯文本显示（所有内容挤成一坨代码）

**已修复。** 根因在 `workers-rs` 的响应构造 API：

- `ResponseBuilder::ok()` 会在内部**强制**把 `Content-Type` 设为 `text/plain; charset=utf-8`
- `ResponseBuilder::from_bytes()` 会强制设为 `application/octet-stream`

两者都在链式调用**之后**执行，会覆盖你在 builder 上用 `with_header("Content-Type", ...)`
先设的值。所以无论怎么写，`Content-Type` 都会是纯文本，浏览器于是直接显示 HTML 源码。

正确做法是在**已经构造好的 `Response`** 上改头：

```rust
let mut res = Response::builder().from_bytes(bytes)?;
res.headers_mut().set("Content-Type", mime)?;   // 这个时机才有效
```

`serve_index` 另可直接用 `Response::builder().from_html(...)`，它内部会正确设 `text/html`。
`node webcheck.cjs` 现在会扫描 `src/*.rs`，一旦再出现"在 builder 链里设 Content-Type"
的写法就直接报错。

### 所有 `/api/*` 返回 500

几乎一定是**没有应用数据库迁移**，服务端日志会显示 `no such table: accounts`：

```bash
npx wrangler d1 migrations apply vrc-utilities --remote   # 线上
npx wrangler d1 migrations apply vrc-utilities --local    # 本地调试
```

注意 `wrangler dev` 不会自动应用迁移，本地调试前也要手动跑一次 `--local`。

### 没登录的时候后台也在调 VRChat API / 页面在空轮询

**已修复。** 有两处，后端那处严重得多：

**后端（真正的元凶）：alarm 链会永续空跑。**
旧代码里 alarm 无条件做两件事：刷新好友 + 重排下一次 alarm。
于是一旦你走到过 2FA 那一步（哪怕最后放弃或失败），alarm 链就自己续下去，
**每 2 分钟调一次 VRChat 的 friends API，永不停止**。

现在 alarm 开头先检查有没有**已认证**的 cookie：

```rust
let has_session = storage.get::<String>(K_COOKIE).await?.is_some();
if !has_session {
    // 停止 alarm 链，避免空跑 API
    let _ = storage.delete_alarm().await;
    return Response::empty();
}
```

待验证状态（pending cookie）存在 storage 里，所以停止 alarm 不会丢，
用户回来继续输验证码仍然有效。

注意这里区分了两种 cookie：
- `K_COOKIE` —— 已认证会话，只有它能触发 API 请求和 pipeline
- `K_PENDING` —— 2FA 中间态，**不**允许用它调 API

`spawn_pipeline()` 也加了一层同样的保险，未登录时连一次请求都不发。

**前端：未登录时不再请求 friends / events。**
轮询改成两步：先只问 `/api/session` 和 `/api/pending` 确认登录状态，
确认已登录才去拉好友和事件。未登录时列表保持空态，页脚显示"未登录 — 已暂停数据请求"。

顺带把固定 `setInterval` 换成了自排程 `setTimeout`，间隔随状态变化：

| 状态 | 轮询间隔 |
|---|---|
| 已登录 | 3 秒（实时性优先） |
| 未登录 / 等验证码 / 限流冷却 | 10 秒 |

这几种状态下数据不会变，高频轮询没有意义。连续失败还会翻倍退避。

**cron 也收紧了**：`accounts` 表只在**认证成功后**才写入，所以从未登录成功的账号
不会被 cron 唤醒 DO。

### 提交验证码报 HTTP 429（Too many requests）

**已处理。** 这是 VRChat 对验证码端点的限流，通常是短时间内多次提交触发的。

关键在于：**限流期间绝对不能继续点提交** —— 每次请求都会让冷却窗口续期，会陷入
"越试越久"的死循环。所以现在的行为是：

- 登录和验证码两条路径都会识别 429，并读取 `Retry-After` 头（拿不到则读响应体里的
  `retryAfter`，再拿不到给 300 秒默认值，并夹紧在 1~3600 秒之间）
- 前端收到 429 后**禁用提交按钮和输入框**，显示 `MM:SS` 倒计时，到点自动恢复
- 冷却截止时间存在 Durable Object 里，所以**刷新页面倒计时也会继续**，不会重置
- 倒计时结束后提示"可以重新尝试了"，此时再提交

冷却期间请耐心等完，反复刷新或换浏览器都绕不过去（状态在服务端）。

### 登录后提交验证码报"响应里没有找到 auth cookie"

**已修复**，原因有两类，现在会分别报出具体原因：
1. **验证码本身不正确** —— VRChat 对错误的 TOTP 返回 `HTTP 200 {"verified": false}`
   且不下发 auth cookie。旧代码只看 cookie 存在与否，于是把"码打错了"报成了
   "没找到 cookie"。现在会明确提示 `verified=false`，并允许**直接重试**（待验证
   会话仍然有效）。
2. **账号用的是邮箱验证码而不是 TOTP** —— VRChat 用 `requiresTwoFactorAuth: ["emailOtp"]`
   区分，两个验证端点不同。旧代码一律走 TOTP 端点，这类账号必然失败。
   现在会记住登录时声明的方式，自动选对端点。

如果提示"请重新提交账号密码"，说明暂存的会话已失效（验证码错太多次或超时），
按提示重新登录即可。错误信息里现在也会带上 VRChat 的原始错误文本和 HTTP 状态码，
例如 `401（账号或密码不正确，或该会话已失效）— Invalid Username/Password`。

### 看板显示"未授权"

`DASHBOARD_TOKEN` 还是默认值 `CHANGE_ME` 时系统会**跳过鉴权**（并打警告）；
一旦你把它设成真实值，就必须用 `?token=<值>` 进站，之后令牌会存进 localStorage。

---

## 九、已知限制 / 后续

- **pipeline 断线期间的事件无法找回**：VRChat 不提供事件回放。
  断档会在时间轴上以 `sys-pipeline-disconnected` / `sys-pipeline-connected` 标出。
- **`events` 表暂无保留策略**：长期运行需要加定期清理或归档到 R2。
- **WebSocket 推送尚未被前端消费**：后端已能广播，前端目前仍走 3 秒轮询。
- **回灌本地 VRCX 未实现**：建议路线是云端导出 VRCX 兼容 JSON + 本地一次性导入器，
  直接写 VRCX 的 SQLite 会随其版本升级而崩坏。
- **多账号未实现**：`accounts` 表已按多账号设计，但路由目前只认最近登录的那个。

## 十、合规提醒

VRChat 的 API 文档由社区维护，长期常驻的自动化客户端属于灰区（VRCX 本身也是第三方工具）。
建议：**自己用、控制频率、不要对外提供服务**。好友轮询当前是 2 分钟一次，
不要改到秒级。
