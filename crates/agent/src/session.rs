//! 一个浏览器 = 一条 WebSocket 信令 + 一个 PeerConnection。
//!
//! 浏览器发 offer(recvonly 视频),agent 收集完候选后回完整 answer;
//! 连上后把采集线程广播出来的 H.264 帧逐帧写进视频轨道。

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::net::{IpAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::ws::{Message, WebSocket};
use protocol::{CHANNEL_CONTROL, CHANNEL_INPUT_FAST, CHANNEL_INPUT_KEY, Control, Input, Signal};
use rtc::interceptor::Registry;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RTCRtpHeaderExtensionCapability, RtpCodecKind,
};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::timeout;
use tracing::{info, warn};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::error::Error as RtcError;
use webrtc::media_stream::Track;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};
use webrtc::rtp_transceiver::RtpSender;
use webrtc::runtime::{Receiver, Sender, channel};

use crate::host::{AudioFrame, Host, InputEvent, ProbeTiming, epoch_ms};
use crate::{keyframe, playout};

const GATHER_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 首帧没有上一帧可比,按 60fps 估一个时长
const FIRST_FRAME_DURATION: Duration = Duration::from_micros(16_667);

/// control 通道:浏览器建好之前是 None
type ControlSlot = Arc<std::sync::Mutex<Option<Arc<dyn DataChannel>>>>;

/// 给视频包带 playout-delay(min = max = 0),见 playout.rs。`--no-playout-delay` 关掉,做对照测量用
static PLAYOUT_DELAY: AtomicBool = AtomicBool::new(true);

pub fn set_playout_delay(on: bool) {
    PLAYOUT_DELAY.store(on, Ordering::Relaxed);
}

struct Handler {
    gathered: Sender<()>,
    connected: Sender<()>,
    done: Sender<()>,
    input: std::sync::mpsc::Sender<InputEvent>,
    control: ControlSlot,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let label = dc.label().await.unwrap_or_default();
        if label == CHANNEL_CONTROL {
            info!("数据通道 {label} 已建立");
            *self.control.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&dc));
            let control = Arc::clone(&self.control);
            tokio::spawn(async move {
                // 浏览器不往这条通道发东西,事件照样读走,关闭后清掉
                while let Some(ev) = dc.poll().await {
                    if matches!(ev, DataChannelEvent::OnClose) {
                        break;
                    }
                }
                *control.lock().unwrap_or_else(|e| e.into_inner()) = None;
                info!("数据通道 {CHANNEL_CONTROL} 已关闭");
            });
            return;
        }
        if label != CHANNEL_INPUT_FAST && label != CHANNEL_INPUT_KEY {
            warn!("未知的数据通道 {label:?},忽略");
            return;
        }
        info!("数据通道 {label} 已建立");
        let input = self.input.clone();
        tokio::spawn(async move {
            while let Some(ev) = dc.poll().await {
                match ev {
                    DataChannelEvent::OnMessage(msg) => {
                        let parsed = std::str::from_utf8(&msg.data)
                            .map_err(|e| e.to_string())
                            .and_then(|s| Input::from_json(s).map_err(|e| e.to_string()));
                        match parsed {
                            Ok(ev) => {
                                // 绝对移动太频繁,只在相对模式(Pointer Lock)下打日志
                                if !matches!(ev, Input::MouseMove { rel: false, .. }) {
                                    tracing::debug!("{label} 收到 {ev:?}");
                                }
                                if input.send(InputEvent::now(ev)).is_err() {
                                    break;
                                }
                            }
                            Err(e) => warn!("{label} 收到无法解析的输入: {e}"),
                        }
                    }
                    DataChannelEvent::OnClose => break,
                    _ => {}
                }
            }
            // 通道断开时别留下按住的键
            let _ = input.send(InputEvent::now(Input::ReleaseAll));
            info!("数据通道 {label} 已关闭");
        });
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        info!("PeerConnection 状态: {state}");
        match state {
            RTCPeerConnectionState::Connected => {
                let _ = self.connected.try_send(());
            }
            RTCPeerConnectionState::Failed
            | RTCPeerConnectionState::Disconnected
            | RTCPeerConnectionState::Closed => {
                let _ = self.done.try_send(());
            }
            _ => {}
        }
    }
}

/// 处理一条 `/ws` 连接,直到浏览器断开或 PeerConnection 结束。
pub async fn handle_ws(mut socket: WebSocket, host: Arc<dyn Host>) {
    let offer = loop {
        match socket.recv().await {
            Some(Ok(Message::Text(t))) => match Signal::from_json(t.as_str()) {
                Ok(Signal::Offer { sdp }) => break sdp,
                Ok(other) => warn!("等待 offer 时收到 {other:?},忽略"),
                Err(e) => {
                    send(&mut socket, &Signal::Error { message: format!("信令解析失败: {e}") })
                        .await;
                    return;
                }
            },
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
            Some(Ok(_)) => {}
        }
    };

    if let Err(e) = run(&mut socket, host, offer).await {
        warn!("会话结束: {e:#}");
        send(&mut socket, &Signal::Error { message: format!("{e:#}") }).await;
    } else {
        info!("会话正常结束");
    }
}

async fn send(socket: &mut WebSocket, sig: &Signal) {
    let _ = socket.send(Message::Text(sig.to_json().into())).await;
}

async fn run(socket: &mut WebSocket, host: Arc<dyn Host>, offer: String) -> Result<()> {
    let codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            // 与 NVENC 的 High 输出一致
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032"
                .to_owned(),
            // 不声明的话浏览器不会发 NACK/PLI/FIR
            rtcp_feedback: vec![
                RTCPFeedback { typ: "nack".to_owned(), parameter: String::new() },
                RTCPFeedback { typ: "nack".to_owned(), parameter: "pli".to_owned() },
                RTCPFeedback { typ: "ccm".to_owned(), parameter: "fir".to_owned() },
            ],
        },
        payload_type: 102,
        ..Default::default()
    };
    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(codec.clone(), RtpCodecKind::Video)?;
    media_engine.register_codec(opus_codec(), RtpCodecKind::Audio)?;
    // playout-delay 只按视频注册:音频流协商出来的扩展表里没有它,PlayoutDelayInterceptor 不会碰音频包
    if PLAYOUT_DELAY.load(Ordering::Relaxed) {
        media_engine.register_header_extension(
            RTCRtpHeaderExtensionCapability { uri: playout::URI.to_owned() },
            RtpCodecKind::Video,
            None,
        )?;
    }
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?
        .with(keyframe::slot(), keyframe::KeyframeRequestInterceptor::default())
        .with(playout::slot(), playout::PlayoutDelayInterceptor::default());

    let (gathered_tx, gathered_rx) = channel::<()>(1);
    let (connected_tx, connected_rx) = channel::<()>(1);
    let (done_tx, done_rx) = channel::<()>(1);
    // 发送端随 PeerConnection 一起释放,输入线程随之松开所有按键并退出
    let input = host.open_input().context("启动输入线程")?;
    let control = ControlSlot::default();
    let handler = Arc::new(Handler {
        gathered: gathered_tx,
        connected: connected_tx,
        done: done_tx,
        input,
        control: Arc::clone(&control),
    });

    let runtime = webrtc::runtime::default_runtime().ok_or_else(|| anyhow!("webrtc 没有可用的运行时"))?;
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_runtime(runtime)
        .with_udp_addrs(udp_addrs())
        .build()
        .await?;

    let mut events = Events {
        gathered: gathered_rx,
        connected: connected_rx,
        done: done_rx,
    };
    let result = stream(socket, host.as_ref(), &control, &pc, &codec, offer, &mut events).await;
    let _ = pc.close().await;
    result
}

struct Events {
    gathered: Receiver<()>,
    connected: Receiver<()>,
    done: Receiver<()>,
}

/// Opus:48 kHz 立体声,10 ms 一帧(见 audio.rs)。stereo=1 让浏览器按双声道解
fn opus_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 111,
        ..Default::default()
    }
}

/// 协商出的音频轨的本地 SSRC 和 payload type
async fn audio_params(track: &TrackLocalStaticSample, sender: &Arc<dyn RtpSender>) -> Result<(u32, u8)> {
    let payload_type = sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|c| c.payload_type)
        .ok_or_else(|| anyhow!("没有协商出音频编码"))?;
    let ssrc = *track.ssrcs().await.first().ok_or(RtcError::ErrSenderWithNoSSRCs)?;
    Ok((ssrc, payload_type))
}

/// 会话结束(正常或出错)时中止后台任务
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 把音频帧逐帧写进音频轨。落后了(Lagged)丢掉旧的接着写,广播关了或轨道写不进去就结束
async fn pump_audio(
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
    mut frames: broadcast::Receiver<AudioFrame>,
) {
    loop {
        match frames.recv().await {
            Ok(f) => {
                let sample = Sample { data: f.data, duration: f.duration, ..Sample::new(Instant::now()) };
                if let Err(e) = track.sample_writer(ssrc, payload_type).write_sample(&sample).await {
                    warn!("写音频轨失败,声音停止: {e}");
                    return;
                }
            }
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
    }
}

/// 协商并推流
async fn stream(
    socket: &mut WebSocket,
    host: &dyn Host,
    control: &ControlSlot,
    pc: &impl PeerConnection,
    codec: &RTCRtpCodecParameters,
    offer: String,
    events: &mut Events,
) -> Result<()> {
    let ssrc = RandomState::new().hash_one(Instant::now()) as u32;
    let track = Arc::new(TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            "cyber-remote".to_owned(),
            "notepad-video".to_owned(),
            "notepad".to_owned(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: codec.rtp_codec.clone(),
                ..Default::default()
            }],
        ),
    )?);
    // 先加轨道再设远端描述,answer 里才会把浏览器的 recvonly 段配成 sendonly
    let sender = pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await?;

    // 声音轨,和视频轨在同一个位置加。stream id 必须和视频轨的不同:同一个 MediaStream 里的
    // 音视频,Chrome 会做唇音同步,把视频的播放时刻压到和音频的抖动缓冲一样晚,
    // playout-delay 和 jitterBufferTarget = 0 就白做了。声音是附带的,建不起来只记日志,不影响画面
    let audio = if host.subscribe_audio().is_some() {
        let added: Result<_> = async {
            let audio_ssrc = RandomState::new().hash_one(Instant::now()) as u32;
            let track = Arc::new(TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    "cyber-remote-audio".to_owned(),
                    "audio".to_owned(),
                    "audio".to_owned(),
                    RtpCodecKind::Audio,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters {
                            ssrc: Some(audio_ssrc),
                            ..Default::default()
                        },
                        codec: opus_codec().rtp_codec,
                        ..Default::default()
                    }],
                ),
            )?);
            let sender = pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await?;
            Ok((track, sender))
        }
        .await;
        match added {
            Ok(a) => Some(a),
            Err(e) => {
                warn!("声音轨没建起来,只推画面: {e:#}");
                None
            }
        }
    } else {
        None
    };

    pc.set_remote_description(RTCSessionDescription::offer(offer)?)
        .await
        .context("设置远端 offer")?;
    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer).await?;

    if timeout(GATHER_TIMEOUT, events.gathered.recv()).await.is_err() {
        warn!("ICE 候选收集超时,先用已有候选");
    }
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("没有本地描述"))?;
    send(socket, &Signal::Answer { sdp: local.sdp }).await;
    info!("已发送 answer,等待连接");

    tokio::select! {
        r = timeout(CONNECT_TIMEOUT, events.connected.recv()) => {
            if r.is_err() { bail!("{} 秒内没连上", CONNECT_TIMEOUT.as_secs()); }
        }
        _ = events.done.recv() => bail!("连接失败"),
        _ = wait_closed(socket) => return Ok(()),
    }

    let payload_type = sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|c| c.payload_type)
        .ok_or_else(|| anyhow!("没有协商出视频编码"))?;
    let ssrc = *track.ssrcs().await.first().ok_or(RtcError::ErrSenderWithNoSSRCs)?;
    info!("开始推流 ssrc={ssrc} pt={payload_type}");

    // 声音连上之后才订阅:握手这几秒攒下的旧帧不发,免得凭空多出几百毫秒延迟
    let _audio_task = match &audio {
        Some((track, sender)) => match audio_params(track, sender).await {
            Ok((audio_ssrc, audio_pt)) => host.subscribe_audio().map(|rx| {
                info!("开始推声音 ssrc={audio_ssrc} pt={audio_pt}");
                AbortOnDrop(tokio::spawn(pump_audio(Arc::clone(track), audio_ssrc, audio_pt, rx)))
            }),
            Err(e) => {
                warn!("声音没协商上,只推画面: {e:#}");
                None
            }
        },
        None => None,
    };

    let mut frames = host.subscribe();
    host.request_keyframe();
    let mut prev: Option<Instant> = None;
    let mut told = false;
    loop {
        tokio::select! {
            f = frames.recv() => match f {
                Ok(f) => {
                    // 采集线程在出第一帧之前就装完了输入隔离,这时状态是确定的
                    if !told {
                        told = true;
                        send(socket, &Signal::Info { text: host.status() }).await;
                    }
                    let duration = prev.map_or(FIRST_FRAME_DURATION, |p| {
                        f.captured_at
                            .saturating_duration_since(p)
                            .clamp(Duration::from_millis(1), Duration::from_secs(1))
                    });
                    prev = Some(f.captured_at);
                    track
                        .sample_writer(ssrc, payload_type)
                        .write_sample(&Sample {
                            data: f.data,
                            duration,
                            ..Sample::new(f.captured_at)
                        })
                        .await?;
                    if let Some(p) = f.probe {
                        report_probe(control, p).await;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    warn!("推流落后,丢了 {n} 帧,请求关键帧");
                    host.request_keyframe();
                }
                Err(RecvError::Closed) => bail!("采集线程已停止"),
            },
            // 只有 keyframe 拦截器放行的 PLI/FIR 会到这里
            ev = track.poll() => match ev {
                Some(TrackLocalEvent::OnRtcpPacket(_)) => {
                    info!("浏览器请求关键帧");
                    host.request_keyframe();
                }
                Some(_) => {}
                None => {
                    info!("视频轨道已解绑");
                    return Ok(());
                }
            },
            _ = events.done.recv() => {
                info!("对端断开");
                return Ok(());
            }
            _ = wait_closed(socket) => return Ok(()),
        }
    }
}

/// 读到浏览器关闭 WebSocket 为止;其他消息 Phase 0 不处理
async fn wait_closed(socket: &mut WebSocket) {
    loop {
        match socket.recv().await {
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
            Some(Ok(_)) => {}
        }
    }
}

/// ICE 只绑本机局域网地址和回环,不用 STUN
fn udp_addrs() -> Vec<String> {
    let mut addrs = vec![];
    // connect 一个外部地址只是让系统选出出口网卡,不会真的发包
    let lan = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("8.8.8.8:80").map(|_| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip());
    match lan {
        Ok(ip) if !ip.is_loopback() && !ip.is_unspecified() => addrs.push(format!("{ip}:0")),
        _ => warn!("没找到局域网地址,只用回环"),
    }
    addrs.push(format!("{}:0", IpAddr::from([127, 0, 0, 1])));
    addrs
}

/// 把延迟探针在 agent 内各环节的时刻发给浏览器(PLAN 0.8)。control 通道没建就不发
async fn report_probe(control: &ControlSlot, p: ProbeTiming) {
    let sent = Instant::now();
    let dc = control.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(dc) = dc else { return };
    let msg = Control::Probe {
        id: p.id,
        received: epoch_ms(p.received),
        posted: epoch_ms(p.posted),
        composed: p.composed.map(epoch_ms),
        captured: epoch_ms(p.captured),
        encoded: epoch_ms(p.encoded),
        sent: epoch_ms(sent),
    };
    if let Err(e) = dc.send_text(&msg.to_json()).await {
        warn!("发送延迟探针结果失败: {e}");
    }
}
