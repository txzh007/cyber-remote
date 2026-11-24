//! cyber_remote 主机进程。
//!
//! 推一路应用画面给浏览器。找到或启动应用、采集编码、注入输入由平台后端(`platform`)做;
//! 这里和 `session` 只管 HTTP、信令和 WebRTC,不直接调平台 API。
//! HTTP 提供网页 `/`、健康检查 `/health` 和信令 `/ws`。

// 只有 Windows 后端用它(hook 截声音),其他平台编译时不算死代码
#[cfg_attr(not(windows), allow(dead_code))]
mod audio;
mod host;
mod keyframe;
mod platform;
mod playout;
mod session;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::response::Response;
use axum::{Router, routing::get};
use tracing::info;

use host::Host;

const INDEX_HTML: &str = include_str!("../../../web/index.html");
const INPUT_JS: &str = include_str!("../../../web/input.js");
const LATENCY_JS: &str = include_str!("../../../web/latency.js");

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    platform::init();
    let args = Args::parse()?;
    session::set_playout_delay(args.playout_delay);
    info!("推流目标 {:?},{:?}", args.target, args.stream);
    let host = platform::spawn(args.target, args.stream)?;

    let app = Router::new()
        .route("/", get(|| async { axum::response::Html(INDEX_HTML) }))
        .route("/input.js", get(|| async { js(INPUT_JS) }))
        .route("/latency.js", get(|| async { js(LATENCY_JS) }))
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(ws_handler))
        .with_state(Arc::clone(&host));

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
    host.shutdown();
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

const USAGE: &str = "用法: agent [--app notepad|gametest] [--exe 程序] [--class 类名] [--title 标题] [--display N] \
[--fps 60] [--bitrate-kbps 20000] [--port 8080] [--new] [--hook] [--exit-on-stdin-close] [--no-playout-delay] \
[-- 程序参数...]";

struct Args {
    target: host::Target,
    stream: host::Stream,
    port: u16,
    exit_on_stdin_close: bool,
    /// 视频包带 playout-delay(min = max = 0)
    playout_delay: bool,
}
impl Args {
    /// 什么都不给时推记事本。`--app` 先套档案,后面的 `--exe` 等再覆盖
    fn parse() -> Result<Self> {
        let mut it = std::env::args().skip(1);
        let mut target: Option<host::Target> = None;
        let mut stream = host::Stream { fps: 60, bitrate: 20_000_000 };
        let mut port = 8080;
        let mut exit_on_stdin_close = false;
        let mut playout_delay = true;
        // --app 会整个换掉 target,所以这几个选项先记着,解析完再套上去
        let (mut fresh, mut hook, mut display) = (false, false, None);
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
                "--no-playout-delay" => {
                    playout_delay = false;
                    continue;
                }
                _ => {}
            }
            let v = it.next().ok_or_else(|| anyhow::anyhow!("{a} 后面缺值\n{USAGE}"))?;
            let t = target.get_or_insert_with(Default::default);
            match a.as_str() {
                "--app" => target = Some(platform::preset(&v)?),
                "--exe" => t.exe = Some(v.into()),
                "--class" => t.class = Some(v),
                "--title" => t.title = Some(v),
                "--display" => display = Some(v.parse().context("--display")?),
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
            None => platform::preset("notepad")?,
        };
        target.fresh = fresh;
        target.hook = hook;
        target.display = display;
        if target.exe.is_none() && target.class.is_none() && target.title.is_none() {
            anyhow::bail!("至少给出 --exe、--class、--title 之一\n{USAGE}");
        }
        Ok(Self { target, stream, port, exit_on_stdin_close, playout_delay })
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(host): State<Arc<dyn Host>>) -> Response {
    ws.on_upgrade(move |socket| session::handle_ws(socket, host))
}

fn js(body: &'static str) -> impl axum::response::IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "text/javascript; charset=utf-8")], body)
}
