//! D1 落库逻辑。
//!
//! 所有写入都是幂等的 upsert，接收方可以放心重放历史事件。
//! 时间列统一毫秒整数。

use serde_json::Value;
use worker::{D1Database, Result};

use crate::db::{json_str, now_ms, params, to_json};
use crate::event::{self, Event};
use crate::vrchat::{CurrentUser, VrcUser};

/// 记录账号自身信息。
pub async fn upsert_account(db: &D1Database, user: &CurrentUser) -> Result<()> {
    let now = now_ms();
    let u = &user.user;
    let trust = user.trust_rank();
    let world = event::world_of(&u.location);
    let raw = to_json(u);

    db.prepare(
        "INSERT INTO accounts (id, vrchat_user_id, display_name, username, status, \
         status_description, location, world_id, platform, trust_rank, raw_json, created_at, updated_at) \
         VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11) \
         ON CONFLICT(id) DO UPDATE SET display_name=excluded.display_name, username=excluded.username, \
         status=excluded.status, status_description=excluded.status_description, location=excluded.location, \
         world_id=excluded.world_id, platform=excluded.platform, trust_rank=excluded.trust_rank, \
         raw_json=excluded.raw_json, updated_at=excluded.updated_at",
    )
    .bind(&params(&[
        &u.id,
        &u.display_name,
        &u.username,
        &u.status,
        &u.status_description,
        &u.location,
        &world,
        &u.platform,
        &trust,
        &raw,
        &now,
    ])?)?
    .run()
    .await?;

    update_session(db, &u.id, "connecting", None, false).await?;
    Ok(())
}

/// 更新会话健康度（看板顶部状态卡片读它）。
pub async fn update_session(
    db: &D1Database,
    account_id: &str,
    state: &str,
    last_error: Option<&str>,
    bump_reconnect: bool,
) -> Result<()> {
    let now = now_ms();
    let connected = state == "connected";
    let err_at = last_error.map(|_| now);

    // ?1 account_id · ?2 state · ?3 now · ?4 last_error · ?5 last_error_at
    // ?6 bump · ?7 last_event_at(仅 connected 时推进)
    db.prepare(
        "INSERT INTO sessions (account_id, pipeline_state, last_connect_at, last_event_at, \
         last_error, last_error_at, reconnect_count, updated_at) \
         VALUES (?1, ?2, ?3, ?7, ?4, ?5, ?6, ?3) \
         ON CONFLICT(account_id) DO UPDATE SET pipeline_state=excluded.pipeline_state, \
         last_connect_at=CASE WHEN excluded.pipeline_state='connected' \
                              THEN excluded.last_connect_at ELSE sessions.last_connect_at END, \
         last_event_at=COALESCE(excluded.last_event_at, sessions.last_event_at), \
         last_error=excluded.last_error, \
         last_error_at=COALESCE(excluded.last_error_at, sessions.last_error_at), \
         reconnect_count=sessions.reconnect_count + ?6, updated_at=excluded.updated_at",
    )
    .bind(&params(&[
        &account_id.to_string(),
        &state.to_string(),
        &now,
        &last_error.map(String::from),
        &err_at,
        &if bump_reconnect { 1i64 } else { 0i64 },
        &if connected { Some(now) } else { None },
    ])?)?
    .run()
    .await?;
    Ok(())
}

/// 标记刚收到过事件（心跳健康位）。
pub async fn touch_session(db: &D1Database, account_id: &str) -> Result<()> {
    let now = now_ms();
    db.prepare("UPDATE sessions SET last_event_at=?1, updated_at=?1 WHERE account_id=?2")
        .bind(&params(&[&now, &account_id.to_string()])?)?
        .run()
        .await?;
    Ok(())
}

/// 好友全量快照。
///
/// 关键优化：先一次性读出当前状态，只对**真正变化**的行做写入。
/// 否则 300 个好友每 2 分钟重写一遍 = 每月约 1300 万行写入，
/// 单这一项就能吃掉 D1 整个写入额度。差异检测把稳态开销压到接近 0。
///
/// `force` 用于首次登录：那时表是空的，全量写入本来就是必要的。
pub async fn replace_friends(db: &D1Database, friends: &[VrcUser], force: bool) -> Result<usize> {
    let now = now_ms();

    // 一次读全部现状（1 次查询，而不是每个好友一次）
    let mut current: std::collections::HashMap<String, FriendRow> = std::collections::HashMap::new();
    for row in db
        .prepare(
            "SELECT f.user_id, f.state, f.location, f.status, f.status_description, f.world_id, \
             f.platform, u.display_name, u.username, u.current_avatar_thumb \
             FROM friends f LEFT JOIN users u ON u.id = f.user_id",
        )
        .all()
        .await?
        .results::<serde_json::Value>()?
    {
        let Some(id) = row.get("user_id").and_then(|v| v.as_str()) else {
            continue;
        };
        current.insert(id.to_string(), FriendRow::from_json(&row));
    }

    let mut written = 0usize;
    for f in friends {
        let state = derive_state(&f.location);
        let thumb = ustr(&f.current_avatar_thumbnail_image_url);

        let unchanged = !force
            && current
                .get(&f.id)
                .map(|c| {
                    c.state == state
                        && c.location == f.location
                        && c.status == f.status
                        && c.status_description == f.status_description
                        && c.platform == f.platform
                        && c.display_name == f.display_name
                        && c.username == f.username
                        && c.avatar_thumb == thumb.clone().unwrap_or_default()
                })
                .unwrap_or(false);

        if unchanged {
            continue;
        }

        let world = event::world_of(&f.location);
        let raw = to_json(f);

        // users 与 friends 合成一个批次，省一次往返
        db.batch(vec![
            db.prepare(
                "INSERT INTO users (id, username, display_name, current_avatar_thumb, \
                 current_avatar_image, user_icon, profile_pic_override, is_friend, \
                 first_seen_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?8) \
                 ON CONFLICT(id) DO UPDATE SET username=COALESCE(excluded.username, users.username), \
                 display_name=COALESCE(excluded.display_name, users.display_name), \
                 current_avatar_thumb=COALESCE(excluded.current_avatar_thumb, users.current_avatar_thumb), \
                 current_avatar_image=COALESCE(excluded.current_avatar_image, users.current_avatar_image), \
                 user_icon=COALESCE(excluded.user_icon, users.user_icon), \
                 profile_pic_override=COALESCE(excluded.profile_pic_override, users.profile_pic_override), \
                 is_friend=1, updated_at=excluded.updated_at",
            )
            .bind(&params(&[
                &f.id,
                &ustr(&f.username),
                &ustr(&f.display_name),
                &thumb,
                &ustr(&f.current_avatar_image_url),
                &ustr(&f.user_icon),
                &ustr(&f.profile_pic_override),
                &now,
            ])?)?,
            db.prepare(
                "INSERT INTO friends (user_id, state, status, status_description, location, world_id, \
                 platform, last_seen_at, raw_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(user_id) DO UPDATE SET state=excluded.state, status=excluded.status, \
                 status_description=excluded.status_description, location=excluded.location, \
                 world_id=excluded.world_id, platform=excluded.platform, last_seen_at=excluded.last_seen_at, \
                 raw_json=excluded.raw_json",
            )
            .bind(&params(&[
                &f.id,
                &state,
                &f.status,
                &f.status_description,
                &f.location,
                &world,
                &f.platform,
                &now,
                &raw,
            ])?)?,
        ])
        .await?;

        written += 1;
    }

    Ok(written)
}

/// 差异检测用的现状快照。
#[derive(Default)]
struct FriendRow {
    state: String,
    location: String,
    status: String,
    status_description: String,
    platform: String,
    display_name: String,
    username: String,
    avatar_thumb: String,
}

impl FriendRow {
    fn from_json(v: &Value) -> Self {
        let s = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };
        Self {
            state: s("state"),
            location: s("location"),
            status: s("status"),
            status_description: s("status_description"),
            platform: s("platform"),
            display_name: s("display_name"),
            username: s("username"),
            avatar_thumb: s("current_avatar_thumb"),
        }
    }
}

/// 空字符串视为"无值"，让 COALESCE 能保留旧值。
fn ustr(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// pipeline 事件带来的增量更新。
pub async fn apply_event(
    db: &D1Database,
    account_id: &str,
    ev: &Event,
    world_name: Option<&str>,
) -> Result<()> {
    let now = now_ms();

    // 1) 好友维度（只在事件真的带了用户对象时才更新资料）
    if let Some(uid) = &ev.actor_user_id {
        let (username, display_name, avatar_thumb) = user_fields_from_raw(&ev.raw);
        if username.is_some() || display_name.is_some() || avatar_thumb.is_some() {
            db.prepare(
                "INSERT INTO users (id, username, display_name, current_avatar_thumb, first_seen_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
                 ON CONFLICT(id) DO UPDATE SET username=COALESCE(excluded.username, users.username), \
                 display_name=COALESCE(excluded.display_name, users.display_name), \
                 current_avatar_thumb=COALESCE(excluded.current_avatar_thumb, users.current_avatar_thumb), \
                 updated_at=excluded.updated_at",
            )
            .bind(&params(&[uid, &username, &display_name, &avatar_thumb, &now])?)?
            .run()
            .await?;
        }
    }

    // 2) 世界维度（拿到名字才写，避免把已有名字覆盖成 NULL）
    if let (Some(wid), Some(name)) = (&ev.world_id, world_name) {
        let name = name.to_string();
        db.prepare(
            "INSERT INTO worlds (id, name, is_public, updated_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(id) DO UPDATE SET name=excluded.name, updated_at=excluded.updated_at",
        )
        .bind(&params(&[
            wid,
            &name,
            &ev.instance_location
                .as_deref()
                .map(event::is_public_instance)
                .unwrap_or(false),
            &now,
        ])?)?
        .run()
        .await?;
    }

    // 3) 好友在线状态漂移
    if let Some(uid) = &ev.actor_user_id {
        match ev.r#type.as_str() {
            "friend-online" | "friend-location" => {
                let state = "online".to_string();
                db.prepare(
                    "INSERT INTO friends (user_id, state, location, world_id, last_seen_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(user_id) DO UPDATE SET state=excluded.state, \
                     location=COALESCE(excluded.location, friends.location), \
                     world_id=COALESCE(excluded.world_id, friends.world_id), \
                     last_seen_at=excluded.last_seen_at",
                )
                .bind(&params(&[uid, &state, &ev.next_location, &ev.world_id, &now])?)?
                .run()
                .await?;
            }
            "friend-offline" => {
                let empty = String::new();
                db.prepare(
                    "INSERT INTO friends (user_id, state, location, last_online_at, last_seen_at) \
                     VALUES (?1, 'offline', ?2, ?3, ?3) \
                     ON CONFLICT(user_id) DO UPDATE SET state='offline', location=?2, \
                     last_online_at=excluded.last_online_at, last_seen_at=excluded.last_seen_at",
                )
                .bind(&params(&[uid, &empty, &now])?)?
                .run()
                .await?;
            }
            "friend-delete" => {
                db.prepare("DELETE FROM friends WHERE user_id=?1")
                    .bind(&params(&[uid])?)?
                    .run()
                    .await?;
            }
            _ => {}
        }
    }

    // 4) 事件流本体
    let account = account_id.to_string();
    let world_name = world_name.map(String::from);
    db.prepare(
        "INSERT INTO events (account_id, type, actor_user_id, actor_display_name, world_id, \
         world_name, instance_location, prev_location, next_location, ts, received_at, raw_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11)",
    )
    .bind(&params(&[
        &account,
        &ev.r#type,
        &ev.actor_user_id,
        &ev.actor_display_name,
        &ev.world_id,
        &world_name,
        &ev.instance_location,
        &ev.prev_location,
        &ev.next_location,
        &now,
        &event::sanitize_raw(&ev.raw),
    ])?)?
    .run()
    .await?;

    Ok(())
}

/// 事件原始 JSON 里可能带完整 user 对象，顺手抽出来省一次 API 调用。
fn user_fields_from_raw(raw: &str) -> (Option<String>, Option<String>, Option<String>) {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return (None, None, None);
    };
    let Some(user) = v.get("content").and_then(|c| c.get("user")) else {
        return (None, None, None);
    };
    (
        json_str(user, "username"),
        json_str(user, "displayName"),
        json_str(user, "currentAvatarThumbnailImageUrl"),
    )
}

/// 从 location 推导在线状态。
pub fn derive_state(location: &str) -> String {
    match location {
        "" | "offline" => "offline",
        _ => "online",
    }
    .to_string()
}
