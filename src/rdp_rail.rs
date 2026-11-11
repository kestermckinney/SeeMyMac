//! RemoteApp (RAIL, MS-RDPERP): run one program on the server instead of its whole desktop.
//!
//! The server draws the program's windows into the session picture, so they show in the
//! normal view; this channel does the RAIL conversation: answer the handshake, announce the
//! client, and launch the program.

use ironrdp_core::{Decode as _, Encode, EncodeResult, ReadCursor, WriteCursor};
use ironrdp_pdu::PduResult;
use ironrdp_rail::{ClientStatusPdu, ExecutePdu, ExecuteResult, HandshakePdu, RailPdu};
use ironrdp_pdu::gcc::ChannelName;
use ironrdp_svc::{SvcClientProcessor, SvcEncode, SvcMessage, SvcProcessor};
use std::sync::mpsc::Sender;

use crate::rfb::Event;

/// A RAIL PDU as a message on the static channel.
#[derive(Debug)]
struct RailMsg(RailPdu);

impl Encode for RailMsg {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        self.0.encode(dst)
    }

    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn size(&self) -> usize {
        self.0.size()
    }
}

impl SvcEncode for RailMsg {}

#[derive(Debug)]
pub struct Rail {
    /// The program to run: a path, or `||alias` for a published RemoteApp.
    app: String,
    events: Sender<Event>,
    launched: bool,
}

ironrdp_core::impl_as_any!(Rail);

impl Rail {
    pub fn new(app: String, events: Sender<Event>) -> Self {
        Self { app, events, launched: false }
    }

    fn send(pdu: RailPdu) -> SvcMessage {
        SvcMessage::from(RailMsg(pdu))
    }

    /// What goes back after the server's handshake: the client's status, then the program to launch.
    fn after_handshake(&mut self, build: u32) -> Vec<SvcMessage> {
        let mut out = vec![Self::send(RailPdu::Handshake(HandshakePdu { build_number: build })), Self::send(RailPdu::ClientStatus(ClientStatusPdu { flags: 0 }))];
        if !self.launched {
            self.launched = true;
            out.push(Self::send(RailPdu::Execute(ExecutePdu { flags: 0, executable: self.app.clone(), working_directory: String::new(), arguments: String::new() })));
            let _ = self.events.send(Event::Stage(format!("Starting {}…", self.app)));
        }
        out
    }
}

impl SvcProcessor for Rail {
    fn channel_name(&self) -> ChannelName {
        ChannelName::from_static(b"rail\0\0\0\0")
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        let pdu = match RailPdu::decode(&mut ReadCursor::new(payload)) {
            Ok(p) => p,
            Err(e) => {
                log::debug!("RAIL: ignoring a message that does not decode: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(match pdu {
            RailPdu::Handshake(h) => self.after_handshake(h.build_number),
            RailPdu::HandshakeEx(h) => self.after_handshake(h.build_number),
            RailPdu::ExecuteResult(r) => {
                if r.result != ExecuteResult::Ok {
                    let why = match r.result {
                        ExecuteResult::FileNotFound => "the program was not found",
                        ExecuteResult::NotInAllowlist => "the server does not allow that program (it is not a published RemoteApp)",
                        ExecuteResult::SessionLocked => "the session is locked",
                        _ => "the server could not start it",
                    };
                    log::warn!("RAIL: {}: {:?}", r.executable, r.result);
                    let _ = self.events.send(Event::Stage(format!("RemoteApp failed: {why}")));
                }
                Vec::new()
            }
            _ => Vec::new(),
        })
    }
}

impl SvcClientProcessor for Rail {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn decode(m: &SvcMessage) -> RailPdu {
        let b = m.encode_unframed_pdu().unwrap();
        RailPdu::decode(&mut ReadCursor::new(&b)).unwrap()
    }

    #[test]
    fn the_handshake_is_answered_and_the_program_launched_once() {
        let (tx, rx) = channel();
        let mut rail = Rail::new("||notepad".into(), tx);
        let hello = ironrdp_core::encode_vec(&RailPdu::Handshake(HandshakePdu { build_number: 7601 })).unwrap();
        let out = rail.process(&hello).unwrap();
        let pdus: Vec<RailPdu> = out.iter().map(decode).collect();
        assert!(matches!(pdus[0], RailPdu::Handshake(_)));
        assert!(matches!(pdus[1], RailPdu::ClientStatus(_)));
        match &pdus[2] {
            RailPdu::Execute(e) => assert_eq!(e.executable, "||notepad"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(rx.try_recv(), Ok(Event::Stage(_))));
        // A second handshake does not launch it again.
        assert_eq!(rail.process(&hello).unwrap().len(), 2);
    }

    #[test]
    fn a_failed_launch_is_reported() {
        let (tx, rx) = channel();
        let mut rail = Rail::new("x".into(), tx);
        let res = ironrdp_core::encode_vec(&RailPdu::ExecuteResult(ironrdp_rail::ExecuteResultPdu { flags: 0, result: ExecuteResult::FileNotFound, raw_result: 2, executable: "x".into() })).unwrap();
        assert!(rail.process(&res).unwrap().is_empty());
        match rx.try_recv() {
            Ok(Event::Stage(s)) => assert!(s.contains("not found"), "{s}"),
            other => panic!("{other:?}"),
        }
    }
}
