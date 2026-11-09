//! cyber_remote 启动器:选要推的应用、调推流参数,启停 agent。
//!
//! 配置存在 exe 旁边的 launcher.json。停止时关掉 agent 的 stdin,
//! agent 自己优雅退出(恢复任务栏按钮),超时才强杀。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LOG_LINES: usize = 1000;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("cyber_remote 启动器")
            .with_inner_size([560.0, 680.0]),
        ..Default::default()
    };
    eframe::run_native(
        "cyber_remote 启动器",
        options,
        Box::new(|cc| Ok(Box::new(Launcher::new(cc)))),
    )
}

/// exe 所在目录;agent.exe、gametest.exe 和配置都放这里
fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Into::into))
        .unwrap_or_else(|| ".".into())
}

fn config_path() -> PathBuf {
    exe_dir().join("launcher.json")
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum AppKind {
    Notepad,
    GameTest,
    Custom,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    app: AppKind,
    exe: String,
    /// 空格分隔,带空格的参数用双引号包起来
    args: String,
    class: String,
    title: String,
    fps: u32,
    bitrate_kbps: u32,
    port: u16,
    /// 总是新开一个进程,不复用已打开的窗口(多开)
    fresh: bool,
    /// 给目标进程装输入隔离钩子
    hook: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            app: AppKind::Notepad,
            exe: String::new(),
            args: String::new(),
            class: String::new(),
            title: String::new(),
            fps: 60,
            bitrate_kbps: 20000,
            port: 8080,
            fresh: true,
            hook: true,
        }
    }
}

impl Config {
    fn load() -> Self {
        std::fs::read_to_string(config_path())
            .ok()
            // 记事本另存可能带 BOM,serde_json 不认
            .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).expect("Config 一定能序列化");
        std::fs::write(config_path(), json)
    }

    /// 拼成 agent 的命令行参数,参数不全时返回原因
    fn agent_args(&self) -> Result<Vec<String>, String> {
        let mut a = Vec::new();
        match self.app {
            AppKind::Notepad => a.extend(["--app".into(), "notepad".into()]),
            AppKind::GameTest => a.extend(["--app".into(), "gametest".into()]),
            AppKind::Custom => {
                let exe = self.exe.trim();
                let class = self.class.trim();
                let title = self.title.trim();
                if exe.is_empty() && class.is_empty() && title.is_empty() {
                    return Err("自定义应用至少要填程序、窗口类名、窗口标题之一".into());
                }
                for (flag, v) in [("--exe", exe), ("--class", class), ("--title", title)] {
                    if !v.is_empty() {
                        a.extend([flag.into(), v.into()]);
                    }
                }
            }
        }
        a.extend([
            "--fps".into(),
            self.fps.to_string(),
            "--bitrate-kbps".into(),
            self.bitrate_kbps.to_string(),
            "--port".into(),
            self.port.to_string(),
            "--exit-on-stdin-close".into(),
        ]);
        if self.fresh {
            a.push("--new".into());
        }
        if self.hook {
            a.push("--hook".into());
        }
        if self.app == AppKind::Custom {
            let extra = split_args(&self.args);
            if !extra.is_empty() {
                a.push("--".into());
                a.extend(extra);
            }
        }
        Ok(a)
    }
}

/// 按空格切参数,双引号里的空格保留
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut has = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                has = true;
            }
            c if c.is_whitespace() && !quoted => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

type Log = Arc<Mutex<Vec<String>>>;

fn push_log(log: &Log, line: String) {
    let mut l = log.lock().unwrap();
    l.push(line);
    if l.len() > MAX_LOG_LINES {
        let n = l.len() - MAX_LOG_LINES;
        l.drain(..n);
    }
}

/// 去掉 ANSI 颜色码,以防 agent 没认 NO_COLOR
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn pipe_to_log(src: impl Read + Send + 'static, log: Log, ctx: egui::Context) {
    std::thread::spawn(move || {
        for line in BufReader::new(src).lines() {
            let Ok(line) = line else { break };
            push_log(&log, strip_ansi(&line));
            ctx.request_repaint();
        }
    });
}

struct Agent {
    child: Child,
    /// 置空即关闭管道,通知 agent 退出
    stdin: Option<ChildStdin>,
    stop_requested: Option<Instant>,
    port: u16,
}

impl Agent {
    fn start(cfg: &Config, log: &Log, ctx: &egui::Context) -> Result<Self, String> {
        let args = cfg.agent_args()?;
        let exe = exe_dir().join("agent.exe");
        if !exe.exists() {
            return Err(format!("找不到 {}", exe.display()));
        }
        push_log(log, format!("> agent {}", args.join(" ")));
        let mut child = Command::new(&exe)
            .args(&args)
            .current_dir(exe_dir())
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("启动 agent 失败: {e}"))?;
        pipe_to_log(child.stdout.take().unwrap(), log.clone(), ctx.clone());
        pipe_to_log(child.stderr.take().unwrap(), log.clone(), ctx.clone());
        Ok(Self { stdin: child.stdin.take(), child, stop_requested: None, port: cfg.port })
    }

    fn request_stop(&mut self) {
        self.stdin = None;
        self.stop_requested.get_or_insert_with(Instant::now);
    }

    /// 已退出时返回退出说明;停止超时就强杀
    fn poll(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(format!("agent 已退出({status})")),
            Ok(None) => {
                if self.stop_requested.is_some_and(|t| t.elapsed() > STOP_TIMEOUT) {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    Some("agent 停止超时,已强制结束".into())
                } else {
                    None
                }
            }
            Err(e) => Some(format!("查询 agent 状态失败: {e}")),
        }
    }
}

struct Launcher {
    cfg: Config,
    agent: Option<Agent>,
    log: Log,
    status: String,
    /// 用户关窗口时 agent 还在跑:等它退出后再关
    quit_after_stop: bool,
}

impl Launcher {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);
        Self {
            cfg: Config::load(),
            agent: None,
            log: Log::default(),
            status: String::new(),
            quit_after_stop: false,
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        if let Err(e) = self.cfg.save() {
            push_log(&self.log, format!("保存配置失败: {e}"));
        }
        match Agent::start(&self.cfg, &self.log, ctx) {
            Ok(a) => {
                self.status = format!("推流中 http://127.0.0.1:{}", a.port);
                self.agent = Some(a);
            }
            Err(e) => self.status = e,
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let cfg = &mut self.cfg;
        ui.heading("应用");
        ui.horizontal(|ui| {
            ui.radio_value(&mut cfg.app, AppKind::Notepad, "记事本");
            ui.radio_value(&mut cfg.app, AppKind::GameTest, "gametest");
            ui.radio_value(&mut cfg.app, AppKind::Custom, "自定义程序");
        });
        if cfg.app == AppKind::Custom {
            egui::Grid::new("custom").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("程序");
                ui.horizontal(|ui| {
                    if ui.button("浏览…").clicked() {
                        let mut dlg = rfd::FileDialog::new().add_filter("程序", &["exe"]);
                        if let Some(dir) = std::path::Path::new(cfg.exe.trim()).parent()
                            && dir.is_dir()
                        {
                            dlg = dlg.set_directory(dir);
                        }
                        if let Some(p) = dlg.pick_file() {
                            cfg.exe = p.display().to_string();
                        }
                    }
                    ui.add(egui::TextEdit::singleline(&mut cfg.exe).desired_width(f32::INFINITY));
                });
                ui.end_row();
                ui.label("启动参数");
                ui.add(egui::TextEdit::singleline(&mut cfg.args).desired_width(f32::INFINITY));
                ui.end_row();
                ui.label("窗口类名");
                ui.add(
                    egui::TextEdit::singleline(&mut cfg.class)
                        .hint_text("可选,精确匹配")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
                ui.label("窗口标题");
                ui.add(
                    egui::TextEdit::singleline(&mut cfg.title)
                        .hint_text("可选,包含即可")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
            });
            ui.small("填了类名或标题就只按它们找窗口;都不填则按程序路径找。找不到窗口会先启动程序。");
        }

        ui.add_space(8.0);
        ui.heading("推流");
        egui::Grid::new("stream").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("帧率");
            ui.add(egui::DragValue::new(&mut cfg.fps).range(1..=240).suffix(" fps"));
            ui.end_row();
            ui.label("码率");
            ui.add(
                egui::DragValue::new(&mut cfg.bitrate_kbps)
                    .range(100..=200_000)
                    .speed(100)
                    .suffix(" kbps"),
            );
            ui.end_row();
            ui.label("端口");
            ui.add(egui::DragValue::new(&mut cfg.port).range(1024..=65535));
            ui.end_row();
        });

        ui.add_space(8.0);
        ui.heading("多开与输入隔离");
        ui.checkbox(&mut cfg.fresh, "总是新开进程(不复用已打开的窗口)");
        ui.checkbox(&mut cfg.hook, "输入隔离(向目标进程注入钩子)");
        ui.small(
            "同时开多路请用 scripts/start-multi.ps1(每路一个端口)。\n\
             输入隔离让目标程序只看到这一路浏览器的键鼠。仅支持 64 位程序;\n\
             带反作弊的游戏不要开,也不受支持。",
        );
    }
}

/// 默认字体没有中文,把微软雅黑挂在后面兜底
fn install_cjk_font(ctx: &egui::Context) {
    let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\msyh.ttc") else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("msyh".into(), Arc::new(egui::FontData::from_owned(bytes)));
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(fam).or_default().push("msyh".into());
    }
    ctx.set_fonts(fonts);
}

fn open_browser(url: &str) {
    let _ = Command::new("cmd")
        .args(["/C", "start", "", url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

impl eframe::App for Launcher {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if let Some(a) = &mut self.agent {
            if let Some(msg) = a.poll() {
                push_log(&self.log, msg.clone());
                self.status = msg;
                self.agent = None;
                if self.quit_after_stop {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            } else {
                // 等 agent 退出时持续刷新
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }

        // 关窗口时先让 agent 优雅退出,退完再真正关
        if ctx.input(|i| i.viewport().close_requested()) {
            if let Some(a) = &mut self.agent {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                a.request_stop();
                self.quit_after_stop = true;
                self.status = "正在停止 agent…".into();
            } else {
                let _ = self.cfg.save();
            }
        }

        egui::Panel::bottom("controls").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let running = self.agent.is_some();
                let stopping = self.agent.as_ref().is_some_and(|a| a.stop_requested.is_some());
                if !running {
                    if ui.button("开始推流").clicked() {
                        self.start(&ctx);
                    }
                } else if ui.add_enabled(!stopping, egui::Button::new("停止推流")).clicked() {
                    if let Some(a) = &mut self.agent {
                        a.request_stop();
                        self.status = "正在停止 agent…".into();
                    }
                }
                let port = self.agent.as_ref().map_or(self.cfg.port, |a| a.port);
                if ui.add_enabled(running, egui::Button::new("打开浏览器")).clicked() {
                    open_browser(&format!("http://127.0.0.1:{port}"));
                }
                ui.label(&self.status);
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_enabled_ui(self.agent.is_none(), |ui| self.settings_ui(ui));
            if self.agent.is_some() {
                ui.small("推流中不能改设置,先停止。");
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("日志");
                if ui.small_button("清空").clicked() {
                    self.log.lock().unwrap().clear();
                }
            });
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
                for line in self.log.lock().unwrap().iter() {
                    ui.monospace(line);
                }
            });
        });
    }
}
