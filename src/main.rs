use std::net::{Ipv4Addr, SocketAddr, IpAddr};
use std::io::{self, Write};
use std::time::Duration;

use hex;
use regex::Regex;
use simple_logger;
use unwrap::unwrap;
use futures::{StreamExt, SinkExt};
use log::{self, info, debug};
use get_if_addrs;
use async_std::{task, sync};
use async_std::net::{TcpListener, TcpStream};
use async_std::fs::File;
use async_std::io as aio;
use async_std::io::prelude::{ReadExt, WriteExt};
use serde::{Serialize, Deserialize};
use futures_codec::{Framed, SerdeCodec};

use peer_discovery::{discover_peers, DiscoveryMsg, TransportProtocol};

/// Prints given formatted string and prompts for input again.
macro_rules! out {
    ($($arg:tt)*) => ({
        print!("\r");
        println!($($arg)*);
        print!("\r> ");
        unwrap!(io::stdout().flush());
    });
}

type PeerId = [u8; 16];

#[derive(Serialize, Deserialize, Debug)]
struct FileRequest {
    sender_id: PeerId,
    name: String,
    file_size: usize,
}

#[derive(Serialize, Deserialize, Debug)]
enum LibredropMsg {
    FileSendRequest(FileRequest),
    FileAccept,
    FileReject,
}

/// Misc app events.
enum Event {
    AppStarted,
    Stdin(String),
    PeerDiscovered(DiscoveryMsg),
    IncomingConnection(TcpStream),
    SendFileRequest(FileRequest),
    /// Allows async tasks to set the app state.
    SetState(State),
    Quit,
}

/// Internal app state.
#[derive(Debug)]
enum State {
    /// Regular app state, nothing special. This is the default.
    Normal,
    /// We received request to send a file to us, waiting for us to confirm.
    AwaitingFileAccept,
}

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

#[async_std::main]
async fn main() {
    unwrap!(simple_logger::init_with_level(log::Level::Info));

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

            let task: task::JoinHandle<aio::Result<()>> = task::spawn(async move {
                let f = File::open(file_path.clone()).await?;
                let file_size = f.metadata().await?.len() as usize;
                let file_request = FileRequest {
                    sender_id: our_id,
                    name: file_path,
                    file_size,
                };

                let mut stream = TcpStream::connect(peer_addr).await?;
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
            });
        } else {
            out!("Ivalid send file command.");
        }
    }

    async fn on_incoming_conn(&mut self, mut stream: TcpStream) {
        // TODO(povilas): spawn conn handler
        out!("New conn accepted: {}", unwrap!(stream.peer_addr()));

        let mut framed = Framed::new(stream, SerdeCodec::<LibredropMsg>::default());
        // TODO(povilas): timeout
        if let Some(msg) = framed.next().await {
            let msg = unwrap!(msg);
            let file_req = match msg {
                LibredropMsg::FileSendRequest(file_req) => file_req,
                msg => panic!("Unexpected message: {:?}", msg),
            };

            out!("{:?} wants to send '{}'. Accept? y/n: ", hex::encode(&file_req.sender_id[0..5]),
                 file_req.name);
            self.state = State::AwaitingFileAccept;

            let accept_rx = self.accept_rx.clone();
            let tx = self.tx.clone();
            let task: task::JoinHandle<aio::Result<()>> = task::spawn(async move {
                if let Some(accepted) = accept_rx.recv().await {
                    if accepted {
                        let _ = unwrap!(framed.send(LibredropMsg::FileAccept).await);
                    } else {
                        let _ = unwrap!(framed.send(LibredropMsg::FileReject).await);
                    }
                    tx.send(Event::SetState(State::Normal)).await;
                }

                Ok(())
            });
        }
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
                Some(Event::SendFileRequest(req)) => (),
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
    let eth0 = unwrap!(net_interfaces.iter()
        .find(|interface| interface.name == "eth0"));
    unwrap!(get_ipv4_addr(eth0))
}

fn get_ipv4_addr(interface: &get_if_addrs::Interface) -> Option<Ipv4Addr> {
    match &interface.addr {
        get_if_addrs::IfAddr::V4(addr) => Some(addr.ip),
        _ => None,
    }
}