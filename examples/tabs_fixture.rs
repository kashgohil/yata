//! Loopback fixture for manually exercising tabs and shared session state.
//!
//! Run with `cargo run --example tabs_fixture`, then open
//! `http://127.0.0.1:8765/a` in yata. Open `/b` in a second tab with `t` and
//! switch with `gt` / `gT` while the pages load.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};

static IMAGE_REQUESTS: AtomicUsize = AtomicUsize::new(0);

fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:8765")?;
    eprintln!("tabs fixture: http://127.0.0.1:8765/a");
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
    let request_line = request.lines().next().unwrap_or("GET / HTTP/1.1");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");
    let cookie = request
        .lines()
        .find_map(|line| header_value(line, "cookie"))
        .unwrap_or("-");
    eprintln!("{method} {path} cookie={cookie}");

    match path {
        "/a" => page(stream, "A", "B", true),
        "/b" => page(stream, "B", "A", false),
        "/shared.png" => image(stream),
        _ => response(stream, "404 Not Found", &[], b"not found"),
    }
}

fn header_value<'a>(line: &'a str, wanted: &str) -> Option<&'a str> {
    let (name, value) = line.split_once(':')?;
    name.eq_ignore_ascii_case(wanted).then(|| value.trim())
}

fn page(stream: &mut TcpStream, name: &str, other: &str, set_cookie: bool) -> std::io::Result<()> {
    let other_path = other.to_ascii_lowercase();
    let paragraphs = (1..=40)
        .map(|line| format!("<p>{name} retained line {line}</p>"))
        .collect::<String>();
    let body = format!(
        "<!doctype html><title>Tab {name}</title>\
         <h1>Page {name}</h1>\
         <p id=counter>timer pending</p>\
         <input name=draft value=\"{name} draft\">\
         <p><a href=/{other_path}>go to {other}</a></p>\
         <img src=/shared.png alt=shared>\
         {paragraphs}\
         <script>setTimeout(() => {{\
           document.getElementById('counter').textContent = 'timer fired';\
         }}, 250);</script>"
    );
    let mut headers = vec![("Content-Type", "text/html; charset=utf-8")];
    if set_cookie {
        headers.push(("Set-Cookie", "tabs_fixture=from_a; Path=/"));
    }
    response(stream, "200 OK", &headers, body.as_bytes())
}

fn image(stream: &mut TcpStream) -> std::io::Result<()> {
    // A 1x1 opaque black PNG. A long freshness lifetime makes the second
    // tab's request demonstrate the shared response/decoded-image caches.
    const PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 8, 215, 99, 96, 96, 96, 248, 15, 0,
        1, 4, 1, 0, 95, 142, 91, 42, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    let count = IMAGE_REQUESTS.fetch_add(1, Ordering::Relaxed) + 1;
    eprintln!("shared image network request #{count}");
    response(
        stream,
        "200 OK",
        &[
            ("Content-Type", "image/png"),
            ("Cache-Control", "max-age=600"),
        ],
        PNG,
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
