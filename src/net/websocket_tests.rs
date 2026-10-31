use super::*;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{RootCertStore, ServerConfig, ServerConnection};
use std::net::TcpListener;
use std::sync::Arc;

use crate::test_websocket as ws;

#[test]
fn decrypted_tls_data_is_read_without_waiting_for_another_socket_event() {
    let cert = CertificateDer::from(
        include_bytes!("../../tests/fixtures/tls/localhost-cert.der").to_vec(),
    );
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        include_bytes!("../../tests/fixtures/tls/localhost-key.der").to_vec(),
    ));
    let mut roots = RootCertStore::empty();
    roots.add(cert.clone()).unwrap();
    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    server_config.send_tls13_tickets = 0;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let (done, finish) = std::sync::mpsc::channel();
    let (sent, written) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let session = ServerConnection::new(Arc::new(server_config)).unwrap();
        let mut stream = StreamOwned::new(session, peer);
        // Both frames fit in one TLS record, but exceed the 8192-byte read buffer.
        let mut frames = ws::frame(WS_OPCODE_TEXT, true, b"first");
        frames.extend(ws::frame(WS_OPCODE_TEXT, true, &vec![b'x'; 12000]));
        stream.write_all(&frames).unwrap();
        stream.flush().unwrap();
        sent.send(()).unwrap();
        finish.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let session = ClientConnection::new(
        Arc::new(client_config),
        ServerName::try_from("localhost").unwrap(),
    )
    .unwrap();
    let mut conn = WebSocketConn {
        reader: BufReader::new(WsUnderlyingStream::Tls(StreamOwned::new(session, socket))),
        write_mu: Mutex::new(()),
        last_activity: Instant::now(),
        incoming: Vec::new(),
        message: Vec::new(),
        message_opcode: 0,
    };
    assert_eq!(conn.read_data_message().unwrap().0, b"first");
    written.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(conn.reader.buffer().is_empty());
    assert!(
        !conn.buffered_frame_ready(),
        "the second frame still needs decrypted bytes"
    );
    let wake = SocketWake::new().unwrap();
    if let WsUnderlyingStream::Tls(stream) = conn.reader.get_ref() {
        assert!(
            !wake.wait(&stream.sock, Duration::ZERO).unwrap(),
            "the peer has no further TCP data"
        );
    }
    let start = Instant::now();
    assert!(conn.wait_readable(&wake, Duration::from_secs(2)).unwrap());
    assert!(start.elapsed() < Duration::from_millis(500));
    assert_eq!(conn.read_data_message().unwrap().0, vec![b'x'; 12000]);
    done.send(()).unwrap();
    server.join().unwrap();
}
