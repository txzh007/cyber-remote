//! 声音:各路输出流的 PCM → 48 kHz 立体声 → Opus,平台无关。
//!
//! 平台后端负责把目标进程的声音取出来(Windows 上是 hook 截下的 WASAPI 缓冲),这里只管
//! 格式转换、混音和编码。一路 = 目标进程里的一个输出流,各自的采样率、声道数和写入节奏
//! 都不一样,所以每路单独重采样、单独攒预缓冲,最后相加。

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use bytes::Bytes;
use hook_abi::{AUDIO_F32, AUDIO_I16, AUDIO_I24, AUDIO_I32, AUDIO_U8};
use tracing::warn;
use unsafe_libopus::{
    OPUS_APPLICATION_RESTRICTED_LOWDELAY, OPUS_OK, OPUS_SET_BITRATE_REQUEST,
    OPUS_SET_COMPLEXITY_REQUEST, OpusEncoder, opus_encode_float, opus_encoder_create,
    opus_encoder_ctl, opus_encoder_destroy, opus_strerror,
};

pub const RATE: u32 = 48_000;
/// 10 ms
pub const FRAME: usize = 480;

/// 预缓冲的下限(20 ms)
const MIN_TARGET: usize = 960;
/// 统计"最大单包"的窗口
const PACKET_WINDOW: Duration = Duration::from_secs(1);
/// 这么久没 push 就当这一路结束了
const IDLE: Duration = Duration::from_secs(2);
/// 时钟漂移检测:FIFO 长度的滑动平均
const EMA_ALPHA: f32 = 0.01;
/// 滑动平均偏离预缓冲目标多少帧才开始补偿
const DRIFT_BAND: f32 = 240.0;
/// FIFO 超过目标这么多帧就硬砍
const CAP_OVER: usize = 4800;
const WARN_EVERY: Duration = Duration::from_secs(5);

/// 一个立体声采样帧
type Frame = [f32; 2];

/// 每个采样占几个字节;不认识的格式是 None
pub fn sample_bytes(format: u16) -> Option<usize> {
    Some(match format {
        AUDIO_F32 | AUDIO_I32 => 4,
        AUDIO_I16 => 2,
        AUDIO_I24 => 3,
        AUDIO_U8 => 1,
        _ => return None,
    })
}

/// 日志里用的格式名
pub fn format_name(format: u16) -> &'static str {
    match format {
        AUDIO_F32 => "f32",
        AUDIO_I16 => "i16",
        AUDIO_I24 => "i24",
        AUDIO_I32 => "i32",
        AUDIO_U8 => "u8",
        _ => "未知格式",
    }
}

/// 小端 PCM → f32([-1, 1]),追加到 `out`。不认识的格式什么也不追加
pub fn to_f32(format: u16, bytes: &[u8], out: &mut Vec<f32>) {
    match format {
        AUDIO_F32 => out.extend(
            bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        ),
        AUDIO_I16 => out.extend(
            bytes
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0),
        ),
        AUDIO_I24 => out.extend(bytes.chunks_exact(3).map(|b| {
            // 最高字节当符号字节,放到 i32 的高 24 位再算术右移,符号自然扩展
            let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
            v as f32 / 8_388_608.0
        })),
        AUDIO_I32 => out.extend(
            bytes
                .chunks_exact(4)
                .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0),
        ),
        AUDIO_U8 => out.extend(bytes.iter().map(|&b| (b as f32 - 128.0) / 128.0)),
        _ => {}
    }
}

/// 下混成立体声,追加到 `out`。1 声道 L=R;2 声道原样;3 声道以上按 FL FR FC LFE BL BR SL SR 的
/// 顺序,LFE 丢掉,缺的声道当 0,最后乘 0.5 防止叠起来削波
fn downmix(channels: usize, pcm: &[f32], out: &mut Vec<Frame>) {
    const K: f32 = 0.707;
    match channels {
        0 => {}
        1 => out.extend(pcm.iter().map(|&s| [s, s])),
        2 => out.extend(pcm.chunks_exact(2).map(|c| [c[0], c[1]])),
        n => out.extend(pcm.chunks_exact(n).map(|c| {
            let ch = |i: usize| c.get(i).copied().unwrap_or(0.0);
            let (fl, fr, fc, bl, br, sl, sr) = (ch(0), ch(1), ch(2), ch(4), ch(5), ch(6), ch(7));
            [
                (fl + K * fc + K * bl + K * sl) * 0.5,
                (fr + K * fc + K * br + K * sr) * 0.5,
            ]
        })),
    }
}

/// 线性插值重采样到 48 kHz。小数相位和上一帧跨 push 保留,包边界不会有断点
struct Resampler {
    /// 每输出一帧要前进多少个输入帧
    step: f64,
    /// 下一个输出点相对 `prev` 的位置,[0, 1)
    frac: f64,
    prev: Frame,
    primed: bool,
}

impl Resampler {
    fn new(rate: u32) -> Self {
        Self {
            step: f64::from(rate) / f64::from(RATE),
            frac: 0.0,
            prev: [0.0; 2],
            primed: false,
        }
    }

    fn push(&mut self, input: &[Frame], out: &mut VecDeque<Frame>) {
        // 48 kHz 是最常见的混音格式,不用插值
        if self.step == 1.0 {
            out.extend(input);
            return;
        }
        for &x in input {
            if !self.primed {
                self.prev = x;
                self.primed = true;
                continue;
            }
            // 有了 prev 和 x,就能算出落在两者之间的所有输出点
            while self.frac < 1.0 {
                let f = self.frac as f32;
                out.push_back([
                    self.prev[0] + (x[0] - self.prev[0]) * f,
                    self.prev[1] + (x[1] - self.prev[1]) * f,
                ]);
                self.frac += self.step;
            }
            self.frac -= 1.0;
            self.prev = x;
        }
    }
}

/// 目标进程里的一个输出流
struct Stream {
    rate: u32,
    channels: u16,
    resampler: Resampler,
    fifo: VecDeque<Frame>,
    /// 预缓冲攒够了,正在参与混音
    ready: bool,
    /// pop 之前 FIFO 长度的滑动平均,用来发现两边时钟的漂移
    ema: f32,
    last_push: Instant,
    /// 最近 1 秒的包,每个记换算到 48 kHz 的帧数
    packets: VecDeque<(Instant, usize)>,
    underruns: u32,
}

impl Stream {
    fn new(rate: u32, channels: u16, now: Instant) -> Self {
        Self {
            rate,
            channels,
            resampler: Resampler::new(rate),
            fifo: VecDeque::new(),
            ready: false,
            ema: 0.0,
            last_push: now,
            packets: VecDeque::new(),
            underruns: 0,
        }
    }

    /// 预缓冲目标:最小 20 ms;大缓冲的程序一次写得多,要多攒一个包的量,不然每个包过后都欠载
    fn target(&self) -> usize {
        let biggest = self.packets.iter().map(|&(_, n)| n).max().unwrap_or(0);
        (biggest + FRAME).max(MIN_TARGET)
    }

    /// 送进一包已经下混的帧。积压超过硬上限时丢掉最旧的,返回 true
    fn add(&mut self, input: &[Frame], now: Instant) -> bool {
        let before = self.fifo.len();
        self.resampler.push(input, &mut self.fifo);
        self.packets.push_back((now, self.fifo.len() - before));
        self.last_push = now;
        while self
            .packets
            .front()
            .is_some_and(|&(t, _)| now.saturating_duration_since(t) > PACKET_WINDOW)
        {
            self.packets.pop_front();
        }
        let target = self.target();
        if self.fifo.len() > target + CAP_OVER {
            let excess = self.fifo.len() - target;
            self.fifo.drain(..excess);
            return true;
        }
        false
    }

    /// 取 10 ms 叠加到 `mix` 上
    fn pop_into(&mut self, mix: &mut [f32; FRAME * 2]) {
        let target = self.target();
        if !self.ready {
            if self.fifo.len() < target {
                return;
            }
            self.ready = true;
            self.ema = self.fifo.len() as f32;
        }
        let len = self.fifo.len();
        self.ema += EMA_ALPHA * (len as f32 - self.ema);
        if len < FRAME {
            // 欠载:有多少出多少,这一路回到预缓冲
            for (i, f) in self.fifo.drain(..).enumerate() {
                mix[i * 2] += f[0];
                mix[i * 2 + 1] += f[1];
            }
            self.ready = false;
            self.underruns += 1;
            tracing::debug!("声音: 欠载,累计 {} 次,回到预缓冲", self.underruns);
            return;
        }
        // 输入时钟比输出快,FIFO 越积越多:这次多吃一帧,和相邻帧平均掉。慢了就少吃一帧,补一个重复帧
        let t = target as f32;
        let take = if self.ema > t + DRIFT_BAND && len > FRAME {
            FRAME + 1
        } else if self.ema < t - DRIFT_BAND {
            FRAME - 1
        } else {
            FRAME
        };
        let mut buf = [[0.0f32; 2]; FRAME + 1];
        for (slot, f) in buf.iter_mut().zip(self.fifo.drain(..take)) {
            *slot = f;
        }
        let mut out = [[0.0f32; 2]; FRAME];
        match take {
            n if n > FRAME => {
                let mid = FRAME / 2;
                out[..mid].copy_from_slice(&buf[..mid]);
                out[mid] = [
                    (buf[mid][0] + buf[mid + 1][0]) * 0.5,
                    (buf[mid][1] + buf[mid + 1][1]) * 0.5,
                ];
                out[mid + 1..].copy_from_slice(&buf[mid + 2..=FRAME]);
            }
            n if n < FRAME => {
                out[..n].copy_from_slice(&buf[..n]);
                out[n] = buf[n - 1];
            }
            _ => out.copy_from_slice(&buf[..FRAME]),
        }
        for (i, f) in out.iter().enumerate() {
            mix[i * 2] += f[0];
            mix[i * 2 + 1] += f[1];
        }
    }
}

/// 把目标进程的各路输出流混成一路 48 kHz 立体声
#[derive(Default)]
pub struct Mixer {
    streams: BTreeMap<u32, Stream>,
    last_warn: Option<Instant>,
    /// 下混用的临时缓冲,反复使用免得每包分配
    scratch: Vec<Frame>,
}

impl Mixer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 送进一包 PCM(交错的 f32,`channels` 个声道)。`now` 是这包到达的时刻
    pub fn push(&mut self, stream: u32, rate: u32, channels: u16, pcm: &[f32], now: Instant) {
        if rate == 0 || channels == 0 {
            return;
        }
        let mut frames = std::mem::take(&mut self.scratch);
        frames.clear();
        downmix(usize::from(channels), pcm, &mut frames);
        self.add(stream, rate, Some(channels), &frames, now);
        self.scratch = frames;
    }

    /// 把一路从混音里移走,缓冲着还没混的数据一并丢掉。没有这一路不算错
    pub fn remove(&mut self, stream: u32) {
        self.streams.remove(&stream);
    }

    /// 送进一段静音:程序写了 `frames` 帧的静音标记,没有数据,但它占着这么长的时间
    pub fn push_silence(&mut self, stream: u32, rate: u32, frames: u32, now: Instant) {
        if rate == 0 {
            return;
        }
        let mut zeros = std::mem::take(&mut self.scratch);
        zeros.clear();
        zeros.resize(frames as usize, [0.0; 2]);
        self.add(stream, rate, None, &zeros, now);
        self.scratch = zeros;
    }

    fn add(&mut self, id: u32, rate: u32, channels: Option<u16>, frames: &[Frame], now: Instant) {
        // 格式变了(同一个流编号换了采样率或声道数)就重新来,旧的重采样状态不能沿用
        let fits = self
            .streams
            .get(&id)
            .is_some_and(|s| s.rate == rate && channels.is_none_or(|c| c == s.channels));
        if !fits {
            self.streams
                .insert(id, Stream::new(rate, channels.unwrap_or(2), now));
        }
        let capped = self
            .streams
            .get_mut(&id)
            .is_some_and(|s| s.add(frames, now));
        if capped
            && self
                .last_warn
                .is_none_or(|t| now.saturating_duration_since(t) >= WARN_EVERY)
        {
            self.last_warn = Some(now);
            warn!("声音: 第 {id} 路积压太多,丢掉最旧的");
        }
    }

    /// 取 10 ms 的混音(交错立体声)。没有任何一路在播时是全零
    pub fn pop(&mut self, now: Instant) -> [f32; FRAME * 2] {
        self.streams
            .retain(|_, s| now.saturating_duration_since(s.last_push) < IDLE);
        let mut mix = [0.0f32; FRAME * 2];
        for s in self.streams.values_mut() {
            s.pop_into(&mut mix);
        }
        for v in &mut mix {
            *v = v.clamp(-1.0, 1.0);
        }
        mix
    }
}

/// 10 ms 一帧的 Opus 编码器。CELT 专用的低延迟模式,算法延迟最低
pub struct Encoder {
    st: *mut OpusEncoder,
}

// 编码器状态由本结构独占,可以挪到别的线程用
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new() -> Result<Self> {
        let mut err = 0;
        let st = unsafe {
            opus_encoder_create(
                RATE as i32,
                2,
                OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut err,
            )
        };
        if st.is_null() || err != OPUS_OK {
            bail!("创建 Opus 编码器失败: {}", opus_strerror(err));
        }
        let enc = Self { st };
        // 调不了就用库的默认值,不影响出声
        let set = |name: &str, request: i32, value: i32| {
            let r = unsafe { opus_encoder_ctl!(st, request, value) };
            if r != OPUS_OK {
                warn!("Opus {name} 设置失败({}),用默认值", opus_strerror(r));
            }
        };
        set("码率", OPUS_SET_BITRATE_REQUEST, 128_000);
        set("复杂度", OPUS_SET_COMPLEXITY_REQUEST, 5);
        Ok(enc)
    }

    pub fn encode(&mut self, pcm: &[f32; FRAME * 2]) -> Result<Bytes> {
        let mut out = [0u8; 1500];
        let n = unsafe {
            opus_encode_float(
                self.st,
                pcm.as_ptr(),
                FRAME as i32,
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        if n < 0 {
            bail!("Opus 编码失败: {}", opus_strerror(n));
        }
        Ok(Bytes::copy_from_slice(&out[..n as usize]))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.st) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// 用来造时刻:`t0 + ms`
    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn sine(i: usize, hz: f32, rate: f32) -> f32 {
        (TAU * hz * i as f32 / rate).sin()
    }

    /// 上升过零点之间的平均间隔换算出的频率
    fn zero_cross_hz(samples: &[f32], rate: f32) -> f32 {
        let crossings: Vec<usize> = (1..samples.len())
            .filter(|&i| samples[i - 1] < 0.0 && samples[i] >= 0.0)
            .collect();
        assert!(crossings.len() > 10, "过零点太少: {}", crossings.len());
        let span = (crossings[crossings.len() - 1] - crossings[0]) as f32 / rate;
        (crossings.len() - 1) as f32 / span
    }

    #[test]
    fn to_f32_each_format() {
        let mut out = vec![];
        let f: Vec<u8> = [0.5f32, -0.25]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        to_f32(AUDIO_F32, &f, &mut out);
        assert_eq!(out, [0.5, -0.25]);

        out.clear();
        let i16s: Vec<u8> = [16384i16, -32768]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        to_f32(AUDIO_I16, &i16s, &mut out);
        assert_eq!(out, [0.5, -1.0]);

        // 3 字节紧凑:0x400000 = +0.5,0x800000 符号扩展成 -8388608 = -1.0,0xFFFFFF = -1
        out.clear();
        to_f32(
            AUDIO_I24,
            &[0x00, 0x00, 0x40, 0x00, 0x00, 0x80, 0xFF, 0xFF, 0xFF],
            &mut out,
        );
        assert_eq!(out, [0.5, -1.0, -1.0 / 8_388_608.0]);

        out.clear();
        let i32s: Vec<u8> = [0x4000_0000i32, i32::MIN]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        to_f32(AUDIO_I32, &i32s, &mut out);
        assert_eq!(out, [0.5, -1.0]);

        out.clear();
        to_f32(AUDIO_U8, &[128, 0, 255], &mut out);
        assert_eq!(out, [0.0, -1.0, 127.0 / 128.0]);

        // 不认识的格式不追加;残缺的尾巴(不够一个采样)丢掉
        out.clear();
        to_f32(99, &[1, 2, 3, 4], &mut out);
        to_f32(AUDIO_I16, &[1, 0, 9], &mut out);
        assert_eq!(out, [1.0 / 32768.0]);
    }

    #[test]
    fn downmix_layouts() {
        let mut out = vec![];
        downmix(1, &[0.3], &mut out);
        downmix(2, &[0.1, 0.2], &mut out);
        assert_eq!(out, [[0.3, 0.3], [0.1, 0.2]]);

        // 5.1:FL FR FC LFE BL BR。FC 同时进左右,LFE 丢掉,再乘 0.5
        out.clear();
        downmix(6, &[0.0, 0.0, 1.0, 0.0, 0.0, 0.0], &mut out);
        assert!((out[0][0] - 0.3535).abs() < 1e-4 && out[0][0] == out[0][1]);
        out.clear();
        downmix(6, &[0.0, 0.0, 0.0, 1.0, 0.0, 0.0], &mut out);
        assert_eq!(out, [[0.0, 0.0]]);
        // 7.1:SL 只进左,SR 只进右
        out.clear();
        downmix(8, &[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0], &mut out);
        assert!((out[0][0] - 0.3535).abs() < 1e-4 && out[0][1] == 0.0);
        // 3 声道只有 FL FR FC,后面的声道当 0
        out.clear();
        downmix(3, &[1.0, 1.0, 0.0], &mut out);
        assert_eq!(out, [[0.5, 0.5]]);
    }

    #[test]
    fn resample_44k1_keeps_pitch() {
        let mut r = Resampler::new(44_100);
        let input: Vec<Frame> = (0..44_100)
            .map(|i| [sine(i, 1000.0, 44_100.0); 2])
            .collect();
        let mut out = VecDeque::new();
        // 小块送,包边界上的相位和上一帧要接得上
        for chunk in input.chunks(441) {
            r.push(chunk, &mut out);
        }
        // 一秒输入换算成约一秒输出
        assert!(
            (out.len() as i64 - 48_000).abs() <= 2,
            "输出 {} 帧",
            out.len()
        );
        let left: Vec<f32> = out.iter().map(|f| f[0]).collect();
        let hz = zero_cross_hz(&left, 48_000.0);
        assert!((hz - 1000.0).abs() < 10.0, "频率 {hz}");
        // 包边界没有断点:相邻采样的差不会比正弦本身的最大斜率(2π·1000/48000 ≈ 0.131)大多少
        let max_step = left
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(max_step < 0.15, "最大相邻差 {max_step}");
    }

    #[test]
    fn prebuffer_outputs_silence_until_target() {
        let t0 = Instant::now();
        let mut m = Mixer::new();
        m.push(1, RATE, 1, &[0.5; 480], at(t0, 0));
        assert!(m.pop(at(t0, 0)).iter().all(|&v| v == 0.0), "没攒够就出声了");
        m.push(1, RATE, 1, &[0.5; 479], at(t0, 10));
        assert!(
            m.pop(at(t0, 10)).iter().all(|&v| v == 0.0),
            "差一帧也不该出声"
        );
        m.push(1, RATE, 1, &[0.5; 1], at(t0, 20));
        // 960 帧到了,这次 pop 开始出声
        assert!(m.pop(at(t0, 20)).iter().all(|&v| v == 0.5));
    }

    #[test]
    fn prebuffer_grows_with_big_packets() {
        let t0 = Instant::now();
        let mut m = Mixer::new();
        // 一次写 100 ms:目标 = 4800 + 480,一包不够
        m.push(1, RATE, 1, &[0.5; 4800], at(t0, 0));
        assert!(m.pop(at(t0, 0)).iter().all(|&v| v == 0.0));
        assert_eq!(m.streams[&1].target(), 5280);
        m.push(1, RATE, 1, &[0.5; 4800], at(t0, 100));
        assert!(m.pop(at(t0, 100)).iter().all(|&v| v == 0.5));
    }

    #[test]
    fn steady_state_is_input_delayed() {
        let t0 = Instant::now();
        let mut m = Mixer::new();
        let signal: Vec<f32> = (0..100 * FRAME)
            .map(|i| 0.5 * sine(i, 440.0, 48_000.0))
            .collect();
        let mut out = vec![];
        for (k, chunk) in signal.chunks(FRAME).enumerate() {
            let now = at(t0, k as u64 * 10);
            m.push(1, RATE, 1, chunk, now);
            out.extend(m.pop(now).chunks_exact(2).map(|f| f[0]));
        }
        assert_eq!(m.streams[&1].underruns, 0, "预缓冲之后不该欠载");
        // 头一个 10 ms 在攒预缓冲,之后输出 = 输入整整晚一块
        assert!(out[..FRAME].iter().all(|&v| v == 0.0));
        assert_eq!(&out[FRAME..], &signal[..signal.len() - FRAME]);
    }

    /// 输入时钟是输出的 `ratio` 倍,跑 `secs` 秒。返回 pop 之前 FIFO 长度的 (最小, 最大)、欠载次数
    fn simulate_drift(ratio: f64, secs: u64) -> (usize, usize, u32, usize) {
        let t0 = Instant::now();
        let mut m = Mixer::new();
        let (mut min, mut max, mut pushed) = (usize::MAX, 0, 0usize);
        let mut target = 0;
        for k in 0..secs * 100 {
            let now = at(t0, k * 10);
            // 累计应该送进去的帧数,每 10 ms 补齐到它
            let due = ((k + 1) as f64 * FRAME as f64 * ratio) as usize;
            let n = due - pushed;
            pushed = due;
            m.push(1, RATE, 1, &vec![0.1; n], now);
            let s = &m.streams[&1];
            if s.ready {
                min = min.min(s.fifo.len());
                max = max.max(s.fifo.len());
                target = s.target();
            }
            m.pop(now);
        }
        (min, max, m.streams[&1].underruns, target)
    }

    #[test]
    fn clock_drift_is_compensated() {
        // 输入快 0.1%:不补偿的话 60 秒会多出 2880 帧,FIFO 涨到 3800 以上
        let (min, max, underruns, target) = simulate_drift(1.001, 60);
        eprintln!("1.001: fifo {min}..{max} 目标 {target}");
        assert_eq!(underruns, 0);
        assert!(
            min >= target - 50 && max <= target + 1000,
            "FIFO 长度 {min}..{max},目标 {target}"
        );
        // 输入慢 0.1%:反方向,不补偿的话 FIFO 会被抽干而欠载
        let (min, max, underruns, target) = simulate_drift(0.999, 60);
        eprintln!("0.999: fifo {min}..{max} 目标 {target}");
        assert_eq!(underruns, 0);
        assert!(
            min >= target - 400 && max <= target + 1000,
            "FIFO 长度 {min}..{max},目标 {target}"
        );
    }

    #[test]
    fn hard_cap_drops_oldest() {
        let t0 = Instant::now();
        let mut m = Mixer::new();
        // 一直不 pop:每包的值不同,超过 目标 + 4800 就砍回目标,留下的是最新的
        for k in 0..30u64 {
            m.push(1, RATE, 1, &[k as f32 / 100.0; FRAME], at(t0, k * 10));
            let s = &m.streams[&1];
            assert!(
                s.fifo.len() <= s.target() + CAP_OVER,
                "第 {k} 包后积压 {}",
                s.fifo.len()
            );
        }
        let s = &m.streams[&1];
        assert!(s.fifo.len() < 30 * FRAME, "一帧也没丢");
        assert_eq!(s.fifo.back().unwrap()[0], 0.29);
        assert!(s.fifo.front().unwrap()[0] > 0.0, "最旧的包还在");
    }

    #[test]
    fn two_streams_add_and_clamp() {
        let t0 = Instant::now();
        // 每路 480 帧一包,送两包刚好攒够预缓冲;一路立体声一路单声道
        let feed = |m: &mut Mixer, a: f32, b: f32| {
            for ms in [0, 10] {
                m.push(1, RATE, 2, &[a; FRAME * 2], at(t0, ms));
                m.push(2, RATE, 1, &[b; FRAME], at(t0, ms));
            }
        };
        let mut m = Mixer::new();
        feed(&mut m, 0.25, 0.5);
        let mix = m.pop(at(t0, 10));
        assert!(mix.iter().all(|&v| (v - 0.75).abs() < 1e-6));
        // 相加超过 1 就钳住
        let mut m = Mixer::new();
        feed(&mut m, 0.8, 0.8);
        let mix = m.pop(at(t0, 10));
        assert!(mix.iter().all(|&v| v == 1.0));
    }

    #[test]
    fn silence_keeps_time_and_idle_stream_is_removed() {
        let t0 = Instant::now();
        let mut m = Mixer::new();
        m.push(1, RATE, 2, &[0.5; 1920], at(t0, 0));
        // 静音段占的时间和数据一样长,混出来的是零
        m.push_silence(1, RATE, 960, at(t0, 10));
        assert_eq!(m.streams[&1].fifo.len(), 1920);
        m.pop(at(t0, 10));
        // 2 秒没 push,这一路就移除了
        m.pop(at(t0, 10 + 1999));
        assert_eq!(m.streams.len(), 1);
        let mix = m.pop(at(t0, 10 + 2000));
        assert!(m.streams.is_empty());
        assert!(mix.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn remove_drops_only_that_stream() {
        let t0 = Instant::now();
        let mut m = Mixer::new();
        // 两路各送两包,攒够预缓冲,混出来是 0.25 + 0.5
        for ms in [0, 10] {
            m.push(1, RATE, 2, &[0.25; FRAME * 2], at(t0, ms));
            m.push(2, RATE, 1, &[0.5; FRAME], at(t0, ms));
        }
        assert!(m.pop(at(t0, 10)).iter().all(|&v| (v - 0.75).abs() < 1e-6));
        m.remove(1);
        assert!(!m.streams.contains_key(&1) && m.streams.contains_key(&2));
        // 移走的那路不再出声,另一路不受影响
        assert!(m.pop(at(t0, 20)).iter().all(|&v| v == 0.5));
        // 没有的流、已经移走的流,再移也不要紧
        m.remove(1);
        m.remove(99);
        assert_eq!(m.streams.len(), 1);
        // 移走之后同一个编号再来包,是全新的一路:旧的缓冲没了,要重新攒预缓冲
        m.push(1, RATE, 2, &[0.25; FRAME * 2], at(t0, 30));
        assert_eq!(m.streams[&1].fifo.len(), FRAME);
        assert!(m.pop(at(t0, 30)).iter().all(|&v| v == 0.0));
    }

    #[test]
    fn format_change_restarts_stream() {
        let t0 = Instant::now();
        let mut m = Mixer::new();
        m.push(1, 44_100, 2, &[0.5; 882], at(t0, 0));
        assert_eq!(m.streams[&1].rate, 44_100);
        m.push(1, RATE, 2, &[0.5; 960], at(t0, 10));
        let s = &m.streams[&1];
        assert_eq!((s.rate, s.fifo.len()), (RATE, 480));
    }

    #[test]
    fn opus_roundtrip_keeps_pitch() {
        use unsafe_libopus::{opus_decode_float, opus_decoder_create, opus_decoder_destroy};

        let mut enc = Encoder::new().unwrap();
        let mut err = 0;
        let dec = unsafe { opus_decoder_create(RATE as i32, 2, &mut err) };
        assert!(!dec.is_null() && err == OPUS_OK);

        let mut decoded = vec![];
        let mut pcm = [0.0f32; FRAME * 2];
        for k in 0..100 {
            for i in 0..FRAME {
                let v = 0.5 * sine(k * FRAME + i, 440.0, 48_000.0);
                pcm[i * 2] = v;
                pcm[i * 2 + 1] = v;
            }
            let packet = enc.encode(&pcm).unwrap();
            assert!(!packet.is_empty() && packet.len() <= 1500);
            let mut out = [0.0f32; FRAME * 2];
            let n = unsafe {
                opus_decode_float(
                    dec,
                    packet.as_ptr(),
                    packet.len() as i32,
                    out.as_mut_ptr(),
                    FRAME as i32,
                    0,
                )
            };
            assert_eq!(n, FRAME as i32);
            decoded.extend(out.chunks_exact(2).map(|f| f[0]));
        }
        unsafe { opus_decoder_destroy(dec) };
        // 跳过开头的编码器启动过渡
        let hz = zero_cross_hz(&decoded[FRAME * 10..], 48_000.0);
        assert!((hz - 440.0).abs() < 440.0 * 0.02, "解回来 {hz} Hz");
        let peak = decoded[FRAME * 10..]
            .iter()
            .fold(0.0f32, |a, &v| a.max(v.abs()));
        assert!((0.4..0.6).contains(&peak), "幅度 {peak}");
    }
}
