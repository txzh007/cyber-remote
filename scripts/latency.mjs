// 延迟测量(PLAN 0.8):起一个 agent(新开 gametest --latency)和一个浏览器,打开 /?latency=N,
// 跑完后打印各环节的 p50/p95/最大值,完整结果存到 target/latency/。
// 用法: cargo build --release -p agent -p gametest 之后
//   node scripts/latency.mjs [--count 200] [--debug] [--headless] [--no-playout-delay] [--display 2 [--display-capture window]]
//                            [--hook [--no-audio]]
// --hook 给 agent 加输入隔离(也就是开声音推流);--no-audio 设 CYBER_REMOTE_AUDIO=off,
// 只关声音,做声音对延迟影响的 A/B 对照。
// 要真实数字就用 release 构建和有头浏览器:debug 构建慢,无头模式可能不走硬件解码和真实合成。
import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { Cdp, findBrowser, until, waitHttp } from './cdp.mjs';

const argv = process.argv.slice(2);
const arg = (name, def) => {
  const i = argv.indexOf(name);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : def;
};
const COUNT = Number(arg('--count', 200));
const PROFILE = argv.includes('--debug') ? 'debug' : 'release';
const HEADLESS = argv.includes('--headless');
const PLAYOUT = !argv.includes('--no-playout-delay');
const HOOK = argv.includes('--hook');
const NO_AUDIO = argv.includes('--no-audio');
// 整屏采集:gametest 铺满这块屏(\\.\DISPLAYn)
const DISPLAY = arg('--display', null);
// 对照:--display-capture window 时窗口照样铺满那块屏,但按窗口采集(WGC)
const DISPLAY_CAPTURE = arg('--display-capture', null);
const PORT = 8093;
const DEBUG_PORT = 9334;

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const bin = path.join(root, 'target', PROFILE);
const out = path.join(root, 'target', 'latency');
mkdirSync(out, { recursive: true });
for (const f of ['agent.exe', 'gametest.exe']) {
  if (!existsSync(path.join(bin, f))) {
    console.error(`找不到 ${path.join(bin, f)},先 cargo build${PROFILE === 'release' ? ' --release' : ''} -p agent -p gametest`);
    process.exit(1);
  }
}

const logPath = path.join(out, 'agent.log');
const log = openSync(logPath, 'w');
const agent = spawn(
  path.join(bin, 'agent.exe'),
  ['--app', 'gametest', '--new', '--port', String(PORT), '--exit-on-stdin-close',
    ...(PLAYOUT ? [] : ['--no-playout-delay']), ...(HOOK ? ['--hook'] : []),
    ...(DISPLAY ? ['--display', DISPLAY] : []), '--', '--latency'],
  {
    stdio: ['pipe', log, log], windowsHide: true,
    env: {
      ...process.env,
      ...(DISPLAY_CAPTURE ? { CYBER_REMOTE_DISPLAY_CAPTURE: DISPLAY_CAPTURE } : {}),
      ...(NO_AUDIO ? { CYBER_REMOTE_AUDIO: 'off' } : {}),
    },
  },
);
const agentExit = new Promise((resolve) => agent.once('exit', resolve));
const kill = (pid, tree = false) => {
  try {
    execFileSync('taskkill', ['/pid', String(pid), ...(tree ? ['/t'] : []), '/f'], { stdio: 'ignore' });
  } catch {}
};
// 中文字符按两格宽对齐
const pad = (s, n) => s + ' '.repeat(Math.max(0, n - [...s].reduce((w, c) => w + (c.charCodeAt(0) > 0x2e80 ? 2 : 1), 0)));

let gametest = null;
let browserProc = null;
let browserDir = null;
let page = null;
let code = 1;
try {
  await waitHttp(`http://127.0.0.1:${PORT}/health`);
  gametest = await until('gametest 进程号', () => {
    const text = readFileSync(logPath, 'utf8').replace(/\x1b\[[0-9;]*m/g, '');
    const m = text.match(/gametest\.exe \(进程 (\d+)\)/);
    return m && Number(m[1]);
  }, 20000);

  browserDir = mkdtempSync(path.join(os.tmpdir(), 'cr-latency-'));
  browserProc = spawn(findBrowser(), [
    ...(HEADLESS ? ['--headless=new'] : []),
    `--remote-debugging-port=${DEBUG_PORT}`, '--remote-allow-origins=*', `--user-data-dir=${browserDir}`,
    '--no-first-run', '--no-default-browser-check', '--autoplay-policy=no-user-gesture-required',
    '--disable-background-timer-throttling', '--disable-renderer-backgrounding',
    '--disable-backgrounding-occluded-windows', '--window-size=1280,860', '--window-position=0,0',
    `http://127.0.0.1:${PORT}/?latency=${COUNT}`,
  ], { stdio: 'ignore' });
  const target = await until('浏览器页面', async () => {
    try {
      const list = await (await fetch(`http://127.0.0.1:${DEBUG_PORT}/json/list`)).json();
      return list.find((t) => t.type === 'page' && t.url.startsWith(`http://127.0.0.1:${PORT}/`));
    } catch {
      return null;
    }
  }, 20000);
  page = await Cdp.connect(target.webSocketDebuggerUrl);
  // 首次运行的浏览器可能另开欢迎页,测量页被挡在后台就不出帧回调
  await page.send('Page.bringToFront');
  console.log(`agent 端口 ${PORT}  gametest 进程 ${gametest}  ${PROFILE} 构建${HEADLESS ? '  无头' : ''}  playout-delay ${PLAYOUT ? '开' : '关'}  hook ${HOOK ? '开' : '关'}${HOOK ? `  声音 ${NO_AUDIO ? '关' : '开'}` : ''}${DISPLAY ?`  铺满 DISPLAY${DISPLAY}${DISPLAY_CAPTURE === 'window' ? ' 按窗口采集' : ' 桌面复制'}` : ''}  测 ${COUNT} 次`);

  const result = await until('测量完成', () => page.eval(
    'window.__latencyError ? { error: window.__latencyError } : (window.__latencyResult || null)',
  ), COUNT * 3000 + 60000);
  if (result.error) throw new Error(result.error);

  console.log(`\n成功 ${result.ok}/${result.count},主机回报 ${result.matched}`);
  console.log(`${pad('环节', 18)}${'p50'.padStart(8)}${'p95'.padStart(8)}${'最大'.padStart(7)}   (毫秒)`);
  for (const [k, label] of result.stages) {
    const s = result.stats[k];
    if (!s) continue;
    console.log(`${pad(label, 18)}${s.p50.toFixed(1).padStart(8)}${s.p95.toFixed(1).padStart(8)}${s.max.toFixed(1).padStart(8)}`);
  }
  const t = result.rtp;
  if (t) console.log(`丢包 ${t.packetsLost}  NACK ${t.nackCount}  PLI ${t.pliCount}  丢帧 ${t.framesDropped}  抖动缓冲平均 ${t.jitterBufferMs?.toFixed(1) ?? '-'} ms`);
  const file = path.join(out, `latency-${new Date().toISOString().replace(/[:.]/g, '-')}.json`);
  writeFileSync(file, JSON.stringify({ profile: PROFILE, headless: HEADLESS, playoutDelay: PLAYOUT, hook: HOOK, noAudio: NO_AUDIO, display: DISPLAY, displayCapture: DISPLAY_CAPTURE, ...result }, null, 2));
  console.log(`\n完整结果: ${file}`);
  code = result.ok > 0 ? 0 : 1;
} catch (e) {
  console.log(`失败: ${e.message}`);
  console.log(`--- agent.log 末尾 ---\n${readFileSync(logPath, 'utf8').split('\n').slice(-20).join('\n')}`);
} finally {
  page?.close();
  agent.stdin.end();
  await Promise.race([agentExit, sleep(8000)]);
  if (agent.exitCode === null) kill(agent.pid);
  if (gametest) kill(gametest);
  if (browserProc?.pid) kill(browserProc.pid, true);
  await sleep(500);
  if (browserDir) rmSync(browserDir, { recursive: true, force: true, maxRetries: 3 });
  process.exit(code);
}
