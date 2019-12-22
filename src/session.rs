//! Handles TCP session between 2 peers.

use async_std::fs::File;
use async_std::io::prelude::{ReadExt, WriteExt};
use async_std::net::{SocketAddr, TcpStream};
use async_std::{io, sync};
use futures::{SinkExt, StreamExt};
use futures_codec::{Framed, SerdeCodec};
use indicatif::{ProgressBar, ProgressStyle};
use unwrap::unwrap;

use crate::app_data::{Event, State};
use crate::proto::{FileRequest, LibredropMsg, PeerId};

const FILE_READ_BUFF_SIZE: usize = 1024 * 32;

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
    framed
        .send(LibredropMsg::FileSendRequest(file_request))
        .await
        .unwrap();

    if let Some(msg) = framed.next().await {
        let msg = unwrap!(msg);
        match msg {
            LibredropMsg::FileAccept => {
                out!("File was accepted. Sending..");
                let pb = make_progress_bar(file_size);
                send_file(framed, f, pb).await?;
            }
            LibredropMsg::FileReject => {
                out!("File was rejected");
            }
            msg => out!("Unexpected message from peer: {:?}", msg),
        }
    }

    Ok(())
}

async fn send_file(
    mut framed: Framed<TcpStream, SerdeCodec<LibredropMsg>>,
    mut f: File,
    pb: ProgressBar,
) -> io::Result<()> {
    let mut buf = vec![0u8; FILE_READ_BUFF_SIZE];

    loop {
        let bytes_read = f.read(&mut buf).await?;
        if bytes_read == 0 {
            break;
        }

        // TODO(povilas): use send_all?
        let msg = LibredropMsg::FileChunk(buf[..bytes_read].to_vec());
        let _ = framed.send(msg).await?;
        pb.inc(bytes_read as u64);
    }

    pb.finish_with_message("Sent");
    out!();

    Ok(())
}

pub async fn handle_incoming_conn(
    stream: TcpStream,
    event_tx: sync::Sender<Event>,
    accept_rx: sync::Receiver<bool>,
) {
    let mut framed = Framed::new(stream, SerdeCodec::<LibredropMsg>::default());
    // TODO(povilas): timeout
    let msg = if let Some(msg) = framed.next().await {
        unwrap!(msg)
    } else {
        return;
    };

    let file_req = match msg {
        LibredropMsg::FileSendRequest(file_req) => file_req,
        msg => panic!("Unexpected message: {:?}", msg),
    };

    event_tx
        .send(Event::SetState(State::AwaitingFileAccept))
        .await;
    // NOTE: this is actually a race condition: I should wait until
    // I'm sure App has processed SetState. In practice, this probably won't be
    // an issue.
    out!(
        "{:?} wants to send '{}', size: {}. Accept? y/n: ",
        hex::encode(&file_req.sender_id[0..5]),
        file_req.name,
        file_req.file_size
    );

    let recv_file = if let Some(accepted) = accept_rx.recv().await {
        if accepted {
            let _ = unwrap!(framed.send(LibredropMsg::FileAccept).await);
        } else {
            let _ = unwrap!(framed.send(LibredropMsg::FileReject).await);
        }
        accepted
    } else {
        out!("Connection dropped");
        false
    };
    event_tx.send(Event::SetState(State::Normal)).await;

    if recv_file {
        let mut f = unwrap!(File::create("vault/".to_string() + &file_req.name).await);
        let mut bytes_received: usize = 0;
        let pb = make_progress_bar(file_req.file_size);

        while let Some(msg) = framed.next().await {
            let data = match unwrap!(msg) {
                LibredropMsg::FileChunk(data) => data,
                msg => {
                    out!("Unexpected message from peer: {:?}", msg);
                    vec![]
                }
            };
            bytes_received += data.len();
            // NOTE: if I put this statement inside FileChunk arm, rustc won't compile
            // because of some arcane generics issue.
            // Could be a compiler bug, idk.
            unwrap!(f.write_all(&data).await);
            pb.inc(bytes_received as u64);
        }

        pb.finish_with_message("Received");
        out!("File received. Size: {}", bytes_received);
    }
}

fn make_progress_bar(file_size: usize) -> ProgressBar {
    let pb = ProgressBar::new(file_size as u64);
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
        .progress_chars("#>-"));
    pb
}
