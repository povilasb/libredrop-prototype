Libredrop prototype: implements communication protocol on top of
[peer-discovery](https://github.com/libredrop/peer-discovery-rust) and CLI
interface.
Uses plain TCP for simplicity without any data signing.

## Usage

```bash
$ cargo run
> /h
Our ID is:
----->  13fd637311a04ba6b607c6592cc7a147  <----
Available commands:
  /q - quit
  /h - print this help message.
  /l - list discovered peers.
  /f <PEER_NUMBER> <FILE_PATH> - send file to a peer. List peers to get a number.
```

Run a second peer on the same LAN. You should be able to see it getting
discovered within a few seconds:

```bash
> /l
Peers:
1. 172.17.0.3:5000
```

Send a file to that peer:
```bash
> /f 1 local_file
```

File will be stored in `./vault/` dir when download is complete.
