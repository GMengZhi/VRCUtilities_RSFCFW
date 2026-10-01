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

process.exit(failed ? 1 : 0);
