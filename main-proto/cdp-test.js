// 轻量 CDP 驱动：启动 headless Chrome，连接 DevTools Protocol，执行交互测试并截图。
// 用法：node cdp-test.js
const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");

const CHROME = process.env.HOME + "/.agent-browser/browsers/chrome-155.0.8059.39/chrome.exe";
const URL = "file:///D:/modular-clipboard/main-proto/index.html";
const OUT = "D:/modular-clipboard/main-proto";
// 每次运行用独立端口 + 独立配置目录：否则会连上上一次运行遗留的 Chrome
// （旧页面已被上轮子测试改过，首条断言必挂）。
const PORT = 9400 + (process.pid % 500);
const PROFILE = require("os").tmpdir() + "/cdp-test-" + process.pid;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  const chrome = spawn(CHROME, [
    "--headless=new", "--disable-gpu", "--no-sandbox",
    `--remote-debugging-port=${PORT}`,
    "--window-size=900,1060",
    "--user-data-dir=" + PROFILE,
    "about:blank",
  ], { stdio: "ignore" });

  try {
    // 等 DevTools 端口就绪
    let targets = null;
    for (let i = 0; i < 40; i++) {
      await sleep(250);
      try {
        const res = await fetch(`http://127.0.0.1:${PORT}/json/list`);
        targets = await res.json();
        if (targets.length) break;
      } catch (_) {}
    }
    if (!targets) throw new Error("DevTools 端口未就绪");
    const page = targets.find((t) => t.type === "page");
    const ws = new WebSocket(page.webSocketDebuggerUrl);
    await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });

    let msgId = 0;
    const pending = new Map();
    ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.id && pending.has(m.id)) {
        const { res, rej } = pending.get(m.id);
        pending.delete(m.id);
        m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result);
      }
    };
    const send = (method, params = {}) =>
      new Promise((res, rej) => {
        const id = ++msgId;
        pending.set(id, { res, rej });
        ws.send(JSON.stringify({ id, method, params }));
      });

    const evalJs = async (expr) => {
      const r = await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
      return r.result.value;
    };
    const shot = async (name) => {
      const r = await send("Page.captureScreenshot", { format: "png" });
      fs.writeFileSync(path.join(OUT, name), Buffer.from(r.data, "base64"));
      console.log("saved", name);
    };

    await send("Page.enable");
    await send("Runtime.enable");
    await send("Page.navigate", { url: URL });
    await sleep(1200);

    const results = [];
    const check = (name, ok, detail = "") => {
      results.push({ name, ok, detail });
      console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  — " + detail : ""}`);
    };

    // ---- 1. 初始状态 ----
    check("初始行数 = 17", (await evalJs(`document.querySelectorAll('.row').length`)) === 17);
    check("托盘默认隐藏", await evalJs(`document.querySelector('#tray').hidden === true && getComputedStyle(document.querySelector('#tray')).display === 'none'`));
    check("首行置顶（实心图钉）", await evalJs(`document.querySelector('.row .abtn[data-act=pin] svg').getAttribute('fill') !== 'none'`));
    check("置顶按钮默认激活(accent)", await evalJs(`document.querySelector('#btnPin svg').getAttribute('fill') === 'var(--accent)' || document.querySelector('#btnPin svg').style.fill === 'var(--accent)'`));
    check("行宽让出滚动条(633px)", Math.abs((await evalJs(`document.querySelector('.row').getBoundingClientRect().width`)) - 633) < 1,
      "w=" + (await evalJs(`document.querySelector('.row').getBoundingClientRect().width`)));

    // ---- 2. 单击选中 ----
    await evalJs(`document.querySelectorAll('.row')[2].click()`);
    check("单击选中（selected 类 + 描边）", await evalJs(`document.querySelectorAll('.row')[2].classList.contains('selected') && !!document.querySelector('.row.selected .sel-fx')`));
    check("选中态预览转 text_bright", await evalJs(`getComputedStyle(document.querySelector('.row.selected .preview')).color === 'rgb(252, 252, 252)'`));
    // 选中一个置顶行：pin-tint 应被隐藏（Rust 选中分支不画 pin_tint）
    await evalJs(`document.querySelector('.row[data-id="1"]').click()`);
    check("选中置顶行不显示 pin-tint", await evalJs(`getComputedStyle(document.querySelector('.row[data-id="1"] .pin-tint')).display === 'none'`));
    await evalJs(`document.querySelectorAll('.row')[2].click()`);

    // ---- 3. 双击复制 → toast ----
    await evalJs(`document.querySelectorAll('.row')[2].dispatchEvent(new MouseEvent('dblclick',{bubbles:true}))`);
    await sleep(300);
    check("双击复制弹 toast「已复制到剪贴板」", await evalJs(`!document.querySelector('#toast').hidden && document.querySelector('#toast').textContent === '已复制到剪贴板'`));

    // ---- 4. 行内置顶 → 置顶组内重排 ----
    await evalJs(`document.querySelectorAll('.row')[4].querySelector('.abtn[data-act=pin]').click()`);
    await sleep(200);
    check("置顶后进入置顶组（排在所有非置顶行之前）", await evalJs(`(() => {
      const rows = [...document.querySelectorAll('.row')];
      const i5 = rows.findIndex(r => r.dataset.id === '5');
      const firstUnpinned = rows.findIndex(r => !r.querySelector('.pin-tint'));
      return i5 >= 0 && i5 < firstUnpinned;
    })()`), "pos=" + await evalJs(`[...document.querySelectorAll('.row')].findIndex(r => r.dataset.id === '5')`));
    check("置顶行有 pin-tint", await evalJs(`!!document.querySelector('.row[data-id="5"] .pin-tint')`));

    // ---- 5. 删除行 ----
    await evalJs(`document.querySelector('.row[data-id="5"] .abtn[data-act=del]').click()`);
    await sleep(200);
    check("删除该行（剩 16 行）", (await evalJs(`document.querySelectorAll('.row').length`)) === 16);
    check("删除 toast「已删除 1 条记录」", await evalJs(`document.querySelector('#toast').textContent === '已删除 1 条记录'`));

    // ---- 5b. 任意操作取消选中 ----
    await evalJs(`document.querySelector('.row[data-id="3"]').click()`);
    check("先选中一行", await evalJs(`!!document.querySelector('.row.selected')`));
    await evalJs(`document.querySelector('.row[data-id="3"] .abtn[data-act=pin]').click()`);
    await sleep(200);
    check("点行内置顶后取消选中", await evalJs(`!document.querySelector('.row.selected')`));
    await evalJs(`document.querySelector('.row[data-id="6"]').click()`);
    await evalJs(`document.querySelector('.row[data-id="6"]').dispatchEvent(new MouseEvent('dblclick',{bubbles:true}))`);
    await sleep(200);
    check("双击复制后取消选中", await evalJs(`!document.querySelector('.row.selected')`));

    // ---- 6. 搜索过滤 ----
    await evalJs(`document.querySelector('#btnSearch').click()`);
    await sleep(300);
    check("点击搜索图标展开悬浮输入框", await evalJs(`!document.querySelector('#searchFloat').hidden && document.querySelector('#searchFloat').classList.contains('show')`));
    await evalJs(`(()=>{const i=document.querySelector('#searchInput'); i.value='cargo'; i.dispatchEvent(new Event('input',{bubbles:true}));})()`);
    await sleep(300);
    check("搜索 'cargo' 匹配 2 行（终端 + Markdown 条目）", (await evalJs(`document.querySelectorAll('.row').length`)) === 2);
    check("清除按钮可见", await evalJs(`document.querySelector('#searchClear').classList.contains('show')`));
    await evalJs(`(()=>{const i=document.querySelector('#searchInput'); i.value='不存在的关键词xyz'; i.dispatchEvent(new Event('input',{bubbles:true}));})()`);
    await sleep(300);
    check("无结果显示「没有匹配的结果」", await evalJs(`!document.querySelector('#empty').hidden && document.querySelector('#empty').textContent === '没有匹配的结果'`));
    await evalJs(`document.querySelector('#searchClear').click()`);
    await sleep(300);
    check("清空搜索恢复 16 行", (await evalJs(`document.querySelectorAll('.row').length`)) === 16);

    // ---- 6b. 类型过滤栏 ----
    // 注意：id5（唯一的 HTML 条目）已在第 5 节被删，此时库内为 文本/文件/图片 三类
    check("类型过滤栏默认 4 枚（HTML 已删；文本/文件/图片/Markdown）", (await evalJs(`document.querySelectorAll('.type-btn').length`)) === 4);
    check("类型栏在主窗口右侧、间距 12px", await evalJs(`(() => {
      const rail = document.querySelector('#typeRail').getBoundingClientRect();
      const w = document.querySelector('#win').getBoundingClientRect();
      return Math.abs(rail.left - w.right - 12) < 0.6;
    })()`));
    await evalJs(`document.querySelector('.type-btn[data-kind="Image"]').click()`);
    await sleep(300);
    check("点图片类型 → 只剩 2 张图片行", (await evalJs(`document.querySelectorAll('.row').length`)) === 2);
    check("图片行内嵌显示 img", await evalJs(`document.querySelectorAll('.row .row-img').length`) === 2);
    check("图片行高 = 179.1（图高 108 + meta + top + pads）", await evalJs(`(() => {
      const h = document.querySelector('.row[data-id="7"]').getBoundingClientRect().height;
      return Math.abs(h - 179.1) < 0.6;
    })()`), "h=" + await evalJs(`document.querySelector('.row[data-id="7"]').getBoundingClientRect().height`));
    check("再点一次取消过滤 → 恢复 16 行", await evalJs(`(() => {
      document.querySelector('.type-btn[data-kind="Image"]').click(); return true; })()`) && (await sleep(300), (await evalJs(`document.querySelectorAll('.row').length`)) === 16));

    // ---- 6c. 图片内嵌设置开关 ----
    await evalJs(`document.querySelector('#cfgShowImg').click()`);
    await sleep(300);
    check("关闭图片直显后无 img、回落文件名", await evalJs(`document.querySelectorAll('.row-img').length === 0 && document.querySelector('.row[data-id="7"] .preview').textContent.includes('截图')`));
    await evalJs(`document.querySelector('#cfgShowImg').click()`);
    await sleep(300);
    check("重新开启后恢复 img", await evalJs(`document.querySelectorAll('.row-img').length`) === 2);

    // ---- 6d. 始终显示设置 ----
    await evalJs(`document.querySelector('#btnClear').click()`);   // 清空后无记录
    await sleep(300);
    check("无记录时默认不显示类型按钮", await evalJs(`document.querySelector('#typeRail').hidden === true`));
    await evalJs(`document.querySelector('#cfgTypeAlways').click()`);
    await sleep(300);
    check("勾选始终显示 → 5 枚按钮仍在（含 HTML）", await evalJs(`document.querySelectorAll('.type-btn').length`) === 5);
    await evalJs(`document.querySelector('#cfgTypeAlways').click()`);
    await evalJs(`document.querySelector('#cfgTypeRail').click()`);  // 关掉整个过滤栏
    await sleep(300);
    check("关闭过滤栏开关 → 隐藏", await evalJs(`document.querySelector('#typeRail').hidden === true`));
    await evalJs(`document.querySelector('#cfgTypeRail').click()`);  // 恢复，供后续截图
    await sleep(200);

    // ---- 6e. Markdown 渲染 + 过滤栏位置 / 大小 ----
    await evalJs(`location.reload()`);
    await sleep(1200);
    check("Markdown 默认格式化渲染（标题/列表/引用/行内码）", await evalJs(`(() => {
      const body = document.querySelector('.row[data-id="16"] .md-body');
      return !!body && !!body.querySelector('.md-h1') && !!body.querySelector('.md-ul')
        && !!body.querySelector('.md-quote') && !!body.querySelector('code');
    })()`));
    check("Markdown 源码态可切（设置条开关）", await evalJs(`(() => {
      document.querySelector('#cfgMdRendered').click();
      return !document.querySelector('.row[data-id="16"] .md-body')
        && !!document.querySelector('.row[data-id="16"] .md-source');
    })()`));
    await sleep(300);
    check("切回渲染态", await evalJs(`(() => { document.querySelector('#cfgMdRendered').click(); return !!document.querySelector('.row[data-id="16"] .md-body'); })()`));
    await sleep(300);
    check("Markdown 行高 > 纯文本下限（渲染内容撑高）", await evalJs(`(() => {
      const h = document.querySelector('.row[data-id="16"]').getBoundingClientRect().height;
      return h > 87;
    })()`), "h=" + await evalJs(`document.querySelector('.row[data-id="16"]').getBoundingClientRect().height`));
    // 位置四向
    await evalJs(`(() => { const s = document.querySelector('#cfgRailPos'); s.value = 'left'; s.dispatchEvent(new Event('change', {bubbles:true})); })()`);
    await sleep(300);
    check("位置=左侧：栏在主窗口左边、间距 12px", await evalJs(`(() => {
      const rail = document.querySelector('#typeRail').getBoundingClientRect();
      const w = document.querySelector('#win').getBoundingClientRect();
      return Math.abs(w.left - rail.right - 12) < 0.6;
    })()`));
    await evalJs(`(() => { const s = document.querySelector('#cfgRailPos'); s.value = 'top'; s.dispatchEvent(new Event('change', {bubbles:true})); })()`);
    await sleep(300);
    check("位置=顶部：横向排在窗口上方、间距 12px", await evalJs(`(() => {
      const rail = document.querySelector('#typeRail').getBoundingClientRect();
      const w = document.querySelector('#win').getBoundingClientRect();
      return Math.abs(w.top - rail.bottom - 12) < 0.6 && rail.width > 600;
    })()`));
    await evalJs(`document.querySelector('#btnSearch').click()`);   // 顶部时搜索框应让位到栏上方
    await sleep(400);
    check("顶部栏 + 搜索展开：搜索框让位到栏上方", await evalJs(`(() => {
      const f = document.querySelector('#searchFloat').getBoundingClientRect();
      const rail = document.querySelector('#typeRail').getBoundingClientRect();
      return Math.abs(rail.top - f.bottom - 12) < 0.6;
    })()`));
    await evalJs(`(() => { const s = document.querySelector('#cfgRailPos'); s.value = 'bottom'; s.dispatchEvent(new Event('change', {bubbles:true})); })()`);
    await sleep(300);
    check("位置=底部：横向排在窗口下方、间距 12px", await evalJs(`(() => {
      const rail = document.querySelector('#typeRail').getBoundingClientRect();
      const w = document.querySelector('#win').getBoundingClientRect();
      return Math.abs(rail.top - w.bottom - 12) < 0.6;
    })()`));
    // 尺寸可调
    await evalJs(`(() => { const r = document.querySelector('#cfgRailSize'); r.value = '48'; r.dispatchEvent(new Event('input', {bubbles:true})); })()`);
    await sleep(300);
    check("图标大小 = 48px（可调 24~48）", await evalJs(`Math.abs(document.querySelector('.type-btn').getBoundingClientRect().width - 48) < 0.6`));
    await evalJs(`(() => { const s = document.querySelector('#cfgRailPos'); s.value = 'right'; s.dispatchEvent(new Event('change', {bubbles:true}));
                        const r = document.querySelector('#cfgRailSize'); r.value = '32'; r.dispatchEvent(new Event('input', {bubbles:true})); })()`);
    await sleep(300);

    // ---- 5c. 双击跳转：链接 → 浏览器 / 文件 → 资源管理器 ----
    await evalJs(`location.reload()`);
    await sleep(1200);
    await evalJs(`document.querySelector('.row[data-id="1"]').dispatchEvent(new MouseEvent('dblclick',{bubbles:true}))`);
    await sleep(300);
    check("双击链接行 → 默认浏览器 toast", await evalJs(`document.querySelector('#toast').textContent.startsWith('已用默认浏览器打开：https://')`));
    await evalJs(`document.querySelector('.row[data-id="2"]').dispatchEvent(new MouseEvent('dblclick',{bubbles:true}))`);
    await sleep(300);
    check("双击文件行 → 资源管理器 toast", await evalJs(`document.querySelector('#toast').textContent.startsWith('已在资源管理器中打开：D:')`));
    await evalJs(`(() => { const i = document.querySelector('#cfgBrowserPath'); i.value = 'C:\\\\Program Files\\\\Google\\\\Chrome\\\\Application\\\\chrome.exe'; i.dispatchEvent(new Event('input',{bubbles:true})); })()`);
    await evalJs(`document.querySelector('.row[data-id="1"]').dispatchEvent(new MouseEvent('dblclick',{bubbles:true}))`);
    await sleep(300);
    check("自定义浏览器路径 → toast 显示 chrome.exe", await evalJs(`document.querySelector('#toast').textContent.startsWith('已调用 chrome.exe 打开')`));
    await evalJs(`document.querySelector('.row[data-id="3"]').dispatchEvent(new MouseEvent('dblclick',{bubbles:true}))`);
    await sleep(300);
    check("普通文本双击仍为复制", await evalJs(`document.querySelector('#toast').textContent === '已复制到剪贴板'`));

    // ---- 7. 窗口置顶切换 ----
    await evalJs(`document.querySelector('#btnPin').click()`);
    await sleep(200);
    check("取消窗口置顶 → 描边图钉 + text_dim", await evalJs(`document.querySelector('#btnPin svg').getAttribute('fill') === 'none'`));
    check("置顶 toast「窗口置顶已关闭」", await evalJs(`document.querySelector('#toast').textContent === '窗口置顶已关闭'`));

    // ---- 8. 清空全部 ----
    await evalJs(`location.reload()`);                  // 6d 已清空过列表，先恢复初始数据
    await sleep(1200);
    await evalJs(`document.querySelector('#btnClear').click()`);
    await sleep(200);
    check("清空后 0 行 + 空态文案", await evalJs(`document.querySelectorAll('.row').length === 0 && !document.querySelector('#empty').hidden && document.querySelector('#empty').textContent === '还没有剪贴板记录'`));
    check("清空 toast", await evalJs(`document.querySelector('#toast').textContent.includes('已清空全部历史')`));

    // ---- 9. 关闭到托盘 → 托盘恢复 ----
    await evalJs(`document.querySelector('#btnClose').click()`);
    await sleep(300);
    check("关闭到托盘：窗口隐藏、托盘出现", await evalJs(`document.querySelector('#win').classList.contains('hidden') && !document.querySelector('#tray').hidden`));
    await evalJs(`document.querySelector('#tray').click()`);
    await sleep(300);
    check("点托盘恢复窗口", await evalJs(`!document.querySelector('#win').classList.contains('hidden') && document.querySelector('#tray').hidden`));

    // ---- 10. 悬停提示（tooltip） ----
    await send("Runtime.evaluate", { expression: `document.querySelector('#btnSettings').dispatchEvent(new MouseEvent('mouseover',{bubbles:true}))` });
    await sleep(500);
    check("悬停设置按钮显示提示「设置」", await evalJs(`getComputedStyle(document.querySelector('#tooltip')).display === 'block' && document.querySelector('#tooltip').textContent === '设置'`));

    // ---- 11. 几何审计：关键坐标 vs Rust 公式导出值（先恢复初始数据） ----
    await evalJs(`location.reload()`);
    await sleep(1200);
    await evalJs(`document.querySelector('#btnSearch').click()`);   // 展开悬浮搜索框
    await sleep(400);
    const geo = await evalJs(`(() => {
      const W = document.querySelector('#win').getBoundingClientRect();
      const rel = (el) => { const r = el.getBoundingClientRect();
        return { x: +(r.left - W.left).toFixed(2), y: +(r.top - W.top).toFixed(2),
                 w: +r.width.toFixed(2), h: +r.height.toFixed(2),
                 cx: +(r.left + r.width/2 - W.left).toFixed(2), cy: +(r.top + r.height/2 - W.top).toFixed(2) }; };
      const row = document.querySelector('.row');
      const btns = [...document.querySelectorAll('.btn-area .tbtn')].map(rel);
      const rowBtns = [...document.querySelectorAll('.row .acts .abtn')].map(rel);
      const badge = rel(document.querySelector('.row .badge'));
      return JSON.stringify({
        topbar: rel(document.querySelector('.topbar')),
        btns, rowBtns, badge,
        search: rel(document.querySelector('#btnSearch')),
        searchFloat: rel(document.querySelector('#searchFloat')),
        typeRail: rel(document.querySelector('#typeRail')),
        row: rel(row),
        metaTop: rel(document.querySelector('.row .meta-top')),
        metaBot: rel(document.querySelector('.row .meta-bot')),
        metaBotFS: getComputedStyle(document.querySelector('.row .meta-bot')).fontSize,
        preview: rel(document.querySelector('.row .preview')),
        previewLH: getComputedStyle(document.querySelector('.row .preview')).lineHeight,
        previewFS: getComputedStyle(document.querySelector('.row .preview')).fontSize,
        rowRadius: getComputedStyle(document.querySelector('.row')).borderRadius,
      });
    })()`);
    const g = JSON.parse(geo);
    const near = (a, b, tol = 0.6) => Math.abs(a - b) <= tol;
    check("顶栏高 66px", near(g.topbar.h, 66));
    check("四按钮热区 36.6px / 槽心 x=495.5,554.5,613.5,672.5",
      g.btns.every((b, i) => near(b.w, 36.6) && near(b.cx, [495.5, 554.5, 613.5, 672.5][i])));
    check("搜索图标中心 (435.3, 33)", near(g.search.cx, 435.3) && near(g.search.cy, 33) && near(g.search.w, 25.4));
    check("悬浮搜索框：宽 720（与窗口对齐）、底部间距 12px、高 44",
      near(g.searchFloat.w, 720) && near(g.searchFloat.x, 0) &&
      near(g.searchFloat.y + g.searchFloat.h, -12) && near(g.searchFloat.h, 44),
      `w=${g.searchFloat.w} x=${g.searchFloat.x} bottom=${(g.searchFloat.y + g.searchFloat.h).toFixed(1)}`);
    check("类型过滤栏：右侧间距 12px、垂直居中、方块 32px",
      near(g.typeRail.x, 732) && near(g.typeRail.y + g.typeRail.h / 2, 400) &&
      near(g.typeRail.w, 32),
      `x=${g.typeRail.x} cy=${(g.typeRail.y + g.typeRail.h / 2).toFixed(1)} w=${g.typeRail.w}`);
    check("行左缘 x=36 / 宽 633 / 圆角 18", near(g.row.x, 36) && near(g.row.w, 633) && g.rowRadius === "18px");
    check("行内三按钮 23.1px、间距 6px、删除中心 x=639.45",
      g.rowBtns.every(b => near(b.w, 23.1)) && near(g.rowBtns[1].cx - g.rowBtns[0].cx, 29.1) && near(g.rowBtns[2].cx, 639.45));
    check("meta-top 左 54 / 右缘 563.7（让出按钮列）", near(g.metaTop.x, 54) && near(g.metaTop.x + g.metaTop.w, 563.7));
    check("预览字号 25.5 / 行高 34.425", g.previewFS === "25.5px" && g.previewLH === "34.425px");
    check("信息组右缘距行右 18px / 徽标 24px / 字号 21px",
      near(g.row.x + g.row.w - (g.metaBot.x + g.metaBot.w), 18) && near(g.badge.w, 24) && g.metaBotFS === "21px",
      "metaBot.right=" + (g.metaBot.x + g.metaBot.w));

    // ---- 12. 截图：选中 + 搜索展开 + toast 同框 ----
    await evalJs(`location.reload()`);
    await sleep(1200);
    await evalJs(`document.querySelectorAll('.row')[3].click()`);
    await evalJs(`document.querySelector('#btnSearch').click()`);
    await evalJs(`(()=>{const i=document.querySelector('#searchInput'); i.value='Modular'; i.dispatchEvent(new Event('input',{bubbles:true}));})()`);
    await sleep(400);
    await shot("shot-interaction.png");

    // ---- 12. 截图：悬停行内按钮（danger 删除） ----
    await evalJs(`location.reload()`);
    await sleep(1200);
    const box = await evalJs(`(()=>{const b=document.querySelector('.row .abtn[data-act=del]'); const r=b.getBoundingClientRect(); return JSON.stringify({x:r.x+r.width/2,y:r.y+r.height/2});})()`);
    const p = JSON.parse(box);
    await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: p.x, y: p.y });
    await sleep(400);
    check("悬停删除按钮 → danger 色", await evalJs(`document.querySelector('.row .abtn[data-act=del] svg').style.stroke === 'var(--danger)'`));
    await shot("shot-hover-danger.png");

    // ---- 13. 截图：滚动条显形 ----
    await evalJs(`location.reload()`);
    await sleep(1200);
    await shot("shot-scrollbar.png");

    const fails = results.filter((r) => !r.ok).length;
    console.log(`\n===== ${results.length - fails}/${results.length} 通过 =====`);
    if (fails) process.exitCode = 1;
  } finally {
    try { await send("Browser.close").catch(() => {}); } catch (_) {}
    chrome.kill();
  }
}
main().catch((e) => { console.error("ERROR:", e.message); process.exit(1); });
