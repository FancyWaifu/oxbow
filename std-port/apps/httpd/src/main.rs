//! httpd — a small static HTTP/1.0 file server, written in ordinary Rust `std` and
//! compiled for `x86_64-unknown-oxbow`. It runs as a normal process on the oxbow
//! microkernel and serves files out of oxbow's ext2 filesystem over real TCP.
//!
//! It exercises, all at once, the std surface brought up on oxbow:
//!   * `net::TcpListener` / `TcpStream` — the on-the-wire socket path (e1000 + the net
//!     server), bound to `0.0.0.0:8080`;
//!   * `thread::spawn` — one handler thread per connection;
//!   * `fs` — `read`, `metadata` (size + mtime), `read_dir` (directory listings),
//!     plus `create_dir_all`/`write` to self-seed a demo site on first run;
//!   * `time::SystemTime` — request log timestamps;
//!   * `String`/`format!` — building responses and the HTML index.
//!
//! There is nothing oxbow-specific in the logic below: it is the same program you would
//! write for Linux. That is the whole point — `std` on oxbow is real.
#![no_main]
#![allow(internal_features)]
extern crate oxbow_rt;

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const BIND_ADDR: &str = "0.0.0.0:8080";
const WEB_ROOT: &str = "/www";

fn main() {
    seed_site();

    let listener = match TcpListener::bind(BIND_ADDR) {
        Ok(l) => l,
        Err(e) => {
            println!("httpd: cannot bind {BIND_ADDR}: {e}");
            return;
        }
    };
    println!("httpd: serving {WEB_ROOT} on http://{BIND_ADDR}/  (oxbow + Rust std)");

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                // One thread per connection — real OS threads on oxbow's scheduler.
                thread::spawn(move || handle(stream));
            }
            Err(e) => println!("httpd: accept error: {e}"),
        }
    }
}

/// Serve a single HTTP/1.0 request, then close (no keep-alive — simple + robust).
fn handle(mut stream: TcpStream) {
    let mut buf = [0u8; 4096];
    let n = match stream.read(&mut buf) {
        Ok(0) => return,
        Ok(n) => n,
        Err(_) => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let (method, raw_path) = parse_request_line(&req);

    let log_path = raw_path.to_string();
    let status = if method != "GET" && method != "HEAD" {
        send_simple(&mut stream, 405, "Method Not Allowed", "405 Method Not Allowed\n");
        405
    } else {
        route(&mut stream, &decode_path(raw_path), method == "HEAD")
    };
    log(method, &log_path, status);
    let _ = stream.flush();
}

/// Resolve `path` under WEB_ROOT and write the response. Returns the status code.
fn route(stream: &mut TcpStream, path: &str, head_only: bool) -> u16 {
    let Some(fs_path) = safe_join(WEB_ROOT, path) else {
        send_simple(stream, 403, "Forbidden", "403 Forbidden\n");
        return 403;
    };

    let meta = match fs::metadata(&fs_path) {
        Ok(m) => m,
        Err(_) => {
            send_simple(stream, 404, "Not Found", "404 Not Found\n");
            return 404;
        }
    };

    if meta.is_dir() {
        // Prefer an index.html; otherwise generate a directory listing.
        let index = fs_path.join("index.html");
        if fs::metadata(&index).map(|m| m.is_file()).unwrap_or(false) {
            return serve_file(stream, &index, head_only);
        }
        let body = directory_listing(&fs_path, path);
        send(stream, 200, "OK", "text/html; charset=utf-8", body.as_bytes(), head_only);
        return 200;
    }

    serve_file(stream, &fs_path, head_only)
}

fn serve_file(stream: &mut TcpStream, fs_path: &Path, head_only: bool) -> u16 {
    match fs::read(fs_path) {
        Ok(bytes) => {
            let ctype = content_type(fs_path);
            send(stream, 200, "OK", ctype, &bytes, head_only);
            200
        }
        Err(_) => {
            send_simple(stream, 404, "Not Found", "404 Not Found\n");
            404
        }
    }
}

/// Build an HTML directory listing from `read_dir`.
fn directory_listing(dir: &Path, url_path: &str) -> String {
    let mut out = String::new();
    out.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
    out.push_str(&format!("<title>Index of {url_path}</title>"));
    out.push_str("<style>body{font-family:monospace;margin:2rem}a{text-decoration:none}");
    out.push_str("li{margin:.2rem 0}h1{font-size:1.2rem}</style></head><body>");
    out.push_str(&format!("<h1>Index of {url_path}</h1><ul>"));

    if url_path != "/" {
        out.push_str("<li><a href=\"../\">../</a></li>");
    }

    let mut entries: Vec<(String, bool, u64)> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            let (is_dir, size) = match ent.metadata() {
                Ok(m) => (m.is_dir(), m.len()),
                Err(_) => (false, 0),
            };
            entries.push((name, is_dir, size));
        }
    }
    // Directories first, then alphabetical.
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    for (name, is_dir, size) in entries {
        let slash = if is_dir { "/" } else { "" };
        let detail = if is_dir { String::from("dir") } else { format!("{size} bytes") };
        out.push_str(&format!(
            "<li><a href=\"{name}{slash}\">{name}{slash}</a> <small>{detail}</small></li>"
        ));
    }
    out.push_str("</ul><hr><small>httpd on oxbow</small></body></html>");
    out
}

// ---------- HTTP wire helpers ----------

fn send(stream: &mut TcpStream, code: u16, reason: &str, ctype: &str, body: &[u8], head_only: bool) {
    let header = format!(
        "HTTP/1.0 {code} {reason}\r\n\
         Server: oxbow-httpd\r\n\
         Content-Type: {ctype}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    if !head_only {
        let _ = stream.write_all(body);
    }
}

fn send_simple(stream: &mut TcpStream, code: u16, reason: &str, body: &str) {
    send(stream, code, reason, "text/plain; charset=utf-8", body.as_bytes(), false);
}

// ---------- request parsing ----------

/// Split "GET /path HTTP/1.1" into ("GET", "/path"). Defaults are conservative.
fn parse_request_line(req: &str) -> (&str, &str) {
    let line = req.lines().next().unwrap_or("");
    let mut it = line.split_whitespace();
    let method = it.next().unwrap_or("");
    let path = it.next().unwrap_or("/");
    (method, path)
}

/// Strip the query string and percent-decode a request path.
fn decode_path(raw: &str) -> String {
    let path = raw.split('?').next().unwrap_or("/");
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Join a URL path onto `root`, rejecting anything that would escape it (`..`).
/// Returns None if the path tries to traverse above the web root.
fn safe_join(root: &str, url_path: &str) -> Option<PathBuf> {
    let mut p = PathBuf::from(root);
    for comp in Path::new(url_path).components() {
        match comp {
            Component::Normal(c) => p.push(c),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => return None, // no escaping the web root
            Component::Prefix(_) => return None,
        }
    }
    Some(p)
}

/// Pick a Content-Type from the file extension.
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript",
        "json" => "application/json",
        "txt" | "md" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

// ---------- logging + self-seeding ----------

fn log(method: &str, path: &str, status: u16) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    println!("httpd: [{secs}] {method} {path} -> {status}");
}

/// On first run, create a small demo site so there is always something to serve.
fn seed_site() {
    if fs::metadata(WEB_ROOT).map(|m| m.is_dir()).unwrap_or(false) {
        return;
    }
    if fs::create_dir_all(WEB_ROOT).is_err() {
        return;
    }
    let index = "<!doctype html><html><head><meta charset=\"utf-8\">\
        <title>oxbow httpd</title>\
        <style>body{font-family:monospace;margin:3rem;line-height:1.5}\
        h1{font-size:1.4rem}code{background:#eee;padding:.1rem .3rem}</style></head><body>\
        <h1>It works.</h1>\
        <p>This page is being served by <code>httpd</code> — a Rust <code>std</code> \
        program compiled for <code>x86_64-unknown-oxbow</code> and running on the oxbow \
        microkernel.</p>\
        <p>The bytes reached you over a real TCP connection: oxbow's e1000 driver and \
        from-scratch network stack, an accept loop in <code>std::net</code>, a handler \
        thread from <code>std::thread</code>, and this file read out of oxbow's ext2 \
        filesystem with <code>std::fs</code>.</p>\
        <ul><li><a href=\"about.txt\">about.txt</a></li>\
        <li><a href=\"files/\">files/</a> (directory listing)</li></ul>\
        <hr><small>httpd on oxbow</small></body></html>";
    let _ = fs::write(format!("{WEB_ROOT}/index.html"), index);
    let _ = fs::write(
        format!("{WEB_ROOT}/about.txt"),
        "httpd: a static file server in pure Rust std, running on the oxbow microkernel.\n\
         No libc, no Linux — std's networking, threads, and filesystem are all native oxbow.\n",
    );
    let _ = fs::create_dir_all(format!("{WEB_ROOT}/files"));
    let _ = fs::write(format!("{WEB_ROOT}/files/hello.txt"), "hello from oxbow's filesystem\n");
    let _ = fs::write(format!("{WEB_ROOT}/files/notes.md"), "# Notes\n\nServed by httpd on oxbow.\n");
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    main();
    std::process::exit(0);
}
