// 语法校验：把前端脚本按 script 方式解析一遍，能挡住绝大多数低级错误。
const fs = require('fs');
const vm = require('vm');

const files = ['web/app.js'];
let failed = 0;

for (const f of files) {
  const src = fs.readFileSync(f, 'utf8');
  try {
    new vm.Script(src, { filename: f });
    console.log(`SYNTAX OK   ${f}  (${src.split('\n').length} lines, ${src.length} bytes)`);
  } catch (e) {
    console.error(`SYNTAX FAIL ${f}: ${e.message}`);
    failed++;
  }
}

// index.html 只做基本结构检查：标签配对数量一致
const html = fs.readFileSync('web/index.html', 'utf8');
const openTags = (html.match(/<(section|form|div|ul|main|header|footer|body|html)\b/g) || []).length;
const closeTags = (html.match(/<\/(section|form|div|ul|main|header|footer|body|html)>/g) || []).length;
if (openTags === closeTags) {
  console.log(`HTML OK     web/index.html (${openTags} 组容器标签配对)`);
} else {
  console.error(`HTML FAIL   容器标签不配对: 开 ${openTags} / 闭 ${closeTags}`);
  failed++;
}

// 回归检查：防止再次踩 workers-rs 的响应构造坑。
//
// 背景：ResponseBuilder::ok()     内部会强制 Content-Type = text/plain; charset=utf-8
//       ResponseBuilder::from_bytes() 内部会强制 Content-Type = application/octet-stream
// 两者都在链式调用之后执行，会覆盖你在 builder 上先设的 Content-Type。
// 症状是网页把 HTML 当纯文本显示。正确做法是在构造好的 Response 上改 headers_mut()。
const risky = [];
for (const f of fs.readdirSync('src').filter((x) => x.endsWith('.rs'))) {
  const src = fs.readFileSync(`src/${f}`, 'utf8');
  const lines = src.split('\n');
  lines.forEach((line, idx) => {
    // 在 builder 链里设置 Content-Type，且同一链后面接了 .ok( 或 .from_bytes(
    const window = lines.slice(idx, idx + 6).join('\n');
    if (/with_header\(\s*"Content-Type"/.test(line) && /\.(ok|from_bytes)\(/.test(window)) {
      risky.push(`src/${f}:${idx + 1}`);
    }
  });
}

if (risky.length) {
  console.error('CONTENT-TYPE FAIL  以下是会失效的写法（请改到 Response 上设头）:');
  for (const r of risky) console.error(`   ${r}`);
  failed++;
} else {
  console.log('CONTENT-TYPE OK   未发现会在 builder 链里被覆盖的写法');
}

process.exit(failed ? 1 : 0);
