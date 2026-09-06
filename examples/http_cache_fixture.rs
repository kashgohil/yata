//! Loopback fixture for manually exercising the document cache.
//!
//! Run with `cargo run --example http_cache_fixture`, then open
//! `http://127.0.0.1:8765/a` in yata. `/change` switches A to a second body and
//! validator without restarting the server.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};

static A_REQUESTS: AtomicUsize = AtomicUsize::new(0);
static VERSION: AtomicUsize = AtomicUsize::new(1);

fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:8765")?;
    eprintln!("HTTP cache fixture: http://127.0.0.1:8765/a");
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = respond(&mut stream) {
                    eprintln!("fixture request failed: {error}");
                }
            }
            Err(error) => eprintln!("fixture accept failed: {error}"),
        }
    }
    Ok(())
}

fn respond(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    let mut buf = [0; 1024];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buf)?;
        if read == 0 || bytes.len() > 32 * 1024 {
            break;
        }
        bytes.extend_from_slice(&buf[..read]);
    }
    let request = String::from_utf8_lossy(&bytes);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    match path {
        "/a" => serve_a(stream, &request),
        "/b" => html(stream, "<h1>B</h1><a href=/a>back to A</a>"),
        "/change" => {
            VERSION.store(2, Ordering::Relaxed);
            response(stream, "303 See Other", &[("Location", "/a")], b"")
        }
        _ => response(stream, "404 Not Found", &[], b"not found"),
    }
}

fn serve_a(stream: &mut TcpStream, request: &str) -> std::io::Result<()> {
    let count = A_REQUESTS.fetch_add(1, Ordering::Relaxed) + 1;
    let version = VERSION.load(Ordering::Relaxed);
    let etag = format!("\"a{version}\"");
    let validated = request.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("if-none-match") && value.trim() == etag
        })
    });
    eprintln!("/a request #{count}: validator={validated}, etag={etag}");
    if validated {
        return response(
            stream,
            "304 Not Modified",
            &[("Cache-Control", "max-age=30"), ("ETag", &etag)],
            b"",
        );
    }
    let body = format!(
        "<h1>A version {version}</h1><p>server request count: {count}</p>\
         <a href=/b>go to B</a> | <a href=/change>change A</a>"
    );
    response(
        stream,
        "200 OK",
        &[
            ("Content-Type", "text/html; charset=utf-8"),
            ("Cache-Control", "max-age=30"),
            ("ETag", &etag),
        ],
        body.as_bytes(),
    )
}

fn html(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {
    response(
        stream,
        "200 OK",
        &[("Content-Type", "text/html; charset=utf-8")],
        body.as_bytes(),
    )
}

fn response(
    stream: &mut TcpStream,
    status: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    write!(stream, "HTTP/1.1 {status}\r\n")?;
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(
        stream,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}
