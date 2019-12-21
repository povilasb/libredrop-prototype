//! Handles TCP session between 2 peers.

use async_std::net::{TcpStream, SocketAddr};
use async_std::fs::File;
use async_std::{io, sync};
use futures_codec::{Framed, SerdeCodec};
use futures::{StreamExt, SinkExt};
use unwrap::unwrap;

use crate::proto::{FileRequest, LibredropMsg, PeerId};
use crate::app_data::{Event, State};

/// Connects to given peer and attempts to send a file to him.
pub async fn conn_send(peer_addr: SocketAddr, file_path: String, our_id: PeerId) -> io::Result<()> {
    let f = File::open(file_path.clone()).await?;
    let file_size = f.metadata().await?.len() as usize;
    let file_request = FileRequest {
        sender_id: our_id,
        name: file_path,
        file_size,
    };

    let stream = TcpStream::connect(peer_addr).await?;
    let mut framed = Framed::new(stream, SerdeCodec::default());
    framed.send(LibredropMsg::FileSendRequest(file_request)).await.unwrap();

    if let Some(msg) = framed.next().await {
        let msg = unwrap!(msg);
        match msg {
            LibredropMsg::FileAccept => {
                out!("File was accepted");
            }
            LibredropMsg::FileReject => {
                out!("File was rejected");
            }
            msg => out!("Unexpected message from peer: {:?}", msg),
        }
    }

    Ok(())
}

pub async fn handle_incoming_conn(stream: TcpStream, event_tx: sync::Sender<Event>,
                                  accept_rx: sync::Receiver<bool>) {
    let mut framed = Framed::new(stream, SerdeCodec::<LibredropMsg>::default());
    // TODO(povilas): timeout
    if let Some(msg) = framed.next().await {
        let msg = unwrap!(msg);
        let file_req = match msg {
            LibredropMsg::FileSendRequest(file_req) => file_req,
            msg => panic!("Unexpected message: {:?}", msg),
        };

        event_tx.send(Event::SetState(State::AwaitingFileAccept)).await;
        out!("{:?} wants to send '{}'. Accept? y/n: ", hex::encode(&file_req.sender_id[0..5]),
             file_req.name);

        if let Some(accepted) = accept_rx.recv().await {
            if accepted {
                let _ = unwrap!(framed.send(LibredropMsg::FileAccept).await);
            } else {
                let _ = unwrap!(framed.send(LibredropMsg::FileReject).await);
            }
            event_tx.send(Event::SetState(State::Normal)).await;
        }
    }
}
