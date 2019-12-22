//! Networking protocol.

use serde::{Deserialize, Serialize};

pub type PeerId = [u8; 16];

#[derive(Serialize, Deserialize, Debug)]
pub struct FileRequest {
    pub sender_id: PeerId,
    pub name: String,
    pub file_size: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum LibredropMsg {
    FileSendRequest(FileRequest),
    FileAccept,
    FileReject,
    FileChunk(Vec<u8>),
}
