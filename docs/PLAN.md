# cyber_remote Windows 计划与技术选型

> 状态:草案,2026-10-28 起草,2026-11-19 更新。首发平台为 Windows;要做跨平台,各平台按 `platform/` 下的后端接口扩展(见 4.1)。

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

2026-11-19 定:做跨平台。一路 = 一个隔离的"座位":一块虚拟屏 + 一路虚拟输入 + 一个音频出口。Linux 每路一个无头 Wayland 合成器(参考 Wolf),Windows 计划改成每路一块 IDD 虚拟屏 + 现有 hook(待 spike),macOS 隔离最弱,多开大概率要靠虚拟机。

代码里的落地比上面的草图简单:`crates/agent/src/host.rs` 放平台无关的类型和 `Host` trait(订阅编码帧、请求关键帧、开输入通道、状态提示、退出清理);`platform/` 下每个平台提供 `init`、`preset`、`spawn`,目前只有 `platform/windows`(采集、NVENC、注入、hook 连接都在里面),其他平台编译时走 `unsupported`,运行时报错。编码器暂时跟着平台走,等第二个平台出现再抽公共部分。协议里按键以 DOM `code` 为准,`vk` 只作提示。

近期顺序(2026-11-19):0.8 延迟基线 → 抽平台边界(这两项已完成)→ Windows 虚拟屏 spike → Linux 后端原型。

### 4.2 目录结构

```
cyber_remote/
├── Cargo.toml          # workspace
├── docs/PLAN.md
├── crates/
│   ├── agent/          # 主机进程
│   │   └── src/
│   │       ├── main.rs、session.rs、keyframe.rs、playout.rs  # HTTP、信令、WebRTC(平台无关)
│   │       ├── host.rs                            # 核心类型与 Host trait
│   │       └── platform/windows/                  # 采集(按窗口 / 整屏)、NVENC、注入、hook 连接
│   ├── protocol/       # 信令与 DataChannel 消息类型(serde)
│   ├── hook/、hook-abi/ # 注入目标进程的 DLL,以及它和 agent 共用的共享内存布局
│   ├── gametest/       # 输入测试窗口;--latency 是延迟测量用的黑白窗口
│   └── launcher/       # egui 启动器
├── scripts/            # e2e.mjs、latency.mjs、start-multi.ps1
└── web/                # 浏览器客户端(原生 JS)
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
- [x] 0.8 延迟测试应用:点击让色块变白,浏览器检测白块,统计 p50/p95(2026-11-19 在本机测过,局域网还没测)
  - 做法:`gametest --latency` 整窗黑色,左键按下变白;页面 `/?latency=N`(`web/latency.js`)用 `requestVideoFrameCallback` 逐帧看画面中心;agent 给带探针编号的点击记下收到、投递、出帧、编码、发送的时刻,经 `control` 通道回报,页面拆成各段。一键跑:`cargo build --release -p agent -p gametest` 后 `node scripts/latency.mjs`,结果存在 `target/latency/`
  - 基线(release、Edge 有头、同一台机器、60Hz 屏、100 次):总计 p50 92 ms、p95 210 ms。主机内部很快:浏览器→agent 0.3、应用重画到采集 9.6、编码 2.1、打包 0.1(都是 p50)。时间几乎都花在浏览器侧:缓冲+等显示 p50 60 ms(分成 25–30 和 90 两档);网络→收齐 p50 0.8 但 p90 51 ms,本机回环不该这样,后来查明不是网络问题(见下);解码 p90 28 ms
  - 2026-11-19 发送端给视频包带上 playout-delay 扩展(min = max = 0,`playout.rs`;`--no-playout-delay` 关掉做对照):总计 p50 45.5 ms、p95 95 ms,同一批关掉的对照组是 134 / 237;缓冲+等显示 p50 27 ms,抖动缓冲平均 0.2 ms,丢包、NACK、丢帧都是 0。测量结果里加了这些入站统计。代价是浏览器不再攒缓冲平滑抖动,Wi-Fi 这类抖动大的网络上可能卡顿,上局域网后要再测
  - 网络段的长尾是探针挂错了帧:三次测量里网络段超过 10 ms 的样本,"应用重画+合成"中位数只有 2 ms(其余样本 10 ms),两段之和约 20 ms(其余约 11 ms)。应用在 vsync 前一刻才画完时,紧接着那次合成还是旧画面,探针挂在了这一帧上,白帧晚一次合成(16.7 ms)才出来,这段时间被算进了网络。总计不受影响。后来查明这是 WGC 的毛病,窗口大时一半以上的点击都这样,桌面复制少得多(见第 8 节)。要拆准,得让 agent 回报探针帧的 RTP 时间戳,浏览器用 rVFC 的 `rtpTimestamp` 对上是哪一帧(还没做)
  - 剩下的大头:缓冲+等显示 27 ms 基本是浏览器的合成管线(解码完到上屏 1–2 个 vsync),再往下要换 WebCodecs + canvas;应用重画+合成约 10 ms 是在等 DWM 合成,虚拟屏开到高刷新率能降。前几次点击偶尔有 130–150 ms 的网络段,原因没查
  - 没取到系统合成时刻:WGC 帧的 `SystemRelativeTime` 换算后都落在接受范围外(可能是下一次 vblank 的时刻,待查),所以"合成→采集"并在"应用重画+合成"里

退出条件:Chrome 中看到记事本内容区,无标题栏;局域网测出 p50/p95 数字。

### Phase 1:单应用可交互

目标:浏览器能操作记事本和 `gametest`,主机本地鼠标不受影响。

- [x] 1.1 数据通道与 `protocol` 消息定义(先做了 `input-fast`、`input-key`;`control` 通道 2026-11-19 随 0.8 加上,目前只回报延迟探针;按键状态快照未做)
- [x] 1.2 鼠标:`WM_MOUSEMOVE`、按键、滚轮,坐标从内容区换算到客户区(2026-10-28 实测:点击定位、拖选、双击选词、滚轮)
- [x] 1.3 键盘:`WM_KEYDOWN/UP`(扫描码 + 虚拟键)、`WM_CHAR`,IME 文本走 `WM_CHAR`(实测:中英文、回车、退格、Ctrl+A、Shift+Home)
  - 输入线程要一直 `AttachThreadInput` 挂在目标线程上,解除挂接时写进去的 Ctrl 状态会丢
  - 每条按键/按钮消息投递后等目标线程处理完(弄脏 1 像素等 `WM_PAINT`),否则回车生成的 `WM_CHAR` 会落到后面的文字之后,修饰键也可能提前松开
- [ ] 1.4 `WM_SETCURSOR`、`WM_NCHITTEST`,光标形状回传客户端
- [x] 1.5 焦点伪装:`WM_ACTIVATE`、`WM_SETFOCUS`、`WM_ACTIVATEAPP`(会话第一次输入时补发一次;窗口本来就在前台则不补。端到端测过 `gametest` 收到这三条。已知限制:只是补发消息,窗口并不真的获得系统焦点,`GetFocus()` 之类的 API 仍返回旧值)
- [ ] 1.6 弹出窗口:枚举同进程顶层窗口(菜单、tooltip、对话框),合成进画面并按坐标路由输入
  - 2026-11-19 暂停:逐窗口合成只有 Windows 需要,而且 19045 上 WGC 多半不接受菜单、tooltip 这类 tool window。等"每路一块虚拟屏"的 spike(第 8 节),那条路上弹出窗口天然在画面里。`platform/windows/compositor.rs` 和 `capture.rs` 里的 `Source` 是这项的半成品
- [x] 1.7 断线时释放所有按键与按钮(实测:按住 Ctrl 刷新页面,重连后打字正常)
- [ ] 1.8 音频:WASAPI 整机环回 → Opus → 音频轨
- [x] 1.9 `gametest` 测试程序;agent 用 `--app` 选择推流目标(2026-10-28 实测:窗口消息和 `GetKeyState` 收到注入的键鼠、Shift 组合字符和回车;`GetAsyncKeyState`、Raw Input、XInput 没有反应,符合预期;主机光标不动)
- [ ] 1.10 浏览器游戏模式:全屏 + Pointer Lock(发相对位移)+ Keyboard Lock(Esc 等不被浏览器吃掉);手柄用 Gamepad API(Pointer Lock 的发送和 agent 侧已做完,见下面进度:页面按 F2 切换游戏模式,相对增量经 `rel` 字段发到 agent,虚拟光标按增量累加,不再受窗口消息回中影响;端到端只测了协议和 agent 这一段,真实 Pointer Lock 的手感要手动验。全屏、Keyboard Lock、Gamepad 还没做)

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
| 多开时声音串流 | 整机环回下每一路都听到所有实例;19045 不能按进程环回 | Win11/Server 2022 起用按进程环回;19045 上给每个实例一个虚拟音频设备 + 按应用指定输出设备,或在 hook DLL 里截 WASAPI |
| 开发机 Win10 已过支持期 | 消费版延长更新 2026-10-13 结束;黄框、不能按进程环回、WGC 拒绝 tool window 都是旧系统的限制 | 生产基线待定(第 8 节) |
| 没有拥塞控制 | 固定 CBR,带宽一掉延迟暴涨,丢包后只能整帧 IDR | 接 webrtc-rs 0.21 自带的 GCC(`configure_congestion_control`),目标码率交给 `nvEncReconfigureEncoder` |
| 浏览器端缓冲 | 0.8 实测"缓冲+等显示"p50 60 ms,是总延迟的大头;`jitterBufferTarget = 0` 只是目标,不是上限 | 已加 playout-delay 扩展(min=max=0),降到 p50 27 ms;再往下要换 WebCodecs + canvas,绕开 video 元素的合成管线 |
| 局域网要 HTTPS | 非 localhost 的 http 页面不是安全上下文,Gamepad、Keyboard Lock、剪贴板、WebCodecs 都用不了 | 上局域网前配好 HTTPS |
| 实例之间没有安全边界 | 同一用户同一会话,实例能互相读内存、投消息;共享内存用默认权限,名字也猜得到 | 只跑运营方挑选的应用;要跑用户自带程序得用虚拟机或 Linux 容器 |

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
- [x] 跨平台:做(2026-11-19)。一路 = 一个座位:虚拟屏 + 虚拟输入 + 音频出口
- [ ] Windows 是否改成每路一块虚拟屏、采集整屏。开发机上向日葵、GameViewer 的虚拟显卡只能由它们自己的客户端开关,没有公开接口,不考虑。分两步:
  - [x] 第一步(2026-11-19):不装驱动,拿实体副屏代替虚拟屏。`--display N`:目标窗口去掉边框、置顶、铺满 `\\.\DISPLAYn` 的工作区(不盖任务栏),DXGI 桌面复制只采这块区域(`platform/windows/display.rs`),退出时还原窗口。结果:右键菜单这类弹出窗口在画面里;本机屏幕上没有黄框;每帧有合成时刻(合成→采集 0.1 ms)。置顶是必须的:后台进程用 `HWND_TOP` 抬不起别的进程的窗口,实测被副屏上原有的窗口挡住,采到的是别的窗口
  - 延迟(2560×1392,各 100 次):桌面复制总计 p50 48.6 / p95 67.5 / 最大 96 ms;同样铺满但按窗口采集(`CYBER_REMOTE_DISPLAY_CAPTURE=window`,对照用)是 55.5 / 79.9 / 196。按窗口采集的拆分里"网络"p50 就有 16 ms:WGC 点击后先交一帧旧画面,新画面晚一次合成才到,0.8 里网络段的长尾也是这个原因;桌面复制交的就是新画面。和小窗口按窗口采集(45.5 / 95)比,p50 高 3 ms 主要是画面大了 4 倍,编码从 2 ms 涨到 5 ms
  - 整屏模式的鼠标路由(2026-11-19,`inject.rs` 的 `aim_screen`):照着系统分发真实鼠标输入的规则来。菜单模式下发给菜单的捕获窗口,坐标填屏幕坐标(菜单循环按屏幕坐标找菜单项);有窗口捕获了鼠标(下拉列表、拖滚动条)就发给它;否则发给坐标下属于目标进程的窗口,落在非客户区发 `WM_NC*`。实测在画面里点右键菜单项、点菜单栏、点菜单栏下拉里的项都能用,弹出的对话框也在画面里。按窗口采集的路由没动
  - 菜单栏是绕过去的:后台窗口收到 `WM_NCLBUTTONDOWN(HTMENU)` 不开菜单(多半要求窗口真被激活,或真实鼠标键按着),键盘进菜单(`SC_KEYMENU`,即 Alt+访问键)却可以。所以点菜单栏时用 `MenuItemFromPoint` 找出点中的项,按它的访问键走键盘那条路(没有访问键就用方向键挪过去);这次点击的按下、抬起和左键状态都不给目标,否则菜单循环看到左键松开、真实光标不在菜单上,会把刚展开的菜单收起来
  - 还没做:屏上别的置顶窗口和系统通知也会被采进去;分辨率变了要重启 agent
  - 已修(2026-11-19):会话一开始没点过就直接打字会丢字符,按窗口采集也一样。焦点伪装补发的激活消息只是投进队列,程序还没在 WM_SETFOCUS 里把焦点交给子控件,紧跟着的文字查不到焦点窗口,发给了顶层窗口。现在补发后用 `sync` 等它处理完(最多 100 ms,每个会话一次)
  - [ ] 第二步:装可编程的虚拟屏驱动(首选 SudoVDA,Apollo 用它给每个连接建屏;备选 parsec-vdd),验证高刷新率、每个实例按需建屏和断开就删。做成了 1.6 的逐窗口合成就作废
- [ ] Windows 生产基线:Win11 24H2+/Server 2022+,还是继续 19045(改成虚拟屏后基本只影响音频)

