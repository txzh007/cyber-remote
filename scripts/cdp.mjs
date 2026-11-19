// e2e.mjs 和 latency.mjs 共用:最小 CDP 客户端、轮询等待、找浏览器
import { existsSync } from 'node:fs';
import { setTimeout as sleep } from 'node:timers/promises';

export const BROWSERS = [
  'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe',
  'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
];

export function findBrowser() {
  const exe = BROWSERS.find(existsSync);
  if (!exe) throw new Error('没找到 Edge 或 Chrome');
  return exe;
}

export async function until(desc, fn, timeoutMs = 5000) {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) {
    const v = await fn();
    if (v) return v;
    await sleep(100);
  }
  throw new Error(`超时: ${desc}`);
}

export async function waitHttp(url, timeoutMs = 30000) {
  return until(url, async () => {
    try {
      return await (await fetch(url)).text();
    } catch {
      return null;
    }
  }, timeoutMs);
}

// 最小的 CDP 客户端:按 id 配对请求和响应
export class Cdp {
  constructor(ws) {
    this.ws = ws;
    this.id = 0;
    this.pending = new Map();
    ws.onmessage = (e) => {
      const m = JSON.parse(e.data);
      const p = m.id && this.pending.get(m.id);
      if (!p) return;
      this.pending.delete(m.id);
      m.error ? p.reject(new Error(`${p.method}: ${m.error.message}`)) : p.resolve(m.result);
    };
  }

  static async connect(url) {
    const ws = new WebSocket(url);
    await new Promise((resolve, reject) => {
      ws.onopen = resolve;
      ws.onerror = () => reject(new Error(`连不上 ${url}`));
    });
    return new Cdp(ws);
  }

  send(method, params = {}) {
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject, method });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }

  async eval(expression) {
    const r = await this.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
    if (r.exceptionDetails) throw new Error(`页面脚本出错: ${r.exceptionDetails.text} ${expression}`);
    return r.result.value;
  }

  close() {
    this.ws.close();
  }
}
