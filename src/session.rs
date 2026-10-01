//! 每个 VRChat 账号一个 Durable Object：持有 pipeline 长连接、保活、重连。
//!
//! 三个设计约束，都来自平台而不是偏好：
//!
//! 1. **cookie 必须待在 DO 里。**
//!    auth cookie 与登录时的出口 IP 绑定。Worker 的出口 IP 会漂移，DO 是
//!    单点可寻址的，所以只有它能稳定持有这个会话。
//!
//! 2. **必须用 alarm 续期。**
//!    出站 WebSocket 不支持 hibernation；没有 `accept_web_socket` 的 DO 在
//!    70~140 秒无事件后被驱逐，且单个出站操作最多只阻止驱逐 15 分钟。
//!    alarm 算 incoming event，周期触发即可一直续期。
//!
//! 3. **不能用 `ctx.wait_until` 起后台任务。**
//!    `State`、`Storage` 都不实现 `Clone`，而 `wait_until` 要求 `'static`
//!    future。所以后台循环只捕获 owned 的 `Env`（可克隆）和一个 `JsValue`
//!    形式的状态句柄，WebSocket 列表按需通过反射取。

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use wasm_bindgen::{JsCast, JsValue};
use worker::*;

use crate::db::{now_ms, params, to_json};
use crate::event::{self as evmod, Event};
use crate::store;
use crate::vrchat::{LoginFailure, LoginOutcome, VerifyFailure, VrcClient};

/// alarm 周期：2 分钟。足够在 15 分钟窗口内多次续期，也不至于太费额度。
const ALARM_PERIOD_MS: i64 = 120_000;
/// 超过这个时间没收到任何 pipeline 消息，就认为连接已死，强制重连。
const STALE_AFTER_MS: i64 = 300_000;

const K_COOKIE: &str = "auth_cookie";
const K_PENDING: &str = "pending_cookie";
/// 登录时 VRChat 声明的 2FA 方式（`totp` / `emailOtp`），决定验证走哪个端点。
const K_2FA_METHODS: &str = "twofa_methods";
/// 被限流后的冷却截止时间（毫秒）。存在这里，刷新页面也能看到剩余等待时间。
const K_COOLDOWN_UNTIL: &str = "cooldown_until";
const K_USER_ID: &str = "user_id";
const K_LAST_EVENT: &str = "last_event_at";
const K_LAST_ERROR: &str = "last_error";
const K_RECONNECTS: &str = "reconnect_count";

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct SessionSnapshot {
    pub user_id: Option<String>,
    pub has_cookie: bool,
    pub last_event_at: Option<i64>,
    pub last_error: Option<String>,
    pub reconnect_count: i64,
    /// 存在"待验证的 2FA 会话"时为 true。
    pub awaiting_2fa: bool,
    /// 被限流时的冷却截止时间（毫秒时间戳）。刷新页面也能据此显示倒计时。
    pub cooldown_until: Option<i64>,
}

impl SessionSnapshot {
    /// 冷却还剩多少秒（0 表示可以提交）。
    pub fn cooldown_remaining_secs(&self) -> i64 {
        match self.cooldown_until {
            Some(until) => ((until - now_ms()) / 1000).max(0),
            None => 0,
        }
    }
}

#[durable_object]
pub struct AccountSession {
    state: State,
    env: Env,
}

impl DurableObject for AccountSession {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let path = req.url()?.path().to_string();

        match path.as_str() {
            "/login" => {
                #[derive(Deserialize)]
                struct Body {
                    username: String,
                    password: String,
                }
                let body: Body = req.json().await?;
                match self.do_login(&body.username, &body.password).await {
                    Ok(res) => Ok(res),
                    Err(e) => Ok(self.json_error(&e.to_string(), "login")),
                }
            }
            "/verify" => {
                #[derive(Deserialize)]
                struct Body {
                    code: String,
                    #[serde(default)]
                    method: Option<String>,
                }
                let body: Body = req.json().await?;
                match self.do_verify(&body.code, body.method.as_deref()).await {
                    Ok(res) => Ok(res),
                    Err(e) => Ok(self.json_error(&e.to_string(), "verify")),
                }
            }
            "/logout" => {
                let storage = self.state.storage();
                let _ = storage.delete(K_COOKIE).await;
                let _ = storage.delete(K_PENDING).await;
                let _ = storage.delete_alarm().await;
                Response::from_json(&serde_json::json!({ "ok": true }))
            }
            "/status" => {
                let snap = self.snapshot().await?;
                Response::from_json(&snap)
            }
            "/poll-friends" => {
                let n = self.refresh_friends().await?;
                Response::from_json(&serde_json::json!({ "ok": true, "count": n }))
            }
            "/start" => {
                self.spawn_pipeline();
                Response::from_json(&serde_json::json!({ "ok": true }))
            }
            "/ws" => self.handle_ws().await,
            other => Response::error(format!("unknown DO route: {other}"), 404),
        }
    }

    /// alarm 承担两件事：给 DO 续期（避免 15 分钟窗口到期被驱逐），
    /// 以及检测 pipeline 是否僵死并重连。
    ///
    /// 关键：**没有已认证的会话就不要再排下一次 alarm**。
    /// 否则一旦走到过 2FA 那一步（即使最后放弃或失败），alarm 链会自己永续，
    /// 每 2 分钟白调一次 VRChat API，永不停止。
    /// 待验证状态（pending cookie）存在 storage 里，不会因为停止 alarm 而丢。
    async fn alarm(&self) -> Result<Response> {
        let storage = self.state.storage();

        let has_session = storage.get::<String>(K_COOKIE).await?.is_some();
        if !has_session {
            console_log!("没有已认证会话，停止 alarm 链（避免空跑 API）");
            let _ = storage.delete_alarm().await;
            return Response::empty();
        }

        let last = storage.get::<i64>(K_LAST_EVENT).await?;
        let stale = match last {
            Some(t) => now_ms() - t > STALE_AFTER_MS,
            None => true,
        };

        if stale {
            console_log!("pipeline 疑似僵死，重建连接");
            self.spawn_pipeline();
        }

        // 好友全量兜底：覆盖"连接建立之前就发生的变化"
        let _ = self.refresh_friends().await;

        self.schedule_alarm().await?;
        Response::empty()
    }
}

impl AccountSession {
    // ---------------------------------------------------------------- 登录

    async fn do_login(&self, username: &str, password: &str) -> Result<Response> {
        let ua = self.user_agent()?;
        let mut client = VrcClient::new(ua);

        let outcome = match client.login(username, password).await {
            Ok(o) => o,
            Err(LoginFailure::RateLimited { retry_after_secs, message }) => {
                self.set_error(&message).await?;
                self.state
                    .storage()
                    .put(K_COOLDOWN_UNTIL, now_ms() + retry_after_secs as i64 * 1000)
                    .await?;
                return Response::from_json(&serde_json::json!({
                    "error": message,
                    "stage": "login",
                    "rate_limited": true,
                    "retry_after_secs": retry_after_secs,
                }))
                .map(|r| r.with_status(429));
            }
            Err(LoginFailure::Other(msg)) => {
                self.set_error(&msg).await?;
                // 返回 400 + JSON 而不是 500：错误文本要能原样显示到网页上，
                // 500 会丢掉可读信息，用户只能看到"Uncaught Error"。
                return Response::from_json(&serde_json::json!({
                    "error": msg,
                    "stage": "login",
                }))
                .map(|r| r.with_status(400));
            }
        };

        match outcome {
            LoginOutcome::Need2fa { cookie, methods } => {
                // 2FA 未完成：暂存 cookie，并**记住登录时声明的验证方式**。
                // VRChat 用 `emailOtp` 和 `totp` 区分两个不同的端点，
                // 一律走 TOTP 会让邮箱验证码账号必然失败。
                self.state.storage().put(K_PENDING, &cookie).await?;
                self.state.storage().put(K_2FA_METHODS, &methods).await?;
                // 新的一次登录尝试：清掉上一次遗留的限流冷却
                let _ = self.state.storage().delete(K_COOLDOWN_UNTIL).await;
                Response::from_json(&serde_json::json!({
                    "ok": false,
                    "need2fa": true,
                    "methods": methods,
                }))
            }
            LoginOutcome::Done { cookie, user } => {
                let db = self.env.d1("DB")?;
                self.state.storage().put(K_COOKIE, &cookie).await?;
                self.state.storage().put(K_USER_ID, &user.user.id).await?;
                let _ = self.state.storage().delete(K_PENDING).await;
                self.clear_error().await?;

                store::upsert_account(&db, &user).await?;
                let friends = client.friends(&user.user.id).await?;
                // 首次登录强制全量写入
                store::replace_friends(&db, &friends, true).await?;

                self.spawn_pipeline();
                self.schedule_alarm().await?;
                Response::from_json(&serde_json::json!({
                    "ok": true,
                    "need2fa": false,
                    "user_id": user.user.id,
                    "display_name": user.user.display_name,
                    "friends": friends.len(),
                }))
            }
        }
    }

    async fn do_verify(&self, code: &str, method: Option<&str>) -> Result<Response> {
        let storage = self.state.storage();
        let pending = storage
            .get::<String>(K_PENDING)
            .await?
            .ok_or_else(|| Error::RustError("没有待验证的登录，请重新提交账号密码".into()))?;

        // 决定走哪个端点：显式指定 > 登录时 VRChat 声明的方式 > TOTP
        let declared: Vec<String> = storage.get(K_2FA_METHODS).await?.unwrap_or_default();
        let use_email = match method {
            Some("email") => true,
            Some("totp") => false,
            _ => declared.iter().any(|m| m.eq_ignore_ascii_case("emailOtp")),
        };

        let ua = self.user_agent()?;
        let mut client = VrcClient::with_cookie(ua, pending);

        let cookie = match if use_email {
            client.verify_email_otp(code).await
        } else {
            client.verify_totp(code).await
        } {
            Ok(c) => c,
            // 验证码错了：待验证会话仍然有效，允许直接重试
            Err(VerifyFailure::WrongCode(msg)) => {
                return Ok(Response::from_json(&serde_json::json!({
                    "error": msg,
                    "stage": "verify",
                    "retryable": true,
                }))
                .map(|r| r.with_status(400))?);
            }
            // 被限流：必须让前端停手，否则每次点击都会续期限流窗口
            Err(VerifyFailure::RateLimited { retry_after_secs, message }) => {
                let _ = storage.put(K_COOLDOWN_UNTIL, now_ms() + retry_after_secs as i64 * 1000).await;
                return Ok(Response::from_json(&serde_json::json!({
                    "error": message,
                    "stage": "verify",
                    "rate_limited": true,
                    "retry_after_secs": retry_after_secs,
                }))
                .map(|r| r.with_status(429))?);
            }
            Err(VerifyFailure::SessionExpired(msg)) => {
                let _ = storage.delete(K_PENDING).await;
                let _ = storage.delete(K_2FA_METHODS).await;
                return Ok(Response::from_json(&serde_json::json!({
                    "error": msg,
                    "stage": "verify",
                    "needs_relogin": true,
                }))
                .map(|r| r.with_status(400))?);
            }
            Err(VerifyFailure::Other(msg)) => {
                return Ok(self.json_error(&msg, "verify"));
            }
        };

        let user = client.get_current_user().await?;
        let db = self.env.d1("DB")?;

        storage.put(K_COOKIE, &cookie).await?;
        storage.put(K_USER_ID, &user.user.id).await?;
        let _ = storage.delete(K_PENDING).await;
        let _ = storage.delete(K_2FA_METHODS).await;
        let _ = storage.delete(K_COOLDOWN_UNTIL).await;
        self.clear_error().await?;

        store::upsert_account(&db, &user).await?;
        let friends = client.friends(&user.user.id).await?;
        // 首次登录强制全量写入
        store::replace_friends(&db, &friends, true).await?;

        self.spawn_pipeline();
        self.schedule_alarm().await?;
        Response::from_json(&serde_json::json!({
            "ok": true,
            "user_id": user.user.id,
            "display_name": user.user.display_name,
            "friends": friends.len(),
        }))
    }

    // ------------------------------------------------------------ pipeline

    /// 起一个后台 pipeline 循环。没有已认证会话时直接返回，不发任何请求。
    ///
    /// 只捕获 owned 值（`Env` 可克隆，状态句柄用 `JsValue` 表示），
    /// 这样 future 是 `'static` 的，不需要 `self` 的生命周期参与。
    fn spawn_pipeline(&self) {
        let env = self.env.clone();
        // 用 JsValue 承载状态句柄：`DurableObjectState` 本身未必实现了
        // wasm-bindgen 的 Clone，但任何 JS 句柄都能转成 JsValue 后克隆。
        let raw_state: JsValue = JsValue::from(self.state.as_raw());

        wasm_bindgen_futures::spawn_local(async move {
            // run_pipeline 自己会再查一次 cookie；这里多一层保险，
            // 确保未登录时连一次 VRChat 请求都不发。
            if read_credentials(&raw_state).await.is_none() {
                return;
            }
            if let Err(e) = run_pipeline(env, raw_state).await {
                console_error!("pipeline 退出: {e}");
            }
        });
    }

    /// 重排 alarm。
    ///
    /// 必须用 `allow_concurrency: true`：默认语义下，若在 alarm 处理器执行期间
    /// 把下一次时间设到"当前或更早"，Cloudflare 会把它当成取消正在执行的处理器，
    /// alarm 链就此断掉 —— 而 alarm 链一断，DO 会因出站 WebSocket 不支持
    /// hibernation 而被驱逐，长连接随之丢失。
    async fn schedule_alarm(&self) -> Result<()> {
        let next = js_sys::Date::new(&JsValue::from_f64((now_ms() + ALARM_PERIOD_MS) as f64));
        self.state
            .storage()
            .set_alarm_with_options(
                ScheduledTime::new(next),
                SetAlarmOptions {
                    allow_concurrency: Some(true),
                    allow_unconfirmed: None,
                },
            )
            .await
    }

    // ---------------------------------------------------------------- 状态

    async fn snapshot(&self) -> Result<SessionSnapshot> {
        let storage = self.state.storage();
        let pending = storage.get::<String>(K_PENDING).await?;
        Ok(SessionSnapshot {
            user_id: storage.get(K_USER_ID).await?,
            has_cookie: storage.get::<String>(K_COOKIE).await?.is_some(),
            last_event_at: storage.get(K_LAST_EVENT).await?,
            last_error: storage.get(K_LAST_ERROR).await?,
            reconnect_count: storage.get(K_RECONNECTS).await?.unwrap_or(0),
            awaiting_2fa: pending.is_some(),
            cooldown_until: storage.get(K_COOLDOWN_UNTIL).await?.filter(|until| {
                // 过期的冷却记录顺手清掉，避免前端永远看到倒计时
                *until > now_ms()
            }),
        })
    }

    async fn set_error(&self, msg: &str) -> Result<()> {
        let storage = self.state.storage();
        storage.put(K_LAST_ERROR, msg).await?;
        if let (Ok(db), Ok(Some(uid))) = (self.env.d1("DB"), storage.get::<String>(K_USER_ID).await) {
            store::update_session(&db, &uid, "error", Some(msg), false).await?;
        }
        Ok(())
    }

    async fn clear_error(&self) -> Result<()> {
        let _ = self.state.storage().delete(K_LAST_ERROR).await;
        Ok(())
    }

    /// 把错误转成 400 + JSON，保证错误文本能完整传到网页上。
    fn json_error(&self, message: &str, stage: &str) -> Response {
        Response::from_json(&serde_json::json!({
            "error": message,
            "stage": stage,
        }))
        .map(|r| r.with_status(400))
        .unwrap_or_else(|_| {
            Response::error("内部错误", 500).unwrap_or_else(|_| unreachable!())
        })
    }

    fn user_agent(&self) -> Result<String> {
        Ok(self
            .env
            .var("VRCHAT_USER_AGENT")
            .map(|v| v.to_string())
            .unwrap_or_else(|_| "VRCUtilities/0.1 (personal use)".to_string()))
    }

    /// 全量刷新好友列表。
    ///
    /// 只在存在**已认证** cookie 时才发请求。pending cookie 不算 ——
    /// 那只是 2FA 的中间态，拿它去调 API 只会白白浪费额度、加重限流。
    async fn refresh_friends(&self) -> Result<usize> {
        let storage = self.state.storage();
        let (Some(cookie), Some(uid)) = (
            storage.get::<String>(K_COOKIE).await?,
            storage.get::<String>(K_USER_ID).await?,
        ) else {
            return Ok(0);
        };

        let client = VrcClient::with_cookie(self.user_agent()?, cookie);
        let friends = client.friends(&uid).await?;
        let db = self.env.d1("DB")?;
        // force=false：稳态下只写差异行，这是控制 D1 写入量的关键
        let changed = store::replace_friends(&db, &friends, false).await?;
        Ok(changed)
    }

    /// 升级浏览器 WebSocket，交给 hibernation API 托管。
    async fn handle_ws(&self) -> Result<Response> {
        let pair = WebSocketPair::new()?;
        self.state.accept_web_socket(&pair.server);
        let _ = pair
            .server
            .send_with_str(r#"{"type":"sys-hello","actor_display_name":"已连接实时推送"}"#);
        Response::from_websocket(pair.client)
    }
}

// ---------------------------------------------------------------------------
// pipeline 循环
// ---------------------------------------------------------------------------

async fn run_pipeline(env: Env, raw_state: JsValue) -> Result<()> {
    let db = env.d1("DB")?;

    // cookie / uid 都存在 DO 存储里，这里通过临时问一次 DO 自己拿不到，
    // 所以改为从环境侧不可得 —— 见下方说明：由调用方预置在 storage 中，
    // 此处通过反射读取不便，故直接复用 env 注入的账号键。
    let (cookie, uid) = match read_credentials(&raw_state).await {
        Some(v) => v,
        None => return Ok(()),
    };

    store::update_session(&db, &uid, "connected", None, false).await?;

    let url = Url::parse(&VrcClient::pipeline_url(&cookie))?;
    let ws = WebSocket::connect(url).await?;
    let mut stream = ws.events()?;

    let start = evmod::system_event("pipeline-connected", "pipeline 已连接");
    write_event(&db, &uid, &start, None).await;

    let mut last_touch = 0i64;

    while let Some(item) = stream.next().await {
        // Close 帧说明服务端主动断开（多半是 authToken 失效或 IP 不匹配），
        // 退出循环，交给 alarm 做退避重连。
        let text = match item {
            Ok(WebsocketEvent::Message(m)) => match m.text() {
                Some(t) => t,
                None => continue,
            },
            Ok(WebsocketEvent::Close(close)) => {
                console_log!("pipeline 关闭: code={} reason={}", close.code(), close.reason());
                break;
            }
            Err(e) => {
                console_error!("pipeline 读取错误: {e}");
                break;
            }
        };

        let Some(mut parsed) = evmod::parse(&text) else {
            continue;
        };

        if parsed.r#type == "sys-pipeline-error" {
            console_error!("pipeline 报错: {}", parsed.actor_display_name.clone().unwrap_or_default());
            break;
        }

        let now = now_ms();
        let _ = write_last_event(&raw_state, now).await;

        if now - last_touch > 1000 {
            last_touch = now;
            let _ = store::touch_session(&db, &uid).await;
        }

        // 补上"上一位置"，让看板能显示 a → b
        if let Some(actor) = &parsed.actor_user_id {
            parsed.prev_location = prev_location(&db, actor).await;
        }

        let world_name = resolve_world_name(&db, &parsed).await;
        write_event(&db, &uid, &parsed, world_name.as_deref()).await;

        broadcast(&raw_state, &parsed);
    }

    let end = evmod::system_event("pipeline-disconnected", "pipeline 断开，等待重连");
    write_event(&db, &uid, &end, None).await;
    store::update_session(&db, &uid, "connecting", Some("pipeline 断线"), true).await?;

    Ok(())
}

/// 落库并吞掉错误，避免单条事件写失败打断整条连接。
async fn write_event(db: &D1Database, uid: &str, ev: &Event, world_name: Option<&str>) {
    if let Err(e) = store::apply_event(db, uid, ev, world_name).await {
        console_error!("事件落库失败 type={}: {e}", ev.r#type);
    }
}

/// 世界名：只查 `worlds` 表缓存，**不**在事件热路径里打 API。
///
/// 原因：每个未知世界都发一次请求会随事件量线性放大调用次数，
/// 而且事件路径上未必有可用的认证客户端。写不进去就让看板退回显示 world_id，
/// 世界元数据由后续的全量同步补齐。
async fn resolve_world_name(db: &D1Database, ev: &Event) -> Option<String> {
    let wid = ev.world_id.clone()?;
    db.prepare("SELECT name FROM worlds WHERE id=?1 AND name IS NOT NULL")
        .bind(&params(&[&wid]).ok()?)
        .ok()?
        .first::<String>(Some("name"))
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// 从 friends 表读这个好友当前的位置，作为"上一位置"。
async fn prev_location(db: &D1Database, user_id: &str) -> Option<String> {
    db.prepare("SELECT location FROM friends WHERE user_id=?1")
        .bind(&params(&[&user_id.to_string()]).ok()?)
        .ok()?
        .first::<String>(Some("location"))
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// 把事件推给所有通过 WebSocket 订阅的浏览器。
///
/// 这里用反射调 `getWebSockets()`，而不是 `State::get_websockets()`：
/// 后台任务只持有 `JsValue` 形式的状态句柄，拿不到 `&State`。
fn broadcast(raw_state: &JsValue, ev: &Event) {
    let Ok(sockets) = get_websockets(raw_state) else {
        return;
    };
    let Ok(array) = sockets.dyn_into::<js_sys::Array>() else {
        return;
    };
    let payload = to_json(ev);
    for item in array.iter() {
        if let Ok(ws) = item.dyn_into::<web_sys::WebSocket>() {
            let _ = ws.send_with_str(&payload);
        }
    }
}

/// `state.getWebSockets()` —— 动态调用，避免为它引入额外的类型绑定。
fn get_websockets(raw_state: &JsValue) -> Result<JsValue> {
    let obj = raw_state
        .dyn_ref::<js_sys::Object>()
        .ok_or_else(|| Error::RustError("状态句柄不是对象".into()))?;
    let func = js_sys::Reflect::get(obj, &JsValue::from_str("getWebSockets"))?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| Error::RustError("getWebSockets 不可用".into()))?;
    Ok(func.call0(obj)?)
}

/// 读 DO 存储里的 cookie 与账号 ID。
///
/// 走的是 `state.storage.get()` 的同一条反射路径，绕开 `Storage` 不可克隆
/// 的限制 —— 后台任务只需要这两个值。
async fn read_credentials(raw_state: &JsValue) -> Option<(String, String)> {
    let storage = storage_of(raw_state).ok()?;
    let cookie = js_get::<String>(&storage, K_COOKIE).await?;
    let uid = js_get::<String>(&storage, K_USER_ID).await?;
    Some((cookie, uid))
}

async fn write_last_event(raw_state: &JsValue, ts: i64) {
    if let Ok(storage) = storage_of(raw_state) {
        let _ = js_put(&storage, K_LAST_EVENT, &ts).await;
    }
}

fn storage_of(raw_state: &JsValue) -> Result<JsValue> {
    let obj = raw_state
        .dyn_ref::<js_sys::Object>()
        .ok_or_else(|| Error::RustError("状态句柄不是对象".into()))?;
    let getter = js_sys::Reflect::get(obj, &JsValue::from_str("storage"))?;
    // `storage` 是 getter，取到的就是 DurableObjectStorage 实例
    if getter.is_undefined() || getter.is_null() {
        return Err(Error::RustError("storage 不可用".into()));
    }
    Ok(getter)
}

async fn js_get<T: serde::de::DeserializeOwned>(storage: &JsValue, key: &str) -> Option<T> {
    let func = js_sys::Reflect::get(storage, &JsValue::from_str("get"))
        .ok()?
        .dyn_into::<js_sys::Function>()
        .ok()?;
    let promise = func.call1(storage, &JsValue::from_str(key)).ok()?;
    let value = wasm_bindgen_futures::JsFuture::from(promise.dyn_into::<js_sys::Promise>().ok()?)
        .await
        .ok()?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    serde_wasm_bindgen::from_value(value).ok()
}

async fn js_put<T: serde::Serialize>(storage: &JsValue, key: &str, value: &T) -> Result<()> {
    let func = js_sys::Reflect::get(storage, &JsValue::from_str("put"))?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| Error::RustError("storage.put 不可用".into()))?;
    let js_value = serde_wasm_bindgen::to_value(value)?;
    let promise = func.call2(storage, &JsValue::from_str(key), &js_value)?;
    wasm_bindgen_futures::JsFuture::from(
        promise
            .dyn_into::<js_sys::Promise>()
            .map_err(|_| Error::RustError("put 未返回 Promise".into()))?,
    )
    .await?;
    Ok(())
}
