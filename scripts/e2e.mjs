// 端到端验证:起两路 agent(各带一个新开的 gametest)和一个无头浏览器,每路开一个页面,
// 用 CDP 发键鼠,再从 gametest 的窗口标题读它各自看到的状态,确认两路输入互不干扰。
// 最后一个阶段验证声音:A 的 gametest 放 440 Hz、B 的放 880 Hz,每个页面只能听到自己那路。
// 用法: cargo build --workspace 之后  node scripts/e2e.mjs
// 产物在 target/e2e/:每路 agent 的日志和浏览器里最后一帧画面。
import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { Cdp, until, waitHttp } from './cdp.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const bin = path.join(root, 'target', 'debug');
const out = path.join(root, 'target', 'e2e');
mkdirSync(out, { recursive: true });

const PORTS = [8091, 8092];
// 每路的 gametest 放的单音(Hz),和 PORTS 一一对应
const TONES = [440, 880];
const DEBUG_PORT = 9333;
const BROWSERS = [
  'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe',
  'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
];

let failures = 0;
function check(name, ok, detail = '') {
  if (!ok) failures++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? '  ' + detail : ''}`);
}

// 起一路 agent:新开一个 gametest(循环放 tone 赫兹的单音)并装输入隔离。
// stdin 保持管道,关掉它 agent 会自己退出
function startAgent(port, tone) {
  const log = openSync(path.join(out, `agent-${port}.log`), 'w');
  const child = spawn(
    path.join(bin, 'agent.exe'),
    ['--app', 'gametest', '--new', '--hook', '--port', String(port), '--exit-on-stdin-close', '--', '--tone', String(tone)],
    { stdio: ['pipe', log, log], windowsHide: true },
  );
  child.exited = new Promise((resolve) => child.once('exit', resolve));
  return child;
}

// 从 agent 日志里取它启动的 gametest 进程号
function gametestPid(port) {
  // 日志可能带 ANSI 颜色码,先去掉
  const text = readFileSync(path.join(out, `agent-${port}.log`), 'utf8').replace(/\x1b\[[0-9;]*m/g, '');
  const m = text.match(/已启动 .*gametest\.exe \(进程 (\d+)\)/);
  return m ? Number(m[1]) : null;
}

// 读每个 gametest 进程的窗口标题。标题是纯 ASCII,其余列按 latin1 解码即可,不会被本地化文字弄乱
function titles() {
  const csv = execFileSync('tasklist', ['/v', '/fo', 'csv', '/nh', '/fi', 'imagename eq gametest.exe'], {
    encoding: 'latin1',
  });
  const map = new Map();
  for (const line of csv.split(/\r?\n/)) {
    // "映像名","PID","会话名","会话#","内存","状态","用户名","CPU 时间","窗口标题"
    const m = line.match(/^"[^"]*","(\d+)",.*,"(gametest [^"]*)"\s*$/);
    if (m) map.set(Number(m[1]), m[2]);
  }
  return map;
}

function parseTitle(t) {
  const m = t?.match(
    /msgs=(\d+) key=\[([^\]]*)\] async=\[([^\]]*)\] raw=(\d+) rawsum=\((-?\d+),(-?\d+)\) fg=(\d) act=(\d)(\d) cursor=\((-?\d+),(-?\d+)\)/,
  );
  if (!m) return null;
  const set = (s) => new Set(s ? s.split(',') : []);
  return {
    msgs: +m[1], key: set(m[2]), async: set(m[3]), raw: +m[4],
    rawsum: [+m[5], +m[6]], fg: +m[7], act: +m[8], app: +m[9], cursor: [+m[10], +m[11]],
  };
}

const state = (pid) => parseTitle(titles().get(pid));
// 没被碰过:Raw Input 事件数、位移、虚拟光标都不变。窗口消息数不比较:
// 主机真光标恰好停在窗口上时,窗口会收到真实的 WM_MOUSEMOVE,钩子不拦窗口消息
const untouched = (a, b) =>
  JSON.stringify([a.raw, a.rawsum, a.cursor]) === JSON.stringify([b.raw, b.rawsum, b.cursor]);
const fmt = (s) =>
  s && `msgs=${s.msgs} key=[${[...s.key]}] async=[${[...s.async]}] raw=${s.raw} rawsum=${s.rawsum} fg=${s.fg} act=${s.act}${s.app} cursor=${s.cursor}`;

// 一个页面 = 一路:CDP 会话 + 它对应的 gametest 进程
async function openPage(browser, port, pid) {
  const { targetId } = await browser.send('Target.createTarget', {
    url: `http://127.0.0.1:${port}/`, newWindow: true, width: 1100, height: 760,
  });
  const meta = await (await fetch(`http://127.0.0.1:${DEBUG_PORT}/json/list`)).json();
  const wsUrl = meta.find((t) => t.id === targetId).webSocketDebuggerUrl;
  const page = await Cdp.connect(wsUrl);
  await page.send('Page.enable');
  // 无头模式下没有真实焦点;让页面认为自己始终有焦点,键盘事件才会送到隐藏输入框
  await page.send('Emulation.setFocusEmulationEnabled', { enabled: true });
  return { port, pid, page };
}

const rect = (p) =>
  p.page.eval(`(() => { const r = document.getElementById('video').getBoundingClientRect();
    return { x: r.left, y: r.top, w: r.width, h: r.height }; })()`);

const mouse = (p, type, x, y, extra = {}) =>
  p.page.send('Input.dispatchMouseEvent', { type, x, y, ...extra });

const VK = { W: 0x57, D: 0x44 };
const key = (p, type, name) =>
  p.page.send('Input.dispatchKeyEvent', {
    type, key: name.toLowerCase(), code: `Key${name}`,
    windowsVirtualKeyCode: VK[name], nativeVirtualKeyCode: VK[name],
  });

// 在视频区域内沿一条折线移动鼠标,每步停一下让不可靠通道把点送出去
async function sweep(p, r, fractions) {
  for (const [fx, fy] of fractions) {
    await mouse(p, 'mouseMoved', r.x + r.w * fx, r.y + r.h * fy);
    await sleep(40);
  }
}

// 把页面里 <video> 当前这一帧存成 PNG,看整条链路(采集、编码、WebRTC、解码)是不是通的
async function saveFrame(p, name) {
  const url = await p.page.eval(`(() => { const v = document.getElementById('video');
    const c = document.createElement('canvas'); c.width = v.videoWidth; c.height = v.videoHeight;
    c.getContext('2d').drawImage(v, 0, 0); return c.toDataURL('image/png'); })()`);
  writeFileSync(path.join(out, name), Buffer.from(url.split(',')[1], 'base64'));
}

// 页面里 audio 的 inbound-rtp 统计
const audioStats = (p) =>
  p.page.eval(`(async () => {
    const report = await pc.getStats();
    let a = null;
    report.forEach((s) => { if (s.type === 'inbound-rtp' && s.kind === 'audio') a = s; });
    return a && { packets: a.packetsReceived, bytes: a.bytesReceived, level: a.audioLevel, lost: a.packetsLost };
  })()`);

// 给 <audio> 的流接一个 AnalyserNode(fftSize 8192),等 1 秒取频谱峰值。
// 频谱全是 -Infinity 说明没拿到数据:finite 为 false,原样上报,不当成通过
const spectrumPeak = (p) =>
  p.page.eval(`(async () => {
    const stream = document.getElementById('audio').srcObject;
    if (!stream) return { error: 'audio.srcObject 是空的' };
    const ctx = new AudioContext();
    await ctx.resume();
    const analyser = ctx.createAnalyser();
    analyser.fftSize = 8192;
    analyser.smoothingTimeConstant = 0;
    ctx.createMediaStreamSource(stream).connect(analyser);
    await new Promise((r) => setTimeout(r, 1000));
    const bins = new Float32Array(analyser.frequencyBinCount);
    analyser.getFloatFrequencyData(bins);
    let peak = 0;
    for (let i = 1; i < bins.length; i++) if (bins[i] > bins[peak]) peak = i;
    const out = {
      hz: peak * ctx.sampleRate / analyser.fftSize,
      db: Number.isFinite(bins[peak]) ? bins[peak] : null,
      finite: bins.some((v) => Number.isFinite(v)),
      state: ctx.state,
      rate: ctx.sampleRate,
    };
    await ctx.close();
    return out;
  })()`);

const agents = PORTS.map((port, i) => startAgent(port, TONES[i]));
const pids = [];
const pages = [];
let browser = null;
let browserProc = null;
let browserDir = null;

try {
  for (const port of PORTS) await waitHttp(`http://127.0.0.1:${port}/health`);
  for (const port of PORTS) pids.push(await until(`端口 ${port} 的 gametest 进程号`, () => gametestPid(port), 20000));
  for (const pid of pids) await until(`进程 ${pid} 的窗口标题`, () => state(pid), 10000);
  console.log(`agent 端口 ${PORTS.join(',')}  gametest 进程 ${pids.join(',')}`);

  const exe = BROWSERS.find(existsSync);
  if (!exe) throw new Error('没找到 Edge 或 Chrome');
  browserDir = mkdtempSync(path.join(os.tmpdir(), 'cr-e2e-'));
  browserProc = spawn(exe, [
    '--headless=new', `--remote-debugging-port=${DEBUG_PORT}`, '--remote-allow-origins=*',
    `--user-data-dir=${browserDir}`, '--no-first-run', '--no-default-browser-check',
    '--disable-background-timer-throttling', '--disable-renderer-backgrounding',
    '--disable-backgrounding-occluded-windows', '--autoplay-policy=no-user-gesture-required',
    'about:blank',
  ], { stdio: 'ignore', windowsHide: true });
  const version = JSON.parse(await waitHttp(`http://127.0.0.1:${DEBUG_PORT}/json/version`));
  browser = await Cdp.connect(version.webSocketDebuggerUrl);
  for (let i = 0; i < PORTS.length; i++) pages.push(await openPage(browser, PORTS[i], pids[i]));

  for (const p of pages) {
    const get = (js) => p.page.eval(js);
    await until(`页面 ${p.port} 连上`, async () => (await get('window.pc && window.pc.connectionState')) === 'connected', 30000);
    await until(`页面 ${p.port} 出画面`, async () => (await get(`document.getElementById('video').videoWidth`)) > 0, 15000);
    const vis = await get('document.visibilityState');
    check(`页面 ${p.port}: 处于可见状态`, vis === 'visible', vis);
    const hud = await until(`页面 ${p.port} 的 HUD`, async () => {
      const t = await get(`document.getElementById('hud').textContent`);
      return t.includes('输入隔离') && t;
    }, 10000);
    check(`页面 ${p.port}: HUD 显示输入隔离已启用`, hud.includes('输入隔离: 已启用'), JSON.stringify(hud));
  }

  // 阶段 1:只操作 A,B 一动不动
  const [A, B] = pages;
  const wait = (desc, fn, ms = 4000) => until(desc, fn, ms).catch(() => null);
  const [rA, rB] = await Promise.all([rect(A), rect(B)]);
  const fgs = pids.map(state);
  check('两路同时报告自己在前台(GetForegroundWindow 被接管)', fgs.every((s) => s?.fg === 1), fgs.map(fmt).join(' | '));
  const a0 = state(A.pid);
  const b0 = state(B.pid);
  const [ax, ay] = [rA.x + rA.w * 0.6, rA.y + rA.h * 0.5];
  await sweep(A, rA, [[0.2, 0.2], [0.4, 0.3], [0.6, 0.5]]);
  await mouse(A, 'mousePressed', ax, ay, { button: 'left', buttons: 1, clickCount: 1 });
  await key(A, 'rawKeyDown', 'W');
  const held = await wait('A 看到 W 和左键', () => {
    const s = state(A.pid);
    return s?.async.has('W') && s.async.has('LMB') && s;
  });
  check('A: GetAsyncKeyState 看到 W 和左键', !!held, fmt(held ?? state(A.pid)));
  check('A: GetKeyState 也看到 W', !!held?.key.has('W'));
  const bMid = state(B.pid);
  check('B: 此时一个键都没有', bMid.key.size === 0 && bMid.async.size === 0, fmt(bMid));
  await key(A, 'keyUp', 'W');
  await mouse(A, 'mouseReleased', ax, ay, { button: 'left', buttons: 0, clickCount: 1 });
  const released = await wait('A 松开后按键清空', () => {
    const s = state(A.pid);
    return s && s.async.size === 0 && s.key.size === 0 && s;
  });
  check('A: 松开后按键清空', !!released, fmt(released ?? state(A.pid)));
  const a1 = state(A.pid);
  check('A: 收到窗口消息和 Raw Input 事件', a1.msgs > a0.msgs && a1.raw > a0.raw, `msgs ${a0.msgs}→${a1.msgs} raw ${a0.raw}→${a1.raw}`);
  check('A: Raw Input 有相对位移', a1.rawsum.some((v, i) => v !== a0.rawsum[i]), `${a0.rawsum}→${a1.rawsum}`);
  check('A: 虚拟光标动了', a1.cursor.some((v, i) => v !== a0.cursor[i]), `${a0.cursor}→${a1.cursor}`);
  // 焦点伪装:agent 在会话第一次输入时补发 WM_ACTIVATEAPP / WM_ACTIVATE / WM_SETFOCUS
  check('A: 收到补发的激活消息(焦点伪装)', a1.act === 1 && a1.app === 1, `act=${a1.act} app=${a1.app}`);
  const b1 = state(B.pid);
  check('B: Raw Input 和虚拟光标都没变', untouched(b0, b1), `${fmt(b0)}  →  ${fmt(b1)}`);

  // 阶段 2:只操作 B,A 一动不动
  const [bx, by] = [rB.x + rB.w * 0.4, rB.y + rB.h * 0.6];
  await sweep(B, rB, [[0.7, 0.7], [0.5, 0.6], [0.4, 0.6]]);
  await mouse(B, 'mousePressed', bx, by, { button: 'left', buttons: 1, clickCount: 1 });
  await key(B, 'rawKeyDown', 'D');
  const heldB = await wait('B 看到 D 和左键', () => {
    const s = state(B.pid);
    return s?.async.has('D') && s.async.has('LMB') && s;
  });
  check('B: GetAsyncKeyState 看到 D 和左键', !!heldB, fmt(heldB ?? state(B.pid)));
  const aMid = state(A.pid);
  check('A: 此时一个键都没有', aMid.key.size === 0 && aMid.async.size === 0, fmt(aMid));
  await key(B, 'keyUp', 'D');
  await mouse(B, 'mouseReleased', bx, by, { button: 'left', buttons: 0, clickCount: 1 });
  await wait('B 松开后按键清空', () => {
    const s = state(B.pid);
    return s && s.async.size === 0 && s.key.size === 0;
  });
  const b2 = state(B.pid);
  check('B: 收到 Raw Input 事件和位移', b2.raw > b1.raw && b2.rawsum.some((v, i) => v !== b1.rawsum[i]), `raw ${b1.raw}→${b2.raw} ${b1.rawsum}→${b2.rawsum}`);
  check('B: 收到补发的激活消息(焦点伪装)', b2.act === 1 && b2.app === 1, `act=${b2.act} app=${b2.app}`);
  const a2 = state(A.pid);
  check('A: Raw Input 和虚拟光标都没变', untouched(a1, a2), `${fmt(a1)}  →  ${fmt(a2)}`);

  // 阶段 3:A 按 W、B 按 D,同时按住,各自只能看到自己的键
  await key(A, 'rawKeyDown', 'W');
  await key(B, 'rawKeyDown', 'D');
  const both = await wait('两路各自看到自己的键', () => {
    const a = state(A.pid);
    const b = state(B.pid);
    return a?.async.has('W') && b?.async.has('D') && { a, b };
  });
  check('A 只有 W', !!both && [...both.a.async].join() === 'W' && [...both.a.key].join() === 'W', fmt(both?.a));
  check('B 只有 D', !!both && [...both.b.async].join() === 'D' && [...both.b.key].join() === 'D', fmt(both?.b));
  await key(A, 'keyUp', 'W');
  await key(B, 'keyUp', 'D');
  await wait('两路松开', () => state(A.pid)?.async.size === 0 && state(B.pid)?.async.size === 0);

  // 阶段 4:Pointer Lock 的相对位移。无头浏览器里拿不到真实的 Pointer Lock,
  // 所以绕过页面的锁定判断,直接往 input-fast 通道发相对消息,验证 agent 这一段:
  // 虚拟光标按增量累加、Raw Input 就是浏览器给的增量,而且不依赖窗口消息回中的坐标
  const rawA = state(A.pid);
  await until('input-fast 通道就绪', () => A.page.eval('window.__channels && window.__channels.fast.readyState === "open"'));
  // seq 必须比页面已经用过的都大,否则 agent 当作乱序旧消息丢弃
  const sent = await A.page.eval(`(() => {
    const m = window.__channels; const ks = [10, 10, -3];
    m.seq += 1000;
    ks.forEach((d) => { m.fast.send(JSON.stringify({ type: 'mouse_move', seq: ++m.seq, dx: d, dy: 0, rel: true, x: 0.5, y: 0.5 })); });
    return ks.reduce((a, b) => a + b, 0);
  })()`);
  check('相对模式: 测试脚本发出非零增量', sent !== 0, `sent=${sent}`);
  const rel = await wait('相对位移落到 gametest', () => {
    const s = state(A.pid);
    return s && s.rawsum[0] - rawA.rawsum[0] === sent && s;
  });
  check('相对模式: Raw Input 位移等于浏览器给的增量', !!rel, `rawsum ${rawA.rawsum}→${rel?.rawsum} 期望 +${sent}`);
  // 虚拟光标是屏幕坐标(客户区位置 + 客户区原点),和绝对模式下的基准不同,
  // 所以只验证相对阶段内部:再发一组纯正增量,光标应恰好增加这个量
  const beforeRel = state(A.pid);
  const more = 40;
  await A.page.eval(`(() => { const m = window.__channels;
    m.seq += 50; m.fast.send(JSON.stringify({ type: 'mouse_move', seq: ++m.seq, dx: ${more}, dy: 0, rel: true, x: 0.5, y: 0.5 })); })()`);
  const relHeld = await wait('相对模式下虚拟光标跟着走', () => {
    const s = state(A.pid);
    return s && s.cursor[0] - beforeRel.cursor[0] === more && s;
  }, 3000).catch(() => null);
  const after = state(A.pid);
  check('相对模式: 虚拟光标按增量累加(不被窗口消息重置)', !!relHeld,
    `cursor ${beforeRel.cursor}→${after?.cursor} 期望 x +${more}`);

  // 阶段 5:声音。A 的 gametest 放 440 Hz、B 的放 880 Hz;两个页面各自只能听到自己那一路,
  // 频谱峰值对得上就证明多开时声音是分开的
  for (const [i, p] of pages.entries()) {
    const want = TONES[i];
    const label = `页面 ${p.port}`;
    const s1 = await audioStats(p);
    await sleep(1000);
    const s2 = await audioStats(p);
    const got = s1 && s2 ? s2.packets - s1.packets : null;
    check(`${label}: 1 秒内收到音频包 > 50`, got > 50, `packetsReceived ${s1?.packets}→${s2?.packets}`);
    check(`${label}: audioLevel > 0.01`, s2?.level > 0.01, `audioLevel=${s2?.level}`);
    const sp = await spectrumPeak(p);
    const detail = sp.error ?? (sp.finite
      ? `峰值 ${sp.hz.toFixed(1)} Hz ${sp.db?.toFixed(1)} dB(期望 ${want}±15,AudioContext ${sp.state} ${sp.rate} Hz)`
      : `AnalyserNode 的频谱全是 -Infinity,没有拿到音频数据(AudioContext ${sp.state} ${sp.rate} Hz)`);
    check(`${label}: 频谱峰值在 ${want}±15 Hz`, !!sp.finite && Math.abs(sp.hz - want) <= 15, detail);
  }

  // 整条链路(采集、编码、WebRTC、解码)最后一帧画面,留着人工看
  for (const [i, p] of pages.entries()) await saveFrame(p, `frame-${PORTS[i]}.png`);

  // 停掉 agent:关 stdin 让它优雅退出,钩子随之撤销,gametest 回到真实输入
  for (const a of agents) a.stdin.end();
  const exits = await Promise.all(agents.map((a) => Promise.race([a.exited, sleep(8000).then(() => 'timeout')])));
  check('agent 在 8 秒内优雅退出', exits.every((c) => c === 0), exits.join(','));
  const alive = pids.map((pid) => titles().has(pid));
  check('agent 退出后 gametest 还活着', alive.every(Boolean), alive.join(','));
  // 钩子撤销后 GetForegroundWindow 回到真实值,两个窗口不可能同时在前台
  const detached = await wait('钩子撤销后前台状态恢复真实', () => {
    const s = pids.map(state);
    return s.every(Boolean) && s[0].fg + s[1].fg <= 1 && s;
  }, 5000);
  check('钩子撤销后不再有两个窗口同时在前台', !!detached, pids.map((pid) => fmt(state(pid))).join(' | '));
} catch (e) {
  failures++;
  console.log(`FAIL  脚本异常: ${e.message}`);
} finally {
  for (const p of pages) p.page.close();
  browser?.close();
  const kill = (pid, tree = false) => {
    try {
      execFileSync('taskkill', ['/pid', String(pid), ...(tree ? ['/t'] : []), '/f'], { stdio: 'ignore' });
    } catch {}
  };
  if (browserProc?.pid) kill(browserProc.pid, true);
  for (const a of agents) if (a.exitCode === null) kill(a.pid);
  for (const pid of pids) kill(pid);
  await sleep(500);
  if (browserDir) rmSync(browserDir, { recursive: true, force: true, maxRetries: 3 });
  if (failures) {
    for (const port of PORTS) {
      const log = readFileSync(path.join(out, `agent-${port}.log`), 'utf8');
      console.log(`--- agent-${port}.log 末尾 ---\n${log.split('\n').slice(-25).join('\n')}`);
    }
  }
  console.log(failures ? `\n${failures} 项失败` : '\n全部通过');
  process.exit(failures ? 1 : 0);
}
