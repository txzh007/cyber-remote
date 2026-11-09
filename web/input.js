// 采集浏览器里的键鼠输入,经 DataChannel 发给 agent。
// 协议见 crates/protocol:
// - 鼠标移动发归一化位置 x/y(0..1);Pointer Lock 下改发相对位移 dx/dy(rel: true)。
//   Pointer Lock 时 x/y 仍带上,只是给按键等消息用的参考位置,agent 不按它移动光标。
// - 按下/抬起、滚轮、按键、文本都发归一化位置。
//
// 游戏模式(Pointer Lock):点画面后调 requestPointerLock(经 attachInput 的第 5 个参数
// 传入开关)。锁定时按 Esc 由浏览器退出锁定,和普通网页一致。

function attachInput(video, ime, fast, key, opts = {}) {
  let seq = 0;
  let buttons = 0;          // 当前按下的鼠标按钮数
  const keysDown = new Set(); // 以 Key 消息发出、还没抬起的 DOM code
  let wheelX = 0, wheelY = 0; // 不足一格的滚动累计
  let locked = false;
  // Pointer Lock 下的虚拟位置(0..1):模拟光标只由 movementX/Y 累加,给按键消息用
  let virtualPos = { x: 0.5, y: 0.5 };

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
    if (locked) {
      // 相对位移按画面高度归一化:浏览器给的是 CSS 像素,比 screenX 的小一些
      const vw = video.videoWidth || 1;
      const vh = video.videoHeight || 1;
      const r = video.getBoundingClientRect();
      const s = Math.min(r.width / vw, r.height / vh) || 1;
      const dx = Math.round(e.movementX / s);
      const dy = Math.round(e.movementY / s);
      if (!dx && !dy) return;
      // 虚拟位置按视频像素累加,再转回 0..1;不钳制方向,按键时再取 0..1 的值
      virtualPos.x = (virtualPos.x * vw + dx) / vw;
      virtualPos.y = (virtualPos.y * vh + dy) / vh;
      seq = (seq + 1) >>> 0;
      send(fast, {
        type: 'mouse_move', seq, dx, dy, rel: true,
        x: Math.min(1, Math.max(0, virtualPos.x)),
        y: Math.min(1, Math.max(0, virtualPos.y)),
      });
      return;
    }
    const p = toVideo(e);
    if (!p || (!p.inside && buttons === 0)) return;
    // 真鼠标移动时虚拟位置跟着走,退出锁定后位置不会跳
    virtualPos = { x: p.x, y: p.y };
    seq = (seq + 1) >>> 0;
    send(fast, { type: 'mouse_move', seq, x: p.x, y: p.y });
  });

  // 游戏模式:点进画面就请求 Pointer Lock;不支持就安静地退回普通模式
  if (opts.pointerLock) {
    document.addEventListener('pointerlockchange', () => {
      locked = document.pointerLockElement === video;
    });
    video.addEventListener('click', () => {
      if (!locked && video.requestPointerLock) {
        video.requestPointerLock().catch(() => {});
      }
    });
  }

  video.addEventListener('pointerdown', e => {
    // 锁定时没有可用指针坐标,用虚拟位置
    const p = locked ? { x: virtualPos.x, y: virtualPos.y, inside: true } : toVideo(e);
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
    const p = locked ? { x: virtualPos.x, y: virtualPos.y, inside: true } : toVideo(e);
    const button = BUTTONS[e.button];
    if (!p || !button || buttons === 0) return;
    e.preventDefault();
    buttons--;
    send(key, { type: 'mouse_button', button, down: false, x: p.x, y: p.y });
  });

  video.addEventListener('contextmenu', e => e.preventDefault());

  video.addEventListener('wheel', e => {
    const p = locked ? { x: virtualPos.x, y: virtualPos.y, inside: true } : toVideo(e);
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
