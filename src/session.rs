use async_std::fs::File;
use async_std::io::prelude::{ReadExt, WriteExt};
use async_std::net::{SocketAddr, TcpStream};
use async_std::{io, sync};
use futures::{SinkExt, StreamExt};
use futures_codec::{Framed, SerdeCodec};
use indicatif::{ProgressBar, ProgressStyle};
use unwrap::unwrap;

use crate::app_data::{Event, State};
use crate::proto::{
    Done, FileEof, FileRequest, LibredropMsg, PeerId, ReceiverSM, SenderSM, SendingFile,
};

const FILE_READ_BUFF_SIZE: usize = 1024 * 32;

/// Connects to given peer and attempts to send a file to him.
pub async fn conn_send(
    peer_addr: SocketAddr,
    file_path: String,
    our_id: PeerId,
) -> io::Result<SenderSM> {
    let f = File::open(file_path.clone()).await?;
    let file_size = f.metadata().await?.len() as usize;
    let file_request = FileRequest {
        sender_id: our_id,
        name: file_path,
        file_size,
    };

    let stream = TcpStream::connect(peer_addr).await?;
    let mut framed = Framed::new(stream, SerdeCodec::default());

    // TODO(povilas): accept FileRequest
    let sender_sm = SenderSM::waiting_accept();
    // TODO(povilas): sender_sm.next_package()
    framed
        .send(LibredropMsg::FileSendRequest(file_request))
        .await?;

    if let Some(msg) = framed.next().await {
        let msg = unwrap!(msg);
        let sender_sm = sender_sm.on_libredrop_msg(msg);
        match sender_sm {
            SenderSM::SendingFile(state) => {
                out!("File was accepted. Sending..");
                let pb = make_progress_bar(file_size);
                Ok(SenderSM::Done(send_file(framed, f, pb, state).await?))
            }
            other => Ok(other),
        }
    } else {
        Err(io::ErrorKind::UnexpectedEof.into())
    }
}

async fn send_file(
    mut framed: Framed<TcpStream, SerdeCodec<LibredropMsg>>,
    mut f: File,
    pb: ProgressBar,
    mut sender_state: SendingFile,
) -> io::Result<Done> {
    let mut buf = vec![0u8; FILE_READ_BUFF_SIZE];

    loop {
        let bytes_read = f.read(&mut buf).await?;
        if bytes_read == 0 {
            break;
        }
        sender_state.send_data(buf[..bytes_read].to_vec());

        // TODO(povilas): use send_all?
        let msg = unwrap!(sender_state.next_packet());
        let _ = framed.send(msg).await?;
        pb.inc(bytes_read as u64);
    }

    pb.finish_with_message("Sent");
    out!();

    Ok(sender_state.on_file_eof(FileEof))
}

struct FileCtx {
    pb: ProgressBar,
    f: File,
}

pub async fn handle_incoming_conn(
    stream: TcpStream,
    event_tx: sync::Sender<Event>,
    accept_rx: sync::Receiver<bool>,
) -> io::Result<ReceiverSM> {
    let mut framed = Framed::new(stream, SerdeCodec::<LibredropMsg>::default());
    let mut receiver_sm = ReceiverSM::waiting_file();
    // TODO(povilas): think of ways to make this non-Optional
    let mut file_ctx: Option<FileCtx> = None;

    loop {
        match framed.next().await {
            Some(packet) => {
                receiver_sm = match receiver_sm.on_packet(packet?) {
                    ReceiverSM::WaitingAccept(state) => {
                        event_tx
                            .send(Event::SetState(State::AwaitingFileAccept))
                            .await;
                        // NOTE: this is actually a race condition: I should wait until I'm sure App has
                        // processed SetState. In practice, this probably won't be an issue.
                        let file_req = &state.file_req;
                        out!(
                            "{:?} wants to send '{}', size: {}. Accept? y/n: ",
                            hex::encode(&file_req.sender_id[0..5]),
                            file_req.name,
                            file_req.file_size
                        );

                        let accepted = accept_rx
                            .recv()
                            .await
                            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
                        event_tx.send(Event::SetState(State::Normal)).await;

                        // TODO(povilas): I'd like this to be handle by the main state machine
                        // driver loop, but that requires some smarter logic to awaken before
                        // framed.next() awakes.
                        match state.on_accept(accepted) {
                            ReceiverSM::Accepted(state) => {
                                let _ = framed.send(state.next_packet()).await?;
                                let f = File::create("vault/".to_string() + &state.file_req.name)
                                    .await?;
                                let pb = make_progress_bar(state.file_req.file_size);
                                file_ctx = Some(FileCtx { pb, f });
                                ReceiverSM::ReceivingFile(state.transition())
                            }
                            ReceiverSM::Rejected(state) => {
                                let _ = framed.send(state.next_packet()).await?;
                                receiver_sm = ReceiverSM::Rejected(state);
                                break;
                            }
                            other => other,
                        }
                    }
                    ReceiverSM::ReceivingFile(mut state) => {
                        let ctx = unwrap!(file_ctx.as_mut());
                        while let Some(data) = state.data() {
                            ctx.f.write_all(&data).await?;
                            ctx.pb.set_position(state.bytes_received as u64);
                        }
                        ReceiverSM::ReceivingFile(state)
                    }
                    other => {
                        receiver_sm = other;
                        break;
                    }
                }
            }
            None => {
                let ctx = unwrap!(file_ctx.as_mut());
                ctx.pb.finish_with_message("Received");
                receiver_sm = receiver_sm.transition();
                break;
            }
        }
    }

    Ok(receiver_sm)
}

fn make_progress_bar(file_size: usize) -> ProgressBar {
    let pb = ProgressBar::new(file_size as u64);
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
        .progress_chars("#>-"));
    pb
}
