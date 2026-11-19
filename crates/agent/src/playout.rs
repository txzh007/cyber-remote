//! 让浏览器解码完立刻显示:给每个发出的视频 RTP 包带上 playout-delay 扩展(min = max = 0)。
//!
//! 浏览器端的 `jitterBufferTarget = 0` 只是目标,Chrome 仍会按自己估的抖动攒缓冲再渲染
//! (0.8 实测"缓冲+等显示"p50 60 ms)。playout-delay 是发送端给接收端的上下限,0/0 表示不留缓冲。
//! 扩展要先在 MediaEngine 里注册,协商成功后 `StreamInfo` 里才有它的 ID;没协商上
//! (`--no-playout-delay`,或浏览器不支持)时这个拦截器什么也不做。
//! 格式:https://webrtc.googlesource.com/src/+/refs/heads/main/docs/native-code/rtp-hdrext/playout-delay

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use bytes::Bytes;
use rtc::interceptor::{Interceptor, Packet, Slot, StreamInfo, TaggedPacket};
use rtc::sansio::Protocol;
use rtc::shared::error::Error;

pub const URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay";

/// 12 位最小延迟 + 12 位最大延迟,单位 10 ms;全 0 = 立刻显示
static ZERO_DELAY: [u8; 3] = [0, 0, 0];

/// 排在 keyframe 拦截器后面,和默认拦截器错开
pub fn slot() -> Slot {
    Slot::from(14_100)
}

#[derive(Default)]
pub struct PlayoutDelayInterceptor {
    /// 本地流的 SSRC → 协商出的扩展 ID
    ids: HashMap<u32, u8>,
    read_queue: VecDeque<TaggedPacket>,
    write_queue: VecDeque<TaggedPacket>,
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for PlayoutDelayInterceptor {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.read_queue.push_back(msg);
        Ok(())
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read_queue.pop_front()
    }

    fn handle_write(&mut self, mut msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtp(pkt) = &mut msg.message.packet
            && let Some(&id) = self.ids.get(&pkt.header.ssrc)
        {
            // 写不进去(比如 ID 超出单字节格式)就照常发,只是没有这个约束
            let _ = pkt.header.set_extension(id, Bytes::from_static(&ZERO_DELAY));
        }
        self.write_queue.push_back(msg);
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write_queue.pop_front()
    }
}

impl Interceptor for PlayoutDelayInterceptor {
    fn bind_local_stream(&mut self, info: &StreamInfo) {
        if let Some(ext) = info.rtp_header_extensions.iter().find(|e| e.uri == URI)
            && let Ok(id) = u8::try_from(ext.id)
        {
            self.ids.insert(info.ssrc, id);
        }
    }

    fn unbind_local_stream(&mut self, info: &StreamInfo) {
        self.ids.remove(&info.ssrc);
    }

    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}

    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}
