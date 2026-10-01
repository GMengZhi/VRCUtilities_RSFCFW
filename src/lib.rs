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

async fn serve_index(_req: Request, _ctx: RouteContext<()>) -> Result<Response> {
    Response::builder()
        .with_header("Content-Type", "text/html; charset=utf-8")?
        .ok(INDEX_HTML)
}

async fn serve_css(_req: Request, _ctx: RouteContext<()>) -> Result<Response> {
    Response::builder()
        .with_header("Content-Type", "text/css; charset=utf-8")?
        .ok(APP_CSS)
}

async fn serve_js(_req: Request, _ctx: RouteContext<()>) -> Result<Response> {
    Response::builder()
        .with_header("Content-Type", "application/javascript; charset=utf-8")?
        .ok(APP_JS)
}

/// cron：pipeline 断线期间靠它把好友快照补齐。
#[event(scheduled)]
async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    if let Err(e) = run_scheduled(&env).await {
        console_error!("cron 执行失败: {e}");
    }
}

async fn run_scheduled(env: &Env) -> Result<()> {
    let db = env.d1("DB")?;
    let ids: Vec<String> = db
        .prepare("SELECT id FROM accounts")
        .all()
        .await?
        .results::<serde_json::Value>()?
        .into_iter()
        .filter_map(|v| v.get("id").and_then(|x| x.as_str()).map(String::from))
        .collect();

    for id in ids {
        if let Ok(stub) = account_stub(env, &id) {
            // 顺带触发一次 alarm 重排，等价于给 DO 续期
            let _ = stub
                .fetch_with_str(&format!("https://do/poll-friends"))
                .await;
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

    let Some(account_id) = current_account_id(&ctx.env).await? else {
        return Response::from_json(&serde_json::json!({
            "account_id": null,
            "pipeline_state": "offline",
            "display_name": null,
            "status": null,
            "location": null,
            "last_event_at": null,
            "reconnect_count": 0,
            "last_error": null,
            "hint": "尚未登录，请 POST /api/login 提交 VRChat 账号",
        }));
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
    let snapshot = account_stub(&ctx.env, &account_id)
        .ok()
        .map(|stub| async move { stub.fetch_with_str("https://do/status").await });

    let mut payload = row.unwrap_or(serde_json::json!({ "id": account_id }));
    if let Some(fut) = snapshot {
        if let Ok(mut res) = fut.await {
            if let Ok(text) = res.text().await {
                if let Ok(snap) = serde_json::from_str::<serde_json::Value>(&text) {
                    payload["last_event_at"] =
                        snap.get("last_event_at").cloned().unwrap_or(serde_json::Value::Null);
                    payload["last_error"] =
                        snap.get("last_error").cloned().unwrap_or(serde_json::Value::Null);
                    if let Some(rc) = snap.get("reconnect_count") {
                        payload["reconnect_count"] = rc.clone();
                    }
                }
            }
        }
    }

    Response::from_json(&payload)
}

/// 2FA 是否处于待验证状态。前端据此决定要不要弹验证码输入框。
async fn pending_login_info(env: &Env) -> Result<Option<serde_json::Value>> {
    let Some(username) = latest_login_name(env).await? else {
        return Ok(None);
    };
    let do_name = normalize_account_name(&username);
    let Ok(stub) = account_stub(env, &do_name) else {
        return Ok(None);
    };
    let Ok(mut res) = stub.fetch_with_str("https://do/status").await else {
        return Ok(None);
    };
    let Ok(text) = res.text().await else {
        return Ok(None);
    };
    let Ok(snap) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(None);
    };

    // has_cookie=false 且存在 pending 才算"等待验证码"
    let awaiting = snap
        .get("has_cookie")
        .and_then(|v| v.as_bool())
        .map(|has| !has)
        .unwrap_or(false);

    Ok(Some(serde_json::json!({
        "awaiting_2fa": awaiting,
        "username": username,
    })))
}

async fn api_pending(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    if let Some(res) = require_auth(&req, &ctx.env) {
        return Ok(res);
    }
    match pending_login_info(&ctx.env).await? {
        Some(v) => Response::from_json(&v),
        None => Response::from_json(&serde_json::json!({ "awaiting_2fa": false })),
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
