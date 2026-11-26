//! Minimal acknowledging Hub endpoint for observing each POST in wire order.
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};

pub fn request(listener: &TcpListener) -> (TcpStream, Value) {
    listener.set_nonblocking(true).unwrap();
    let start = Instant::now();
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    && start.elapsed() < Duration::from_secs(5) =>
            {
                thread::sleep(Duration::from_millis(2))
            }
            Err(e) => panic!("expected a Hub ingest request: {e}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut length = None;
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
    }
    let mut body = vec![0; length.unwrap()];
    reader.read_exact(&mut body).unwrap();
    (reader.into_inner(), serde_json::from_slice(&body).unwrap())
}
pub fn ack(mut stream: TcpStream, request: &Value) {
    let body=serde_json::json!({"ok":true,"run":{"id":request["run"]["run_id"],"status":request["snapshot"]["status"]}}).to_string();
    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
}
