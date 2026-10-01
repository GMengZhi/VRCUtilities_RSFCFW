//! pipeline 事件解析。
//!
//! 注意 VRChat 的 pipeline 消息有个坑：大部分 `notification` 类事件的
//! `content` 是**二次编码的 JSON 字符串**，必须先 `serde_json::from_str` 再读；
//! 而 `friend-*` / `user-*` 类事件的 `content` 是真正的对象。这里统一做兼容。

use serde::Serialize;
use serde_json::Value;

use crate::db::{json_str, now_ms};

#[derive(Debug, Clone, Serialize, Default)]
pub struct Event {
    pub r#type: String,
    pub actor_user_id: Option<String>,
    pub actor_display_name: Option<String>,
    pub world_id: Option<String>,
    pub instance_location: Option<String>,
    pub prev_location: Option<String>,
    pub next_location: Option<String>,
    pub raw: String,
}

impl Event {
    /// 位置是否发生了变化（用于决定要不要写 friends.location）。
    pub fn is_location_change(&self) -> bool {
        matches!(
            self.r#type.as_str(),
            "friend-location" | "user-location" | "friend-online" | "friend-offline"
        )
    }
}

/// 解析失败时返回 None（未知 type 不当作错误，只忽略）。
pub fn parse(text: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(text).ok()?;

    // 服务端错误帧：{"err": "...", ...}
    if let Some(err) = v.get("err").and_then(|x| x.as_str()) {
        return Some(Event {
            r#type: "sys-pipeline-error".into(),
            raw: text.to_string(),
            actor_display_name: Some(err.to_string()),
            ..Default::default()
        });
    }

    let ty = v.get("type").and_then(|x| x.as_str())?.to_string();
    let content = decode_content(v.get("content"));
    let mut ev = Event {
        r#type: ty.clone(),
        raw: text.to_string(),
        ..Default::default()
    };

    // content 是数组时（少见），取第一个对象
    let c: &Value = match &content {
        Value::Array(a) => a.first().unwrap_or(&Value::Null),
        other => other,
    };

    ev.actor_user_id = json_str(c, "userId").or_else(|| json_str(c, "userid"));
    ev.actor_display_name = c
        .get("user")
        .and_then(|u| json_str(u, "displayName"))
        .or_else(|| json_str(c, "senderUsername"));

    // 位置信息：不同事件放在不同字段
    let location = json_str(c, "location").or_else(|| json_str(c, "worldId"));
    ev.next_location = location.clone();
    ev.world_id = json_str(c, "worldId").or_else(|| {
        location
            .as_deref()
            .and_then(|l| l.split(':').next().map(String::from))
            .filter(|w| w.starts_with("wrld_"))
    });
    ev.instance_location = location.clone();

    // VRCX 风格的"上一位置"只能由接收方维护，这里留给会话层填充
    ev.prev_location = None;

    Some(ev)
}

/// `notification` 类事件的 content 是字符串化的 JSON；其余是对象。
fn decode_content(content: Option<&Value>) -> Value {
    match content {
        None => Value::Null,
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
        Some(other) => other.clone(),
    }
}

/// 服务自己产生的事件（连接建立/断开），用于在时间轴上标出监控空档。
pub fn system_event(kind: &str, detail: &str) -> Event {
    Event {
        r#type: format!("sys-{kind}"),
        actor_display_name: Some(detail.to_string()),
        raw: serde_json::json!({ "type": format!("sys-{kind}"), "detail": detail, "ts": now_ms() })
            .to_string(),
        ..Default::default()
    }
}

/// 落库前清洗原始 JSON。
///
/// 存在的意义是防呆：raw_json 会被长期保存，绝不能把 authToken 这类
/// 凭据写进 D1。这里递归删掉所有已知的敏感键，而不是只处理顶层。
pub fn sanitize_raw(raw: &str) -> String {
    const SENSITIVE: &[&str] = &[
        "authToken",
        "auth_token",
        "authCookie",
        "password",
        "totp",
        "code",
        "twoFactorAuth",
    ];

    fn scrub(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.retain(|k, _| !SENSITIVE.contains(&k.as_str()));
                for (_, v) in map.iter_mut() {
                    scrub(v);
                }
            }
            Value::Array(items) => {
                for v in items.iter_mut() {
                    scrub(v);
                }
            }
            _ => {}
        }
    }

    match serde_json::from_str::<Value>(raw) {
        Ok(mut v) => {
            scrub(&mut v);
            // 截断超长字段，防止单个事件把 D1 行撑爆
            let s = v.to_string();
            if s.len() > 16_000 {
                format!("{{\"truncated\":true,\"len\":{}}}", s.len())
            } else {
                s
            }
        }
        // 不是合法 JSON 就整体丢弃，只留长度信息
        Err(_) => format!("{{\"unparsable\":true,\"len\":{}}}", raw.len()),
    }
}

/// 从 location 字符串里取世界 ID（形如 `wrld_xxx:12345~region(us)`）。
pub fn world_of(location: &str) -> Option<String> {
    let head = location.split(':').next()?;
    if head.starts_with("wrld_") {
        Some(head.to_string())
    } else {
        None
    }
}

/// 从 location 字符串里取实例 ID（冒号后的部分）。
pub fn instance_of(location: &str) -> Option<String> {
    location.split_once(':').map(|(_, rest)| rest.to_string())
}

/// 从 location 里提取 region（`~region(us)`）。
pub fn region_of(location: &str) -> Option<String> {
    let after = location.split('~').nth(1)?;
    let start = after.find('(')? + 1;
    let end = after.find(')')?;
    if end > start {
        Some(after[start..end].to_string())
    } else {
        None
    }
}

/// 判断是否是可枚举的公开实例（VRCX 里显示的"公开"）。
pub fn is_public_instance(location: &str) -> bool {
    let inst = location.split_once(':').map(|(_, r)| r).unwrap_or("");
    !inst.is_empty()
        && !inst.contains("hidden")
        && !inst.contains("friends")
        && !inst.contains("private")
        && !inst.contains("group")
        && !inst.contains("invite")
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRID: &str = "wrld_5b89c79e-c340-4510-be27-639bcc76c895";
    const LOC: &str = "wrld_5b89c79e-c340-4510-be27-639bcc76c895:12345~region(jp)~nonce(abc)";

    #[test]
    fn parses_friend_location_object_content() {
        // friend-* 事件的 content 是真正的对象（不是二次编码的字符串）
        let raw = format!(
            r#"{{"type":"friend-location","content":{{"userId":"usr_abc","location":"{LOC}","worldId":"{WRID}","user":{{"displayName":"小明"}}}}}}"#
        );
        let ev = parse(&raw).expect("应当解析成功");
        assert_eq!(ev.r#type, "friend-location");
        assert_eq!(ev.actor_user_id.as_deref(), Some("usr_abc"));
        assert_eq!(ev.actor_display_name.as_deref(), Some("小明"));
        assert_eq!(ev.world_id.as_deref(), Some(WRID));
        assert_eq!(ev.next_location.as_deref(), Some(LOC));
    }

    #[test]
    fn parses_double_encoded_notification_content() {
        // notification 的 content 是字符串化的 JSON —— 这是最容易踩的坑
        let raw = r#"{"type":"notification","content":"{\"userId\":\"usr_xyz\",\"senderUsername\":\"阿花\"}"}"#;
        let ev = parse(raw).expect("应当解析成功");
        assert_eq!(ev.r#type, "notification");
        assert_eq!(ev.actor_user_id.as_deref(), Some("usr_xyz"));
        assert_eq!(ev.actor_display_name.as_deref(), Some("阿花"));
    }

    #[test]
    fn parses_error_frame() {
        let raw = r#"{"err":"authToken doesn't correspond with an active session","ip":"1.2.3.4"}"#;
        let ev = parse(raw).expect("错误帧也应当被识别");
        assert_eq!(ev.r#type, "sys-pipeline-error");
    }

    #[test]
    fn ignores_unknown_and_invalid_payloads() {
        assert!(parse(r#"{"type":"something-brand-new","content":{}}"#).is_some());
        assert!(parse("not json at all").is_none());
        assert!(parse(r#"{"no_type":true}"#).is_none());
    }

    #[test]
    fn extracts_location_parts() {
        assert_eq!(world_of(LOC).as_deref(), Some(WRID));
        assert_eq!(instance_of(LOC).as_deref(), Some("12345~region(jp)~nonce(abc)"));
        assert_eq!(region_of(LOC).as_deref(), Some("jp"));

        // 非 wrld_ 前缀不应被当成世界
        assert_eq!(world_of("offline"), None);
        assert_eq!(world_of("private"), None);
        assert_eq!(region_of("offline"), None);
    }

    #[test]
    fn detects_public_instances() {
        assert!(is_public_instance(LOC));
        assert!(!is_public_instance(&format!("{WRID}:12345~hidden(usr_x)")));
        assert!(!is_public_instance(&format!("{WRID}:12345~friends(usr_x)")));
        assert!(!is_public_instance(&format!("{WRID}:12345~group(grp_x)")));
        assert!(!is_public_instance(&format!("{WRID}:12345~private(usr_x)")));
        assert!(!is_public_instance("offline"));
        assert!(!is_public_instance(""));
    }

    #[test]
    fn sanitize_strips_credentials_recursively() {
        let raw = r#"{"type":"x","authToken":"SECRET","nested":{"password":"P","totp":"123456","keep":"ok"},"list":[{"authToken":"S2","keep2":1}]}"#;
        let out = sanitize_raw(raw);
        assert!(!out.contains("SECRET"), "authToken 必须被移除: {out}");
        assert!(!out.contains("S2"), "嵌套数组里的 authToken 也要移除: {out}");
        assert!(!out.contains("\"P\""), "password 必须被移除: {out}");
        assert!(!out.contains("123456"), "totp 必须被移除: {out}");
        assert!(out.contains("ok"), "无关字段必须保留: {out}");
        assert!(out.contains("keep2"), "无关字段必须保留: {out}");
    }

    #[test]
    fn sanitize_handles_garbage_and_oversize() {
        assert!(sanitize_raw("<<<not json>>>").contains("unparsable"));

        let big = "x".repeat(20_000);
        let raw = format!(r#"{{"type":"x","blob":"{big}"}}"#);
        let out = sanitize_raw(&raw);
        assert!(out.contains("truncated"), "超大事件应被截断: {out}");
        assert!(out.len() < 1_000, "截断后不应保留原始体积");
    }

    #[test]
    fn derive_state_from_location() {
        assert_eq!(crate::store::derive_state(""), "offline");
        assert_eq!(crate::store::derive_state("offline"), "offline");
        assert_eq!(crate::store::derive_state(LOC), "online");
        assert_eq!(crate::store::derive_state("traveling"), "online");
    }
}
