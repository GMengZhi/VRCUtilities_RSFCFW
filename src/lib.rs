//! vrc-utilities —— 把 VRCX 的数据采集能力搬到 Cloudflare Workers。
//!
//! 职责划分：
//!   Worker（本文件）  无状态门面：路由、看板鉴权、静态页、D1 只读查询。
//!   AccountSession    有状态：持有 pipeline 长连接、保活、重连、写库。
//!   cron              兜底：定期让每个账号刷新一次好友全量快照。

use worker::*;

pub mod db;
pub mod event;
pub mod session;
pub mod store;
pub mod vrchat;

pub use session::AccountSession;

/// 看板静态页直接编进 wasm，省掉 Assets 绑定，也避免和 Worker 路由抢路径。
const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_CSS: &str = include_str!("../web/app.css");
const APP_JS: &str = include_str!("../web/app.js");

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let router = Router::new()
        .get_async("/", serve_index)
        .get_async("/app.css", serve_css)
        .get_async("/app.js", serve_js)
        .get_async("/api/session", api_session)
        .get_async("/api/pending", api_pending)
        .get_async("/api/friends", api_friends)
        .get_async("/api/events", api_events)
        .post_async("/api/login", api_login)
        .post_async("/api/verify", api_verify)
        .post_async("/api/logout", api_logout)
        .get_async("/ws", ws_upgrade);
    router.run(req, env).await
}

/// 用指定 MIME 类型返回字节内容。
///
/// 踩过的坑：`ResponseBuilder::ok()` 会**强制**把 Content-Type 设成
/// `text/plain; charset=utf-8`，`from_bytes()` 则强制设成
/// `application/octet-stream`，两者都在链式调用之后执行、会覆盖先前设的值。
/// 结果就是 HTML 被浏览器当纯文本显示。
/// 唯一可靠的时机是在**已经构造好的 `Response`** 上改 `headers_mut()`。
fn serve_bytes(mime: &str, body: &'static str) -> Result<Response> {
    let bytes = body.as_bytes().to_vec();
    let mut res = Response::builder().from_bytes(bytes)?;
    res.headers_mut().set("Content-Type", mime)?;
    // 文本资源不需要 octet-stream 那套推断，明确禁用嗅探
    res.headers_mut().set("X-Content-Type-Options", "nosniff")?;
    Ok(res)
}

async fn serve_index(_req: Request, _ctx: RouteContext<()>) -> Result<Response> {
    // from_html 内部会正确设置 text/html; charset=utf-8
    Response::builder().from_html(INDEX_HTML)
}

async fn serve_css(_req: Request, _ctx: RouteContext<()>) -> Result<Response> {
    serve_bytes("text/css; charset=utf-8", APP_CSS)
}

async fn serve_js(_req: Request, _ctx: RouteContext<()>) -> Result<Response> {
    serve_bytes("application/javascript; charset=utf-8", APP_JS)
}

/// cron：pipeline 断线期间靠它把好友快照补齐。
///
/// 只对**已完成认证**的账号触发。DO 侧也会再确认一次有没有已认证 cookie，
/// 所以未登录 / 半登录状态的账号不会被空跑 API。
#[event(scheduled)]
async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    if let Err(e) = run_scheduled(&env).await {
        console_error!("cron 执行失败: {e}");
    }
}

async fn run_scheduled(env: &Env) -> Result<()> {
    let db = env.d1("DB")?;
    // 只取已经成功登录过的账号（accounts 行只在认证成功后才写入）
    let ids: Vec<String> = db
        .prepare("SELECT id FROM accounts")
        .all()
        .await?
        .results::<serde_json::Value>()?
        .into_iter()
        .filter_map(|v| v.get("id").and_then(|x| x.as_str()).map(String::from))
        .collect();

    if ids.is_empty() {
        console_log!("cron：没有已登录的账号，跳过");
        return Ok(());
    }

    for id in ids {
        if let Ok(stub) = account_stub(env, &id) {
            // 走 DO 的 /poll-friends：DO 内部会检查已认证 cookie 再决定是否调 API
            let _ = stub.fetch_with_str("https://do/poll-friends").await;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 鉴权
// ---------------------------------------------------------------------------

/// 看板令牌校验。支持 `Authorization: Bearer <token>` 和 `?token=<token>`。
///
/// WebSocket 的浏览器 API 不能自定义请求头，那个端点只能走 query。
/// 这也意味着令牌会出现在 Worker 日志里 —— 所以别把它当成高价值密钥，
/// 真要更强的话请在前面挂 Cloudflare Access。
fn require_auth(req: &Request, env: &Env) -> Option<Response> {
    let expected = env
        .var("DASHBOARD_TOKEN")
        .map(|v| v.to_string())
        .unwrap_or_default();

    if expected.is_empty() || expected == "CHANGE_ME" {
        console_warn!("DASHBOARD_TOKEN 未设置，看板处于无鉴权状态");
        return None;
    }

    let header = req.headers().get("Authorization").ok().flatten();
    let from_header = header
        .as_deref()
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::to_string);

    let from_query = req
        .url()
        .ok()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "token")
                .map(|(_, v)| v.to_string())
        });

    match from_header.or(from_query) {
        Some(t) if t == expected => None,
        _ => Some(
            Response::from_json(&serde_json::json!({ "error": "unauthorized" }))
                .map(|r| r.with_status(401))
                .unwrap_or_else(|_| Response::error("unauthorized", 401).unwrap()),
        ),
    }
}

// ---------------------------------------------------------------------------
// 账号请求代理
// ---------------------------------------------------------------------------

fn account_stub(env: &Env, account_id: &str) -> Result<Stub> {
    env.durable_object("ACCOUNT")?.get_by_name(account_id)
}

/// 把请求体转发给 DO，并原样返回 DO 的响应。
async fn forward(env: &Env, account_id: &str, path: &str, body: Option<&str>) -> Result<Response> {
    let stub = account_stub(env, account_id)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    if let Some(b) = body {
        let headers = Headers::new();
        headers.set("Content-Type", "application/json")?;
        init.with_headers(headers);
        init.with_body(Some(b.into()));
    }
    let req = Request::new_with_init(&format!("https://do{path}"), &init)?;
    stub.fetch_with_request(req).await
}

fn json_error(msg: &str, status: u16) -> Result<Response> {
    Response::from_json(&serde_json::json!({ "error": msg })).map(|r| r.with_status(status))
}

/// 取当前账号 ID：优先 D1，其次问 DO 的 snapshot。
async fn current_account_id(env: &Env) -> Result<Option<String>> {
    let db = env.d1("DB")?;
    let row: Option<serde_json::Value> = db
        .prepare("SELECT id FROM accounts ORDER BY updated_at DESC LIMIT 1")
        .first(None)
        .await?;
    Ok(row.and_then(|v| {
        v.get("id")
            .and_then(|x| x.as_str())
            .map(String::from)
    }))
}

// ---------------------------------------------------------------------------
// 看板 API
// ---------------------------------------------------------------------------

async fn api_session(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Some(res) = require_auth(&req, &ctx.env) {
        return Ok(res);
    }

    // 最近一次尝试登录的用户名。2FA 和限流状态都在这个账号的 DO 里，
    // 所以即使用户还没登录成功，也要靠它定位到对应的 DO。
    //
    // 注意：这一行在一次失败的登录尝试后会**永久留存**。所以下面用它去问 DO
    // 之前必须先确认真的还有"未完成的登录"，否则未登录状态下每 10 秒就会
    // 白唤醒一次 DO。
    let pending_username = latest_login_name(&ctx.env).await?;

    let Some(account_id) = current_account_id(&ctx.env).await? else {
        // 从未登录成功过。要把 2FA / 冷却状态报出去，
        // 否则"提交了密码、正在等验证码"这个中间态前端就看不到。
        //
        // 但**不能无条件问 DO**：D1 里的 pending_login 在一次失败的登录尝试后
        // 会永久留存，无条件问就等于未登录状态下每 10 秒白唤醒一次 DO。
        // 所以先低成本探一下"到底有没有未完成的登录"，探到了就连快照一起带回来，
        // 不再重复请求。
        let snap = match &pending_username {
            Some(u) => pending_snapshot(&ctx.env, u).await,
            None => None,
        };
        let has_pending = snap.is_some();

        let payload = serde_json::json!({
            "account_id": null,
            "display_name": null,
            "status": null,
            "status_description": null,
            "location": null,
            "pipeline_state": "offline",
            "last_event_at": null,
            "last_connect_at": null,
            "last_error": null,
            "reconnect_count": 0,
            "hint": if has_pending {
                "已有待验证会话，请输入验证码"
            } else {
                "尚未登录，请提交 VRChat 账号密码"
            },
        });
        return Response::from_json(&session_payload(payload, pending_username, snap.as_ref()));
    };

    let db = ctx.env.d1("DB")?;
    let row: Option<serde_json::Value> = db
        .prepare(
            "SELECT a.id, a.display_name, a.status, a.location, a.status_description, \
             s.pipeline_state, s.last_event_at, s.last_connect_at, s.last_error, s.reconnect_count \
             FROM accounts a LEFT JOIN sessions s ON s.account_id = a.id WHERE a.id = ?1",
        )
        .bind(&[worker::wasm_bindgen::JsValue::from_str(&account_id)])?
        .first(None)
        .await?;

    // DO 里的实时状态比 D1 新（事件写库有节流），能拿到就以它为准
    let snap = fetch_snapshot(&ctx.env, &account_id).await;
    let mut payload = row.unwrap_or(serde_json::json!({ "id": account_id }));

    if let Some(snap) = &snap {
        for (dst, src) in [
            ("last_event_at", "last_event_at"),
            ("last_error", "last_error"),
            ("reconnect_count", "reconnect_count"),
        ] {
            if let Some(v) = snap.get(src) {
                payload[dst] = v.clone();
            }
        }
    }

    Response::from_json(&session_payload(payload, pending_username, snap.as_ref()))
}

/// 组装 `/api/session` 的响应。
///
/// 2FA 待验证状态和限流冷却只存在于 DO 快照里，而这个接口本来就要取快照，
/// 所以顺手一起返回 —— 这样前端只需要一个轮询请求，而不是两个各打一次 DO。
fn session_payload(
    mut payload: serde_json::Value,
    pending_username: Option<String>,
    snap: Option<&serde_json::Value>,
) -> serde_json::Value {
    let awaiting = snap
        .and_then(|v| v.get("awaiting_2fa"))
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let cooldown = snap
        .and_then(|v| v.get("cooldown_until"))
        .and_then(|x| x.as_i64())
        .map(|until| ((until - crate::db::now_ms()) / 1000).max(0))
        .unwrap_or(0);

    payload["awaiting_2fa"] = serde_json::json!(awaiting);
    payload["cooldown_secs"] = serde_json::json!(cooldown);
    if let Some(u) = pending_username {
        payload["pending_username"] = serde_json::json!(u);
    }
    payload
}

/// 2FA 待验证状态与限流冷却。
///
/// 保留这个接口是为了让"只想知道要不要输验证码"的调用方（比如脚本）能少拉数据。
/// 网页看板不用它 —— 那些信息已经并进 `/api/session`，避免两次 DO 往返。
async fn api_pending(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Some(res) = require_auth(&req, &ctx.env) {
        return Ok(res);
    }

    let username = latest_login_name(&ctx.env).await?;
    let Some(username) = username else {
        return Response::from_json(&serde_json::json!({
            "awaiting_2fa": false,
            "cooldown_secs": 0,
        }));
    };

    // 和 /api/session 用同一套判断，避免在无事可做时白唤醒 DO
    let snap = pending_snapshot(&ctx.env, &username).await;
    let awaiting = snap
        .as_ref()
        .and_then(|v| v.get("awaiting_2fa"))
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let cooldown = snap
        .as_ref()
        .and_then(|v| v.get("cooldown_until"))
        .and_then(|x| x.as_i64())
        .map(|until| ((until - crate::db::now_ms()) / 1000).max(0))
        .unwrap_or(0);

    Response::from_json(&serde_json::json!({
        "awaiting_2fa": awaiting,
        "cooldown_secs": cooldown,
        "username": username,
    }))
}

/// 读 DO 的 `/status` 快照（失败返回 None）。
async fn fetch_snapshot(env: &Env, do_name: &str) -> Option<serde_json::Value> {
    let stub = account_stub(env, &normalize_account_name(do_name)).ok()?;
    let mut res = stub.fetch_with_str("https://do/status").await.ok()?;
    let text = res.text().await.ok()?;
    serde_json::from_str(&text).ok()
}

/// 这个账号的 DO 里到底有没有"未完成的登录"。
///
/// 判断依据是 DO 存储里是否存在 pending cookie（正在等验证码）或尚未过期的
/// 限流冷却，或者已经有已认证 cookie。
///
/// 返回 `Some(快照)` 表示"有未完成状态，快照已经在手上"，调用方不必再问一次 DO；
/// 返回 `None` 表示确实什么都没有 —— 此时未登录状态下就完全不需要唤醒 DO，
/// 这正是"页面开着但没登录"时省掉每 10 秒一次无谓请求的关键。
async fn pending_snapshot(env: &Env, do_name: &str) -> Option<serde_json::Value> {
    let snap = fetch_snapshot(env, do_name).await?;
    let awaiting = snap
        .get("awaiting_2fa")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let cooling = snap
        .get("cooldown_until")
        .and_then(|v| v.as_i64())
        .map(|until| until > crate::db::now_ms())
        .unwrap_or(false);
    let has_cookie = snap
        .get("has_cookie")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if should_query_do(awaiting, cooling, has_cookie) {
        Some(snap)
    } else {
        None
    }
}

/// 未登录状态下，是否还需要为了状态去唤醒 DO。
///
/// 抽成纯函数是因为这里是个容易写错、又不容易察觉的**成本开关**：
/// 判断错成 `true` 就是每 10 秒一次无谓的 DO 唤醒（页面开着挂一整天），
/// 判断错成 `false` 则验证码界面和限流倒计时会失灵。
fn should_query_do(awaiting_2fa: bool, cooling: bool, has_cookie: bool) -> bool {
    awaiting_2fa || cooling || has_cookie
}

#[cfg(test)]
mod tests {
    use super::should_query_do;

    #[test]
    fn queries_do_only_when_there_is_something_to_report() {
        // 正在等验证码 → 必须问，否则验证码界面看不到
        assert!(should_query_do(true, false, false));
        // 限流冷却中 → 必须问，否则倒计时会消失
        assert!(should_query_do(false, true, false));
        // 已有已认证 cookie（只是 D1 还没写上）→ 必须问，否则看不到 pipeline 状态
        assert!(should_query_do(false, false, true));

        // 什么都没有 → 不问。
        // 这一条是成本护栏：判断错了就是页面挂一天、每 10 秒一次无谓的 DO 唤醒。
        assert!(!should_query_do(false, false, false));
    }
}

async fn api_friends(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Some(res) = require_auth(&req, &ctx.env) {
        return Ok(res);
    }
    let db = ctx.env.d1("DB")?;

    let list: Vec<serde_json::Value> = db
        .prepare(
            "SELECT f.user_id, f.state, f.status, f.status_description, f.location, f.world_id, \
             f.platform, f.last_seen_at, u.display_name, u.username, u.current_avatar_thumb, \
             u.user_icon, w.name AS world_name, i.region \
             FROM friends f \
             LEFT JOIN users u ON u.id = f.user_id \
             LEFT JOIN worlds w ON w.id = f.world_id \
             LEFT JOIN instances i ON i.location = f.location \
             ORDER BY CASE f.state WHEN 'online' THEN 0 WHEN 'active' THEN 1 ELSE 2 END, \
             u.display_name",
        )
        .all()
        .await?
        .results()?;

    Response::from_json(&serde_json::json!({ "friends": list }))
}

async fn api_events(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Some(res) = require_auth(&req, &ctx.env) {
        return Ok(res);
    }
    let url = req.url()?;
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

    let since_id: i64 = query
        .get("since_id")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let limit: i64 = query
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    let only_type = query.get("type").cloned();

    let db = ctx.env.d1("DB")?;

    // since_id 是增量游标：
    //   >0 → 取 id 更大的新事件（按 id 升序，前端 prepend 后顺序正确）
    //   =0 → 首屏，取最近 N 条（按 id 降序）
    let mut sql = String::from(
        "SELECT id, type, actor_user_id, actor_display_name, world_id, world_name, \
         instance_location, prev_location, next_location, ts \
         FROM events WHERE 1=1",
    );
    let mut binds: Vec<worker::wasm_bindgen::JsValue> = Vec::new();

    if since_id > 0 {
        sql.push_str(" AND id > ?");
        binds.push(worker::wasm_bindgen::JsValue::from_f64(since_id as f64));
    }
    // 注意绑定顺序：D1 按出现顺序编号，LIMIT 的占位符在最后，
    // 所以它的值必须最后 push。
    if let Some(t) = &only_type {
        sql.push_str(" AND type = ?");
        binds.push(worker::wasm_bindgen::JsValue::from_str(t));
    }
    if since_id > 0 {
        sql.push_str(" ORDER BY id ASC LIMIT ?");
    } else {
        sql.push_str(" ORDER BY id DESC LIMIT ?");
    }
    binds.push(worker::wasm_bindgen::JsValue::from_f64(limit as f64));
    let list: Vec<serde_json::Value> = db.prepare(sql).bind(&binds)?.all().await?.results()?;

    Response::from_json(&serde_json::json!({ "events": list }))
}

// ---------------------------------------------------------------------------
// 登录 / 登出（转发给 DO，因为 cookie 必须待在 DO 里）
// ---------------------------------------------------------------------------

/// 登录凭据在 DO 存储里，所以 Worker 每次都要把请求转到同一个 DO 实例。
/// DO 名字由用户名派生（VRChat 的 Basic 用户名就是邮箱）。
async fn api_login(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Some(res) = require_auth(&req, &ctx.env) {
        return Ok(res);
    }
    let body: serde_json::Value = match req.json().await {
        Ok(v) => v,
        Err(_) => return json_error("请求体必须是 JSON", 400),
    };

    let username = body.get("username").and_then(|v| v.as_str()).unwrap_or("");
    let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
    if username.is_empty() || password.is_empty() {
        return json_error("username / password 不能为空", 400);
    }

    let do_name = normalize_account_name(username);
    // 记住本次登录尝试：2FA 的下一步靠它定位同一个 DO
    remember_login_name(&ctx.env, username).await?;
    forward(&ctx.env, &do_name, "/login", Some(&body.to_string())).await
}

async fn api_verify(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Some(res) = require_auth(&req, &ctx.env) {
        return Ok(res);
    }
    let body: serde_json::Value = match req.json().await {
        Ok(v) => v,
        Err(_) => return json_error("请求体必须是 JSON", 400),
    };

    // 允许不带 username：回退到 D1 里记录的最近登录账号，
    // 这样 2FA 步骤在前端只需要用户填验证码。
    let username = match body.get("username").and_then(|v| v.as_str()) {
        Some(u) if !u.is_empty() => u.to_string(),
        _ => match latest_login_name(&ctx.env).await? {
            Some(u) => u,
            None => return json_error("没有待验证的登录，请先提交账号密码", 400),
        },
    };

    let do_name = normalize_account_name(&username);
    forward(&ctx.env, &do_name, "/verify", Some(&body.to_string())).await
}

/// 读"最近一次尝试登录"的用户名，用于让 2FA 步骤无需重复输入账号。
async fn latest_login_name(env: &Env) -> Result<Option<String>> {
    let db = env.d1("DB")?;
    let row: Option<serde_json::Value> = db
        .prepare("SELECT value FROM app_state WHERE key='pending_login'")
        .first(None)
        .await?;
    Ok(row.and_then(|v| v.get("value").and_then(|x| x.as_str()).map(String::from)))
}

/// 记录最近一次登录尝试（在 api_login 里写入）。
async fn remember_login_name(env: &Env, username: &str) -> Result<()> {
    let db = env.d1("DB")?;
    db.prepare(
        "INSERT INTO app_state (key, value, updated_at) VALUES ('pending_login', ?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
    )
    .bind(&[
        worker::wasm_bindgen::JsValue::from_str(username),
        worker::wasm_bindgen::JsValue::from_f64(crate::db::now_ms() as f64),
    ])?
    .run()
    .await?;
    Ok(())
}

async fn api_logout(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Some(res) = require_auth(&req, &ctx.env) {
        return Ok(res);
    }
    let Some(account_id) = current_account_id(&ctx.env).await? else {
        return json_error("当前没有已登录的账号", 404);
    };
    let stub = account_stub(&ctx.env, &account_id)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    let r = Request::new_with_init("https://do/logout", &init)?;
    stub.fetch_with_request(r).await
}

/// DO 名字不能含任意字符，做一次稳定归一化。
/// 只做字符替换，不引入哈希依赖 —— 同一个用户名必然得到同一个名字。
fn normalize_account_name(username: &str) -> String {
    let cleaned: String = username
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '@' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("acct-{cleaned}")
}

// ---------------------------------------------------------------------------
// WebSocket：把浏览器接到 DO 上收实时事件
// ---------------------------------------------------------------------------

async fn ws_upgrade(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if req.headers().get("Upgrade")?.as_deref() != Some("websocket") {
        return json_error("此端点需要 Upgrade: websocket", 426);
    }
    if let Some(res) = require_auth(&req, &ctx.env) {
        return Ok(res);
    }

    let Some(account_id) = current_account_id(&ctx.env).await? else {
        return json_error("当前没有已登录的账号", 404);
    };

    let stub = account_stub(&ctx.env, &account_id)?;
    // 直接把原始 Request 交给 DO：升级响应里带着 webSocket 句柄，
    // 必须原样透传，不能用 Response 重新构造。
    let res = stub.fetch_with_request(req).await?;
    Ok(res.into())
}
