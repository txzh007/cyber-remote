//! 一个浏览器 = 一条 WebSocket 信令 + 一个 PeerConnection。
//!
//! 浏览器发 offer(recvonly 视频),agent 收集完候选后回完整 answer;
//! 连上后把采集线程广播出来的 H.264 帧逐帧写进视频轨道。

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::net::{IpAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::ws::{Message, WebSocket};
use protocol::{CHANNEL_INPUT_FAST, CHANNEL_INPUT_KEY, Input, Signal};
use rtc::interceptor::Registry;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MediaEngine};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RtpCodecKind,
};
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
use webrtc::runtime::{Receiver, Sender, channel};

use crate::capture::Pipeline;
use crate::{inject, keyframe};

const GATHER_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 首帧没有上一帧可比,按 60fps 估一个时长
const FIRST_FRAME_DURATION: Duration = Duration::from_micros(16_667);

struct Handler {
    gathered: Sender<()>,
    connected: Sender<()>,
    done: Sender<()>,
    input: std::sync::mpsc::Sender<Input>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let label = dc.label().await.unwrap_or_default();
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
                                if !matches!(ev, Input::MouseMove { .. }) {
                                    tracing::debug!("{label} 收到 {ev:?}");
                                }
                                if input.send(ev).is_err() {
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
            let _ = input.send(Input::ReleaseAll);
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
pub async fn handle_ws(mut socket: WebSocket, pipeline: Pipeline) {
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

    if let Err(e) = run(&mut socket, pipeline, offer).await {
        warn!("会话结束: {e:#}");
        send(&mut socket, &Signal::Error { message: format!("{e:#}") }).await;
    } else {
        info!("会话正常结束");
    }
}

async fn send(socket: &mut WebSocket, sig: &Signal) {
    let _ = socket.send(Message::Text(sig.to_json().into())).await;
}

async fn run(socket: &mut WebSocket, pipeline: Pipeline, offer: String) -> Result<()> {
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
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?
        .with(keyframe::slot(), keyframe::KeyframeRequestInterceptor::default());

    let (gathered_tx, gathered_rx) = channel::<()>(1);
    let (connected_tx, connected_rx) = channel::<()>(1);
    let (done_tx, done_rx) = channel::<()>(1);
    // 发送端随 PeerConnection 一起释放,输入线程随之松开所有按键并退出
    let input = inject::spawn(pipeline.clone()).context("启动输入线程")?;
    let handler = Arc::new(Handler {
        gathered: gathered_tx,
        connected: connected_tx,
        done: done_tx,
        input,
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
    let result = stream(socket, &pipeline, &pc, &codec, offer, &mut events).await;
    let _ = pc.close().await;
    result
}

struct Events {
    gathered: Receiver<()>,
    connected: Receiver<()>,
    done: Receiver<()>,
}

/// 协商并推流
async fn stream(
    socket: &mut WebSocket,
    pipeline: &Pipeline,
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

    let mut frames = pipeline.frames.subscribe();
    pipeline.request_keyframe();
    let mut prev: Option<Instant> = None;
    let mut told = false;
    loop {
        tokio::select! {
            f = frames.recv() => match f {
                Ok(f) => {
                    // 采集线程在出第一帧之前就装完了输入隔离,这时状态是确定的
                    if !told {
                        told = true;
                        send(socket, &Signal::Info { text: pipeline.hook_note() }).await;
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
                }
                Err(RecvError::Lagged(n)) => {
                    warn!("推流落后,丢了 {n} 帧,请求关键帧");
                    pipeline.request_keyframe();
                }
                Err(RecvError::Closed) => bail!("采集线程已停止"),
            },
            // 只有 keyframe 拦截器放行的 PLI/FIR 会到这里
            ev = track.poll() => match ev {
                Some(TrackLocalEvent::OnRtcpPacket(_)) => {
                    info!("浏览器请求关键帧");
                    pipeline.request_keyframe();
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
