//! cyber_remote 主机进程。
//!
//! 采集一个应用窗口 → NVENC H.264 → WebRTC 推给浏览器。
//! HTTP 提供网页 `/`、健康检查 `/health` 和信令 `/ws`。

mod capture;
mod hooklink;
mod inject;
mod keyframe;
mod nvenc;
mod session;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::response::Response;
use axum::{Router, routing::get};
use tracing::info;

use capture::Pipeline;

const INDEX_HTML: &str = include_str!("../../../web/index.html");
const INPUT_JS: &str = include_str!("../../../web/input.js");

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // 让窗口坐标和 WGC 帧都用物理像素,裁剪才对得上
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let args = Args::parse()?;
    info!("推流目标 {:?},{:?}", args.target, args.stream);
    let pipeline = capture::spawn(args.target, args.stream)?;

    let app = Router::new()
        .route("/", get(|| async { axum::response::Html(INDEX_HTML) }))
        .route(
            "/input.js",
            get(|| async { ([(axum::http::header::CONTENT_TYPE, "text/javascript; charset=utf-8")], INPUT_JS) }),
        )
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(ws_handler))
        .with_state(pipeline.clone());

    // 只在本机监听;局域网测试时再改成 0.0.0.0 并加认证
    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("agent listening on http://{addr}");
    let stdin_closed = args.exit_on_stdin_close;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = wait_stdin_closed(), if stdin_closed => info!("stdin 已关闭,退出"),
            }
        })
        .await?;
    // 先撤钩子:目标进程里的钩子立刻回落到真实输入,再恢复任务栏按钮
    pipeline.detach_hook();
    if let Some(hwnd) = pipeline.window() {
        capture::restore_taskbar(hwnd);
    }
    Ok(())
}

/// 启动器关掉管道(或自己退出)时 agent 跟着退,好有机会恢复任务栏按钮
async fn wait_stdin_closed() {
    let _ = tokio::task::spawn_blocking(|| {
        use std::io::Read;
        let mut buf = [0u8; 64];
        let mut stdin = std::io::stdin();
        while matches!(stdin.read(&mut buf), Ok(n) if n > 0) {}
    })
    .await;
}

const USAGE: &str = "用法: agent [--app notepad|gametest] [--exe 程序] [--class 类名] [--title 标题] \
[--fps 60] [--bitrate-kbps 20000] [--port 8080] [--new] [--hook] [--exit-on-stdin-close] \
[-- 程序参数...]";

struct Args {
    target: capture::Target,
    stream: capture::Stream,
    port: u16,
    exit_on_stdin_close: bool,
}
impl Args {
    /// 什么都不给时推记事本。`--app` 先套档案,后面的 `--exe` 等再覆盖
    fn parse() -> Result<Self> {
        let mut it = std::env::args().skip(1);
        let mut target: Option<capture::Target> = None;
        let mut stream = capture::Stream { fps: 60, bitrate: 20_000_000 };
        let mut port = 8080;
        let mut exit_on_stdin_close = false;
        // --app 会整个换掉 target,所以这两个开关先记着,解析完再套上去
        let (mut fresh, mut hook) = (false, false);
        while let Some(a) = it.next() {
            if a == "--" {
                target.get_or_insert_with(Default::default).args = it.by_ref().collect();
                break;
            }
            match a.as_str() {
                "--exit-on-stdin-close" => {
                    exit_on_stdin_close = true;
                    continue;
                }
                "--new" => {
                    fresh = true;
                    continue;
                }
                "--hook" => {
                    hook = true;
                    continue;
                }
                _ => {}
            }
            let v = it.next().ok_or_else(|| anyhow::anyhow!("{a} 后面缺值\n{USAGE}"))?;
            let t = target.get_or_insert_with(Default::default);
            match a.as_str() {
                "--app" => target = Some(capture::Target::preset(&v)?),
                "--exe" => t.exe = Some(v.into()),
                "--class" => t.class = Some(v),
                "--title" => t.title = Some(v),
                "--fps" => stream.fps = v.parse().context("--fps")?,
                "--bitrate-kbps" => stream.bitrate = v.parse::<u32>().context("--bitrate-kbps")? * 1000,
                "--port" => port = v.parse().context("--port")?,
                _ => anyhow::bail!("未知参数 {a}\n{USAGE}"),
            }
        }
        if !(1..=240).contains(&stream.fps) {
            anyhow::bail!("--fps 要在 1..=240 之间");
        }
        if !(100_000..=200_000_000).contains(&stream.bitrate) {
            anyhow::bail!("--bitrate-kbps 要在 100..=200000 之间");
        }
        let mut target = match target {
            Some(t) => t,
            None => capture::Target::preset("notepad")?,
        };
        target.fresh = fresh;
        target.hook = hook;
        if target.exe.is_none() && target.class.is_none() && target.title.is_none() {
            anyhow::bail!("至少给出 --exe、--class、--title 之一\n{USAGE}");
        }
        Ok(Self { target, stream, port, exit_on_stdin_close })
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(pipeline): State<Pipeline>) -> Response {
    ws.on_upgrade(move |socket| session::handle_ws(socket, pipeline))
}
