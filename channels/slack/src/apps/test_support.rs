use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use super::Provider;

/// Keep no-call regression paths on an owned loopback socket even if they regress.
pub(super) struct OfflineProvider {
    provider: Provider,
    listener: TcpListener,
}

impl std::ops::Deref for OfflineProvider {
    type Target = Provider;
    fn deref(&self) -> &Self::Target {
        &self.provider
    }
}

impl Drop for OfflineProvider {
    fn drop(&mut self) {
        if !thread::panicking() {
            assert!(
                matches!(self.listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
                "unexpected provider call on a no-call path"
            );
        }
    }
}

pub(super) fn offline() -> OfflineProvider {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let provider = Provider::for_tests(
        &format!("http://{}", listener.local_addr().unwrap()),
        Duration::from_millis(100),
    );
    OfflineProvider { provider, listener }
}

pub(super) fn script(
    responses: Vec<(&'static str, u16, Value)>,
) -> (Provider, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sent, received) = mpsc::channel();
    let task = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        for (path, status, body) in responses {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("expected local provider request: {e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
                assert!(request.len() < 16384);
            }
            let headers = String::from_utf8(request.clone()).unwrap();
            assert!(
                headers.starts_with(&format!("POST /{path} HTTP/1.1\r\n")),
                "unexpected local provider method"
            );
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let end = request.len();
            assert!(length <= 4 * 1024 * 1024);
            request.resize(end + length, 0);
            stream.read_exact(&mut request[end..]).unwrap();
            let _ = sent.send(String::from_utf8_lossy(&request).into_owned());
            let body = body.to_string();
            write!(stream, "HTTP/1.1 {status} Local\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (
        Provider::for_tests(&format!("http://{address}"), Duration::from_secs(60)),
        received,
        task,
    )
}
