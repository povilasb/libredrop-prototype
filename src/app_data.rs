use async_std::net::TcpStream;
use peer_discovery::DiscoveryMsg;

/// Internal app state.
#[derive(Debug)]
pub enum State {
    /// Regular app state, nothing special. This is the default.
    Normal,
    /// We received request to send a file to us, waiting for us to confirm.
    AwaitingFileAccept,
}

/// Misc app events.
#[derive(Debug)]
pub enum Event {
    AppStarted,
    Stdin(String),
    PeerDiscovered(DiscoveryMsg),
    IncomingConnection(TcpStream),
    /// Allows async tasks to set the app state.
    SetState(State),
    Quit,
}
