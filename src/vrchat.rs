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

    /// 带用户名密码的登录尝试。
    pub async fn login(&mut self, username: &str, password: &str) -> Result<LoginOutcome> {
        let basic = base64(&format!("{username}:{password}"));
        let mut res = self
            .request(Method::Get, "auth/user", Some(format!("Basic {basic}")), None)
            .await?;

        let cookie = extract_auth_cookie(res.headers())?;
        let body: serde_json::Value = parse_or_err(&mut res).await?;

        let requires: Vec<String> = body
            .get("requiresTwoFactorAuth")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();

        self.cookie = Some(cookie.clone());

        if requires.is_empty() {
            let user: CurrentUser = serde_json::from_value(body)
                .map_err(|e| worker::Error::RustError(format!("解析 /auth/user 失败: {e}")))?;
            Ok(LoginOutcome::Done { cookie, user })
        } else {
            Ok(LoginOutcome::Need2fa { cookie, methods: requires })
        }
    }

    /// 校验 TOTP 动态码。成功后服务端会下发新的 auth cookie。
    pub async fn verify_totp(&mut self, code: &str) -> Result<String> {
        let body = to_json(&serde_json::json!({ "code": code }));
        let res = self
            .request(Method::Post, "auth/twofactorauth/totp/verify", None, Some(body))
            .await?;
        let cookie = extract_auth_cookie(res.headers())?;
        self.cookie = Some(cookie.clone());
        Ok(cookie)
    }

    /// 校验邮箱验证码（少数账号走这条路径）。
    pub async fn verify_email_otp(&mut self, code: &str) -> Result<String> {
        let body = to_json(&serde_json::json!({ "code": code }));
        let res = self
            .request(
                Method::Post,
                "auth/twofactorauth/emailotp/verify",
                None,
                Some(body),
            )
            .await?;
        let cookie = extract_auth_cookie(res.headers())?;
        self.cookie = Some(cookie.clone());
        Ok(cookie)
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
    extract_auth_cookie_inner(headers).ok_or_else(|| {
        worker::Error::RustError("响应里没有找到 auth cookie（凭证错误、验证码错误或触发风控）".into())
    })
}

fn extract_auth_cookie_inner(headers: &Headers) -> Option<String> {
    let all = headers.get_all("set-cookie").ok()?;
    for raw in all {
        for part in raw.split(';') {
            let part = part.trim();
            if let Some(value) = part.strip_prefix("auth=") {
                if !value.is_empty() && !value.starts_with("deleted") {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

async fn parse_or_err<T: serde::de::DeserializeOwned>(res: &mut Response) -> Result<T> {
    let status = res.status_code();
    let text = res.text().await?;
    if !(200..300).contains(&status) {
        return Err(worker::Error::RustError(format!(
            "VRChat API {status}: {}",
            truncate(&text, 300)
        )));
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
    use super::base64;

    #[test]
    fn base64_basic_auth() {
        // 标准向量的 RFC 4648 测试值
        assert_eq!(base64(""), "");
        assert_eq!(base64("f"), "Zg==");
        assert_eq!(base64("fo"), "Zm8=");
        assert_eq!(base64("foo"), "Zm9v");
        assert_eq!(base64("foob"), "Zm9vYg==");
        assert_eq!(base64("user@example.com:pass"), "dXNlckBleGFtcGxlLmNvbTpwYXNz");
    }
}
