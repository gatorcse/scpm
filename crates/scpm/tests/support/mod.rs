//! A tiny HTTP/1.1 server for tests: serves a Maven layout from memory, optionally behind basic
//! auth, with scripted failures. One connection per request (`Connection: close`).

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
}

pub enum Reply {
    /// 200 with the body and its `Content-Length`.
    Ok(Vec<u8>),
    /// Any status with extra headers and an empty body.
    Status(u16, Vec<(String, String)>),
    /// 200 with only part of the body, no `Content-Length`, then the connection closes.
    Truncated(Vec<u8>),
}

type Handler = dyn Fn(&Request, usize) -> Reply + Send + Sync;

pub struct Server {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<Request>>>,
}

impl Server {
    /// Starts a server; `handler` gets each request and how many earlier requests had the
    /// same path.
    pub fn start(handler: impl Fn(&Request, usize) -> Reply + Send + Sync + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests: Arc<Mutex<Vec<Request>>> = Arc::default();
        let handler: Arc<Handler> = Arc::new(handler);
        let log = requests.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let handler = handler.clone();
                let log = log.clone();
                thread::spawn(move || serve(stream, &*handler, &log));
            }
        });
        Server { port, requests }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn count(&self, path: &str) -> usize {
        self.requests.lock().unwrap().iter().filter(|r| r.path == path).count()
    }

    /// Serves `files` (path → bytes); with `auth`, requires that basic-auth user and password.
    pub fn maven(files: HashMap<String, Vec<u8>>, auth: Option<(&str, &str)>) -> Server {
        let expected = auth.map(|(u, p)| format!("Basic {}", base64(format!("{u}:{p}").as_bytes())));
        Server::start(move |req, _| {
            if let Some(expected) = &expected
                && req.authorization.as_deref() != Some(expected.as_str())
            {
                return Reply::Status(401, vec![("WWW-Authenticate".into(), "Basic realm=\"test\"".into())]);
            }
            match files.get(&req.path) {
                Some(body) => Reply::Ok(body.clone()),
                None => Reply::Status(404, vec![]),
            }
        })
    }
}

fn serve(stream: TcpStream, handler: &Handler, log: &Mutex<Vec<Request>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
    let mut authorization = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("authorization")
        {
            authorization = Some(value.trim().to_string());
        }
    }
    let req = Request { method: method.clone(), path, authorization };
    let seen = {
        let mut log = log.lock().unwrap();
        let seen = log.iter().filter(|r| r.path == req.path).count();
        log.push(req.clone());
        seen
    };
    let head_only = method == "HEAD";
    let mut out = stream;
    let _ = match handler(&req, seen) {
        Reply::Ok(body) => {
            let mut r =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
            if !head_only {
                r.extend(body);
            }
            out.write_all(&r)
        }
        Reply::Status(status, headers) => {
            let mut r = format!("HTTP/1.1 {status} Status\r\nContent-Length: 0\r\nConnection: close\r\n");
            for (k, v) in headers {
                r.push_str(&format!("{k}: {v}\r\n"));
            }
            r.push_str("\r\n");
            out.write_all(r.as_bytes())
        }
        Reply::Truncated(body) => {
            let mut r = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
            if !head_only {
                r.extend(body);
            }
            out.write_all(&r)
        }
    };
    let _ = out.flush();
    let _ = out.shutdown(std::net::Shutdown::Both);
}

pub fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A one-artifact Maven repository: `com.example:hello:1.0` (POM and jar) under `/maven/`.
pub fn hello_repo_files(jar: &[u8]) -> HashMap<String, Vec<u8>> {
    let pom = br#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>hello</artifactId>
  <version>1.0</version>
</project>
"#;
    HashMap::from([
        ("/maven/com/example/hello/1.0/hello-1.0.pom".to_string(), pom.to_vec()),
        ("/maven/com/example/hello/1.0/hello-1.0.jar".to_string(), jar.to_vec()),
    ])
}

pub const HELLO_JAR_PATH: &str = "/maven/com/example/hello/1.0/hello-1.0.jar";
