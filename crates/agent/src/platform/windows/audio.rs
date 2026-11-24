//! 声音线程:每 10 ms 从 hook 的环里取一次 PCM,混成 48 kHz 立体声,Opus 编码后广播给各个会话。
//!
//! 这个线程的心跳(`alive_ms`)就是 hook 的开关:心跳新鲜,hook 才截目标进程的声音并给它静音;
//! 线程停了(或 agent 崩了)300 ms 内,声音自动回到主机扬声器。没有会话订阅时照样取、照样混
//! (环要排空,混音器的时钟也要走),只是不编码。

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hook_abi::{AUDIO_PACKET_ABANDONED, AUDIO_PACKET_SILENT, AudioReader};
use tokio::sync::broadcast;
use tracing::{info, warn};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::PCWSTR;

use super::hooklink::AudioLink;
use crate::audio::{self, Encoder, Mixer};
use crate::host::AudioFrame;

const TICK: Duration = Duration::from_millis(10);
/// 落后超过这么久(比如机器睡了一觉)就重新对齐,不去补发这段时间欠的帧
const MAX_LAG: Duration = Duration::from_millis(100);
/// 警告的最小间隔
const WARN_EVERY: Duration = Duration::from_secs(5);

/// 睡到绝对时刻。优先用高精度可等待定时器(Win10 1803 起,精度约 0.5 ms),建不了就退回 `thread::sleep`
struct Ticker(Option<HANDLE>);

impl Ticker {
    fn new() -> Self {
        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        };
        if let Err(e) = &timer {
            warn!("高精度定时器建不了({e}),声音线程退回 thread::sleep,节拍会抖");
        }
        Self(timer.ok())
    }

    fn sleep_until(&self, deadline: Instant) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        if let Some(timer) = self.0 {
            // 负数表示相对时间,单位 100 ns
            let due = -((left.as_nanos() / 100) as i64);
            if unsafe { SetWaitableTimer(timer, &due, 0, None, None, false) }.is_ok() {
                unsafe { WaitForSingleObject(timer, INFINITE) };
                return;
            }
        }
        std::thread::sleep(left);
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        if let Some(timer) = self.0 {
            unsafe {
                let _ = CloseHandle(timer);
            }
        }
    }
}

/// 限频:距上次放行至少 `WARN_EVERY` 才返回 true
fn gate(last: &mut Option<Instant>, now: Instant) -> bool {
    let ok = last.is_none_or(|t| now.saturating_duration_since(t) >= WARN_EVERY);
    if ok {
        *last = Some(now);
    }
    ok
}

/// 共享内存是别的进程写的,包头里的数字不能全信:离谱的包直接丢,免得按它分配出一大块内存
fn plausible(h: &hook_abi::AudioHeader) -> bool {
    (8_000..=768_000).contains(&h.rate)
        && (1..=32).contains(&h.channels)
        // 一个包不会超过 2 秒
        && u64::from(h.frames) <= u64::from(h.rate) * 2
}

pub struct AudioThread {
    stop: Arc<AtomicBool>,
    link: AudioLink,
    handle: Option<JoinHandle<()>>,
}

impl AudioThread {
    pub fn start(link: AudioLink, tx: broadcast::Sender<AudioFrame>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = std::thread::Builder::new()
            .name("audio".into())
            .spawn({
                let (link, stop) = (link.clone(), Arc::clone(&stop));
                move || run(&link, &tx, &stop)
            })
            .context("启动声音线程")?;
        Ok(Self {
            stop,
            link,
            handle: Some(handle),
        })
    }

    /// 停线程并清掉心跳,目标进程的声音立刻回到主机扬声器
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.link.ring().alive_ms.store(0, Ordering::Relaxed);
    }
}

impl Drop for AudioThread {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(link: &AudioLink, tx: &broadcast::Sender<AudioFrame>, stop: &AtomicBool) {
    let ring = link.ring();
    let mut encoder = match Encoder::new() {
        Ok(e) => e,
        Err(e) => {
            // 心跳不开始,hook 就不会截声音,目标的声音照常走主机扬声器
            warn!("声音未启用: {e:#}");
            return;
        }
    };
    let mut reader = AudioReader::new(ring);
    let mut mixer = Mixer::new();
    let ticker = Ticker::new();
    let mut pcm = Vec::new();
    let mut seen = HashSet::new();
    let mut last_warn = None;
    let mut next = Instant::now() + TICK;
    info!("声音线程已启动");

    while !stop.load(Ordering::Relaxed) {
        ticker.sleep_until(next);
        let now = Instant::now();
        // 心跳:hook 看到它新鲜才截声音
        ring.alive_ms
            .store(unsafe { GetTickCount64() }, Ordering::Relaxed);

        let lost = reader.drain(ring, |h, data| {
            // 放弃包只带流编号,别的字段不看:这一路之后不会再有包,马上从混音里移走
            if h.flags & AUDIO_PACKET_ABANDONED != 0 {
                mixer.remove(h.stream);
                warn!(
                    "声音: 第 {} 路在注入前就建好了,格式猜不对,放弃这一路(主机照常放)",
                    h.stream
                );
                return;
            }
            if !plausible(h) {
                return;
            }
            if seen.insert(h.stream) {
                info!(
                    "声音: 第 {} 路 {} Hz {} 声道 {}",
                    h.stream,
                    h.rate,
                    h.channels,
                    audio::format_name(h.format)
                );
            }
            if h.flags & AUDIO_PACKET_SILENT != 0 {
                mixer.push_silence(h.stream, h.rate, h.frames, now);
                return;
            }
            let Some(sample) = audio::sample_bytes(h.format) else {
                return;
            };
            // 数据后面带着对齐填充,按帧数截出真正的那部分
            let want = h.frames as usize * usize::from(h.channels) * sample;
            pcm.clear();
            audio::to_f32(h.format, &data[..want.min(data.len())], &mut pcm);
            mixer.push(h.stream, h.rate, h.channels, &pcm, now);
        });
        if lost && gate(&mut last_warn, now) {
            warn!("声音: 读得太慢,环里的数据被覆盖,丢了一段");
        }

        if now.saturating_duration_since(next) > MAX_LAG {
            if gate(&mut last_warn, now) {
                warn!(
                    "声音线程落后 {} ms,重新对齐",
                    now.saturating_duration_since(next).as_millis()
                );
            }
            next = now;
        }
        while now >= next {
            let mix = mixer.pop(now);
            // 没人听就不编码;但帧照样 pop,混音器的时钟要走
            if tx.receiver_count() > 0 {
                match encoder.encode(&mix) {
                    Ok(data) => {
                        let _ = tx.send(AudioFrame {
                            data,
                            duration: TICK,
                        });
                    }
                    Err(e) => {
                        if gate(&mut last_warn, now) {
                            warn!("声音: {e:#}");
                        }
                    }
                }
            }
            next += TICK;
        }
    }
    ring.alive_ms.store(0, Ordering::Relaxed);
    info!("声音线程已停止");
}
