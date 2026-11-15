// ClaudeBot app: the window you talk to. It finds (or starts) the runner and
// serves the chat UI; the page then talks to the runner directly.
#![cfg_attr(windows, windows_subsystem = "windows")]
use serde_json::{json, Value};
use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Response, Server};

const PAGE: &str = include_str!("index.html");
const ADDR: &str = "127.0.0.1:7718";

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn data_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("ClaudeBot")
    } else {
        std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join(".config").join("claudebot")
    }
}

fn read_json(p: &PathBuf) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn reachable(url: &str) -> bool {
    let host = url.trim_start_matches("http://").trim_end_matches('/');
    let host = if host.contains(':') { host.to_string() } else { format!("{host}:80") };
    match host.to_socket_addrs() {
        Ok(mut addrs) => addrs.next().map(|a| TcpStream::connect_timeout(&a, Duration::from_millis(900)).is_ok()).unwrap_or(false),
        Err(_) => false,
    }
}

fn start_runner() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let name = if cfg!(windows) { "claudebot-runner.exe" } else { "claudebot-runner" };
    let path = exe.with_file_name(name);
    if !path.is_file() {
        return false;
    }
    let mut c = Command::new(path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0000_0010); // its own console window
    }
    c.spawn().is_ok()
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let ok = Command::new("cmd").args(["/c", "start", "", "msedge", &format!("--app={url}")])
            .creation_flags(0x0800_0000).status().map(|s| s.success()).unwrap_or(false);
        if !ok {
            let _ = Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
        }
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
}

fn main() {
    let url = format!("http://{ADDR}/");
    let no_open = std::env::args().any(|a| a == "--no-open");
    let server = match Server::http(ADDR) {
        Ok(s) => s,
        Err(_) => {
            // Already running: just bring the window up again.
            if !no_open {
                open_browser(&url);
            }
            return;
        }
    };
    let data = data_dir();
    let _ = std::fs::create_dir_all(&data);
    let launched = Arc::new(AtomicBool::new(false));
    let last_ping = Arc::new(AtomicU64::new(0));
    {
        let last_ping = last_ping.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(20));
            let t = last_ping.load(Ordering::Relaxed);
            if t != 0 && now().saturating_sub(t) > 300 {
                std::process::exit(0); // window was closed a while ago
            }
        });
    }
    if !no_open {
        open_browser(&url);
    }
    let json_hdr = Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..]).unwrap();
    let nostore = Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap();
    for mut req in server.incoming_requests() {
        let path = req.url().split('?').next().unwrap_or("").to_string();
        let method = req.method().clone();
        let reply = |req: tiny_http::Request, v: Value| {
            let _ = req.respond(Response::from_string(v.to_string()).with_header(json_hdr.clone()).with_header(nostore.clone()));
        };
        match (&method, path.as_str()) {
            (Method::Get, "/") => {
                let h = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
                let _ = req.respond(Response::from_string(PAGE).with_header(h).with_header(nostore.clone()));
            }
            (Method::Get, "/app/conn") => {
                last_ping.store(now(), Ordering::Relaxed);
                let remote = read_json(&data.join("app.json"));
                let is_remote = remote.is_some();
                let cfg = remote.or_else(|| read_json(&data.join("runner.json")));
                let (u, t) = match &cfg {
                    Some(c) => (c["url"].as_str().unwrap_or("").to_string(), c["token"].as_str().unwrap_or("").to_string()),
                    None => (String::new(), String::new()),
                };
                let mut up = !u.is_empty() && reachable(&u);
                let mut note = "";
                if !up && !is_remote {
                    if !launched.swap(true, Ordering::Relaxed) {
                        if start_runner() {
                            note = "starting";
                        } else {
                            note = "no-runner";
                        }
                    } else {
                        note = "starting";
                        up = false;
                    }
                }
                reply(req, json!({"url": u, "token": t, "reachable": up, "remote": is_remote, "note": note}));
            }
            (Method::Post, "/app/conn") => {
                let mut body = String::new();
                let _ = req.as_reader().take(10_000).read_to_string(&mut body);
                let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let mut u = v["url"].as_str().unwrap_or("").trim().trim_end_matches('/').to_string();
                let code = v["code"].as_str().unwrap_or("").trim().to_string();
                if u.is_empty() {
                    let _ = std::fs::remove_file(data.join("app.json")); // back to this PC's runner
                    launched.store(false, Ordering::Relaxed);
                    reply(req, json!({"ok": true}));
                    continue;
                }
                if !u.starts_with("http://") && !u.starts_with("https://") {
                    u = format!("http://{u}");
                }
                if u.matches(':').count() < 2 {
                    u.push_str(":7717");
                }
                let _ = std::fs::write(data.join("app.json"), json!({"url": u, "token": code}).to_string());
                reply(req, json!({"ok": true}));
            }
            (Method::Post, "/app/quit") => {
                reply(req, json!({"ok": true}));
                std::process::exit(0);
            }
            _ => {
                let _ = req.respond(Response::from_string("not found").with_status_code(404));
            }
        }
    }
}
