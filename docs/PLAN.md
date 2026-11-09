# cyber_remote Windows 计划与技术选型

> 状态:草案,2026-10-28。首发平台为 Windows,macOS/Linux 后续按 `SessionBackend` 接口扩展。

## 1. 目标

把 Windows 上的任意 GUI 应用以 WebRTC 推到浏览器,做到:

- 画面只包含应用内容区,不含标题栏和边框。
- 同一台主机可多开应用(含同一应用多份),各路键鼠输入互不干扰,也不影响主机本地鼠标。
- 低延迟:局域网点击到出图 p50 ≤ 70 ms、p95 ≤ 100 ms(估算目标,Phase 0 实测后修正)。

## 2. 已确定的决策

| 项 | 决策 |
|---|---|
| 首发平台 | Windows |
| 主机端语言 | Rust(stable,`x86_64-pc-windows-msvc`) |
| WebRTC 库 | webrtc-rs(`webrtc` crate 0.21,Tokio 运行时,ring 加密) |
| 应用范围 | 单机/独立游戏、模拟器和普通桌面应用;不支持带反作弊的游戏,也不做任何绕过(2026-10-28 定) |
| 测试应用 | 记事本;游戏输入用自写的 `gametest`(显示窗口消息、`GetAsyncKeyState`、Raw Input、XInput 各自看到的输入) |
| 输入隔离路线 | 窗口消息注入(P1)+ 进程内 API hook(P2),不使用 `SendInput` |

## 3. 技术选型

版本和许可来自 crates.io / GitHub API(2026-10-28)。

| 模块 | 选型 | 许可 | 说明 |
|---|---|---|---|
| 平台 API | `windows` 0.62 | MIT/Apache-2.0 | 微软官方绑定 |
| WebRTC | `webrtc` 0.21 | MIT/Apache-2.0 | async 层 + Sans-I/O `rtc` 核心,自带 ICE/TURN |
| 窗口采集 | 直接用 `windows` crate 调 Windows.Graphics.Capture | MIT/Apache-2.0 | 按窗口采集,输出 D3D11 纹理;不再引入 `windows-capture`,少一层封装,和 NVENC 共用同一个 D3D11 设备 |
| 视频编码 | 方案 A:直接调 NVENC,自写薄 FFI | MIT 式(头文件) | 见 3.1 |
| 软件编码兜底 | `openh264` 0.9 | BSD-2 | 无 GPU 或调试用 |
| 音频 | WASAPI 环回(`windows` crate)+ Opus | — | 本机 19045 只能整机环回,见第 6 节 |
| API hook(P2) | `minhook` 0.9(已选,不用 `retour`) | MIT / BSD | hook DLL 需 x86 与 x64 两份 |
| 异步运行时 | `tokio` | MIT | webrtc-rs 默认运行时 |
| 信令 | WebSocket(`axum`) | MIT | Phase 0 内嵌在 agent 里 |
| Web 客户端 | TypeScript + 原生 WebRTC API | — | 不引入前端框架 |

### 3.1 视频编码方案(已定:A)

| 方案 | 额外安装 | 低延迟控制 | 说明 |
|---|---|---|---|
| **A. 直接调 NVENC(采用)** | 无 | 完整 | 运行时加载系统自带 `nvEncodeAPI64.dll`,D3D11 纹理输入,不需要 CUDA;FFI 依据 nv-codec-headers 的 `nvEncodeAPI.h`,其文件头为 NVIDIA 的 MIT 式许可(2026-10-28 核实,n13.0.19.0 为 API 13.0、master 为 13.1);运行时用 `NvEncodeAPIGetMaxSupportedVersion` 检查驱动支持的版本 |
| B. Media Foundation | 无 | 有限 | 系统自带硬件 MFT,N/A/I 卡通用;帧内刷新、参考帧失效等控制较粗 |

已排除:`nvidia-video-codec-sdk` crate,编译期需要 CUDA Toolkit 和 Video Codec SDK 的 `nvEncodeAPI.lib`,本机都没有,SDK 需登录 NVIDIA 开发者账号下载。

### 3.2 未采用

- **str0m**:无 TURN 客户端,对称 NAT 下主机侧连通性无保证,改用 webrtc-rs。
- **Sandboxie**(GPL-3.0):避免许可传染,多开先靠改互斥量或启动参数。
- **ViGEmBus**:已归档退役;手柄改为 P2 里 hook `XInputGetState`。
- **`SendInput`**:全会话共享一个光标和焦点,多开时必然冲突。

## 4. 架构

```
浏览器 ──WebSocket 信令──▶ agent(Phase 0 内嵌信令)
   ▲                              │
   └── SRTP 视频/音频 + DataChannel ┘
```

agent 内部流水线(每路会话一条):

```
WGC 按窗口采集 → 裁剪标题栏/边框(GPU) → 编码 H.264 → webrtc-rs 视频轨
WASAPI 环回    → Opus                  → webrtc-rs 音频轨
DataChannel 输入 → 坐标换算(内容区→窗口) → 窗口消息注入 [P2: + hook DLL 虚拟输入状态]
```

数据通道:

| 通道 | 可靠性 | 内容 |
|---|---|---|
| `input-fast` | 不可靠、无序 | 鼠标移动、手柄状态,带序号,只取最新 |
| `input-key` | 可靠、有序 | 按键、鼠标按钮、滚轮、IME 文本;定期发按键状态快照防卡键 |
| `control` | 可靠、有序 | 分辨率、码率档位、光标形状、统计 |

光标:采集时不画光标,形状和位置走 `control`,客户端本地绘制。

### 4.1 跨平台扩展点

```rust
/// 一路流 = 一个隔离会话:独立的画面、光标、焦点、音频
trait SessionBackend {
    fn spawn(&self, app: &AppProfile, size: Size, fps: u32) -> Result<Box<dyn Session>>;
    fn capabilities(&self) -> Caps;
}

trait Session {
    fn next_frame(&mut self) -> Result<GpuFrame>;        // 已去标题栏
    fn next_audio(&mut self) -> Result<AudioChunk>;
    fn inject(&mut self, ev: InputEvent) -> Result<()>;  // 内容区坐标
    fn resize(&mut self, size: Size) -> Result<()>;
    fn cursor(&mut self) -> Option<CursorUpdate>;
    fn close(self: Box<Self>) -> Result<()>;
}
```

Windows 先实现 `Win32WindowSession`。Linux(每路一个无头 Wayland 合成器)和 macOS 以后各自实现这两个 trait。

### 4.2 目录结构(计划)

```
cyber_remote/
├── Cargo.toml          # workspace
├── docs/PLAN.md
├── crates/
│   ├── agent/          # 主机进程:采集、编码、WebRTC、注入、内嵌信令
│   ├── protocol/       # 信令与 DataChannel 消息类型(serde)
│   └── hook/           # P2:注入目标进程的 cdylib(x86 + x64)
└── web/                # 浏览器客户端(TypeScript)
```

## 5. 分阶段计划

### Phase 0:最小链路

目标:记事本画面无标题栏出现在 Chrome,延迟可测。不做输入、不做多开。

- [x] 0.1 建 Rust workspace(`agent`、`protocol`)和 `web/` 静态页,`cargo build` 通过
- [x] 0.2 按窗口采集记事本(WGC),关闭光标采集,得到 D3D11 纹理
- [x] 0.3 根据窗口边界和客户区计算偏移,裁掉标题栏和边框(记事本的菜单栏不属于客户区,会一起被裁掉)
- [x] 0.4 H.264 编码(按 3.1 的结论),低延迟配置:High profile、无 B 帧、CBR 默认 20 Mbps、4 帧 VBV(单帧 VBV 时 IDR 和大幅变化画面会糊)、长 GOP + 按需 IDR
- [x] 0.5 webrtc-rs 建连,视频轨发送编码帧;自定义拦截器(`keyframe.rs`)把 PLI/FIR 交给应用触发 IDR,每 20 秒一次 IDR 兜底
- [x] 0.6 agent 内嵌 WebSocket 信令和静态页服务
- [x] 0.7 浏览器收流,`jitterBufferTarget = 0`,HUD 显示码率、帧率、丢包(2026-10-28 Chromium 实测:60fps,丢包 0)
- [ ] 0.8 延迟测试应用:点击让色块变白,浏览器检测白块,统计 p50/p95

退出条件:Chrome 中看到记事本内容区,无标题栏;局域网测出 p50/p95 数字。

### Phase 1:单应用可交互

目标:浏览器能操作记事本和 `gametest`,主机本地鼠标不受影响。

- [x] 1.1 数据通道与 `protocol` 消息定义(先做了 `input-fast`、`input-key`;`control` 等 1.4 再加;按键状态快照未做)
- [x] 1.2 鼠标:`WM_MOUSEMOVE`、按键、滚轮,坐标从内容区换算到客户区(2026-10-28 实测:点击定位、拖选、双击选词、滚轮)
- [x] 1.3 键盘:`WM_KEYDOWN/UP`(扫描码 + 虚拟键)、`WM_CHAR`,IME 文本走 `WM_CHAR`(实测:中英文、回车、退格、Ctrl+A、Shift+Home)
  - 输入线程要一直 `AttachThreadInput` 挂在目标线程上,解除挂接时写进去的 Ctrl 状态会丢
  - 每条按键/按钮消息投递后等目标线程处理完(弄脏 1 像素等 `WM_PAINT`),否则回车生成的 `WM_CHAR` 会落到后面的文字之后,修饰键也可能提前松开
- [ ] 1.4 `WM_SETCURSOR`、`WM_NCHITTEST`,光标形状回传客户端
- [ ] 1.5 焦点伪装:`WM_ACTIVATE`、`WM_SETFOCUS`、`WM_ACTIVATEAPP`
- [ ] 1.6 弹出窗口:枚举同进程顶层窗口(菜单、tooltip、对话框),合成进画面并按坐标路由输入
- [x] 1.7 断线时释放所有按键与按钮(实测:按住 Ctrl 刷新页面,重连后打字正常)
- [ ] 1.8 音频:WASAPI 整机环回 → Opus → 音频轨
- [x] 1.9 `gametest` 测试程序;agent 用 `--app` 选择推流目标(2026-10-28 实测:窗口消息和 `GetKeyState` 收到注入的键鼠、Shift 组合字符和回车;`GetAsyncKeyState`、Raw Input、XInput 没有反应,符合预期;主机光标不动)
- [ ] 1.10 浏览器游戏模式:全屏 + Pointer Lock(发相对位移)+ Keyboard Lock(Esc 等不被浏览器吃掉);手柄用 Gamepad API

退出条件:记事本输入、选区、右键菜单、"文件"菜单正常;`gametest` 的窗口消息一行能看到全部键鼠输入;操作期间主机鼠标不动。Raw Input、`GetAsyncKeyState`、XInput 留给 Phase 2。

### Phase 2:多开与输入隔离

目标:同一应用开两份,两个浏览器标签页同时操作互不干扰。

- [ ] 2.1 `hook` cdylib(x86 + x64)与注入器(`CreateRemoteThread` + `LoadLibraryW`)(x64 做完并通过端到端测试;x86 DLL 只通过编译,注入器还不能打 32 位目标)
- [x] 2.2 每实例虚拟输入状态,agent 与 hook DLL 之间用共享内存同步(没用命名管道;另有就绪事件,DLL 装好钩子后通知 agent,5 秒没等到就放弃隔离)
- [x] 2.3 hook:`GetCursorPos`/`SetCursorPos`/`ClipCursor`、`GetAsyncKeyState`/`GetKeyState`/`GetKeyboardState`、`GetForegroundWindow`/`GetActiveWindow`/`GetFocus`、`GetCapture`(`SetCapture` 不钩,见下面的进度说明)(端到端测过 `GetAsyncKeyState`、`GetKeyState`、`GetCursorPos`、`GetForegroundWindow`;其余已实现但没测)
- [x] 2.4 Raw Input:hook `GetRawInputData`/`GetRawInputBuffer`,伪造 `WM_INPUT`(`GetRawInputData` 端到端测过;`GetRawInputBuffer` 没测)
- [ ] 2.5 手柄:hook `XInputGetState`,每路返回自己的手柄(钩子一侧用冒烟脚本验过;浏览器手柄转发是 1.10,没做,目前没有真实的手柄数据源)
- [ ] 2.6 多开:处理单实例互斥量和配置目录冲突
- [ ] 2.7 代码签名,主流杀软报白

进度(2026-11-05):hook 和 agent 两侧都写完了(只支持 64 位目标),`gametest` ×2 的端到端测试通过。回归:`cargo build --workspace` 后 `node scripts/e2e.mjs`;手动多开:`powershell -File scripts\start-multi.ps1`。

- agent 一侧:`hooklink.rs` 建共享内存和就绪事件、注入 DLL、等回执;`inject.rs` 每次输入先写快照(按键、虚拟光标、前台/焦点窗口)再投窗口消息,并为鼠标移动、按钮、滚轮、按键各伪造一条 Raw Input(`WM_INPUT` 投给顶层窗口)。命令行 `--new`(总是新开进程,按进程号认窗口)、`--hook`(装输入隔离);启动器里这两项默认都勾上。
- 总开关 `FLAG_ACTIVE`:agent 优雅退出时清掉它,目标进程里的钩子回落到真实输入,Raw Input 也放行真实事件。
- DLL 钩了 `GetAsyncKeyState`、`GetKeyState`、`GetKeyboardState`、`GetCursorPos`、`SetCursorPos`、`ClipCursor`、`GetForegroundWindow`、`GetActiveWindow`、`GetFocus`、`GetCapture`、`GetRawInputData`、`GetRawInputBuffer`、`XInputGetState`。`SetCapture` 不钩:捕获状态属于调用线程自己的输入队列,多个实例是不同进程,互不影响。
- 共享内存打不开或校验失败时一个钩子都不装;user32 的钩子有一个创建失败就整体放弃;手柄钩子失败只记日志。
- agent 接管期间 Raw Input 的真句柄一律拒绝(`ERROR_INVALID_HANDLE`),主机真鼠标键盘不会漏进实例。代价是用 Raw Input 读 HID 手柄的游戏也拿不到主机的真手柄。
- 端到端测试 `scripts/e2e.mjs`(2 个 agent + 各自新开的 `gametest` + 无头 Edge,经 CDP 发键鼠,读 `gametest` 窗口标题里的状态):
  - 只操作 A 时,A 的 `GetKeyState`/`GetAsyncKeyState` 看到 W 和左键,B 一个键都没有;只操作 B 时反过来;A 按 W、B 按 D 同时按住,各自只看到自己的键。
  - 两路的伪造 Raw Input(事件数、相对位移)和虚拟光标互不影响;两路同时报告自己在前台。
  - 钩子在位期间 Raw Input 事件数只和伪造的条数一致(阶段 1 后 A 是 6 条);停掉 agent、撤销钩子后读到 459 和 613,说明之前主机真鼠标确实被挡住了。这条证据依赖那次运行时主机鼠标在动:鼠标不动的那几次,撤销后只有个位数(如 8),什么也证明不了。
  - agent 关 stdin 后 8 秒内优雅退出,`gametest` 继续运行,撤销后不再有两个窗口同时在前台。
- 记事本(单路):从浏览器打 `hello` 再 Shift+H,编辑框内容是 `helloH`,有无钩子都一样。共跑了 27 次,有 4 次编辑框始终是空的(都在前 9 次里,有无钩子都出现过),原因没查出来;之后给 agent 加了调试日志,连续 18 次通过,没能再复现。记事本 ×2 没测。
- 冒烟脚本(`gametest` + 手工写共享内存,不经 agent)验过 `XInputGetState` 槽位 0:按钮、扳机、摇杆都对。
- 没测过:x86 DLL(只通过编译)、`GetKeyboardState`、`SetCursorPos`、`ClipCursor`、`GetActiveWindow`、`GetFocus`、`GetCapture`、`GetRawInputBuffer`,游戏晚加载 XInput 时的轮询(DLL 装好后每 0.5 秒查一次,持续 60 秒),目标进程已被装过钩子时 agent 拒绝再装的路径。
- 已知缺口:
  - 钩子不拦窗口消息。e2e 里没被操作的 B,窗口消息数也在涨(16→48→57),应该是主机真光标扫过窗口产生的真实鼠标消息;手动测试时别把真鼠标停在 `gametest` 窗口上。
  - 相对位移是浏览器绝对坐标的差,不是 Pointer Lock 的真相对位移,靠无限转视角的游戏(FPS)用不了,等 1.10。
  - 焦点伪装(1.5)没做:`GetForegroundWindow` 被伪造成前台,但窗口没收到 `WM_ACTIVATE`/`WM_SETFOCUS`(`gametest` 画面里"焦点 否 激活 否"),靠这些消息判断失焦的游戏仍会认为自己不在前台。
  - agent 被强杀(没走优雅退出)时,目标进程里的钩子停在最后一次快照上,按键可能一直显示按下;关掉目标进程即可恢复。
  - `--new` 按进程号认窗口,假设新启动的进程自己拥有窗口;经启动器转一手的程序(比如 Win11 的记事本)可能找不到,开发机是 Win10,没验。

退出条件:记事本 ×2、`gametest` ×2 同时独立操作,两路的 Raw Input 相对鼠标、`GetAsyncKeyState`、XInput 互不干扰;再找一款无反作弊的窗口化单机游戏实测。目前 `gametest` ×2 的 Raw Input 和 `GetAsyncKeyState` 已通过;记事本 ×2、XInput、真实游戏还没做。

### Phase 3:产品化

会话管理与应用档案、AMF/QSV 编码后端、码率自适应、断线重连、安装包、控制面与认证、TURN 部署。

- [x] 3.0 简易启动器 `crates/launcher`(egui):选应用(记事本 / gametest / 自定义 exe + 参数 + 类名/标题)、帧率、码率、端口;配置存 `launcher.json`;启停 agent、看日志、开浏览器。单实例,还不是会话管理

## 6. 已知限制与风险

| 项 | 影响 | 应对 |
|---|---|---|
| 本机 build 19045 < 20348 | 无法关闭 WGC 黄色采集边框;无法按进程环回音频 | 黄框只在主机屏幕上显示,不进采集画面(2026-10-28 浏览器截图已确认);音频先用整机环回 |
| 消息注入的覆盖面 | 用 Raw Input / DirectInput / XInput 或主动查询系统状态的应用收不到 | P2 hook 层补齐 |
| 弹出窗口 | 菜单、tooltip 是独立顶层窗口 | P1.6 按进程枚举并合成 |
| OLE 拖放 | `DoDragDrop` 跟踪真实鼠标,hook 难以覆盖 | 暂不支持,列入兼容性说明 |
| 反作弊 / 受保护进程 / UAC | 禁止注入和 hook | 不支持,也不做绕过 |
| 独占全屏游戏 | WGC 按窗口采集不到 | 要求游戏用窗口化或无边框窗口 |
| 失焦暂停 | 很多游戏不在前台就暂停或降帧 | P1.5 焦点伪装,P2 hook `GetForegroundWindow` |
| 杀软误报 | DLL 注入 + hook 手法与恶意软件相同 | 代码签名、报白 |
| 注入 DLL 依赖 VC++ 运行库 | hook DLL 动态链接 `VCRUNTIME140.dll` 和 UCRT(2026-11-05 dumpbin 实测,x86 和 x64 相同)。目标机缺对应位宽的运行库时 `LoadLibraryW` 失败,该实例不受隔离 | 发布前改成静态 CRT(`+crt-static`,要写进 `.cargo/config.toml`,会影响整个 workspace 的构建);DLL 装好后给 agent 回执,超时就拒绝该实例 |
| 窗口最小化 | WGC 采集不到最小化窗口 | 阻止最小化,或将窗口移到屏幕外 |
| 消费级显卡 NVENC 并发上限 | 限制同时编码的路数 | 实测;多路时考虑混合编码后端 |

## 7. 开发环境

| 项 | 状态(2026-10-28) |
|---|---|
| OS | Windows 10 Pro 19045 |
| GPU | RTX 3060,`nvEncodeAPI64.dll`、`nvcuda.dll` 存在 |
| Rust | 系统默认 1.89.0;项目用 `rust-toolchain.toml` 固定 1.99.1(webrtc-rs 0.21 需要 ≥ 1.91),crates 源为阿里云镜像 |
| MSVC 链接器 / Windows SDK | VS 2022 Build Tools(C++ 工作负载),SDK 10.0.26100.0 |
| CUDA Toolkit | 未安装(方案 A/B 都不需要) |
| Node | 22.19.0 |
| 其他虚拟显示驱动 | Oray IDD、GameViewer 已安装(按窗口采集不依赖它们) |

## 8. 待决事项

- [x] 视频编码方案:A 直接调 NVENC(3.1)
- [x] 产品定位:单机/独立游戏和模拟器云化,兼顾普通桌面应用;不含带反作弊的网游
- [ ] 是否闭源商用
- [ ] 首发浏览器范围(`Keyboard.lock` 只有 Chrome 系支持)

