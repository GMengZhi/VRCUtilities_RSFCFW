// 端到端验证登录链路的错误报告质量。
// 用明显错误的凭据打真实的 VRChat API，确认错误文本可读、可指导下一步操作。
const BASE = process.env.BASE || 'http://127.0.0.1:8790';

async function waitReady() {
  for (let i = 0; i < 60; i++) {
    try {
      const r = await fetch(BASE + '/');
      if (r.ok) return true;
    } catch { /* 还没起来 */ }
    await new Promise((r) => setTimeout(r, 2000));
  }
  return false;
}

async function post(path, payload) {
  const res = await fetch(BASE + path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  });
  const text = await res.text();
  let json = null;
  try { json = JSON.parse(text); } catch { /* 非 JSON */ }
  return { status: res.status, ct: res.headers.get('content-type'), json, text };
}

(async () => {
  if (!(await waitReady())) {
    console.error('服务未就绪');
    process.exit(1);
  }
  console.log('服务就绪\n');

  console.log('=== 1) 缺少字段 ===');
  let r = await post('/api/login', { username: '', password: '' });
  console.log(r.status, r.ct, JSON.stringify(r.json));

  console.log('\n=== 2) 错误凭据（真实打到 VRChat API）===');
  r = await post('/api/login', {
    username: 'vrc-utilities-probe@example.invalid',
    password: 'definitely-not-the-password',
  });
  console.log(r.status, r.ct);
  console.log(r.json ? JSON.stringify(r.json, null, 2) : r.text.slice(0, 400));

  console.log('\n=== 3) 没有待验证会话时提交验证码 ===');
  r = await post('/api/verify', { code: '123456' });
  console.log(r.status, r.ct);
  console.log(r.json ? JSON.stringify(r.json, null, 2) : r.text.slice(0, 400));

  console.log('\n=== 4) 静态页 Content-Type ===');
  for (const p of ['/', '/app.css', '/app.js']) {
    const res = await fetch(BASE + p);
    console.log(`${p.padEnd(10)} ${res.status}  ${res.headers.get('content-type')}`);
  }
})();
