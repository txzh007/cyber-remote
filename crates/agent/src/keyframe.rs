//! 把浏览器的关键帧请求(PLI/FIR)交给应用。
//!
//! 入站 RTCP 走到拦截器链末尾就停了,只有带 `DeliverToApplication` 标记的才会
//! 以 `TrackLocalEvent::OnRtcpPacket` 出现在 `TrackLocal::poll()` 里。这个拦截器
//! 放在链的最后,只挑出 PLI/FIR 打上标记,其余 RTCP 留给默认拦截器处理。
//! 写法照 webrtc-rs 0.21 的 `rtcp-processing` 示例。

use std::collections::VecDeque;
use std::time::Instant;

use rtc::interceptor::{Attribute, Interceptor, Packet, Slot, StreamInfo, TaggedPacket};
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::sansio::Protocol;
use rtc::shared::error::Error;

/// 比默认拦截器都靠后,它们先看完全部入站 RTCP
pub fn slot() -> Slot {
    Slot::from(14_000)
}

#[derive(Default)]
pub struct KeyframeRequestInterceptor {
    read_queue: VecDeque<TaggedPacket>,
    write_queue: VecDeque<TaggedPacket>,
}

fn is_keyframe_request(packet: &Box<dyn rtc::rtcp::Packet>) -> bool {
    let p = packet.as_any();
    p.is::<PictureLossIndication>() || p.is::<FullIntraRequest>()
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for KeyframeRequestInterceptor {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, mut msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(packets) = &msg.message.packet {
            let requests: Vec<_> = packets.iter().filter(|p| is_keyframe_request(p)).cloned().collect();
            if requests.is_empty() {
                // 其他 RTCP 默认拦截器已经处理过,不往应用送
                return Ok(());
            }
            msg.message.packet = Packet::Rtcp(requests);
            msg.message.add(Attribute::DeliverToApplication);
        }
        self.read_queue.push_back(msg);
        Ok(())
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read_queue.pop_front()
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.write_queue.push_back(msg);
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write_queue.pop_front()
    }
}

impl Interceptor for KeyframeRequestInterceptor {
    fn bind_local_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _info: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}
