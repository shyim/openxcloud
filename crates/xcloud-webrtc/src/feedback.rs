//! RTCP sender-report timing and compound receiver reports, preserving the
//! receiver-liveness fixes in the original LibPeerWebRtcBridge transport.
use parking_lot::Mutex;
use rtc::interceptor::{Interceptor, Packet, StreamInfo, TaggedPacket};
use rtc::rtcp::{
    self,
    receiver_report::ReceiverReport,
    sender_report::SenderReport,
    source_description::{
        SdesType, SourceDescription, SourceDescriptionChunk, SourceDescriptionItem,
    },
};
use rtc::sansio::Protocol;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

pub(crate) type SenderReports = Arc<Mutex<HashMap<u32, (u32, Instant)>>>;

pub(crate) struct ReportObserver {
    reports: SenderReports,
    reads: VecDeque<TaggedPacket>,
    writes: VecDeque<TaggedPacket>,
}
impl ReportObserver {
    pub(crate) fn new(reports: SenderReports) -> Self {
        Self {
            reports,
            reads: VecDeque::new(),
            writes: VecDeque::new(),
        }
    }
}
impl Protocol<TaggedPacket, TaggedPacket, ()> for ReportObserver {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = rtc::shared::error::Error;
    type Time = Instant;
    fn handle_read(&mut self, message: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(packets) = &message.message.packet {
            for packet in packets {
                if let Some(sr) = packet.as_any().downcast_ref::<SenderReport>() {
                    self.reports
                        .lock()
                        .insert(sr.ssrc, ((sr.ntp_time >> 16) as u32, message.now));
                }
            }
        }
        self.reads.push_back(message);
        Ok(())
    }
    fn poll_read(&mut self) -> Option<TaggedPacket> {
        self.reads.pop_front()
    }
    fn handle_write(&mut self, message: TaggedPacket) -> Result<(), Self::Error> {
        self.writes.push_back(message);
        Ok(())
    }
    fn poll_write(&mut self) -> Option<TaggedPacket> {
        self.writes.pop_front()
    }
}
impl Interceptor for ReportObserver {
    fn bind_local_stream(&mut self, _: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, info: &StreamInfo) {
        self.reports.lock().remove(&info.ssrc);
    }
}

pub(crate) fn prepare_feedback(
    packet: Box<dyn rtcp::Packet>,
    reports: &SenderReports,
    now: Instant,
) -> Vec<Box<dyn rtcp::Packet>> {
    let Some(rr) = packet.as_any().downcast_ref::<ReceiverReport>() else {
        return vec![packet];
    };
    let mut rr = rr.clone();
    for report in &mut rr.reports {
        if let Some(&(lsr, received)) = reports.lock().get(&report.ssrc) {
            report.last_sender_report = lsr;
            // RFC 3550 DLSR uses 1/65536 seconds, not milliseconds.
            report.delay = (now.saturating_duration_since(received).as_secs_f64() * 65536.0) as u32;
        }
    }
    let sdes = SourceDescription {
        chunks: vec![SourceDescriptionChunk {
            source: rr.ssrc,
            items: vec![SourceDescriptionItem {
                sdes_type: SdesType::SdesCname,
                text: bytes::Bytes::from_static(b"openxcloud"),
            }],
        }],
    };
    vec![Box::new(rr), Box::new(sdes)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::interceptor::AttributedPacket;
    use rtc::rtcp::reception_report::ReceptionReport;
    #[test]
    fn sender_report_timing_is_reflected_in_compound_receiver_report() {
        let reports = SenderReports::default();
        let mut observer = ReportObserver::new(reports.clone());
        let received = Instant::now();
        observer
            .handle_read(TaggedPacket {
                now: received,
                transport: Default::default(),
                message: AttributedPacket::new(Packet::Rtcp(vec![Box::new(SenderReport {
                    ssrc: 42,
                    ntp_time: 0x123456789abcdef0,
                    ..Default::default()
                })])),
            })
            .unwrap();
        assert!(observer.poll_read().is_some());
        let packets = prepare_feedback(
            Box::new(ReceiverReport {
                ssrc: 1,
                reports: vec![ReceptionReport {
                    ssrc: 42,
                    ..Default::default()
                }],
                ..Default::default()
            }),
            &reports,
            received + std::time::Duration::from_millis(500),
        );
        let rr = packets[0]
            .as_any()
            .downcast_ref::<ReceiverReport>()
            .unwrap();
        assert_eq!(rr.reports[0].last_sender_report, 0x56789abc);
        assert_eq!(rr.reports[0].delay, 32768);
        let sdes = packets[1]
            .as_any()
            .downcast_ref::<SourceDescription>()
            .unwrap();
        assert_eq!(sdes.chunks[0].source, rr.ssrc);
        assert_eq!(sdes.chunks[0].items[0].sdes_type, SdesType::SdesCname);
    }
}
