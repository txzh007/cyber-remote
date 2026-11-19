// 延迟测量(PLAN 0.8),配合 gametest --latency:整窗黑色,左键按下变白,抬起变黑。
//
// 每轮:随机等 100–300 ms(避免和帧周期锁相)→ 经 input-key 发带探针编号的左键按下,
// 记下发送时刻 → 用 requestVideoFrameCallback 逐帧看画面中心,第一帧变白时取它的
// expectedDisplayTime → 发抬起,等画面变回黑色。
// agent 经 control 通道回报这次点击在主机内各环节的时刻(Unix 毫秒)。页面时间轴
// performance.timeOrigin + now() 也是 Unix 毫秒,同一台机器上两边可以直接相减;
// 跨机器测时,"浏览器→agent"和"网络"两段要先对时钟才可信。

const LATENCY_STAGES = [
  ['total', '总计 点击→显示'],
  ['input', '浏览器→agent'],
  ['inject', 'agent 收到→投递'],
  ['app', '应用重画+合成'],
  ['capture', '合成→采集'],
  ['encode', '编码'],
  ['send', '打包发送'],
  ['network', '网络→收齐'],
  ['decode', '解码'],
  ['render', '缓冲+等显示'],
];

async function runLatencyTest({ video, count = 200, onStatus = () => {} }) {
  const reports = new Map();
  addEventListener('agent-control', e => {
    if (e.detail.type === 'probe') reports.set(e.detail.id, e.detail);
  });
  const wait = ms => new Promise(r => setTimeout(r, ms));
  for (let i = 0; ; i++) {
    const ch = window.__channels;
    if (ch && ch.key.readyState === 'open' && video.videoWidth > 0) break;
    if (i > 300) throw new Error('30 秒内没连上或没出画面');
    await wait(100);
  }
  const key = window.__channels.key;
  const before = await inboundVideoStats();
  const canvas = document.createElement('canvas');
  canvas.width = canvas.height = 8;
  const ctx = canvas.getContext('2d', { willReadFrequently: true });
  // 画面中心 20% 区域的平均亮度(0..255)
  const brightness = () => {
    const vw = video.videoWidth, vh = video.videoHeight;
    ctx.drawImage(video, vw * 0.4, vh * 0.4, vw * 0.2, vh * 0.2, 0, 0, 8, 8);
    const d = ctx.getImageData(0, 0, 8, 8).data;
    let sum = 0;
    for (let i = 0; i < d.length; i += 4) sum += d[i] + d[i + 1] + d[i + 2];
    return sum / (d.length / 4) / 3;
  };
  // 等第一帧满足条件的画面,返回它的元数据;超时返回 null
  const waitFrame = (pred, timeoutMs = 2000) => new Promise(resolve => {
    let done = false;
    const timer = setTimeout(() => { done = true; resolve(null); }, timeoutMs);
    const step = (_now, meta) => {
      if (done) return;
      if (pred(brightness())) {
        done = true;
        clearTimeout(timer);
        resolve(meta);
      } else {
        video.requestVideoFrameCallback(step);
      }
    };
    video.requestVideoFrameCallback(step);
  });
  const click = (down, probe) => key.send(JSON.stringify({
    type: 'mouse_button', button: 'left', down, x: 0.5, y: 0.5, ...(probe ? { probe } : {}),
  }));

  if (brightness() > 64 && !(await waitFrame(b => b < 64, 3000))) {
    throw new Error('画面不是黑色,目标要用 gametest --latency');
  }
  const samples = [];
  for (let id = 1; id <= count; id++) {
    onStatus(`延迟测量 ${id}/${count}…`);
    await wait(100 + Math.random() * 200);
    const sent = performance.now();
    click(true, id);
    const meta = await waitFrame(b => b > 128);
    click(false);
    if (meta) samples.push({ id, sent, meta });
    await waitFrame(b => b < 64);
  }
  await wait(500); // 最后几条回报可能还在路上
  return summarizeLatency(samples, reports, count, rtpDelta(before, await inboundVideoStats()));
}

// 视频入站统计,测量前后各取一次求差,看丢包、重传、抖动缓冲
async function inboundVideoStats() {
  let s = null;
  (await window.pc.getStats()).forEach(r => { if (r.type === 'inbound-rtp' && r.kind === 'video') s = r; });
  return s;
}

function rtpDelta(a, b) {
  if (!a || !b) return null;
  const d = k => (b[k] ?? 0) - (a[k] ?? 0);
  const emitted = d('jitterBufferEmittedCount');
  return {
    packetsLost: d('packetsLost'), nackCount: d('nackCount'), pliCount: d('pliCount'),
    framesDropped: d('framesDropped'), framesDecoded: d('framesDecoded'),
    // 每帧在抖动缓冲里平均待多久
    jitterBufferMs: emitted > 0 ? d('jitterBufferDelay') / emitted * 1000 : null,
  };
}

function summarizeLatency(samples, reports, count, rtp) {
  const epoch = t => performance.timeOrigin + t;
  const rows = samples.map(({ id, sent, meta }) => {
    const shown = meta.expectedDisplayTime;
    const recv = meta.receiveTime; // 这一帧最后一个包到达的时刻,只有 WebRTC 的帧有
    const decode = meta.processingDuration === undefined ? undefined : meta.processingDuration * 1000;
    const row = { id, total: shown - sent, decode };
    if (recv !== undefined && decode !== undefined) row.render = shown - recv - decode;
    const r = reports.get(id);
    if (r) {
      row.input = r.received - epoch(sent);
      row.inject = r.posted - r.received;
      if (r.composed != null) {
        row.app = r.composed - r.posted;
        row.capture = r.captured - r.composed;
      } else {
        row.app = r.captured - r.posted; // 拿不到合成时刻,和采集算在一起
      }
      row.encode = r.encoded - r.captured;
      row.send = r.sent - r.encoded;
      if (recv !== undefined) row.network = epoch(recv) - r.sent;
    }
    return row;
  });
  const stats = {};
  for (const [k] of LATENCY_STAGES) {
    const v = rows.map(r => r[k]).filter(Number.isFinite).sort((a, b) => a - b);
    if (!v.length) continue;
    const q = p => v[Math.min(v.length - 1, Math.floor(p * v.length))];
    stats[k] = { n: v.length, p50: q(0.5), p95: q(0.95), max: v[v.length - 1] };
  }
  return {
    count, ok: samples.length, matched: rows.filter(r => r.input !== undefined).length,
    stages: LATENCY_STAGES, stats, rtp, rows, userAgent: navigator.userAgent,
  };
}

function formatLatency(r) {
  const lines = [`延迟(毫秒) 成功 ${r.ok}/${r.count},主机回报 ${r.matched}   p50 / p95 / 最大`];
  for (const [k, label] of LATENCY_STAGES) {
    const s = r.stats[k];
    if (s) lines.push(`${label}: ${s.p50.toFixed(1)} / ${s.p95.toFixed(1)} / ${s.max.toFixed(1)}`);
  }
  if (r.rtp) lines.push(formatRtp(r.rtp));
  return lines.join('\n');
}

function formatRtp(t) {
  const jb = t.jitterBufferMs == null ? '-' : t.jitterBufferMs.toFixed(1);
  return `丢包 ${t.packetsLost}  NACK ${t.nackCount}  PLI ${t.pliCount}  丢帧 ${t.framesDropped}  抖动缓冲平均 ${jb} ms`;
}
