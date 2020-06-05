//! Networking protocol.

use err_derive::Error;
use machine::{machine, transitions};
use serde::{Deserialize, Serialize};

pub mod protobuff {
    include!(concat!(env!("OUT_DIR"), "/libredrop.message.rs"));
}

/// Protocol version.
/// 0 is prototype version - anything can break at any time.
pub const VERSION: u16 = 0;

pub type PeerId = [u8; 16];

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FileRequest {
    pub sender_id: PeerId,
    pub name: String,
    pub file_size: usize,
}

// TODO(povilas): replace with protobuff::LibredropMsg
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum LibredropMsg {
    FileSendRequest(FileRequest),
    FileAccept,
    FileReject,
    FileChunk(Vec<u8>),
}

#[derive(Debug, Error)]
pub enum Error {
    /// Some packet was not expected while in some state.
    #[error(display = "Unexpected packet {:?} in state {}", _0, _1)]
    UnexpectedPacket(LibredropMsg, &'static str),
}

/// A marker used to notify about the end of file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileEof;

machine!(
    /// State machine of a peer that sends file.
    #[derive(Debug)]
    enum SenderSM {
        WaitingAccept,
        /// In progress of sending file chunks. You can feed `[u8]` data and get `LibredropMsg`
        /// packets from this state.
        // TODO(povilas): use smth smarter, like rope structure?
        SendingFile {
            data: Vec<Vec<u8>>,
        },
        Rejected,
        Done,
    }
);

transitions!(SenderSM,
    [
        (WaitingAccept, LibredropMsg) => [SendingFile, Error, Rejected],
        (SendingFile, FileEof) => Done
    ]
);

impl WaitingAccept {
    pub fn on_libredrop_msg(self, msg: LibredropMsg) -> SenderSM {
        match msg {
            LibredropMsg::FileAccept => SenderSM::SendingFile(SendingFile::new()),
            LibredropMsg::FileReject => SenderSM::Rejected(Rejected {}),
            // TODO(povilas): add metadata to error state
            _ => SenderSM::Error,
        }
    }
}

impl SendingFile {
    pub fn new() -> Self {
        Self {
            data: Default::default(),
        }
    }

    pub fn on_file_eof(self, _: FileEof) -> Done {
        Done {}
    }

    /// Buffer data to send.
    pub fn send_data(&mut self, data: Vec<u8>) {
        self.data.push(data);
    }

    /// Returns next file chunk packet to send to the other peer.
    pub fn next_packet(&mut self) -> Option<LibredropMsg> {
        self.data.pop().map(LibredropMsg::FileChunk)
    }
}

/// State machine of a peer that receives file.
#[derive(Debug)]
pub enum ReceiverSM {
    WaitingFile(WaitingFile),
    WaitingAccept(ReceiverWaitingAccept),
    Accepted(Accepted),
    ReceivingFile(ReceivingFile),
    Rejected(ReceiverRejected),
    Done(ReceiverDone),
    Failed(Failed),
}

impl ReceiverSM {
    pub fn waiting_file() -> Self {
        ReceiverSM::WaitingFile(WaitingFile {})
    }

    pub fn on_packet(self, packet: LibredropMsg) -> Self {
        match self {
            ReceiverSM::WaitingFile(state) => state.on_packet(packet),
            ReceiverSM::WaitingAccept(state) => state.on_packet(packet),
            ReceiverSM::ReceivingFile(state) => state.on_packet(packet),
            state => state, // if we're in Done, Failed or Rejected already
        }
    }

    pub fn transition(self) -> Self {
        match self {
            ReceiverSM::ReceivingFile(state) => ReceiverSM::Done(ReceiverDone {
                bytes_received: state.bytes_received,
            }),
            state => state, // if we're in Done, Failed or Rejected, etc. already
        }
    }
}

/// Peer has failed.
#[derive(Debug)]
pub struct Failed {
    pub err: Error,
}

impl Failed {
    pub fn with_error(err: Error) -> Self {
        Self { err }
    }
}

/// Waiting for a file proposition from the remote peer.
#[derive(Debug)]
pub struct WaitingFile {}

impl WaitingFile {
    pub fn on_packet(self, packet: LibredropMsg) -> ReceiverSM {
        match packet {
            LibredropMsg::FileSendRequest(file_req) => {
                ReceiverSM::WaitingAccept(ReceiverWaitingAccept { file_req })
            }
            other => ReceiverSM::Failed(Failed::with_error(Error::UnexpectedPacket(
                other,
                "WaitingFile",
            ))),
        }
    }
}

/// Waiting until we confirm or deny file that remote peer wants to send us.
#[derive(Debug)]
pub struct ReceiverWaitingAccept {
    pub file_req: FileRequest,
}

impl ReceiverWaitingAccept {
    pub fn on_packet(self, packet: LibredropMsg) -> ReceiverSM {
        ReceiverSM::Failed(Failed::with_error(Error::UnexpectedPacket(
            packet,
            "WaitingAccept",
        )))
    }

    /// Given our input transitions to the next state.
    pub fn on_accept(self, accepted: bool) -> ReceiverSM {
        if accepted {
            ReceiverSM::Accepted(Accepted {
                file_req: self.file_req,
            })
        } else {
            ReceiverSM::Rejected(ReceiverRejected {})
        }
    }
}

/// File send request was accepted.
#[derive(Debug)]
pub struct Accepted {
    pub file_req: FileRequest,
}

impl Accepted {
    pub fn next_packet(&self) -> LibredropMsg {
        LibredropMsg::FileAccept
    }

    pub fn transition(self) -> ReceivingFile {
        ReceivingFile::new(self.file_req)
    }
}

/// File send request was rejected by us.
#[derive(Debug)]
pub struct ReceiverRejected {}

impl ReceiverRejected {
    pub fn next_packet(&self) -> LibredropMsg {
        LibredropMsg::FileReject
    }
}

/// In progress of receiving file chunks.
#[derive(Debug)]
pub struct ReceivingFile {
    pub file_req: FileRequest,
    pub bytes_received: usize,
    data: Vec<Vec<u8>>,
}

impl ReceivingFile {
    pub fn new(file_req: FileRequest) -> Self {
        Self {
            file_req,
            bytes_received: 0,
            data: Default::default(),
        }
    }

    /// Extract received data buffer from packet.
    pub fn on_packet(mut self, packet: LibredropMsg) -> ReceiverSM {
        match packet {
            LibredropMsg::FileChunk(data) => {
                self.bytes_received += data.len();
                self.data.push(data);
                ReceiverSM::ReceivingFile(self)
            }
            packet => ReceiverSM::Failed(Failed::with_error(Error::UnexpectedPacket(
                packet,
                "ReceivingFile",
            ))),
        }
    }

    /// Gets data received data chunk.
    // TODO(povilas): return an impl Read over a rope structure of read data.
    pub fn data(&mut self) -> Option<Vec<u8>> {
        self.data.pop()
    }
}

/// File was fully received.
#[derive(Debug)]
pub struct ReceiverDone {
    pub bytes_received: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuff::LibredropMsg;
    use protobuff::libredrop_msg::Variant;
    use prost::Message;

    #[test]
    fn encode_decode_msg() {
        let my_id = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16].to_vec();

        let mut msg = LibredropMsg::default();
        msg.variant = Some(Variant::FileRequest(
            protobuff::FileRequest{sender_id: my_id.clone(), file_name: "hello.txt".to_string(), file_size: 5}
        ));

        let mut out_buff = vec![];
        msg.encode_length_delimited(&mut out_buff).unwrap();

        // Skip 1 byte since it's not part of the protobuff message.
        let received_msg = LibredropMsg::decode(&out_buff[1..]).unwrap();
        match received_msg.variant {
            Some(Variant::FileRequest(file_req)) => {
                assert_eq!(file_req.sender_id, my_id);
                assert_eq!(file_req.file_size, 5);
                assert_eq!(file_req.file_name, "hello.txt".to_string());
            },
            other => panic!("Unexpected message: {:?}", other)
        }
    }
}
