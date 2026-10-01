//! VRChat API 客户端（api.vrchat.cloud）。
//!
//! 三条硬性约束，破坏任何一条都会失败：
//!   1. 每个请求都必须带 User-Agent，否则被拒。
//!   2. 认证走 `Authorization: Basic base64(username:password)`，
//!      登录后服务端下发 `auth` cookie，后续请求带 `Cookie: auth=...`。
//!   3. auth cookie 与**登录时的出口 IP** 绑定。pipeline 在 IP 不匹配时会
//!      直接回 `authToken doesn't correspond with an active session` 并断开。

use serde::{Deserialize, Serialize};
use worker::{Headers, Method, Request, RequestInit, Response, Result};

use crate::db::to_json;

pub const API_BASE: &str = "https://api.vrchat.cloud/api/1";
pub const PIPELINE_HOST: &str = "wss://pipeline.vrchat.cloud";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VrcUser {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub status_description: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub current_avatar_thumbnail_image_url: String,
    #[serde(default)]
    pub current_avatar_image_url: String,
    #[serde(default)]
    pub profile_pic_override: String,
    #[serde(default)]
    pub user_icon: String,
}

/// `GET /auth/user` 的返回。字段用 default 兜底，VRChat 会省略部分字段。
#[derive(Debug, Clone, Deserialize)]
pub struct CurrentUser {
    #[serde(flatten)]
    pub user: VrcUser,
    #[serde(default, rename = "requiresTwoFactorAuth")]
    pub requires_two_factor_auth: Vec<String>,
    #[serde(default, rename = "twoFactorAuthEnabled")]
    pub two_factor_auth_enabled: bool,
}

impl CurrentUser {
    pub fn needs_2fa(&self) -> bool {
        !self.requires_two_factor_auth.is_empty()
    }

    /// VRChat 的 tags 里同时含信任等级（如 `system_trust_veteran`）和徽章。
    pub fn trust_rank(&self) -> Option<String> {
        self.user
            .tags
            .iter()
            .find(|t| t.starts_with("system_trust_"))
            .cloned()
    }
}

/// 登录结果。2FA 未完成时只返回 cookie，调用方需继续校验。
pub enum LoginOutcome {
    Done { cookie: String, user: CurrentUser },
    Need2fa { cookie: String, methods: Vec<String> },
}

/// 2FA 校验失败的分类。
///
/// 必须区分"码打错了"和"被限流了"：前者让用户直接重试，后者必须
/// **阻止**用户重试，否则每次点击都会让限流窗口续期，永远出不来。
pub enum VerifyFailure {
    /// 验证码不正确 —— 待验证会话仍然有效，可以直接重试。
    WrongCode(String),
    /// 被限流 —— 必须等够这么久再试。
    RateLimited { retry_after_secs: u64, message: String },
    /// 待验证会话已失效，必须重新提交账号密码。
    SessionExpired(String),
    /// 其他错误。
    Other(String),
}

pub struct VrcClient {
    user_agent: String,
    cookie: Option<String>,
}

impl VrcClient {
    pub fn new(user_agent: impl Into<String>) -> Self {
        Self {
            user_agent: user_agent.into(),
            cookie: None,
        }
    }

    pub fn with_cookie(user_agent: impl Into<String>, cookie: impl Into<String>) -> Self {
        Self {
            user_agent: user_agent.into(),
            cookie: Some(cookie.into()),
        }
    }

    pub fn cookie(&self) -> Option<&str> {
        self.cookie.as_deref()
    }

    /// `GET /auth/user`
    pub async fn get_current_user(&self) -> Result<CurrentUser> {
        let mut res = self.get("auth/user").await?;
        parse_or_err::<CurrentUser>(&mut res).await
    }
}

/// 登录失败的分类。和 2FA 一样，限流必须和凭证错误区分开。
pub enum LoginFailure {
    /// 被限流，必须等够这么久再试。
    RateLimited { retry_after_secs: u64, message: String },
    /// 其他问题（凭证错误、风控、网络）。
    Other(String),
}

impl VrcClient {
    /// 带用户名密码的登录尝试。
    pub async fn login(
        &mut self,
        username: &str,
        password: &str,
    ) -> std::result::Result<LoginOutcome, LoginFailure> {
        let basic = base64(&format!("{username}:{password}"));
        let mut res = self
            .request(Method::Get, "auth/user", Some(format!("Basic {basic}")), None)
            .await
            .map_err(|e| LoginFailure::Other(e.to_string()))?;

        // 先判状态：401 说明账号密码或 2FA 状态有问题，此时的响应体是
        // {"error":{"message":"..."}}，直接把它带出去比"没找到 cookie"有用得多。
        let status = res.status_code();
        let set_cookie = res.headers().get_all("set-cookie").unwrap_or_default();
        let text = read_body(&mut res).await;

        if status == 429 {
            return Err(LoginFailure::RateLimited {
                retry_after_secs: parse_retry_after(
                    res.headers().get("retry-after").ok().flatten().as_deref(),
                    &text,
                ),
                message: api_error("登录", status, &text).to_string(),
            });
        }
        if !(200..300).contains(&status) {
            return Err(LoginFailure::Other(api_error("登录", status, &text).to_string()));
        }

        let cookie = parse_auth_cookie(&set_cookie).ok_or_else(|| {
            LoginFailure::Other(format!(
                "登录响应里没有 auth cookie（HTTP {status}）。Set-Cookie: {}",
                describe_set_cookie(&set_cookie)
            ))
        })?;

        let body: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| LoginFailure::Other(format!("解析 /auth/user 失败: {e}")))?;

        let requires: Vec<String> = body
            .get("requiresTwoFactorAuth")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();

        self.cookie = Some(cookie.clone());

        if requires.is_empty() {
            let user: CurrentUser = serde_json::from_value(body)
                .map_err(|e| LoginFailure::Other(format!("解析 /auth/user 失败: {e}")))?;
            Ok(LoginOutcome::Done { cookie, user })
        } else {
            Ok(LoginOutcome::Need2fa { cookie, methods: requires })
        }
    }

    /// 校验 TOTP 动态码。成功后服务端会下发新的 auth cookie。
    pub async fn verify_totp(
        &mut self,
        code: &str,
    ) -> std::result::Result<String, VerifyFailure> {
        self.verify_2fa("auth/twofactorauth/totp/verify", "TOTP", code)
            .await
    }

    /// 校验邮箱验证码（`requiresTwoFactorAuth` 里含 `emailOtp` 时走这条）。
    pub async fn verify_email_otp(
        &mut self,
        code: &str,
    ) -> std::result::Result<String, VerifyFailure> {
        self.verify_2fa("auth/twofactorauth/emailotp/verify", "邮箱验证码", code)
            .await
    }

    /// 两个 2FA 端点除了路径完全同构，抽出来统一处理错误与 cookie 提取。
    ///
    /// 这里的关键是**不要**在失败时只说"没找到 cookie"：
    ///   · 验证码错误 → HTTP 200 `{"verified": false}`，且不带 auth cookie
    ///   · 被限流     → HTTP 429，且必须让调用方阻止用户继续重试
    ///   · 会话失效   → HTTP 401 `{"error":{"message":"..."}}`
    /// 三种情况要给出三种不同的下一步动作。
    async fn verify_2fa(
        &mut self,
        path: &str,
        label: &str,
        code: &str,
    ) -> std::result::Result<String, VerifyFailure> {
        let body = to_json(&serde_json::json!({ "code": code }));
        let mut res = self
            .request(Method::Post, path, None, Some(body))
            .await
            .map_err(|e| VerifyFailure::Other(e.to_string()))?;

        let status = res.status_code();
        let text = res.text().await.unwrap_or_default();

        if status == 429 {
            let retry = retry_after_secs(res.headers(), &text);
            return Err(VerifyFailure::RateLimited {
                retry_after_secs: retry,
                message: api_error(label, status, &text).to_string(),
            });
        }
        if status == 401 {
            return Err(VerifyFailure::SessionExpired(format!(
                "{}。请重新输入账号密码。",
                api_error(label, status, &text)
            )));
        }
        if !(200..300).contains(&status) {
            return Err(VerifyFailure::Other(api_error(label, status, &text).to_string()));
        }

        // 按 VRChat 的语义字段判断，拿不到再退回 cookie 是否存在
        let ok_by_body = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("verified").and_then(|x| x.as_bool()));
        let set_cookie = res.headers().get_all("set-cookie").unwrap_or_default();
        let cookie = parse_auth_cookie(&set_cookie);

        match (ok_by_body, cookie) {
            (Some(false), _) => Err(VerifyFailure::WrongCode(format!(
                "{label}校验未通过（VRChat 返回 verified=false）。\
                 请确认动态码是当前有效的：TOTP 每 30 秒轮换，邮箱验证码请用最新一封。\
                 响应体: {}",
                truncate(&text, 200)
            ))),
            (_, Some(c)) => {
                self.cookie = Some(c.clone());
                Ok(c)
            }
            (Some(true), None) => Err(VerifyFailure::SessionExpired(format!(
                "{label}已通过，但响应里没有下发 auth cookie —— \
                 该会话可能已在别处登录、或登录出口 IP 发生了变化。\
                 请重新提交账号密码。Set-Cookie: {}",
                describe_set_cookie(&set_cookie)
            ))),
            (None, None) => Err(VerifyFailure::Other(format!(
                "{label}校验结果无法判定：HTTP {status}，既没有 verified 字段也没有 auth cookie。\
                 响应体: {}；Set-Cookie: {}",
                truncate(&text, 200),
                describe_set_cookie(&set_cookie)
            ))),
        }
    }

    /// `GET /users/{id}/friends`
    pub async fn friends(&self, user_id: &str) -> Result<Vec<VrcUser>> {
        let mut res = self
            .get(&format!("users/{user_id}/friends?offline=true"))
            .await?;
        parse_or_err::<Vec<VrcUser>>(&mut res).await
    }

    /// `GET /users/{id}`
    pub async fn user(&self, user_id: &str) -> Result<VrcUser> {
        let mut res = self.get(&format!("users/{user_id}")).await?;
        parse_or_err::<VrcUser>(&mut res).await
    }

    /// `GET /worlds/{id}` —— 只为拿世界名做缓存，失败不致命。
    pub async fn world_name(&self, world_id: &str) -> Result<String> {
        let mut res = self.get(&format!("worlds/{world_id}")).await?;
        let v: serde_json::Value = parse_or_err(&mut res).await?;
        Ok(v.get("name")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// pipeline 长连接的完整 URL。authToken 就是 auth cookie 的值。
    pub fn pipeline_url(cookie: &str) -> String {
        format!("{PIPELINE_HOST}/?authToken={cookie}")
    }

    async fn get(&self, path: &str) -> Result<Response> {
        self.request(Method::Get, path, None, None).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        authorization: Option<String>,
        body: Option<String>,
    ) -> Result<Response> {
        let headers = Headers::new();
        headers.set("User-Agent", &self.user_agent)?;
        headers.set("Accept", "application/json")?;

        if let Some(auth) = authorization {
            headers.set("Authorization", &auth)?;
        } else if let Some(cookie) = &self.cookie {
            headers.set("Cookie", &format!("auth={cookie}"))?;
        }
        if body.is_some() {
            headers.set("Content-Type", "application/json")?;
        }

        let mut init = RequestInit::new();
        init.with_method(method).with_headers(headers);
        if let Some(b) = body {
            init.with_body(Some(b.into()));
        }

        let url = format!("{API_BASE}/{path}");
        let req = Request::new_with_init(&url, &init)?;
        worker::Fetch::Request(req).send().await
    }
}

/// 从响应的 set-cookie 里取出 `auth` 的值。
///
/// 注意：VRChat 一次会下发 `auth` 和 `twoFactorAuth` 两个 cookie，
/// 所以必须遍历全部 set-cookie，不能只取第一个。
pub fn extract_auth_cookie(headers: &Headers) -> Result<String> {
    let raw = headers.get_all("set-cookie").unwrap_or_default();
    parse_auth_cookie(&raw).ok_or_else(|| {
        worker::Error::RustError(format!(
            "响应里没有找到 auth cookie。Set-Cookie 实际内容: {}",
            describe_set_cookie(&raw)
        ))
    })
}

/// 解析逻辑的纯函数版本。
///
/// 之所以要和 `Headers` 解耦：`web_sys::Headers` 只能在 JS 运行时里工作，
/// 在宿主平台跑单元测试会直接 panic。解析是这里真正容易出错的部分，
/// 必须能被测试覆盖。
///
/// 按 cookie **名字**精确匹配（`name=value`），而不是靠字符串前缀 ——
/// 前缀匹配会误伤 `twoFactorAuth=`。`auth=deleted` 表示会话已作废。
///
/// 前提：每个 `Set-Cookie` 头里只有一个 cookie。这是 Fetch 规范保证的语义
/// （`Headers.getSetCookie()` 按头逐个返回），Workers 运行时遵守该语义。
/// 刻意**不**尝试在一个头里切分多个 cookie —— `expires=Wed, 01 Oct 2025`
/// 本身含逗号，任何按逗号切分的启发式都会破坏合法格式。
fn parse_auth_cookie(values: &[String]) -> Option<String> {
    for raw in values {
        for part in raw.split(';') {
            let part = part.trim();
            let Some((name, value)) = part.split_once('=') else {
                continue;
            };
            if name.trim() == "auth" {
                let value = value.trim();
                if !value.is_empty() && value != "deleted" {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// 把 Set-Cookie 渲染成一行，供错误信息使用。
/// 只暴露 cookie 名字和值的长度 —— 不能把会话凭据写进日志或错误响应。
fn describe_set_cookie(values: &[String]) -> String {
    if values.is_empty() {
        return "<没有 Set-Cookie 头>".into();
    }
    const ATTRS: &[&str] = &[
        "path", "expires", "domain", "max-age", "samesite", "secure", "httponly", "priority",
    ];
    let mut names: Vec<String> = Vec::new();
    for raw in values {
        for part in raw.split(';') {
            let part = part.trim();
            let Some((name, value)) = part.split_once('=') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() || ATTRS.contains(&name.to_ascii_lowercase().as_str()) {
                continue;
            }
            names.push(format!("{name}(值长度 {})", value.trim().len()));
        }
    }
    if names.is_empty() {
        "<只有属性、没有 cookie>".into()
    } else {
        names.join(", ")
    }
}

/// 把 VRChat 的错误响应转成一句能指导下一步操作的提示。
///
/// VRChat 的错误体是 `{"error":{"message":"..."}}`，直接带出来比
/// 自己编一句笼统的话有用得多。
fn api_error(stage: &str, status: u16, body: &str) -> worker::Error {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| truncate(body, 300));

    let hint = match status {
        401 => "（账号或密码不正确，或该会话已失效）",
        403 => "（被拒绝：可能是账号未验证邮箱、被限制登录，或触发了风控）",
        429 => "（请求过于频繁）",
        _ => "",
    };

    worker::Error::RustError(format!("{stage}失败：HTTP {status}{hint} — {message}"))
}

/// 遇到限流时该等多久，单位秒。
///
/// 优先信 `Retry-After` 头（HTTP 标准），拿不到再从响应体里的
/// `retryAfter`/`retry_after` 找，最后给一个保守默认值。
/// 这个值会一路传到前端去禁用提交按钮 —— 限流期间反复点提交只会让它续期。
pub fn retry_after_secs(headers: &Headers, body: &str) -> u64 {
    let header = headers.get("retry-after").ok().flatten();
    parse_retry_after(header.as_deref(), body)
}

/// 纯函数版本，便于测试。
fn parse_retry_after(header: Option<&str>, body: &str) -> u64 {
    if let Some(secs) = header.and_then(|v| v.trim().parse::<u64>().ok()) {
        return secs.clamp(1, 3600);
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        for key in ["retryAfter", "retry_after", "retryAfterSeconds"] {
            if let Some(n) = v.get(key).and_then(|x| x.as_u64()) {
                return n.clamp(1, 3600);
            }
        }
    }
    300
}

/// 429 / 5xx 值得自动重试：VRChat 的限流窗口通常只有几十秒，
/// 而 4xx（凭证错、验证码错）重试没有意义，只会加重限流。
pub fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// 读响应体，失败时给一个占位串，用于错误信息。
async fn read_body(res: &mut Response) -> String {
    res.text().await.unwrap_or_else(|_| "<读取响应体失败>".into())
}

async fn parse_or_err<T: serde::de::DeserializeOwned>(res: &mut Response) -> Result<T> {
    let status = res.status_code();
    let text = res.text().await?;
    if !(200..300).contains(&status) {
        return Err(api_error("API 请求", status, &text));
    }
    serde_json::from_str::<T>(&text).map_err(|e| {
        worker::Error::RustError(format!(
            "JSON 解析失败: {e}; body={}",
            truncate(&text, 200)
        ))
    })
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n).collect();
        format!("{head}…")
    }
}

/// 标准 base64（带 padding）。Basic 认证头需要它，这里不引入额外依赖。
pub fn base64(input: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_basic_auth() {
        // RFC 4648 标准向量
        assert_eq!(base64(""), "");
        assert_eq!(base64("f"), "Zg==");
        assert_eq!(base64("fo"), "Zm8=");
        assert_eq!(base64("foo"), "Zm9v");
        assert_eq!(base64("foob"), "Zm9vYg==");
        assert_eq!(base64("user@example.com:pass"), "dXNlckBleGFtcGxlLmNvbTpwYXNz");
    }

    /// 测试用：把字符串字面量转成 set-cookie 列表。
    /// 不构造 `web_sys::Headers`，因为它在宿主平台会 panic。
    fn cookies(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn picks_auth_not_twenty_factor_auth() {
        // 关键用例：`twoFactorAuth=...` 里含有子串 `auth=`，
        // 用字符串前缀匹配会把它错认成 auth cookie。
        assert_eq!(parse_auth_cookie(&cookies(&["twoFactorAuth=TFVALUE; Path=/; HttpOnly"])), None);

        let v = cookies(&["twoFactorAuth=TFVALUE; Path=/", "auth=REALVALUE; Path=/; HttpOnly"]);
        assert_eq!(parse_auth_cookie(&v).as_deref(), Some("REALVALUE"));
    }

    #[test]
    fn handles_order_and_multiple_headers() {
        let v = cookies(&["auth=FIRST; Path=/", "twoFactorAuth=x; Path=/"]);
        assert_eq!(parse_auth_cookie(&v).as_deref(), Some("FIRST"));

        // 顺序颠倒也要能取到 auth
        let v = cookies(&["twoFactorAuth=x; Path=/", "auth=SECOND; Path=/"]);
        assert_eq!(parse_auth_cookie(&v).as_deref(), Some("SECOND"));

        // 带 Expires 的合法 cookie：逗号不能被当成分隔符
        let v = cookies(&["auth=THIRD; Expires=Wed, 01 Oct 2025 07:28:00 GMT; Path=/"]);
        assert_eq!(parse_auth_cookie(&v).as_deref(), Some("THIRD"));
    }

    #[test]
    fn treats_deleted_and_empty_as_missing() {
        assert_eq!(parse_auth_cookie(&cookies(&["auth=deleted; Path=/"])), None);
        assert_eq!(parse_auth_cookie(&cookies(&["auth=; Path=/"])), None);
        assert_eq!(parse_auth_cookie(&cookies(&["auth=   ; Path=/"])), None);
        assert_eq!(parse_auth_cookie(&cookies(&[])), None);
    }

    #[test]
    fn does_not_match_suffix_lookalikes() {
        // 记录一下真实行为：因为切分的是**第一个** `=`，
        // `someauth=...` 的名字是 `someauth`，不会被误判成 auth。
        assert_eq!(parse_auth_cookie(&cookies(&["xauth=LOOKALIKE; Path=/"])), None);
        assert_eq!(parse_auth_cookie(&cookies(&["auth_token=LOOKALIKE; Path=/"])), None);
        // 但 `twoFactorAuth=` 这种情况本来就切不出 "auth" 作为名字，
        // 真正危险的是有人改用 `strip_prefix("auth=")` 那种写法。
        assert_eq!(parse_auth_cookie(&cookies(&["twoFactorAuth=x; Path=/"])), None);
    }

    #[test]
    fn describe_set_cookie_never_leaks_values() {
        let d = describe_set_cookie(&cookies(&["auth=SUPERSECRETTOKENVALUE; Path=/; HttpOnly"]));
        assert!(!d.contains("SUPERSECRETTOKENVALUE"), "不能泄露 cookie 值: {d}");
        assert!(d.contains("auth"), "应当报出 cookie 名字: {d}");
        assert!(d.contains("长度"), "应当报出值长度: {d}");
        assert!(!d.contains("Path"), "属性不应出现在摘要里: {d}");
        assert!(!d.contains("HttpOnly"), "属性不应出现在摘要里: {d}");
    }

    #[test]
    fn api_error_surfaces_vrchat_message() {
        let body = r#"{"error":{"message":"Invalid Username/Password","status_code":401}}"#;
        let msg = api_error("登录", 401, body).to_string();
        assert!(msg.contains("Invalid Username/Password"), "应带出 VRChat 原文: {msg}");
        assert!(msg.contains("401"), "应带出状态码: {msg}");
        assert!(msg.contains("账号或密码"), "401 应给出针对性提示: {msg}");
    }

    #[test]
    fn api_error_gives_status_specific_hints() {
        assert!(api_error("x", 429, "{}").to_string().contains("过于频繁"));
        assert!(api_error("x", 403, "{}").to_string().contains("风控"));
    }

    #[test]
    fn retry_after_prefers_header_then_body_then_default() {
        // Retry-After 头优先
        assert_eq!(parse_retry_after(Some("42"), "{}"), 42);
        assert_eq!(parse_retry_after(Some(" 42 "), "{}"), 42);

        // 头拿不到就找响应体里的字段
        assert_eq!(parse_retry_after(None, r#"{"retryAfter":30}"#), 30);
        assert_eq!(parse_retry_after(None, r#"{"retry_after":15}"#), 15);

        // 都没有就退回保守默认值
        assert_eq!(parse_retry_after(None, "{}"), 300);
        assert_eq!(parse_retry_after(None, "not json"), 300);

        // 非数字的 Retry-After（HTTP 允许日期格式）退回默认值
        assert_eq!(parse_retry_after(Some("Wed, 01 Oct 2025 07:28:00 GMT"), "{}"), 300);

        // 越界值被夹紧，避免前端显示荒谬的倒计时
        assert_eq!(parse_retry_after(Some("999999"), "{}"), 3600);
        assert_eq!(parse_retry_after(Some("0"), "{}"), 1);
    }

    #[test]
    fn only_429_and_5xx_are_retryable() {
        // 限流和服务端故障值得重试
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(502));
        assert!(is_retryable_status(503));

        // 4xx 重试没有意义，而且在 429 场景下会加重限流
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(403));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(200));
    }

    #[test]
    fn api_error_falls_back_to_raw_body() {
        // 不是 JSON 时也要把原文带出来，而不是只说"失败了"
        let msg = api_error("登录", 500, "<html>gateway boom</html>").to_string();
        assert!(msg.contains("gateway boom"), "应回退到原始响应体: {msg}");
    }
}
