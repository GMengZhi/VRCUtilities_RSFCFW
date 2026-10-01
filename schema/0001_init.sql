-- vrc-utilities · D1 初始化 schema
-- 放到 D1 的 migrations 目录下按序号顺序执行：
--   wrangler d1 migrations apply vrc-utilities --remote
--
-- 设计原则：
--   1. 所有时间戳统一用整数毫秒（INTEGER），不要用字符串，方便区间查询和排序。
--   2. 事件表保留 raw_json，解读逻辑将来改版时可以回放历史，不必回源 VRChat。
--   3. 维度表（users/worlds/instances）只存展示必需的列，其余原始对象进 raw_json。

-- ---------------------------------------------------------------------------
-- 账号 / 会话
-- ---------------------------------------------------------------------------

-- 杂项状态。目前只用来记住"最近一次登录尝试的用户名"，这样 2FA 那一步
-- 前端不必让用户重复输入账号，Worker 也能据此定位到同一个 Durable Object。
CREATE TABLE IF NOT EXISTS app_state (
  key                    TEXT PRIMARY KEY,
  value                  TEXT,
  updated_at             INTEGER NOT NULL
);

-- 一个 VRChat 账号一行。auth cookie 不落库，只存在 Durable Object 存储里。
CREATE TABLE IF NOT EXISTS accounts (
  id                     TEXT PRIMARY KEY,           -- 内部主键，等于 vrchat_user_id
  vrchat_user_id         TEXT NOT NULL UNIQUE,       -- usr_xxxx
  display_name           TEXT,
  username               TEXT,
  status                 TEXT,                       -- active / join me / ask me / busy / offline
  status_description     TEXT,
  location               TEXT,
  world_id               TEXT,
  platform               TEXT,
  trust_rank             TEXT,
  is_plus                INTEGER NOT NULL DEFAULT 0,
  raw_json               TEXT,
  created_at             INTEGER NOT NULL,
  updated_at             INTEGER NOT NULL
);

-- 会话健康度。DO 每次拿到 pipeline 事件、每次心跳都会更新这一行。
-- 网页看板顶部的“连接状态/最后事件时间”就读这张表。
CREATE TABLE IF NOT EXISTS sessions (
  account_id             TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  pipeline_state         TEXT NOT NULL,              -- offline | connecting | connected | error
  last_event_at          INTEGER,
  last_connect_at        INTEGER,
  last_error             TEXT,
  last_error_at          INTEGER,
  reconnect_count        INTEGER NOT NULL DEFAULT 0,
  updated_at             INTEGER NOT NULL
);

-- ---------------------------------------------------------------------------
-- 维度：用户 / 世界 / 实例
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS users (
  id                     TEXT PRIMARY KEY,           -- usr_xxxx
  username               TEXT,
  display_name           TEXT,
  current_avatar_image   TEXT,
  current_avatar_thumb   TEXT,
  profile_pic_override   TEXT,
  user_icon              TEXT,
  bio                    TEXT,
  trust_rank             TEXT,
  is_friend              INTEGER NOT NULL DEFAULT 0,
  first_seen_at          INTEGER NOT NULL,
  updated_at             INTEGER NOT NULL,
  raw_json               TEXT
);
CREATE INDEX IF NOT EXISTS idx_users_display_name ON users(display_name);
CREATE INDEX IF NOT EXISTS idx_users_is_friend    ON users(is_friend);

CREATE TABLE IF NOT EXISTS worlds (
  id                     TEXT PRIMARY KEY,           -- wrld_xxxx
  name                   TEXT,
  author_name            TEXT,
  thumbnail_image_url    TEXT,
  capacity               INTEGER,
  occupants              INTEGER,                    -- 上次已知人数（公开实例才有意义）
  is_public              INTEGER NOT NULL DEFAULT 0,
  updated_at             INTEGER NOT NULL,
  raw_json               TEXT
);

-- 实例 = 世界 + 实例后缀。location 形如 wrld_xxx:12345~region(us)~...
CREATE TABLE IF NOT EXISTS instances (
  location               TEXT PRIMARY KEY,           -- 完整 location 字符串
  world_id               TEXT NOT NULL,
  instance_id            TEXT NOT NULL,              -- 冒号后那段
  world_name             TEXT,
  region                 TEXT,
  occupants              INTEGER,
  n_users                INTEGER,
  is_public              INTEGER NOT NULL DEFAULT 0,
  first_seen_at          INTEGER NOT NULL,
  last_seen_at           INTEGER NOT NULL,
  raw_json               TEXT
);
CREATE INDEX IF NOT EXISTS idx_instances_world    ON instances(world_id);
CREATE INDEX IF NOT EXISTS idx_instances_lastseen ON instances(last_seen_at);

-- ---------------------------------------------------------------------------
-- 关系：好友快照
-- ---------------------------------------------------------------------------
-- 每个用户只保留一行“当前状态”，另外在 users 表里留档资料。
-- 历史变化走 events 表，不要在这里堆版本。
CREATE TABLE IF NOT EXISTS friends (
  user_id                TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  state                  TEXT,                       -- online | active | offline
  status                 TEXT,
  status_description     TEXT,
  location               TEXT,
  world_id               TEXT,
  platform               TEXT,
  trust_rank             TEXT,
  last_online_at         INTEGER,
  last_seen_at           INTEGER NOT NULL,
  raw_json               TEXT
);
CREATE INDEX IF NOT EXISTS idx_friends_state    ON friends(state);
CREATE INDEX IF NOT EXISTS idx_friends_location ON friends(location);

-- ---------------------------------------------------------------------------
-- 事件流：整个系统的核心
-- ---------------------------------------------------------------------------
-- type 取值（与 VRChat pipeline 的 type 字段对齐）：
--   friend-add / friend-delete / friend-online / friend-offline
--   friend-location / friend-update / friend-active
--   user-update / user-location
--   notification / notification-v2 / response-notification
--   group-joined / group-left / group-member-updated / group-role-updated
--   instance-queue-joined / instance-queue-ready
-- 另有本服务自己产生的事件：
--   sys-pipeline-connected / sys-pipeline-disconnected（用于时间轴断点可视化）
CREATE TABLE IF NOT EXISTS events (
  id                     INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id             TEXT NOT NULL,
  type                   TEXT NOT NULL,
  actor_user_id          TEXT,                       -- 事件主体（通常是好友）
  actor_display_name     TEXT,                       -- 冗余一份，避免看历史时 join 不到
  world_id               TEXT,
  world_name             TEXT,
  instance_location      TEXT,
  prev_location          TEXT,                       -- 仅 friend-location / user-location 有值
  next_location          TEXT,
  ts                     INTEGER NOT NULL,           -- 事件时间（毫秒）
  received_at            INTEGER NOT NULL,
  raw_json               TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_ts      ON events(ts DESC);
CREATE INDEX IF NOT EXISTS idx_events_actor   ON events(actor_user_id, ts DESC);
CREATE INDEX IF NOT EXISTS idx_events_type    ON events(type, ts DESC);
CREATE INDEX IF NOT EXISTS idx_events_world   ON events(world_id, ts DESC);
