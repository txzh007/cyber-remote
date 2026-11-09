// 采集浏览器里的键鼠输入,经 DataChannel 发给 agent。
// 坐标是视频画面内的归一化位置(0..1),agent 按窗口客户区换算。

function attachInput(video, ime, fast, key) {
  let seq = 0;
  let buttons = 0;          // 当前按下的鼠标按钮数
  const keysDown = new Set(); // 以 Key 消息发出、还没抬起的 DOM code
  let wheelX = 0, wheelY = 0; // 不足一格的滚动累计

  const send = (ch, msg) => { if (ch.readyState === 'open') ch.send(JSON.stringify(msg)); };

  // object-fit: contain 会留黑边,只算画面本身的区域
  function toVideo(e) {
    const r = video.getBoundingClientRect();
    const vw = video.videoWidth, vh = video.videoHeight;
    if (!vw || !vh) return null;
    const s = Math.min(r.width / vw, r.height / vh);
    const cw = vw * s, ch = vh * s;
    const x = (e.clientX - r.left - (r.width - cw) / 2) / cw;
    const y = (e.clientY - r.top - (r.height - ch) / 2) / ch;
    const inside = x >= 0 && x <= 1 && y >= 0 && y <= 1;
    return { x: Math.min(1, Math.max(0, x)), y: Math.min(1, Math.max(0, y)), inside };
  }

  const BUTTONS = ['left', 'middle', 'right', 'back', 'forward'];

  video.addEventListener('pointermove', e => {
    const p = toVideo(e);
    if (!p || (!p.inside && buttons === 0)) return;
    seq = (seq + 1) >>> 0;
    send(fast, { type: 'mouse_move', seq, x: p.x, y: p.y });
  });

  video.addEventListener('pointerdown', e => {
    const p = toVideo(e);
    const button = BUTTONS[e.button];
    if (!p || !p.inside || !button) return;
    e.preventDefault();
    // 点进画面后键盘归远端:焦点给隐藏输入框
    ime.focus({ preventScroll: true });
    video.setPointerCapture(e.pointerId);
    buttons++;
    send(key, { type: 'mouse_button', button, down: true, x: p.x, y: p.y });
  });

  video.addEventListener('pointerup', e => {
    const p = toVideo(e);
    const button = BUTTONS[e.button];
    if (!p || !button || buttons === 0) return;
    e.preventDefault();
    buttons--;
    send(key, { type: 'mouse_button', button, down: false, x: p.x, y: p.y });
  });

  video.addEventListener('contextmenu', e => e.preventDefault());

  video.addEventListener('wheel', e => {
    const p = toVideo(e);
    if (!p || !p.inside) return;
    e.preventDefault();
    // 换成 Windows 单位(一格 120):像素模式下浏览器一格约 100px,行模式一格 3 行
    const k = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 120 * 3 : 1.2;
    wheelX += e.deltaX * k;
    wheelY -= e.deltaY * k; // DOM 向下为正,Windows 向上为正
    const dx = Math.trunc(wheelX), dy = Math.trunc(wheelY);
    wheelX -= dx; wheelY -= dy;
    if (dx || dy) send(key, { type: 'wheel', dx, dy, x: p.x, y: p.y });
  }, { passive: false });

  ime.addEventListener('keydown', e => {
    // 输入法组字期间的按键交给输入法,上屏文字在 compositionend 里发
    if (e.isComposing || e.keyCode === 229) return;
    e.preventDefault();
    // 字母数字也发按键:游戏要看到 WASD 的按下和抬起,字符由目标程序的 TranslateMessage 生成。
    // 没有虚拟键码的字符才退回文字
    if (!e.keyCode) {
      if (e.key.length === 1) send(key, { type: 'text', text: e.key });
      return;
    }
    keysDown.add(e.code);
    send(key, { type: 'key', code: e.code, vk: e.keyCode, down: true, repeat: e.repeat });
  });

  ime.addEventListener('keyup', e => {
    if (!keysDown.delete(e.code)) return;
    e.preventDefault();
    send(key, { type: 'key', code: e.code, vk: e.keyCode, down: false, repeat: false });
  });

  ime.addEventListener('compositionend', e => {
    if (e.data) send(key, { type: 'text', text: e.data });
    ime.value = '';
  });
  ime.addEventListener('input', e => {
    if (e.isComposing) return;
    // 不经 keydown 直接插入的文字(粘贴式输入、部分输入法、自动化工具)
    if (e.inputType === 'insertText' && e.data) send(key, { type: 'text', text: e.data });
    ime.value = '';
  });

  // 浏览器失焦时收不到 keyup,让 agent 把按住的都松开
  function releaseAll() {
    keysDown.clear();
    buttons = 0;
    send(key, { type: 'release_all' });
  }
  ime.addEventListener('blur', releaseAll);
  document.addEventListener('visibilitychange', () => { if (document.hidden) releaseAll(); });
}
