# cyber_remote

把任意 GUI 程序推到浏览器里用:WebRTC 低延迟串流,一台机器开多个实例,画面不带标题栏,每个实例的键鼠互不干扰。首要场景是单机游戏和模拟器。

- 主机端 Rust,浏览器端原生 JS,打开网页即用,不用装客户端
- 首发 Windows(需要 NVIDIA 显卡);架构按跨平台设计,macOS、Linux 后端在规划中
- 本机实测点击到画面显示:p50 约 45 ms(按窗口采集)/ 49 ms(2560×1392 整屏采集)

> 不支持带反作弊的游戏:输入隔离要往目标进程里注入 DLL。

## 架构设计

### 数据流

```
画面:目标程序 → 采集(WGC 按窗口 / DXGI 整屏)→ D3D11 → NVENC H.264 → WebRTC 视频轨 → 浏览器 <video>
输入:浏览器键鼠 → DataChannel(input-fast 不可靠 / input-key 可靠)→ agent 注入线程 → 窗口消息 + 焦点伪装 → 目标程序
隔离:agent 把每个实例的虚拟键鼠状态写进共享内存 → 目标进程里的 hook DLL 拦截输入 API,改读这份状态
声音:目标程序 → hook 拦 WASAPI ReleaseBuffer,PCM 抄进共享内存环(主机静音)→ agent 混成 48 kHz 立体声 → Opus → WebRTC 音频轨 → 浏览器 <audio>
回报:agent → control 通道 → 浏览器(延迟探针各环节的时刻)
```

### 模块划分

| crate | 作用 |
|---|---|
| `agent` | 主机进程:网页、WebSocket 信令、WebRTC 会话、各平台后端 |
| `protocol` | 信令和 DataChannel 消息类型(serde),浏览器和 agent 的约定 |
| `hook` / `hook-abi` | 注入目标进程的 DLL(minhook 拦截输入 API),以及它和 agent 共用的共享内存布局 |
| `gametest` | 输入测试窗口;`--latency` 是延迟测量用的黑白窗口;`--tone HZ` 等 hook 装好再循环放一个单音(测声音),`--tone-now HZ` 一启动就放(造注入前建好的声音流) |
| `launcher` | egui 启动器 |

agent 内部分两层,平台无关的部分不碰任何系统 API:

- `session.rs`:一个浏览器连接一个 PeerConnection(webrtc-rs 0.21)。视频轨 H.264;两个拦截器,一个把 PLI/FIR 转成关键帧请求,一个给视频包带 playout-delay(min = max = 0)让浏览器不攒缓冲
- `host.rs`:`Host` trait,包括订阅编码帧、请求关键帧、打开输入通道、状态提示、退出清理。session 只依赖它
- `platform/<系统>/`:各平台后端,提供 `init` / `preset` / `spawn`。目前只有 `platform/windows`,其他平台编译时走 `unsupported`

### Windows 后端

- **找窗口**:按程序路径、窗口类名、标题匹配,找不到就启动;`--new` 总是新开一个进程(多开)。实例窗口从任务栏移除,退出时还回去
- **采集**
  - 按窗口(默认):WGC `CreateForWindow`,裁出客户区
  - 整屏(`--display N`):窗口去边框、置顶、铺满 `\\.\DISPLAYn` 的工作区,DXGI 桌面复制采这块区域。菜单、提示框、对话框天然在画面里,没有黄框,每帧带合成时刻
- **编码**:直接调 NVENC(FFI),H.264 High,P4 + 超低延迟,CBR 20 Mbps,4 帧 VBV,无限 GOP;收到 PLI/FIR 或每 20 秒出一个 IDR;画面不动时每 500 ms 补编一帧
- **输入**:`PostMessage` 投窗口消息,不动真实的键鼠。`AttachThreadInput` + `SetKeyboardState` 同步按键状态;按键后用"弄脏 1 像素、等 WM_PAINT 画掉"做屏障,保证状态和消息的顺序;会话第一次输入时补发激活消息(焦点伪装),让程序以为自己在前台;按键以 DOM `code` 为准
- **输入隔离**(`--hook`):注入 hook DLL,拦截 `GetAsyncKeyState`、`GetKeyState`、`GetKeyboardState`、`GetCursorPos` / `SetCursorPos` / `ClipCursor`、`GetForegroundWindow` / `GetActiveWindow` / `GetFocus` / `GetCapture`、`GetRawInputData` / `GetRawInputBuffer`、`XInputGetState`,全部改读 agent 写在共享内存(seqlock 快照 + Raw Input 环形缓冲)里的本实例状态
- **声音**(`--hook`):hook 截 WASAPI 输出(在 `ReleaseBuffer` 里把 PCM 抄进共享内存的环,再给原调用加静音标志),agent 每 10 ms 取一次,混成 48 kHz 立体声、Opus 编码后走 WebRTC 音频轨。按实例单独推流,agent 运行期间主机不放这个实例的声音;agent 退出或崩了,300 ms 内声音回到主机扬声器
- **整屏模式的鼠标路由**照着系统分发真实输入的规则:菜单开着时按屏幕坐标发给菜单的捕获窗口;有窗口捕获了鼠标就发给它;否则发给坐标下属于目标进程的窗口,落在非客户区发 `WM_NC*`;点菜单栏改走键盘进菜单

### 跨平台:一路 = 一个"座位"

一路串流 = 一块虚拟屏 + 一路虚拟输入 + 一个音频出口。

- Linux:每路一个无头 Wayland 合成器(参考 Wolf)
- Windows:计划每个实例一块 IDD 虚拟屏 + 现有 hook。第一步已用实体副屏验证了整屏采集;下一步装可编程的虚拟屏驱动(SudoVDA),验证高刷新率和按需建屏
- macOS:隔离手段最弱,多开大概率要靠虚拟机

## 技术痛点

### 1. Windows 只有一个前台,多开的输入隔离只能靠伪装

同一个会话里只有一个前台窗口、一个光标、一份全局键盘状态。用 `SendInput` 发真实输入,所有实例会串台,所以每个实例只能收"投递的消息",随之而来一串问题:

- 投递的消息不带按键状态,程序用 `GetKeyState` 查到的还是旧的 → 同步线程键盘状态 + WM_PAINT 屏障
- 程序会查自己是不是前台、焦点在哪 → 补发激活消息,hook 掉 `GetForegroundWindow` 一族
- `GetAsyncKeyState`、Raw Input、XInput 读的是全局硬件状态 → hook DLL 改读共享内存里本实例的虚拟状态
- 代价:要往目标进程注入 DLL,带反作弊的游戏不支持

### 2. 内核里按真实状态做的判断骗不过去

有些行为由 win32k 按真实的激活、按键状态决定,伪装够不着,只能绕:

- 后台窗口收到 `WM_NCLBUTTONDOWN(HTMENU)` 不开菜单,键盘进菜单却可以 → 点菜单栏时找出点中的是哪一项,按它的访问键走 `SC_KEYMENU`;这次点击的按下、抬起和左键状态都不给目标,否则菜单刚展开就被收起
- 菜单循环把鼠标消息里的坐标当屏幕坐标 → 菜单模式下改发屏幕坐标
- 补发的激活消息只是进了队列,紧跟着的按键查不到焦点窗口会丢 → 补发后等目标处理完再继续

### 3. 按窗口采集在 Win10 上的限制

开发机是 Win10 19045:

- WGC 不接受 tool window、不激活的窗口、没有任务栏按钮的窗口(`E_INVALIDARG`),菜单、提示框采不到
- 被采集的窗口周围有黄框,能关掉要 Win11 / Server 2022
- 点击后 WGC 常先交一帧旧画面,新画面晚一次合成(16.7 ms)才到,窗口大时一半以上的点击都这样

整屏采集(DXGI 桌面复制)没有这些问题,同样 2560×1392 下端到端 p50 48.6 ms,按窗口采集是 55.5 ms。代价是那块屏上的东西都会被采进去,所以需要"一个实例一块屏",也就是虚拟屏。

### 4. 延迟的大头在浏览器

延迟测量把一次点击拆成九段:浏览器→agent、投递、重画+合成、采集、编码、发送、网络、解码、缓冲+显示。主机侧合计十几到二十几毫秒,时间主要花在浏览器:

- Chrome 的 `jitterBufferTarget = 0` 只是目标,它照样按估计的抖动攒缓冲 → 发送端带 playout-delay(min = max = 0),p50 从 92 ms 降到 45 ms
- 剩下二十几毫秒是 `<video>` 的合成管线(解码完到上屏要 1–2 个 vsync)→ 再往下要换 WebCodecs + canvas
- 代价:浏览器不再平滑网络抖动,Wi-Fi 上可能卡顿,上局域网后要再测

### 5. 多开密度和部署

- 消费级 GeForce 显卡的 NVENC 同时编码会话数有上限,驱动许可也不允许数据中心部署
- 声音按实例分开靠 hook 截 WASAPI(Win10 19045 不能按进程环回,Win11 / Server 2022 起可以),所以要 `--hook`。独占模式、空间音频(ISpatialAudioClient)、声音放在子进程里的程序(Electron / CEF)截不到
- 注入之前程序就建好的声音流,hook 没见过它的格式:
  - 只能按系统混音格式去猜
  - 数据明显不对(读不到、放不下、按 f32 看不像声音)就放弃这一路:远端听不到,主机照常放
  - 采样率猜错检查不出来,会变调
- 实例之间没有安全边界(同一用户、同一会话),只适合运营方挑选的应用;要跑用户自带的程序得用虚拟机或容器

### 6. 还没做的

- 拥塞控制:现在固定 CBR,带宽一掉延迟就暴涨 → 接 webrtc-rs 的 GCC,目标码率交给 `nvEncReconfigureEncoder`
- 局域网 HTTPS:非 localhost 的 http 页面不是安全上下文,Gamepad、Keyboard Lock、剪贴板、WebCodecs 都用不了
- 多实例管理界面

计划、实测数据和决策记录见 [docs/PLAN.md](docs/PLAN.md)。

## 快速开始

需要:Windows 10/11 x64、带 NVENC 的 NVIDIA 显卡、Rust(按 `rust-toolchain.toml` 自动安装)、Edge 或 Chrome;跑测试脚本要 Node.js 22+。

```powershell
cargo build --release
target\release\agent.exe --app notepad                              # 采记事本,网页在 8080
target\release\agent.exe --app gametest --new --hook --port 8081    # 再开一个测试窗口,带输入隔离
```

浏览器打开 `http://127.0.0.1:8080`(或对应端口),F2 切换游戏模式(Pointer Lock 相对鼠标)。

| 参数 | 说明 |
|---|---|
| `--app notepad\|gametest` | 内置档案 |
| `--exe` / `--class` / `--title` | 自己指定目标:程序路径、窗口类名(精确)、标题(包含即可) |
| `--new` | 总是新开一个进程(多开) |
| `--hook` | 输入隔离;声音推流也靠它 |
| `--display N` | 整屏采集:窗口铺满 `\\.\DISPLAYn` 的工作区并采这块屏 |
| `--fps 60` / `--bitrate-kbps 20000` | 帧率、码率 |
| `--port 8080` | 网页端口,多开时每个实例一个 |
| `--no-playout-delay` | 关掉 playout-delay,对照测量用 |
| `-- 参数...` | 后面的都传给目标程序 |

## 测试

```powershell
cargo test -p protocol -p agent -p hook-abi -p hook
node scripts/e2e.mjs                    # 两个 gametest 实例同时操作:输入隔离、焦点伪装、Pointer Lock、声音(两路各放 440 / 880 Hz,各听各的)
node scripts/latency.mjs --count 100    # 本机延迟测量,结果存在 target/latency/
node scripts/latency.mjs --display 2    # 整屏采集模式
```

延迟测量要用 release 构建和有头浏览器,数字才真实。

## 目录

```
crates/
  agent/src/
    main.rs、session.rs、keyframe.rs、playout.rs   网页、信令、WebRTC(平台无关)
    host.rs                                        核心类型与 Host trait
    platform/windows/                              采集(按窗口 / 整屏)、NVENC、注入、hook 连接
  protocol/                                        信令与 DataChannel 消息
  hook/、hook-abi/                                 注入目标进程的 DLL 和共享内存布局
  gametest/                                        输入 / 延迟测试窗口
  launcher/                                        egui 启动器
web/                                               浏览器客户端,编译时嵌进 agent
scripts/                                           e2e、延迟测量、多开脚本
docs/PLAN.md                                       计划、实测数据、决策记录
```
