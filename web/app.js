/* VRC Utilities · 看板前端
 * 全部渲染走 textContent / createElement，绝不拼 innerHTML —— 
 * 好友昵称和状态描述来自 VRChat，属于不可信数据，拼 HTML 会出 XSS。
 */
'use strict';

const POLL_MS = 3000;
const IDLE_POLL_MS = 10000;   // 未登录 / 等验证码 / 限流冷却时的轮询间隔
const TOKEN_KEY = 'vru_token';

const el = (id) => document.getElementById(id);
const tokenInput = el('token');
const onlyLocation = el('only-location');

let sinceId = 0;               // 事件增量游标
let sessionState = 'offline';
let currentSessionId = null;   // 服务端确认已登录时的账号 ID

/* ------------------------------------------------------------------ */
/* 令牌 */
/* ------------------------------------------------------------------ */
function getToken() {
  // 支持 ?token=xxx 直接进站（方便加到浏览器书签），随后存入 localStorage
  const url = new URL(location.href);
  const fromQuery = url.searchParams.get('token');
  if (fromQuery) {
    localStorage.setItem(TOKEN_KEY, fromQuery);
    url.searchParams.delete('token');
    history.replaceState(null, '', url.toString());
  }
  return localStorage.getItem(TOKEN_KEY) || '';
}

function setToken(t) {
  if (t) localStorage.setItem(TOKEN_KEY, t);
  else localStorage.removeItem(TOKEN_KEY);
}

async function api(path) {
  const res = await fetch(path, {
    headers: { Authorization: 'Bearer ' + getToken() },
  });
  if (res.status === 401) throw new Error('UNAUTHORIZED');
  if (!res.ok) throw new Error('HTTP ' + res.status);
  return res.json();
}

/* ------------------------------------------------------------------ */
/* 事件文案 */
/* ------------------------------------------------------------------ */
const TYPE_LABEL = {
  'friend-add': '成为好友',
  'friend-delete': '删除好友',
  'friend-online': '上线',
  'friend-offline': '下线',
  'friend-location': '换房间',
  'friend-update': '资料更新',
  'friend-active': '网页端活跃',
  'user-update': '我的资料更新',
  'user-location': '我换房间',
  'notification': '通知',
  'notification-v2': '通知',
  'response-notification': '通知响应',
  'group-joined': '加入群组',
  'group-left': '离开群组',
  'group-member-updated': '群组成员变动',
  'group-role-updated': '群组角色变动',
  'instance-queue-joined': '进入排队',
  'instance-queue-ready': '排队到号',
  'sys-pipeline-connected': '监控已连接',
  'sys-pipeline-disconnected': '监控已断开',
};

const SHORT_LOCATION = {
  '': '未知',
  'offline': '离线',
  'traveling': '切图中',
  'private': '私密房间',
  'private:private': '私密房间',
};

function locText(loc) {
  if (loc == null) return '';
  if (loc in SHORT_LOCATION) return SHORT_LOCATION[loc];
  return loc;
}

function relTime(ms) {
  const d = Date.now() - ms;
  if (d < 0) return '刚刚';
  if (d < 60_000) return Math.floor(d / 1000) + '秒前';
  if (d < 3600_000) return Math.floor(d / 60_000) + '分钟前';
  if (d < 86400_000) return Math.floor(d / 3600_000) + '小时前';
  return new Date(ms).toLocaleString('zh-CN', { hour12: false });
}

/* 记录每个好友上一次的位置，用来把 friend-location 渲染成 a → b */
const lastLocation = new Map();

/* ------------------------------------------------------------------ */
/* 渲染 */
/* ------------------------------------------------------------------ */
function renderSession(s) {
  sessionState = s.pipeline_state;
  currentSessionId = s.account_id || s.id || s.user_id || null;
  const dot = el('conn-dot');
  dot.className = 'dot ' + s.pipeline_state;
  const map = {
    connected: '监控运行中',
    connecting: '连接中…',
    error: '连接异常',
    offline: '未连接',
  };
  el('conn-text').textContent = map[s.pipeline_state] || s.pipeline_state;

  const cards = [
    ['账号', s.display_name || s.account_id || '—'],
    ['我的状态', s.status || '—'],
    ['我所在位置', locText(s.location) || '—'],
    ['最后事件', s.last_event_at ? relTime(s.last_event_at) : '暂无'],
    ['重连次数', String(s.reconnect_count ?? 0)],
  ];
  if (s.last_error) cards.push(['最近错误', s.last_error]);

  const box = el('account-cards');
  box.replaceChildren(...cards.map(([k, v]) => {
    const div = document.createElement('div');
    div.className = 'card';
    const kk = document.createElement('div');
    kk.className = 'k';
    kk.textContent = k;
    const vv = document.createElement('div');
    vv.className = 'v';
    vv.textContent = v;
    div.append(kk, vv);
    return div;
  }));
}

function renderEvents(events) {
  const ul = el('events');
  if (!events.length && sinceId === 0) {
    ul.replaceChildren(Object.assign(document.createElement('li'),
      { className: 'empty', textContent: '暂无事件' }));
    return;
  }

  // 新事件插到最前面
  for (const ev of events) {
    const li = document.createElement('li');

    const time = document.createElement('span');
    time.className = 'time';
    time.title = new Date(ev.ts).toLocaleString('zh-CN', { hour12: false });
    time.textContent = relTime(ev.ts);

    const body = document.createElement('div');
    body.className = 'body';

    const tag = document.createElement('span');
    tag.className = 'tag ' + ev.type;
    tag.textContent = TYPE_LABEL[ev.type] || ev.type;
    body.append(tag);

    const who = document.createElement('span');
    who.className = 'who';
    who.textContent = ev.actor_display_name || ev.actor_user_id || '系统';
    body.append(who);

    // 位置类事件渲染成 "旧位置 → 新位置"
    let loc = null;
    if (ev.type === 'friend-location' || ev.type === 'user-location') {
      const prev = ev.prev_location ?? lastLocation.get(ev.actor_user_id) ?? '';
      if (ev.next_location != null) lastLocation.set(ev.actor_user_id, ev.next_location);
      loc = locText(prev) + ' → ' + locText(ev.next_location ?? '');
    } else if (ev.type === 'friend-online') {
      loc = locText(ev.next_location ?? '');
      if (ev.next_location != null) lastLocation.set(ev.actor_user_id, ev.next_location);
    } else if (ev.world_name) {
      loc = ev.world_name;
    }

    if (loc) {
      const l = document.createElement('span');
      l.className = 'loc';
      l.textContent = ' ' + loc;
      body.append(l);
    }

    li.append(time, body);
    ul.prepend(li);
  }

  // 控制 DOM 规模
  while (ul.children.length > 300) ul.lastElementChild.remove();
}

function renderFriends(friends) {
  el('friends-count').textContent = '(' + friends.length + ')';
  const ul = el('friends');
  if (!friends.length) {
    ul.replaceChildren(Object.assign(document.createElement('li'),
      { className: 'empty', textContent: '暂无好友数据' }));
    return;
  }
  ul.replaceChildren(...friends.map((f) => {
    const li = document.createElement('li');

    const img = document.createElement('img');
    img.loading = 'lazy';
    img.alt = '';
    img.src = f.current_avatar_thumb || f.user_icon || '';
    if (!img.src) img.removeAttribute('src');

    const name = document.createElement('div');
    name.className = 'name';
    const b = document.createElement('b');
    b.textContent = f.display_name || f.user_id;
    const sub = document.createElement('span');
    const where = f.state === 'online'
      ? (locText(f.location) || f.world_name || '在线')
      : (f.status_description || '');
    sub.textContent = where;
    name.append(b, sub);

    const badge = document.createElement('span');
    badge.className = 'badge ' + (f.state || 'offline');
    badge.textContent = f.state === 'online' ? '游戏中'
      : f.state === 'active' ? '网页端' : '离线';

    li.append(img, name, badge);
    return li;
  }));
}

function setFoot(msg) { el('foot-status').textContent = msg; }

/* ------------------------------------------------------------------ */
/* 登录 / 2FA                                                          */
/* ------------------------------------------------------------------ */
const loginPanel = el('login-panel');
const loginForm = el('login-form');
const otpForm = el('otp-form');
const loginMsg = el('login-msg');

let lastUsername = '';
let cooldownTimer = null;
let cooldownUntil = 0;

/* 限流倒计时：禁用提交按钮，到点自动恢复。
   这是必须的 —— VRChat 的验证码端点限流很紧，限流期间反复点提交
   会让冷却窗口不断续期，用户永远出不来。 */
function startCooldown(seconds, reason) {
  const submit = otpForm.querySelector('button[type="submit"]');
  const codeInput = el('otp-code');
  cooldownUntil = Date.now() + seconds * 1000;

  if (cooldownTimer) clearInterval(cooldownTimer);

  const render = () => {
    const left = Math.ceil((cooldownUntil - Date.now()) / 1000);
    if (left <= 0) {
      clearInterval(cooldownTimer);
      cooldownTimer = null;
      submit.disabled = false;
      codeInput.disabled = false;
      submit.textContent = '提交验证码';
      loginMsg.textContent = '可以重新尝试了。' + (reason || '');
      return;
    }
    submit.disabled = true;
    codeInput.disabled = true;
    submit.textContent = `请等待 ${left} 秒`;
    const mm = String(Math.floor(left / 60)).padStart(2, '0');
    const ss = String(left % 60).padStart(2, '0');
    loginMsg.textContent =
      `被 VRChat 限流（429）。这是提交过频触发的，冷却期间请勿重复提交，` +
      `剩余 ${mm}:${ss}。${reason || ''}`;
  };

  render();
  cooldownTimer = setInterval(render, 1000);
}

function clearCooldown() {
  if (cooldownTimer) clearInterval(cooldownTimer);
  cooldownTimer = null;
  cooldownUntil = 0;
  const submit = otpForm.querySelector('button[type="submit"]');
  const codeInput = el('otp-code');
  if (submit) { submit.disabled = false; submit.textContent = '提交验证码'; }
  if (codeInput) codeInput.disabled = false;
}

function showLogin(needLogin, awaiting2fa, username) {
  loginPanel.hidden = !(needLogin || awaiting2fa);
  if (loginPanel.hidden) return;

  loginForm.hidden = awaiting2fa;
  otpForm.hidden = !awaiting2fa;
  el('login-title').textContent = awaiting2fa ? '输入两步验证码' : '登录 VRChat 账号';

  if (username) {
    lastUsername = username;
    if (awaiting2fa) el('login-username').value = username;
  }
  if (awaiting2fa && !cooldownTimer) el('otp-code').focus();
}

async function post(path, payload) {
  const res = await fetch(path, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Authorization: 'Bearer ' + getToken(),
    },
    body: JSON.stringify(payload),
  });
  let data = {};
  try { data = await res.json(); } catch { /* 空响应体或非 JSON */ }
  if (res.status === 401) throw new Error('令牌无效或未设置，请先在右上角填入看板访问令牌');
  if (data.error) {
    // 把结构化标记原样挂到 error 上，调用方据此决定下一步动作
    const err = new Error(data.error);
    err.needsRelogin = !!data.needs_relogin
      || /重新提交账号密码|没有待验证的登录/.test(data.error);
    err.rateLimited = !!data.rate_limited;
    err.retryAfterSecs = data.retry_after_secs || 0;
    err.retryable = !!data.retryable;
    throw err;
  }
  if (!res.ok) throw new Error('HTTP ' + res.status);
  return data;
}

loginForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  loginMsg.textContent = '正在登录…';
  lastUsername = el('login-username').value.trim();
  try {
    const data = await post('/api/login', {
      username: lastUsername,
      password: el('login-password').value,
    });
    el('login-password').value = '';
    clearCooldown();
    if (data.need2fa) {
      const methods = (data.methods || []).join(', ') || 'totp';
      loginMsg.textContent = '需要两步验证（' + methods + '），请输入验证码。';
      showLogin(true, true, lastUsername);
    } else {
      loginMsg.textContent = '登录成功，已同步 ' + (data.friends ?? 0) + ' 位好友，正在建立实时连接…';
      showLogin(false, false);
      tick();
    }
  } catch (err) {
    if (err.rateLimited) {
      startCooldown(err.retryAfterSecs || 300, '');
      return;
    }
    loginMsg.textContent = '登录失败：' + err.message;
  }
});

otpForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  if (cooldownTimer) return;  // 冷却期间直接忽略（按钮也已被禁用）
  loginMsg.textContent = '正在校验验证码…';
  try {
    const data = await post('/api/verify', {
      username: lastUsername,
      code: el('otp-code').value.trim(),
    });
    el('otp-code').value = '';
    clearCooldown();
    loginMsg.textContent = '验证成功，已同步 ' + (data.friends ?? 0) + ' 位好友。';
    showLogin(false, false);
    tick();
  } catch (err) {
    if (err.rateLimited) {
      el('otp-code').value = '';
      startCooldown(err.retryAfterSecs || 300, '');
      return;
    }
    loginMsg.textContent = '验证失败：' + err.message;
    if (err.needsRelogin) {
      // 待验证会话已失效，回到账号密码步骤；否则用户会卡在永远失败的验证码表单上
      el('otp-code').value = '';
      clearCooldown();
      showLogin(true, false);
      loginMsg.textContent = '验证失败：' + err.message + ' 请重新输入账号密码。';
    }
  }
});

/* ------------------------------------------------------------------ */
/* 轮询 */
/* ------------------------------------------------------------------ */
function isLocationEvent(t) {
  return t === 'friend-location' || t === 'user-location'
      || t === 'friend-online' || t === 'friend-offline';
}

async function tick() {
  try {
    // 第一步只问"谁登录了"和"要不要输验证码"。
    // 未登录时绝不请求 friends / events —— 那些数据本来就不存在，
    // 请求它们只会制造无意义的流量，也会让你误以为后台在空跑。
    const [session, pending] = await Promise.all([
      api('/api/session'),
      api('/api/pending').catch(() => ({ awaiting_2fa: false, cooldown_secs: 0 })),
    ]);

    const loggedIn = !!(session.account_id || session.id || session.user_id);
    renderSession(session);

    // 没有账号就显示登录表单；有账号但 cookie 还没建立则停在验证码那一步
    showLogin(!loggedIn, !!pending.awaiting_2fa, pending.username);

    // 刷新页面后恢复限流倒计时（冷却状态存在服务端）
    if (pending.cooldown_secs > 0 && !cooldownTimer) {
      startCooldown(pending.cooldown_secs, '');
    }

    if (!loggedIn) {
      // 保持列表为空态，不触发任何数据请求
      if (sinceId === 0) {
        renderEvents([]);
        renderFriends([]);
      }
      setFoot('未登录 — 已暂停数据请求');
      return;
    }

    // 只有确认已登录才拉取实际数据
    const q = new URLSearchParams({ since_id: String(sinceId), limit: '100' });
    const [events, friends] = await Promise.all([
      api('/api/events?' + q),
      api('/api/friends'),
    ]);

    renderFriends(friends.friends || []);

    let list = events.events || [];
    if (onlyLocation.checked) list = list.filter((e) => isLocationEvent(e.type));

    if (list.length) {
      renderEvents(list);
      sinceId = Math.max(sinceId, ...list.map((e) => e.id));
      el('events-count').textContent = '(' + el('events').children.length + ')';
    } else if (sinceId === 0) {
      renderEvents([]);
    }

    setFoot('最后刷新 ' + new Date().toLocaleTimeString('zh-CN', { hour12: false }));
  } catch (err) {
    if (err.message === 'UNAUTHORIZED') {
      setFoot('令牌无效或未设置，请在右上角填入看板访问令牌。');
      el('conn-dot').className = 'dot error';
      el('conn-text').textContent = '未授权';
      return;
    }
    setFoot('请求失败：' + err.message);
  }
}

/* 轮询循环。
 *
 * 用自排程的 setTimeout 而不是 setInterval，因为要让间隔**随状态变化**：
 *   · 已登录   → 3 秒（实时性优先）
 *   · 未登录 / 等验证码 / 限流冷却 → 放慢到 10 秒
 * 这三种情况下数据不会变，高频轮询纯属浪费。 */
let tickTimer = null;
let tickFailures = 0;

async function loop() {
  const fast = !!currentSessionId && !cooldownTimer;
  try {
    await tick();
    tickFailures = 0;
  } catch {
    // tick 内部已处理；这里只累计失败次数用于退避
    tickFailures = Math.min(tickFailures + 1, 5);
  }
  const delay = (fast ? POLL_MS : IDLE_POLL_MS) * (tickFailures ? 2 : 1);
  tickTimer = setTimeout(loop, delay);
}

/* ------------------------------------------------------------------ */
/* 启动 */
/* ------------------------------------------------------------------ */
tokenInput.value = '';
el('save-token').addEventListener('click', () => {
  setToken(tokenInput.value.trim());
  resetViews();
});
tokenInput.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') el('save-token').click();
});
el('refresh').addEventListener('click', () => {
  resetViews();
});
onlyLocation.addEventListener('change', () => {
  resetViews();
});

function resetViews() {
  sinceId = 0;
  el('events').replaceChildren();
  if (tickTimer) clearTimeout(tickTimer);
  tickTimer = setTimeout(loop, 0);
}

loop();
