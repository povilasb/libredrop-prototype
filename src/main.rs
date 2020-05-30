#[macro_use]
mod utils;
mod app_data;
mod proto;
mod session;

use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use async_std::io as aio;
use async_std::net::{TcpListener, TcpStream};
use async_std::{sync, task};
use futures::StreamExt;
use get_if_addrs;
use hex;
use log::{self, debug, info};
use regex::Regex;
use simple_logger;
use unwrap::unwrap;

use peer_discovery::{discover_peers, DiscoveryMsg, TransportProtocol};

use crate::app_data::{Event, State};
use crate::proto::{PeerId, ReceiverSM, SenderSM};

/// An app built on top of homebrew async event bus.
struct App {
    tx: sync::Sender<Event>,
    rx: sync::Receiver<Event>,

    /// When user confirmation is required, this is the channel to communicate user acction.
    accept_tx: sync::Sender<bool>,
    accept_rx: sync::Receiver<bool>,

    peers: Vec<SocketAddr>,
    port: u16,
    discovery_msg: DiscoveryMsg,
    our_id: PeerId,
    state: State,
}

use prost::Message;

pub mod protobuff {
    include!(concat!(env!("OUT_DIR"), "/libredrop.message.rs"));
}

use protobuff::{LibredropMsg, libredrop_msg};

fn demo_proto_msg() {
    let my_id = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

    let mut msg = LibredropMsg::default();
    // msg.variant = Some(libredrop_msg::Variant::FileAccept(proto::FileAccept{}));
    msg.variant = Some(libredrop_msg::Variant::FileRequest(
        protobuff::FileRequest{sender_id: my_id.to_vec(), file_name: "hello.txt".to_string(), file_size: 5}
    ));

    println!("Message: {:?}", msg);
    println!("Length on wire: {}", msg.encoded_len());

    let mut out_buff = vec![];
    msg.encode_length_delimited(&mut out_buff).unwrap();
    println!("Will send over wire: {:?}", out_buff);

    // Skip 1 byte since it's not part of the protobuff message.
    let received_msg = LibredropMsg::decode(&out_buff[1..]).unwrap();
    println!("Decoded: {:?}", received_msg);
}

#[async_std::main]
async fn main() {
    unwrap!(simple_logger::init_with_level(log::Level::Info));

    demo_proto_msg();

    App::new().run().await;
}

impl App {
    fn new() -> Self {
        let (tx, rx) = sync::channel(1024);
        let (accept_tx, accept_rx) = sync::channel(1);
        let port = 5000;
        let mut msg = DiscoveryMsg::new("libredrop".into(), TransportProtocol::Tcp, port);
        msg.add_addrv4(my_ip());
        Self {
            tx,
            rx,
            accept_tx,
            accept_rx,
            peers: Default::default(),
            port,
            // I'm just reusing discovery message ID for simplicity.
            // Ideally this would be an ID derived from encryption keys.
            our_id: msg.id(),
            discovery_msg: msg,
            state: State::Normal,
        }
    }

    async fn on_app_start(&mut self) {
        print_help(self.our_id);
        self.start_server();
        self.start_peer_discovery();
        self.start_stdin_reader();
    }

    async fn on_stdin(&self, line: String) {
        match self.state {
            State::Normal => self.on_stdin_normal_mode(line).await,
            State::AwaitingFileAccept => self.on_stdin_awaiting_file_accept(line).await,
        }
    }

    async fn on_stdin_awaiting_file_accept(&self, input: String) {
        self.accept_tx.send(input.starts_with("y")).await;
    }

    async fn on_stdin_normal_mode(&self, cmd: String) {
        if cmd.len() < 2 || !cmd.starts_with('/') {
            out!();
            return;
        }

        match cmd[1..2].as_ref() {
            "q" => self.tx.send(Event::Quit).await,
            "h" => print_help(self.our_id),
            "l" => self.list_peers(),
            "f" => self.on_send_file(&cmd[2..]).await,
            _ => out!(),
        };
    }

    async fn on_peer_discovered(&mut self, msg: DiscoveryMsg) {
        debug!("Peer: {}", msg.addrsv4()[0]);
        let peer_addr = SocketAddr::new(IpAddr::V4(msg.addrsv4()[0]), msg.service_port());
        if !self.peers.contains(&peer_addr) {
            let _ = self.peers.push(peer_addr);
        }
    }

    async fn on_send_file(&self, args: &str) {
        if let Some((peer_nr, file_path)) = parse_send(args) {
            let peer_addr = self.peers[peer_nr - 1];
            out!("Sending {} to {}", file_path, peer_addr);

            let our_id = self.our_id;
            let _: task::JoinHandle<()> = task::spawn(async move {
                match session::conn_send(peer_addr, file_path, our_id).await {
                    Err(e) => out!("Sending file failed, I/O error: {}", e),
                    Ok(SenderSM::Done(_)) => {
                        out!("File was sent successfully");
                    }
                    Ok(SenderSM::Rejected(_)) => {
                        out!("File was rejected");
                    }
                    Ok(SenderSM::Error) => {
                        out!("Unexpected message from peer");
                    }
                    other => {
                        out!("Unexpected state: {:?}", other);
                    }
                }
            });
        } else {
            out!("Ivalid send file command.");
        }
    }

    async fn on_incoming_conn(&mut self, stream: TcpStream) {
        out!("New conn accepted: {}", unwrap!(stream.peer_addr()));
        let tx = self.tx.clone();
        let accept_rx = self.accept_rx.clone();
        let _: task::JoinHandle<()> = task::spawn(async move {
            match session::handle_incoming_conn(stream, tx, accept_rx).await {
                Err(e) => out!("Receiving file failed, I/O error: {}", e),
                Ok(ReceiverSM::Done(state)) => {
                    out!("File received. Size: {}", state.bytes_received);
                }
                Ok(ReceiverSM::Rejected(_)) => {
                    out!("I don't want this file. Closing connection...");
                }
                Ok(ReceiverSM::Failed(state)) => {
                    out!("Unexpected message from peer: {:?}", state.err);
                }
                other => {
                    out!("Unexpected state: {:?}", other);
                }
            }
        });
    }

    /// The heart of our demo app.
    async fn run(&mut self) {
        self.tx.send(Event::AppStarted).await;

        loop {
            match self.rx.recv().await {
                Some(Event::AppStarted) => self.on_app_start().await,
                Some(Event::Stdin(line)) => self.on_stdin(line).await,
                Some(Event::PeerDiscovered(msg)) => self.on_peer_discovered(msg).await,
                Some(Event::IncomingConnection(stream)) => self.on_incoming_conn(stream).await,
                Some(Event::Quit) => break,
                Some(Event::SetState(state)) => self.state = state,
                None => {
                    info!("[event_bus] All senders were dropped. Exiting");
                    break;
                }
            };
        }
    }

    fn start_stdin_reader(&self) {
        let tx = self.tx.clone();

        let _ = task::spawn(async move {
            let stdin = aio::stdin();
            loop {
                let mut line = String::new();
                let n = unwrap!(stdin.read_line(&mut line).await);
                if n == 0 {
                    break;
                }
                tx.send(Event::Stdin(line)).await;
            }
        });
    }

    fn start_peer_discovery(&self) {
        let tx = self.tx.clone();
        let msg = self.discovery_msg.clone();
        let _ = task::spawn(async move {
            let mut rx_peers = unwrap!(discover_peers(msg));
            while let Some(msg) = rx_peers.next().await {
                tx.send(Event::PeerDiscovered(msg)).await;
            }
        });
    }

    fn start_server(&self) {
        let _ = task::spawn(run_server(self.port, self.tx.clone()));
    }

    fn list_peers(&self) {
        println!("Peers:");
        for (i, peer) in self.peers.iter().enumerate() {
            println!("{}. {}", i + 1, peer);
        }
        out!();
    }
}

async fn run_server(port: u16, tx: sync::Sender<Event>) -> aio::Result<()> {
    let listen_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), port);
    let listener = TcpListener::bind(listen_addr).await?;
    let mut incoming = listener.incoming();

    out!("Listening for connections on port {}", port);

    while let Some(stream) = incoming.next().await {
        let stream = stream?;
        tx.send(Event::IncomingConnection(stream)).await;
    }

    Ok(())
}

fn print_help(our_id: PeerId) {
    println!("Our ID is:");
    println!("----->  {}  <----", hex::encode(&our_id));
    println!("Available commands:");
    println!("  /q - quit");
    println!("  /h - print this help message.");
    println!("  /l - list discovered peers.");
    println!("  /f <PEER_NUMBER> <FILE_PATH> - send file to a peer. List peers to get a number.");
    print!("\r> ");
    unwrap!(io::stdout().flush());
}

/// Returns peer number and file path to send to the peer or None if given string is invalid send
/// command.
fn parse_send(cmd: &str) -> Option<(usize, String)> {
    let re = unwrap!(Regex::new(r"\s+(\w+)\s+(.+)"));
    let caps = re.captures(cmd)?;
    let peer_nr = unwrap!(caps.get(1)?.as_str().parse::<usize>());
    let fpath = caps.get(2)?.as_str().to_string();
    Some((peer_nr, fpath))
}

fn my_ip() -> Ipv4Addr {
    let net_interfaces = unwrap!(get_if_addrs::get_if_addrs());
    // It's eth0 on Docker container, will have to change this when running on host
    // machine.
    let main_if = net_interfaces
        .iter()
        .find(|interface| is_main_interface(interface))
        .expect("Network interface not found");
    unwrap!(get_ipv4_addr(main_if))
}

fn is_main_interface(interface: &get_if_addrs::Interface) -> bool {
    interface.name.starts_with("eth")
        || interface.name.starts_with("en")
        || interface.name.starts_with("wlp")
}

fn get_ipv4_addr(interface: &get_if_addrs::Interface) -> Option<Ipv4Addr> {
    match &interface.addr {
        get_if_addrs::IfAddr::V4(addr) => Some(addr.ip),
        _ => None,
    }
}
