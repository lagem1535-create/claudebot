// ClaudeBot runner: keeps a team of Claude Code agents, each with its own
// work folder (and its own virtual screen on Linux), and exposes them over HTTP.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Request, Response, Server};

mod assist;

const MAX_HOPS: u32 = 8;
const MAX_MSGS: usize = 600;

#[derive(Clone, Serialize, Deserialize)]
struct Agent {
    id: String,
    name: String,
    role: String,
    mode: String, // plan | acceptEdits | bypassPermissions
    browser: bool,
    #[serde(default)]
    model: String, // "" = Claude Code's default
    #[serde(default)]
    effort: String,
    #[serde(default)]
    actual_model: String,
    #[serde(default)]
    session: String,
    #[serde(default)]
    status: String, // idle | working | error
    #[serde(default)]
    display: u32,
    #[serde(default)]
    queue: u32,
    #[serde(default)]
    project: String,
    #[serde(default)]
    instructions: String,
    #[serde(default)]
    style: String,
    #[serde(default)]
    allowed: String,
    #[serde(default)]
    mcp: String, // 쉼표로 구분한 MCP 서버 이름 (내 Claude 에 연결된 것 중 이 에이전트가 쓰게 허용한 것)
    #[serde(default = "dtrue")]
    skills: bool,
    #[serde(default)]
    denied: String,
    #[serde(default)]
    imported: bool, // Claude Code 에서 가져온 대화: 사용자의 기존 폴더를 쓰므로 CLAUDE.md 를 덮어쓰지 않습니다.
    dir: String,
    created: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Msg {
    id: u64,
    agent: String,
    from: String, // "user" | "agent" | "system" | peer name
    kind: String, // text | thinking | tool | meta | error | handoff | peer | system
    text: String,
    time: u64,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    data: Value,
}

struct Job {
    text: String,
    from: String,
    hops: u32,
}

#[derive(Clone, Serialize, Deserialize)]
struct Project {
    id: String,
    name: String,
    dir: String,
    created: u64,
    #[serde(default)]
    instructions: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    projects: Vec<Project>,
    agents: Vec<Agent>,
    msgs: HashMap<String, Vec<Msg>>,
    next_id: u64,
}

#[derive(Default)]
struct Inner {
    s: Saved,
    events: VecDeque<(u64, Value)>,
    seq: u64,
    senders: HashMap<String, mpsc::Sender<Job>>,
    pids: HashMap<String, u32>,
    stopped: HashSet<String>,
}

struct App {
    inner: Mutex<Inner>,
    cv: Condvar,
    data: PathBuf,
    work: PathBuf,
    token: String,
    claude: Option<PathBuf>,
    version: String,
    screens: bool,
    backup: Mutex<Backup>,
    bk_busy: Mutex<()>,
    git: Mutex<GitCfg>,
    git_busy: Mutex<()>,
    tools: Mutex<Tools>,
    upd: Mutex<UpdInfo>,
    term_cwd: Mutex<PathBuf>,
    login_br: Mutex<Option<std::process::Child>>,
    extra: Mutex<assist::Extra>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn which(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for n in names {
            let p = dir.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn find_claude() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CLAUDEBOT_CLAUDE") {
        return Some(PathBuf::from(p));
    }
    if cfg!(windows) {
        if let Some(p) = which(&["claude.exe", "claude.cmd", "claude.bat"]) {
            return Some(p);
        }
        let home = std::env::var_os("USERPROFILE")?;
        let p = Path::new(&home).join(".local").join("bin").join("claude.exe");
        p.is_file().then_some(p)
    } else {
        if let Some(p) = which(&["claude"]) {
            return Some(p);
        }
        let home = std::env::var_os("HOME")?;
        let p = Path::new(&home).join(".local").join("bin").join("claude");
        p.is_file().then_some(p)
    }
}

fn dtrue() -> bool {
    true
}

// MCP 서버 이름을 도구 이름 접두사(mcp__이름)로 쓰는 형태로 바꿉니다 (영문·숫자·_·- 외에는 _).
fn mcp_norm(n: &str) -> String {
    n.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

fn data_dir() -> PathBuf {
    if cfg!(windows) {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        base.join("ClaudeBot")
    } else {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        home.join(".config").join("claudebot")
    }
}

// ---------- 백업 (Firebase Realtime Database REST, curl 사용) ----------
#[derive(Default, Clone, Serialize, Deserialize)]
struct Backup {
    #[serde(default)]
    url: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    auth: String,
    #[serde(default)]
    last_ok: u64,
    #[serde(default)]
    last_err: String,
    #[serde(default)]
    last_hash: u64,
    #[serde(default)]
    hist_hash: u64,
}

fn bk_norm_url(u: &str) -> Option<String> {
    let u = u.trim().trim_end_matches('/').to_string();
    if u.is_empty() || u.chars().any(|c| c.is_whitespace() || c == '"' || c == '?' || c == '#' || c == '\\') {
        return None;
    }
    if std::env::var("CLAUDEBOT_BACKUP_ALLOW_HTTP").is_ok() && u.starts_with("http://127.0.0.1") {
        return Some(u);
    }
    let rest = u.strip_prefix("https://")?;
    let host = rest.split('/').next().unwrap_or("");
    if rest.contains('/') {
        return None; // 데이터베이스 주소만 (경로 없이)
    }
    if host.len() > 7 && (host.ends_with(".firebaseio.com") || host.ends_with(".firebasedatabase.app")) {
        Some(u)
    } else {
        None
    }
}

fn bk_key_ok(k: &str) -> bool {
    k.len() >= 16 && k.len() <= 64 && k.chars().all(|c| c.is_ascii_alphanumeric())
}

fn bk_new_key() -> String {
    use std::hash::{BuildHasher, Hasher};
    let alphabet = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut out = String::new();
    for i in 0..28u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(now() ^ (i << 17));
        out.push(alphabet[(h.finish() % alphabet.len() as u64) as usize] as char);
    }
    out
}

fn bk_enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{:02X}", b));
        }
    }
    o
}

fn bk_full(b: &Backup, leaf: &str) -> String {
    let mut u = format!("{}/claudebot/{}/{}.json", b.url, b.key, leaf);
    if !b.auth.is_empty() {
        u.push_str(&format!("?auth={}", bk_enc(&b.auth)));
    }
    u
}

// curl 로 요청합니다. 주소(인증 값 포함)는 명령줄이 아니라 표준입력으로 넘깁니다.
fn bk_curl(method: &str, url: &str, body_file: Option<&Path>) -> Result<String, String> {
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "-m", "120", "-X", method, "-H", "Content-Type: application/json", "-w", "\n__HTTP__%{http_code}", "-K", "-"]);
    if let Some(f) = body_file {
        cmd.arg("--data-binary").arg(format!("@{}", f.display()));
    }
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
        .map_err(|_| "curl 을 실행하지 못했습니다. curl 이 설치되어 있어야 합니다 (Windows 10/11 에는 기본 포함).".to_string())?;
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(format!("url = \"{}\"\n", url).as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = match text.rsplit_once("\n__HTTP__") {
        Some((b, c)) => (b.to_string(), c.trim().to_string()),
        None => {
            let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(if e.is_empty() { "응답이 없습니다.".into() } else { e.chars().take(200).collect() });
        }
    };
    if code.starts_with('2') {
        Ok(body)
    } else {
        let hint = if code == "401" || code == "403" { " (데이터베이스 규칙이 막고 있습니다. 규칙을 확인하세요.)" } else { "" };
        Err(format!("HTTP {code}{hint} {}", body.chars().take(120).collect::<String>()))
    }
}

fn bk_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn bk_chunks(s: &str, max: usize) -> Vec<&str> {
    let mut v = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let mut j = (i + max).min(s.len());
        while !s.is_char_boundary(j) {
            j -= 1;
        }
        v.push(&s[i..j]);
        i = j;
    }
    if v.is_empty() {
        v.push("");
    }
    v
}

fn bk_load(data: &Path) -> Backup {
    std::fs::read_to_string(data.join("backup.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn bk_store(data: &Path, b: &Backup) {
    let _ = std::fs::write(data.join("backup.json"), serde_json::to_string(b).unwrap_or_default());
}

// 지금의 대화 전체를 데이터베이스에 올립니다.
fn bk_push(app: &Arc<App>) -> Result<usize, String> {
    let _busy = app.bk_busy.lock().unwrap();
    let b = app.backup.lock().unwrap().clone();
    if b.url.is_empty() || b.key.is_empty() {
        return Err("백업 주소가 설정되지 않았습니다.".into());
    }
    let txt = {
        let g = app.inner.lock().unwrap();
        serde_json::to_string(&g.s).map_err(|e| e.to_string())?
    };
    let h = bk_hash(&txt);
    let tmp = app.data.join("bk_upload.json");
    let chunks = bk_chunks(&txt, 4_000_000);
    let res = (|| -> Result<(), String> {
        for (i, c) in chunks.iter().enumerate() {
            std::fs::write(&tmp, serde_json::to_string(c).unwrap_or_default()).map_err(|e| e.to_string())?;
            bk_curl("PUT", &bk_full(&b, &format!("c{:03}", i)), Some(&tmp))?;
        }
        // 화면에서 잘려 나간 오래된 대화(history.jsonl)도 올립니다 (바뀐 때만).
        let hist = std::fs::read_to_string(app.data.join("history.jsonl")).unwrap_or_default();
        let hh = bk_hash(&hist);
        if hh != b.hist_hash && !hist.is_empty() {
            let hc = bk_chunks(&hist, 4_000_000);
            for (i, c) in hc.iter().enumerate() {
                std::fs::write(&tmp, serde_json::to_string(c).unwrap_or_default()).map_err(|e| e.to_string())?;
                bk_curl("PUT", &bk_full(&b, &format!("h{:03}", i)), Some(&tmp))?;
            }
            std::fs::write(&tmp, json!({"chunks": hc.len(), "bytes": hist.len()}).to_string()).map_err(|e| e.to_string())?;
            bk_curl("PUT", &bk_full(&b, "hmeta"), Some(&tmp))?;
            app.backup.lock().unwrap().hist_hash = hh;
        }
        let meta = json!({"updated": now(), "bytes": txt.len(), "chunks": chunks.len(), "version": app.version}).to_string();
        std::fs::write(&tmp, meta).map_err(|e| e.to_string())?;
        bk_curl("PUT", &bk_full(&b, "meta"), Some(&tmp))?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&tmp);
    let mut bm = app.backup.lock().unwrap();
    match res {
        Ok(()) => {
            bm.last_ok = now();
            bm.last_err.clear();
            bm.last_hash = h;
            bk_store(&app.data, &bm);
            Ok(chunks.len())
        }
        Err(e) => {
            bm.last_err = e.clone();
            bk_store(&app.data, &bm);
            Err(e)
        }
    }
}

// 데이터베이스에서 내려받아 Saved 로 만듭니다.
fn bk_pull(app: &Arc<App>) -> Result<Saved, String> {
    let b = app.backup.lock().unwrap().clone();
    if b.url.is_empty() || b.key.is_empty() {
        return Err("백업 주소가 설정되지 않았습니다.".into());
    }
    let meta: Value = serde_json::from_str(&bk_curl("GET", &bk_full(&b, "meta"), None)?).unwrap_or(Value::Null);
    let n = meta["chunks"].as_u64().ok_or("이 백업 키로 저장된 데이터가 없습니다.")? as usize;
    if n == 0 || n > 200 {
        return Err("백업 데이터가 올바르지 않습니다.".into());
    }
    let mut all = String::new();
    for i in 0..n {
        let body = bk_curl("GET", &bk_full(&b, &format!("c{:03}", i)), None)?;
        let part: String = serde_json::from_str(&body).map_err(|_| "백업 조각을 읽지 못했습니다.".to_string())?;
        all.push_str(&part);
    }
    serde_json::from_str::<Saved>(&all).map_err(|e| format!("백업 내용을 해석하지 못했습니다: {e}"))
}

// 오래된 대화(history.jsonl)를 내려받아 이 기기의 기록 파일로 되돌립니다. 없으면 조용히 넘어갑니다.
fn bk_pull_hist(app: &Arc<App>) {
    let b = app.backup.lock().unwrap().clone();
    let meta: Value = match bk_curl("GET", &bk_full(&b, "hmeta"), None) {
        Ok(t) => serde_json::from_str(&t).unwrap_or(Value::Null),
        Err(_) => return,
    };
    let Some(n) = meta["chunks"].as_u64() else { return };
    if n == 0 || n > 200 {
        return;
    }
    let mut all = String::new();
    for i in 0..n {
        match bk_curl("GET", &bk_full(&b, &format!("h{:03}", i)), None).ok().and_then(|t| serde_json::from_str::<String>(&t).ok()) {
            Some(p) => all.push_str(&p),
            None => return,
        }
    }
    let _ = std::fs::write(app.data.join("history.jsonl"), &all);
    app.backup.lock().unwrap().hist_hash = bk_hash(&all);
}

fn bk_normalize(saved: &mut Saved) {
    for a in &mut saved.agents {
        a.status = "idle".into();
        a.queue = 0;
        a.mode = clean_mode(&json!(a.mode));
    }
    for list in saved.msgs.values_mut() {
        for m in list.iter_mut() {
            if m.data["live"] == true {
                m.data = Value::Null;
            }
        }
    }
}

// 다른 기기에서 받은 백업이면 폴더 경로가 이 기기에 없으므로, 이 기기의 작업 폴더 아래로 옮겨 적습니다.
fn bk_rebase(work: &Path, sv: &mut Saved) {
    let last = |d: &str| -> String { d.replace('\\', "/").trim_end_matches('/').rsplit('/').next().unwrap_or("x").to_string() };
    for p in &mut sv.projects {
        if !Path::new(&p.dir).starts_with(work) {
            p.dir = work.join("projects").join(last(&p.dir)).to_string_lossy().to_string();
        }
    }
    let pdirs: HashMap<String, String> = sv.projects.iter().map(|p| (p.id.clone(), p.dir.clone())).collect();
    for a in &mut sv.agents {
        if !Path::new(&a.dir).starts_with(work) {
            let l = last(&a.dir);
            a.dir = match pdirs.get(&a.project) {
                Some(pd) if !a.project.is_empty() => PathBuf::from(pd).join(l),
                _ => work.join(l),
            }
            .to_string_lossy()
            .to_string();
        }
    }
}

fn bk_status(app: &Arc<App>) -> Value {
    let b = app.backup.lock().unwrap();
    json!({"url": b.url, "key": b.key, "has_auth": !b.auth.is_empty(), "last_ok": b.last_ok, "last_error": b.last_err})
}

// ---------- git 자동 저장 + STATUS.md ----------
#[derive(Default, Clone, Serialize, Deserialize)]
struct GitCfg {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    last_ok: u64,
    #[serde(default)]
    last_err: String,
    #[serde(default)]
    last_note: String,
    #[serde(default)]
    status_hash: u64,
}

fn kst(ms: u64) -> String {
    let secs = (ms / 1000) as i64 + 9 * 3600;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

fn git_load(data: &Path) -> GitCfg {
    std::fs::read_to_string(data.join("git.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn git_store(data: &Path, c: &GitCfg) {
    let _ = std::fs::write(data.join("git.json"), serde_json::to_string(c).unwrap_or_default());
}

fn git_run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut child = Command::new("git")
        .arg("-C").arg(dir)
        .args(["-c", "user.name=ClaudeBot", "-c", "user.email=claudebot@users.noreply.github.com"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().map_err(|_| "git 을 실행하지 못했습니다. git 이 설치되어 있어야 합니다.".to_string())?;
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if t0.elapsed() > Duration::from_secs(90) {
                    let _ = child.kill();
                    return Err("git 이 90초 안에 끝나지 않았습니다 (로그인/권한 문제일 수 있습니다).".into());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let o = child.wait_with_output().map_err(|e| e.to_string())?;
    let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
    if o.status.success() {
        Ok(out)
    } else {
        Err(format!("git {}: {}", args.first().unwrap_or(&""), err.chars().take(300).collect::<String>()))
    }
}

fn git_root(work: &Path) -> Option<PathBuf> {
    git_run(work, &["rev-parse", "--show-toplevel"]).ok().filter(|s| !s.is_empty()).map(PathBuf::from)
}

fn status_md(app: &Arc<App>) -> String {
    let g = app.inner.lock().unwrap();
    let mut t = String::from("# ClaudeBot 에이전트 상태\n\n| 에이전트 | 프로젝트 | 상태 | 대기 | 마지막 활동 | 마지막 요청 |\n|---|---|---|---|---|---|\n");
    for a in &g.s.agents {
        let proj = g.s.projects.iter().find(|p| p.id == a.project).map(|p| p.name.clone()).unwrap_or_else(|| "-".into());
        let st = match a.status.as_str() {
            "working" => "🟢 일하는 중",
            "error" => "🔴 오류",
            _ => "⚪ 대기",
        };
        let msgs = g.s.msgs.get(&a.id);
        let last_t = msgs.and_then(|v| v.last()).map(|m| kst(m.time)).unwrap_or_else(|| "-".into());
        let last_req: String = msgs
            .and_then(|v| v.iter().rev().find(|m| m.from == "user" && m.kind == "text"))
            .map(|m| m.text.replace(['\n', '\r', '|'], " ").chars().take(50).collect())
            .unwrap_or_default();
        t.push_str(&format!("| {} | {} | {} | {} | {} | {} |\n", a.name.replace('|', "/"), proj.replace('|', "/"), st, a.queue, last_t, last_req));
    }
    if g.s.agents.is_empty() {
        t.push_str("| (에이전트 없음) | | | | | |\n");
    }
    t
}

fn git_sync(app: &Arc<App>) -> Result<String, String> {
    let _busy = app.git_busy.lock().unwrap();
    let res = (|| -> Result<String, String> {
        let root = git_root(&app.work).ok_or("작업 폴더가 git 저장소 안에 없습니다. 실행기를 저장소 폴더 안에서 --work work 로 켜 주세요.")?;
        let rel = app.work.canonicalize().unwrap_or(app.work.clone());
        let rootc = root.canonicalize().unwrap_or(root.clone());
        let relw = rel.strip_prefix(&rootc).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let wpath = if relw.is_empty() { ".".to_string() } else { relw };
        let gi = app.work.join(".gitignore");
        if !gi.exists() {
            let _ = std::fs::write(&gi, "node_modules/\n__pycache__/\n*.pyc\n*.log\n*.tmp\n.cache/\n.config/\n.npm/\n.venv/\n");
        }
        // 상태표는 내용이 바뀐 때만 다시 씁니다 (갱신 시각만 달라진 커밋이 쌓이지 않게).
        let body = status_md(app);
        let h = bk_hash(&body);
        if h != app.git.lock().unwrap().status_hash || !root.join("STATUS.md").exists() {
            let full = format!("{body}\n_갱신: {} (한국 시간)_\n", kst(now()));
            std::fs::write(root.join("STATUS.md"), full).map_err(|e| e.to_string())?;
            app.git.lock().unwrap().status_hash = h;
        }
        git_run(&root, &["add", "-A", "--", &wpath, "STATUS.md"])?;
        let st = git_run(&root, &["status", "--porcelain", "--", &wpath, "STATUS.md"])?;
        if st.trim().is_empty() {
            return Ok("변경 없음".into());
        }
        let n = st.lines().count();
        git_run(&root, &["commit", "-m", &format!("ClaudeBot 자동 저장 {} (파일 {}개)", kst(now()), n), "--", &wpath, "STATUS.md"])?;
        git_run(&root, &["push", "origin", "HEAD"])?;
        Ok(format!("파일 {n}개를 저장하고 push 했습니다"))
    })();
    let mut c = app.git.lock().unwrap();
    match &res {
        Ok(n) => {
            c.last_ok = now();
            c.last_err.clear();
            c.last_note = n.clone();
        }
        Err(e) => c.last_err = e.clone(),
    }
    git_store(&app.data, &c);
    res
}

fn git_status(app: &Arc<App>) -> Value {
    let c = app.git.lock().unwrap().clone();
    let root = git_root(&app.work);
    let remote = root.as_ref().and_then(|r| git_run(r, &["remote", "get-url", "origin"]).ok()).unwrap_or_default();
    // 주소에 토큰이 섞여 있어도 화면에는 내보내지 않습니다.
    let remote = match remote.split_once('@') {
        Some((_, rest)) if remote.starts_with("http") => format!("https://{rest}"),
        _ => remote,
    };
    json!({"enabled": c.enabled, "in_repo": root.is_some(), "repo": root.map(|r| r.to_string_lossy().to_string()).unwrap_or_default(),
           "remote": remote, "last_ok": c.last_ok, "last_error": c.last_err, "note": c.last_note, "work": app.work.to_string_lossy()})
}

// ---------- 터미널 / 내 컴퓨터 저장소 / 자동 업데이트 ----------
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn hdir() -> PathBuf {
    home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn jload<T: Default + for<'a> Deserialize<'a>>(data: &Path, name: &str) -> T {
    std::fs::read_to_string(data.join(name)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn jstore<T: Serialize>(data: &Path, name: &str, v: &T) {
    let _ = std::fs::write(data.join(name), serde_json::to_string(v).unwrap_or_default());
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct Tools {
    #[serde(default)]
    term_enabled: bool,
    #[serde(default)]
    repo_url: String,
    #[serde(default)]
    repo_dir: String,
    #[serde(default)]
    upd_repo: String,
}

#[derive(Default, Clone)]
struct UpdInfo {
    checked: u64,
    latest: String,
    available: bool,
    notes: String,
    err: String,
    assets: Vec<(String, String)>,
}

// 명령 한 줄을 실행하고 (종료코드, 출력)을 돌려줍니다. 입력을 기다리는 프로그램은 쓸 수 없습니다.
fn term_exec(cwd: &Path, cmd: &str) -> (i32, String) {
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", &format!("{cmd} 2>&1")]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", &format!("{cmd} 2>&1")]);
        c
    };
    let child = c.current_dir(cwd).env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (-1, format!("실행하지 못했습니다: {e}")),
    };
    let mut so = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = so.read(&mut chunk) {
            if n == 0 {
                break;
            }
            if buf.len() < 200_000 {
                buf.extend_from_slice(&chunk[..n]);
            }
        }
        let _ = tx.send(String::from_utf8_lossy(&buf).to_string());
    });
    let t0 = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(-1),
            Ok(None) => {
                if t0.elapsed() > Duration::from_secs(300) {
                    let _ = child.kill();
                    let out = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
                    return (-1, format!("{out}\n[5분이 지나 중단했습니다]"));
                }
                std::thread::sleep(Duration::from_millis(80));
            }
            Err(_) => break -1,
        }
    };
    (code, rx.recv_timeout(Duration::from_secs(3)).unwrap_or_default())
}

fn repo_url_ok(u: &str) -> bool {
    if std::env::var("CLAUDEBOT_ALLOW_LOCAL_REPO").is_ok() && u.starts_with('/') {
        return true;
    }
    let Some(rest) = u.strip_prefix("https://github.com/") else { return false };
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut it = rest.split('/');
    let (a, b, c) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next());
    let ok = |s: &str| !s.is_empty() && s.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.');
    c.is_none() && ok(a) && ok(b)
}

fn repo_status(app: &Arc<App>) -> Value {
    let t = app.tools.lock().unwrap().clone();
    let dir = if t.repo_dir.is_empty() { hdir().join("ClaudeBot-저장소").to_string_lossy().to_string() } else { t.repo_dir.clone() };
    let cloned = Path::new(&dir).join(".git").exists();
    let last = if cloned { git_run(Path::new(&dir), &["log", "-1", "--format=%cd %s", "--date=short"]).unwrap_or_default() } else { String::new() };
    json!({"url": t.repo_url, "dir": dir, "cloned": cloned, "last": last})
}

fn repo_pull(app: &Arc<App>) -> Result<String, String> {
    let t = app.tools.lock().unwrap().clone();
    if t.repo_url.is_empty() {
        return Err("저장소 주소를 먼저 저장하세요.".into());
    }
    let dir = PathBuf::from(if t.repo_dir.is_empty() { hdir().join("ClaudeBot-저장소").to_string_lossy().to_string() } else { t.repo_dir.clone() });
    if dir.join(".git").exists() {
        let o = git_run(&dir, &["pull", "--rebase", "--autostash"])?;
        Ok(format!("최신으로 갱신했습니다. {}", o.lines().last().unwrap_or("")))
    } else {
        if dir.exists() && std::fs::read_dir(&dir).map(|mut d| d.next().is_some()).unwrap_or(false) {
            return Err("그 폴더가 비어 있지 않습니다. 빈 폴더(또는 없는 폴더)를 지정하세요.".into());
        }
        let parent = dir.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
        std::fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
        git_run(&parent, &["clone", &t.repo_url, &dir.to_string_lossy()])?;
        Ok("내 컴퓨터로 받았습니다.".into())
    }
}

fn repo_push(app: &Arc<App>) -> Result<String, String> {
    let t = app.tools.lock().unwrap().clone();
    let dir = PathBuf::from(if t.repo_dir.is_empty() { hdir().join("ClaudeBot-저장소").to_string_lossy().to_string() } else { t.repo_dir.clone() });
    if !dir.join(".git").exists() {
        return Err("아직 받아 오지 않았습니다. 먼저 '받기'를 누르세요.".into());
    }
    git_run(&dir, &["add", "-A"])?;
    if git_run(&dir, &["status", "--porcelain"])?.trim().is_empty() {
        return Ok("올릴 변경이 없습니다.".into());
    }
    git_run(&dir, &["commit", "-m", &format!("내 컴퓨터에서 수정 {}", kst(now()))])?;
    git_run(&dir, &["pull", "--rebase", "--autostash"]).ok();
    git_run(&dir, &["push", "origin", "HEAD"])?;
    Ok("변경을 GitHub 로 올렸습니다.".into())
}

fn ver_parts(v: &str) -> Vec<u64> {
    v.trim_start_matches(['v', 'V']).split(|c: char| !c.is_ascii_digit()).filter(|x| !x.is_empty()).map(|x| x.parse().unwrap_or(0)).collect()
}

fn upd_api() -> String {
    std::env::var("CLAUDEBOT_UPDATE_API").unwrap_or_else(|_| "https://api.github.com".into())
}

fn upd_repo_ok(r: &str) -> bool {
    let mut it = r.split('/');
    let (a, b, c) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next());
    let ok = |s: &str| !s.is_empty() && s.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.');
    c.is_none() && ok(a) && ok(b)
}

fn upd_check(app: &Arc<App>) -> Result<(), String> {
    let repo = app.tools.lock().unwrap().upd_repo.clone();
    if repo.is_empty() {
        return Err("업데이트를 확인할 GitHub 저장소(주인/이름)를 먼저 저장하세요.".into());
    }
    let url = format!("{}/repos/{}/releases/latest", upd_api(), repo);
    let res = bk_curl("GET", &url, None).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|_| "응답을 읽지 못했습니다.".to_string()));
    let mut u = app.upd.lock().unwrap();
    u.checked = now();
    match res {
        Ok(v) => {
            let tag = v["tag_name"].as_str().unwrap_or("").to_string();
            if tag.is_empty() {
                u.err = "릴리스가 아직 없습니다 (저장소의 Releases 에 새 버전을 만들어야 합니다).".into();
                return Err(u.err.clone());
            }
            u.available = ver_parts(&tag) > ver_parts(APP_VERSION);
            u.latest = tag;
            u.notes = v["body"].as_str().unwrap_or("").chars().take(600).collect();
            u.assets = v["assets"].as_array().map(|a| a.iter().filter_map(|x| Some((x["name"].as_str()?.to_string(), x["browser_download_url"].as_str()?.to_string()))).collect()).unwrap_or_default();
            u.err.clear();
            Ok(())
        }
        Err(e) => {
            u.err = if e.contains("404") { "저장소나 릴리스를 찾지 못했습니다. 저장소는 Public 이어야 하고 Releases 가 있어야 합니다.".into() } else { e };
            Err(u.err.clone())
        }
    }
}

fn upd_status(app: &Arc<App>) -> Value {
    let u = app.upd.lock().unwrap().clone();
    let repo = app.tools.lock().unwrap().upd_repo.clone();
    json!({"version": APP_VERSION, "repo": repo, "checked": u.checked, "latest": u.latest, "available": u.available, "notes": u.notes, "error": u.err})
}

fn dl_url_ok(u: &str) -> bool {
    if std::env::var("CLAUDEBOT_UPDATE_API").is_ok() && u.starts_with("http://127.0.0.1") {
        return true;
    }
    u.starts_with("https://github.com/") || u.starts_with("https://objects.githubusercontent.com/") || u.starts_with("https://release-assets.githubusercontent.com/")
}

fn upd_apply(app: &Arc<App>) -> Result<String, String> {
    upd_check(app)?;
    let u = app.upd.lock().unwrap().clone();
    if !u.available {
        return Err("이미 최신 버전입니다.".into());
    }
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = me.parent().ok_or("실행 파일 위치를 알 수 없습니다.")?.to_path_buf();
    let (run_asset, app_asset, app_name) = if cfg!(windows) { ("claudebot-runner.exe", "claudebot.exe", "claudebot.exe") } else { ("claudebot-runner-linux", "claudebot-linux", "claudebot") };
    let find = |n: &str| u.assets.iter().find(|a| a.0 == n).map(|a| a.1.clone());
    let run_url = find(run_asset).ok_or(format!("이 릴리스에 {run_asset} 파일이 없습니다."))?;
    let mut jobs = vec![(run_url, me.clone())];
    if let Some(au) = find(app_asset) {
        let target = dir.join(app_name);
        if target.exists() {
            jobs.push((au, target));
        }
    }
    let magic: &[u8] = if cfg!(windows) { b"MZ" } else { b"\x7fELF" };
    // 먼저 전부 받아서 검사하고, 문제가 없을 때만 교체합니다.
    let mut staged = Vec::new();
    for (url, target) in &jobs {
        if !dl_url_ok(url) {
            return Err("허용되지 않은 다운로드 주소입니다.".into());
        }
        let newp = PathBuf::from(format!("{}.new", target.display()));
        let _ = std::fs::remove_file(&newp);
        let mut cmd = Command::new("curl");
        cmd.args(["-sSL", "-m", "300", "--fail", "-o"]).arg(&newp).args(["-K", "-"]);
        let mut ch = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|_| "curl 을 실행하지 못했습니다.".to_string())?;
        if let Some(mut si) = ch.stdin.take() {
            let _ = si.write_all(format!("url = \"{}\"\n", url).as_bytes());
        }
        let o = ch.wait_with_output().map_err(|e| e.to_string())?;
        if !o.status.success() {
            return Err(format!("다운로드 실패: {}", String::from_utf8_lossy(&o.stderr).trim().chars().take(150).collect::<String>()));
        }
        let head = std::fs::read(&newp).map(|b| (b.len(), b.starts_with(magic))).unwrap_or((0, false));
        if head.0 < 100_000 || !head.1 {
            let _ = std::fs::remove_file(&newp);
            return Err("내려받은 파일이 실행 파일 형식이 아닙니다. 교체하지 않았습니다.".into());
        }
        staged.push((newp, target.clone()));
    }
    for (newp, target) in &staged {
        let old = PathBuf::from(format!("{}.old", target.display()));
        let _ = std::fs::remove_file(&old);
        std::fs::rename(target, &old).map_err(|e| format!("현재 파일을 옮기지 못했습니다: {e}"))?;
        if let Err(e) = std::fs::rename(newp, target) {
            let _ = std::fs::rename(&old, target);
            return Err(format!("새 파일을 놓지 못했습니다(원래대로 되돌렸습니다): {e}"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755));
        }
    }
    // 잠시 뒤 새 파일로 다시 시작합니다.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let data = app.data.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1200));
        if cfg!(windows) {
            let q = |s: &str| format!("\"{}\"", s);
            let line = format!("@echo off\r\nping -n 3 127.0.0.1 >nul\r\n{} {}\r\n", q(&me.to_string_lossy()), args.iter().map(|a| q(a)).collect::<Vec<_>>().join(" "));
            let f = data.join("restart.cmd");
            let _ = std::fs::write(&f, line);
            let _ = Command::new("cmd").args(["/C", "start", "", "/MIN"]).arg(&f).spawn();
        } else {
            let _ = Command::new("sh").arg("-c").arg("sleep 2; exec \"$0\" \"$@\"").arg(&me).args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        }
        std::process::exit(0);
    });
    Ok(format!("{} 로 업데이트했습니다. 곧 다시 시작합니다.", u.latest))
}

// ---------- 로그인 한 번으로 모든 에이전트 브라우저에 적용 ----------
const LOGIN_DISPLAY: u32 = 20;

fn chromium_exe() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CLAUDEBOT_CHROMIUM") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut cache = Vec::new();
    if cfg!(windows) {
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            cache.push((PathBuf::from(l).join("ms-playwright"), "chrome-win/chrome.exe"));
        }
    } else if let Some(h) = home_dir() {
        cache.push((h.join(".cache").join("ms-playwright"), "chrome-linux/chrome"));
    }
    for (dir, rel) in cache {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.starts_with("chromium-")).collect();
            names.sort();
            for n in names.iter().rev() {
                let p = dir.join(n).join(rel);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    if cfg!(windows) {
        for p in ["C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe", "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe", "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe"] {
            if Path::new(p).is_file() {
                return Some(PathBuf::from(p));
            }
        }
        None
    } else {
        which(&["chromium", "chromium-browser", "google-chrome", "google-chrome-stable"])
    }
}

// 로그인 저장소 범위: "all" = 모든 에이전트가 같이 씀, 프로젝트 id = 그 프로젝트 안의 에이전트만 씀.
static LOGIN_SCOPE: Mutex<String> = Mutex::new(String::new());

fn scope_clean(scope: &str) -> String {
    let t: String = scope.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(60).collect();
    if t.is_empty() || t == "all" { "all".into() } else { t }
}

/// 브라우저 프로필(Chromium 이 쓰는 폴더)을 둘 곳입니다.
/// Chromium 은 프로필 안에 심볼릭 링크(SingletonLock)를 만들어야 하는데, 안드로이드 내장 메모리처럼 링크를 못 만드는 곳에서는
/// "Browser is already in use" 로 실패합니다. 그래서 데이터 폴더에서 링크가 안 만들어지면 홈 폴더로 자동 옮깁니다.
/// 직접 정하려면 환경변수 CLAUDEBOT_BROWSER_DIR 를 쓰세요.
fn browser_base(data: &Path) -> PathBuf {
    static BASE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        if let Ok(d) = std::env::var("CLAUDEBOT_BROWSER_DIR") {
            if !d.trim().is_empty() {
                let p = PathBuf::from(d.trim());
                let _ = std::fs::create_dir_all(&p);
                return p;
            }
        }
        #[cfg(unix)]
        {
            let probe = data.join(".symlink_probe");
            let _ = std::fs::remove_file(&probe);
            let ok = std::os::unix::fs::symlink("probe", &probe).is_ok();
            let _ = std::fs::remove_file(&probe);
            if !ok {
                if let Ok(home) = std::env::var("HOME") {
                    let p = PathBuf::from(home).join(".claudebot-browser");
                    if std::fs::create_dir_all(&p).is_ok() {
                        return p;
                    }
                }
            }
        }
        data.to_path_buf()
    })
    .clone()
}

fn login_profile(app: &App, scope: &str) -> PathBuf {
    let sc = scope_clean(scope);
    let base = browser_base(&app.data);
    if sc == "all" { base.join("browser_login") } else { base.join(format!("browser_login_{sc}")) }
}

fn login_stop(app: &App) -> bool {
    let child = app.login_br.lock().unwrap().take();
    let Some(mut ch) = child else { return false };
    let pid = ch.id();
    // 쿠키가 파일에 저장되도록 먼저 정중하게 끝내 달라고 요청합니다.
    if cfg!(windows) {
        let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    } else {
        let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
    }
    let t0 = Instant::now();
    loop {
        if let Ok(Some(_)) = ch.try_wait() {
            break;
        }
        if t0.elapsed() > Duration::from_secs(8) {
            let _ = ch.kill();
            let _ = ch.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let sc = LOGIN_SCOPE.lock().unwrap().clone();
    let _ = std::fs::write(login_profile(app, &sc).join(".cb_login_stamp"), now().to_string());
    true
}

fn login_open(app: &Arc<App>, url: &str, scope: &str) -> Result<(), String> {
    login_stop(app);
    let scope = scope_clean(scope);
    if scope != "all" && !app.inner.lock().unwrap().s.projects.iter().any(|p| p.id == scope) {
        return Err("프로젝트를 찾지 못했습니다.".into());
    }
    let exe = chromium_exe().ok_or("Chromium(또는 Chrome)을 찾지 못했습니다. 터미널에서 `npx playwright install chromium` 을 한 번 실행한 뒤 다시 눌러 주세요.")?;
    let prof = login_profile(app, &scope);
    std::fs::create_dir_all(&prof).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg(format!("--user-data-dir={}", prof.display()))
        .args(["--no-first-run", "--no-default-browser-check", "--password-store=basic", "--use-mock-keychain", "--window-size=1280,720", "--window-position=0,0"]);
    if ko_locale_ok() {
        cmd.arg("--lang=ko");
        ko_env(&mut cmd);
    }
    if cfg!(unix) {
        cmd.arg("--no-sandbox");
        if app.screens {
            let sock = format!("/tmp/.X11-unix/X{LOGIN_DISPLAY}");
            if !Path::new(&sock).exists() {
                let _ = Command::new("Xvfb").args([&format!(":{LOGIN_DISPLAY}"), "-screen", "0", "1280x720x24", "-nolisten", "tcp"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
                std::thread::sleep(Duration::from_millis(700));
                decorate_display(&app.data, &format!(":{LOGIN_DISPLAY}"), None);
            }
            cmd.env("DISPLAY", format!(":{LOGIN_DISPLAY}"));
        } else if std::env::var_os("DISPLAY").is_none() {
            return Err("이 컴퓨터에는 화면(Xvfb)이 없어 브라우저를 열 수 없습니다.".into());
        }
    }
    let u = if url.starts_with("https://") || url.starts_with("http://") { url } else { "https://accounts.google.com" };
    cmd.arg(u).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let ch = cmd.spawn().map_err(|e| format!("브라우저를 시작하지 못했습니다: {e}"))?;
    *LOGIN_SCOPE.lock().unwrap() = scope;
    *app.login_br.lock().unwrap() = Some(ch);
    Ok(())
}

fn login_status(app: &Arc<App>, scope: &str) -> Value {
    let scope = scope_clean(scope);
    let open = {
        let mut g = app.login_br.lock().unwrap();
        match g.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    };
    let open_scope = LOGIN_SCOPE.lock().unwrap().clone();
    let stamp = std::fs::read_to_string(login_profile(app, &scope).join(".cb_login_stamp")).ok().and_then(|t| t.trim().parse::<u64>().ok()).unwrap_or(0);
    json!({"open": open, "open_scope": if open { open_scope } else { String::new() }, "scope": scope, "saved": stamp, "chromium": chromium_exe().is_some(), "viewable": app.screens, "display": LOGIN_DISPLAY})
}

fn copy_profile(src: &Path, dst: &Path, depth: u32) {
    if depth > 12 {
        return;
    }
    let _ = std::fs::create_dir_all(dst);
    let Ok(rd) = std::fs::read_dir(src) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("Singleton") || name == "lockfile" || ["Cache", "Code Cache", "GPUCache", "ShaderCache", "GrShaderCache", "Crashpad", "DawnCache", "component_crx_cache"].contains(&name.as_str()) {
            continue;
        }
        let p = e.path();
        let q = dst.join(&name);
        if p.is_dir() {
            copy_profile(&p, &q, depth + 1);
        } else {
            let _ = std::fs::copy(&p, &q);
        }
    }
}

// 로그인 브라우저의 저장 시각이 바뀌었으면, 이 에이전트의 브라우저 프로필에 로그인 정보를 복사해 둡니다.
fn sync_login_profile(app: &App, agent_profile: &Path, project: &str) {
    // 프로젝트에 저장된 로그인이 있으면 그것을, 없으면 전체 공용 로그인을 이어받습니다.
    let has = |sc: &str| login_profile(app, sc).join(".cb_login_stamp").exists();
    let proj = scope_clean(project);
    let scope = if proj != "all" && has(&proj) { proj } else { "all".to_string() };
    if app.login_br.lock().unwrap().is_some() && *LOGIN_SCOPE.lock().unwrap() == scope {
        return;
    }
    let master = login_profile(app, &scope);
    let Ok(stamp) = std::fs::read_to_string(master.join(".cb_login_stamp")) else { return };
    let want = format!("{scope}:{}", stamp.trim());
    let mark = agent_profile.join(".cb_login_synced");
    if std::fs::read_to_string(&mark).map(|t| t.trim() == want).unwrap_or(false) {
        return;
    }
    let _ = std::fs::remove_dir_all(agent_profile);
    copy_profile(&master, agent_profile, 0);
    let _ = std::fs::write(&mark, want);
}

// ── 한국어 팩: 가상 화면의 앱(계산기·메모장·브라우저)이 한글로 보이도록 ──
const KOPACK_SH: &str = r#"set -e
SUDO=""; [ "$(id -u)" != "0" ] && SUDO="sudo"
export DEBIAN_FRONTEND=noninteractive
echo "[1/4] 번역 파일 설치 차단 해제"
for f in /etc/dpkg/dpkg.cfg.d/*; do [ -f "$f" ] && $SUDO sed -i '/path-exclude=\/usr\/share\/locale/d' "$f"; done
echo "[2/4] 패키지 목록 갱신"
$SUDO apt-get update -y || true
echo "[3/4] 한글 글꼴·언어팩·앱 설치"
$SUDO apt-get install -y --no-install-recommends locales fonts-nanum fonts-noto-cjk language-pack-ko language-pack-gnome-ko gnome-calculator mousepad dbus-x11 gnome-calendar pcmanfm ristretto evince xfce4-terminal adwaita-icon-theme humanity-icon-theme librsvg2-common tumbler gnome-weather xfce4-settings x11vnc novnc
$SUDO apt-get install -y --no-install-recommends --reinstall mousepad pcmanfm libfm-data libfm-gtk-data ristretto xfce4-terminal language-pack-ko-base language-pack-gnome-ko-base || true
echo "[4/4] 한국어 로캘 만들기"
$SUDO locale-gen ko_KR.UTF-8 || true
fc-cache -f >/dev/null 2>&1 || true
echo "완료"
"#;

fn ko_locale_ok() -> bool {
    Command::new("locale").arg("-a").output().map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("ko_kr")).unwrap_or(false)
}

fn ko_env(cmd: &mut Command) {
    if cfg!(unix) && ko_locale_ok() {
        cmd.env("LANG", "ko_KR.UTF-8").env("LANGUAGE", "ko").env_remove("LC_ALL");
    }
}

fn kopack_status(app: &App) -> Value {
    let font = Command::new("fc-list").arg(":lang=ko").output().map(|o| !o.stdout.is_empty()).unwrap_or(false);
    let log = std::fs::read_to_string(app.data.join("kopack.log")).unwrap_or_default();
    let tail: Vec<&str> = log.lines().rev().take(12).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    json!({
        "supported": cfg!(target_os = "linux") && which(&["apt-get"]).is_some(),
        "font": font, "locale": ko_locale_ok(),
        "calc": which(&["gnome-calculator"]).is_some(), "memo": which(&["mousepad"]).is_some(),
        "cal": which(&["gnome-calendar"]).is_some(), "files": which(&["pcmanfm"]).is_some(), "img": which(&["ristretto"]).is_some(), "pdf": which(&["evince"]).is_some(), "termapp": which(&["xfce4-terminal"]).is_some(), "weather": which(&["gnome-weather"]).is_some(), "vncapp": which(&["x11vnc"]).is_some() && Path::new(NOVNC_DIR).join("vnc.html").exists(), "setapp": which(&["xfce4-settings-manager"]).is_some(),
        "trans": Path::new("/usr/share/locale/ko/LC_MESSAGES/mousepad.mo").exists(),
        "running": app.data.join("kopack.run").exists(), "log": tail.join("\n"),
    })
}

fn kopack_install(app: &Arc<App>) -> Result<(), String> {
    if !(cfg!(target_os = "linux") && which(&["apt-get"]).is_some()) {
        return Err("apt 를 쓰는 Linux(Ubuntu·Codespaces)에서만 설치할 수 있습니다.".into());
    }
    let run = app.data.join("kopack.run");
    if run.exists() {
        return Err("이미 설치 중입니다.".into());
    }
    let _ = std::fs::write(&run, "1");
    let sh = app.data.join("kopack.sh");
    std::fs::write(&sh, KOPACK_SH).map_err(|e| e.to_string())?;
    let logp = app.data.join("kopack.log");
    std::thread::spawn(move || {
        let ok = std::fs::File::create(&logp).and_then(|f| {
            let f2 = f.try_clone()?;
            Command::new("bash").arg(&sh).stdin(Stdio::null()).stdout(f).stderr(f2).status()
        });
        if !ok.map(|s| s.success()).unwrap_or(false) {
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&logp) {
                use std::io::Write;
                let _ = writeln!(f, "실패: 위 내용을 확인해 주세요 (sudo 권한·인터넷 연결).");
            }
        }
        let _ = std::fs::remove_file(&run);
    });
    Ok(())
}

// ── VNC 화면: x11vnc 로 가상 화면을 내보내고, noVNC 화면과 WebSocket 중계를 별도 포트(실행기 포트 + 100)에서 냅니다 ──
static VNC_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for i in 0..5 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        o.push(T[(n >> 18) as usize & 63] as char);
        o.push(T[(n >> 12) as usize & 63] as char);
        o.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        o.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    o
}

const NOVNC_DIR: &str = "/usr/share/novnc";

fn vnc_ok() -> bool {
    cfg!(unix) && VNC_PORT.load(std::sync::atomic::Ordering::Relaxed) > 0 && which(&["x11vnc"]).is_some() && Path::new(NOVNC_DIR).join("vnc.html").exists()
}

fn vnc_tcp(display: u32) -> Option<std::net::TcpStream> {
    let port = 5900 + display as u16;
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().ok()?;
    let try_conn = || std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok();
    if let Some(c) = try_conn() {
        return Some(c);
    }
    let x = which(&["x11vnc"])?;
    let _ = Command::new(x)
        .args(["-display", &format!(":{display}"), "-rfbport", &port.to_string(), "-localhost", "-nopw", "-forever", "-shared", "-noxdamage", "-quiet"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(150));
        if let Some(c) = try_conn() {
            return Some(c);
        }
    }
    None
}

fn http_simple(mut s: &std::net::TcpStream, code: u16, ctype: &str, body: &[u8]) {
    let head = format!("HTTP/1.1 {code} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len());
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
}

fn ws_frame(op: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0x80 | op];
    let n = payload.len();
    if n < 126 {
        f.push(n as u8);
    } else if n < 65536 {
        f.push(126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        f.push(127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    f.extend_from_slice(payload);
    f
}

fn vnc_conn(app: Arc<App>, mut s: std::net::TcpStream) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
        if buf.len() > 16384 {
            return;
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut it = first.split(' ');
    let (method, url) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
    let path = url.split('?').next().unwrap_or("");
    let q = query(url);
    let hdr = |name: &str| -> String {
        text.split("\r\n").skip(1).find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string())).unwrap_or_default()
    };
    if method != "GET" {
        return http_simple(&s, 405, "text/plain", b"no");
    }
    // 화면 파일(noVNC)은 비밀이 없어 그대로 내줍니다.
    if let Some(rel) = path.strip_prefix("/novnc/") {
        if rel.contains("..") || rel.contains('\\') {
            return http_simple(&s, 400, "text/plain", b"bad");
        }
        let f = Path::new(NOVNC_DIR).join(if rel.is_empty() { "vnc.html" } else { rel });
        let ct = match f.extension().and_then(|e| e.to_str()).unwrap_or("") {
            "html" => "text/html; charset=utf-8",
            "js" | "mjs" => "text/javascript",
            "css" => "text/css",
            "svg" => "image/svg+xml",
            "png" => "image/png",
            "ico" => "image/x-icon",
            "json" => "application/json",
            "woff" | "woff2" => "font/woff2",
            "wav" | "mp3" | "oga" => "audio/mpeg",
            _ => "application/octet-stream",
        };
        return match std::fs::read(&f) {
            Ok(b) => http_simple(&s, 200, ct, &b),
            Err(_) => http_simple(&s, 404, "text/plain", b"not found"),
        };
    }
    // WebSocket: /ws/<에이전트 id>?t=<연결 코드>
    let Some(id) = path.strip_prefix("/ws/") else { return http_simple(&s, 404, "text/plain", b"not found") };
    let given = q.get("t").cloned().unwrap_or_default();
    if given.replace('-', "").to_uppercase() != app.token.replace('-', "") {
        return http_simple(&s, 401, "text/plain", b"code");
    }
    let key = hdr("Sec-WebSocket-Key");
    if key.is_empty() {
        return http_simple(&s, 400, "text/plain", b"ws");
    }
    let display = if id == "login" { Some(LOGIN_DISPLAY) } else { app.inner.lock().unwrap().s.agents.iter().find(|a| a.id == id).map(|a| a.display) };
    let Some(display) = display.filter(|d| *d > 0) else { return http_simple(&s, 404, "text/plain", b"no screen") };
    let Some(tcp) = vnc_tcp(display) else { return http_simple(&s, 503, "text/plain", b"vnc") };
    let accept = b64(&sha1(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()));
    let proto = if hdr("Sec-WebSocket-Protocol").contains("binary") { "Sec-WebSocket-Protocol: binary\r\n" } else { "" };
    let resp = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n{proto}\r\n");
    if s.write_all(resp.as_bytes()).is_err() {
        return;
    }
    let _ = s.set_read_timeout(None);
    let _ = tcp.set_nodelay(true);
    let _ = s.set_nodelay(true);
    let (Ok(mut tcp_r), Ok(mut tcp_w), Ok(ws_w)) = (tcp.try_clone(), tcp.try_clone(), s.try_clone()) else { return };
    let ws_w = Arc::new(Mutex::new(ws_w));
    let ws_w2 = ws_w.clone();
    // VNC → 브라우저
    let t = std::thread::spawn(move || {
        let mut b = vec![0u8; 65536];
        loop {
            match tcp_r.read(&mut b) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_w2.lock().unwrap().write_all(&ws_frame(2, &b[..n])).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = ws_w2.lock().unwrap().shutdown(std::net::Shutdown::Both);
    });
    // 브라우저 → VNC
    let mut r = s;
    let mut pending: Vec<u8> = Vec::new();
    'outer: loop {
        let mut h2 = [0u8; 2];
        if r.read_exact(&mut h2).is_err() {
            break;
        }
        let op = h2[0] & 0x0f;
        let masked = h2[1] & 0x80 != 0;
        let mut len = (h2[1] & 0x7f) as u64;
        if len == 126 {
            let mut e = [0u8; 2];
            if r.read_exact(&mut e).is_err() { break; }
            len = u16::from_be_bytes(e) as u64;
        } else if len == 127 {
            let mut e = [0u8; 8];
            if r.read_exact(&mut e).is_err() { break; }
            len = u64::from_be_bytes(e);
        }
        if len > 16 * 1024 * 1024 {
            break;
        }
        let mut mask = [0u8; 4];
        if masked && r.read_exact(&mut mask).is_err() {
            break;
        }
        pending.clear();
        pending.resize(len as usize, 0);
        if r.read_exact(&mut pending).is_err() {
            break;
        }
        if masked {
            for (i, b) in pending.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        match op {
            8 => break 'outer,
            9 => {
                let _ = ws_w.lock().unwrap().write_all(&ws_frame(10, &pending));
            }
            0 | 1 | 2 => {
                if tcp_w.write_all(&pending).is_err() {
                    break;
                }
            }
            _ => {}
        }
    }
    let _ = tcp_w.shutdown(std::net::Shutdown::Both);
    let _ = ws_w.lock().unwrap().shutdown(std::net::Shutdown::Both);
    let _ = t.join();
}

fn start_vnc_gateway(app: Arc<App>, listen: &str) {
    if !cfg!(unix) || which(&["x11vnc"]).is_none() || !Path::new(NOVNC_DIR).join("vnc.html").exists() {
        return;
    }
    let Some((host, port)) = listen.rsplit_once(':') else { return };
    let Ok(p) = port.parse::<u16>() else { return };
    let vp = p.saturating_add(100);
    let Ok(l) = std::net::TcpListener::bind(format!("{host}:{vp}")) else { return };
    VNC_PORT.store(vp, std::sync::atomic::Ordering::Relaxed);
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let app = app.clone();
            std::thread::spawn(move || vnc_conn(app, c));
        }
    });
}

// 가상 화면에 배경화면과 아래 작업 표시줄(시계·열린 창 목록)을 꾸밉니다. 설치된 것만 씁니다(openbox, feh, tint2).
const TINT_RC: &str = r#"rounded = 0
border_width = 0
background_color = #2b2420 92
border_color = #e8793f 100
background_color_hover = #2b2420 92
background_color_pressed = #2b2420 92

rounded = 8
border_width = 0
background_color = #ffffff 10
border_color = #ffffff 0
background_color_hover = #ffffff 22
background_color_pressed = #ffffff 22

rounded = 8
border_width = 0
background_color = #e8793f 95
border_color = #e8793f 100
background_color_hover = #f19a68 100
background_color_pressed = #f19a68 100

panel_items = :T:C
panel_size = 100% 46
panel_position = bottom center horizontal
panel_padding = 8 5 10
panel_background_id = 1
panel_layer = top
task_text = 1
task_maximum_size = 190 36
task_padding = 12 4 12
task_background_id = 2
task_active_background_id = 3
task_font = Sans 10
task_font_color = #efe9de 100
task_active_font_color = #ffffff 100
task_iconified_font_color = #b3ada2 100
taskbar_padding = 0 0 8
time1_format = %H:%M
time2_format = %Y-%m-%d
time1_font = Sans Bold 10
time2_font = Sans 8
clock_font_color = #efe9de 100
clock_padding = 12 4
"#;

const OB_THEMERC: &str = r#"border.width: 1
padding.width: 8
padding.height: 6
window.handle.width: 0
window.client.padding.width: 0
*.justify: left
window.active.border.color: #e8793f
window.inactive.border.color: #4b443e
window.active.title.separator.color: #e8793f
window.inactive.title.separator.color: #4b443e
window.active.title.bg: flat solid
window.active.title.bg.color: #e8793f
window.inactive.title.bg: flat solid
window.inactive.title.bg.color: #3b3631
window.active.label.bg: parentrelative
window.inactive.label.bg: parentrelative
window.active.label.text.color: #ffffff
window.inactive.label.text.color: #b3ada2
window.active.button.unpressed.bg: parentrelative
window.inactive.button.unpressed.bg: parentrelative
window.active.button.unpressed.image.color: #ffffff
window.inactive.button.unpressed.image.color: #b3ada2
window.active.button.hover.bg: flat solid
window.active.button.hover.bg.color: #f19a68
window.active.button.hover.image.color: #ffffff
window.inactive.button.hover.bg: flat solid
window.inactive.button.hover.bg.color: #4b443e
window.inactive.button.hover.image.color: #ffffff
window.active.button.pressed.bg: flat solid
window.active.button.pressed.bg.color: #b9531f
window.active.button.pressed.image.color: #ffffff
window.inactive.button.pressed.bg: flat solid
window.inactive.button.pressed.bg.color: #b9531f
window.inactive.button.pressed.image.color: #ffffff
window.active.button.disabled.bg: parentrelative
window.inactive.button.disabled.bg: parentrelative
window.active.button.disabled.image.color: #f3b48f
window.inactive.button.disabled.image.color: #6d665e
window.active.button.toggled.unpressed.bg: parentrelative
window.active.button.toggled.unpressed.image.color: #ffffff
window.inactive.button.toggled.unpressed.bg: parentrelative
window.inactive.button.toggled.unpressed.image.color: #b3ada2
window.active.button.toggled.hover.bg: flat solid
window.active.button.toggled.hover.bg.color: #f19a68
window.active.button.toggled.hover.image.color: #ffffff
window.active.button.toggled.pressed.bg: flat solid
window.active.button.toggled.pressed.bg.color: #b9531f
window.active.button.toggled.pressed.image.color: #ffffff
window.inactive.button.toggled.hover.bg: flat solid
window.inactive.button.toggled.hover.bg.color: #4b443e
window.inactive.button.toggled.hover.image.color: #ffffff
window.inactive.button.toggled.pressed.bg: flat solid
window.inactive.button.toggled.pressed.bg.color: #b9531f
window.inactive.button.toggled.pressed.image.color: #ffffff
window.active.grip.bg: flat solid
window.active.grip.bg.color: #e8793f
window.inactive.grip.bg: flat solid
window.inactive.grip.bg.color: #4b443e
window.active.handle.bg: flat solid
window.active.handle.bg.color: #e8793f
window.inactive.handle.bg: flat solid
window.inactive.handle.bg.color: #4b443e
menu.border.color: #e8793f
menu.border.width: 1
menu.title.bg: flat solid
menu.title.bg.color: #e8793f
menu.title.text.color: #ffffff
menu.items.bg: flat solid
menu.items.bg.color: #2b2623
menu.items.text.color: #efe9de
menu.items.disabled.text.color: #7a736a
menu.items.active.bg: flat solid
menu.items.active.bg.color: #e8793f
menu.items.active.text.color: #ffffff
osd.border.width: 1
osd.border.color: #e8793f
osd.bg: flat solid
osd.bg.color: #2b2623
osd.label.bg: parentrelative
osd.label.text.color: #ffffff
osd.hilight.bg: flat solid
osd.hilight.bg.color: #e8793f
osd.unhilight.bg: flat solid
osd.unhilight.bg.color: #3b3631
"#;

const OB_XBM: [(&str, &str); 8] = [
    ("close", "0x00,0x66,0x3c,0x18,0x3c,0x66,0x00,0x00"),
    ("max", "0x00,0x7e,0x7e,0x42,0x42,0x42,0x7e,0x00"),
    ("max_toggled", "0x00,0x1c,0x74,0x7c,0x4c,0x70,0x00,0x00"),
    ("iconify", "0x00,0x00,0x00,0x00,0x00,0x7e,0x7e,0x00"),
    ("desk", "0x00,0x00,0x18,0x3c,0x3c,0x18,0x00,0x00"),
    ("desk_toggled", "0x00,0x00,0x18,0x3c,0x3c,0x18,0x00,0x00"),
    ("shade", "0x00,0x7e,0x7e,0x00,0x00,0x00,0x00,0x00"),
    ("shade_toggled", "0x00,0x00,0x00,0x00,0x00,0x7e,0x7e,0x00"),
];

// 주황색 윈도우 느낌의 테마(배경·작업 표시줄·창 틀)를 만듭니다. 설치된 도구만 쓰고, 없으면 건너뜁니다.
// 가상 화면 바탕화면에 놓을 기본 앱: (표시 이름, 실행 파일, 아이콘 후보). 위에서부터 차례로 놓입니다.
const DESK_APPS: [(&str, &str, &[&str]); 9] = [
    ("파일", "pcmanfm", &["/usr/share/icons/Humanity/apps/48/system-file-manager.svg"]),
    ("메모장", "mousepad", &["/usr/share/icons/hicolor/scalable/apps/org.xfce.mousepad.svg"]),
    ("계산기", "gnome-calculator", &["/usr/share/icons/hicolor/scalable/apps/org.gnome.Calculator.svg"]),
    ("달력", "gnome-calendar", &["/usr/share/icons/hicolor/scalable/apps/org.gnome.Calendar.svg"]),
    ("날씨", "gnome-weather", &["/usr/share/icons/hicolor/scalable/apps/org.gnome.Weather.svg"]),
    ("사진 보기", "ristretto", &["/usr/share/icons/hicolor/scalable/apps/org.xfce.ristretto.svg"]),
    ("문서 보기", "evince", &["/usr/share/icons/hicolor/scalable/apps/org.gnome.Evince.svg"]),
    ("터미널", "xfce4-terminal", &["/usr/share/icons/hicolor/scalable/apps/org.xfce.terminal.svg"]),
    ("설정", "xfce4-settings-manager", &["/usr/share/icons/hicolor/scalable/apps/org.xfce.settings.manager.svg"]),
];

fn dbus_path(disp: &str) -> String {
    format!("/tmp/cb-dbus-{}", disp.trim_start_matches(':'))
}

fn dbus_env(cmd: &mut Command, disp: &str) {
    let p = dbus_path(disp);
    if Path::new(&p).exists() {
        cmd.env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={p}"));
    }
}

fn decorate_display(data: &Path, disp: &str, browser_profile: Option<&Path>) {
    let dnum = disp.trim_start_matches(':').to_string();
    // 앱들이 서로 통신할 수 있게 화면마다 세션 버스를 켭니다 (달력 같은 앱에 필요).
    if which(&["dbus-daemon"]).is_some() {
        let p = dbus_path(disp);
        let _ = std::fs::remove_file(&p);
        let _ = Command::new("dbus-daemon").args(["--session", &format!("--address=unix:path={p}"), "--fork", "--nopidfile"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    let wall = data.join("wallpaper_orange.png");
    if !wall.exists() {
        if let Some(c) = which(&["convert", "magick"]) {
            let _ = Command::new(c)
                .args(["-size", "1280x720", "gradient:#ffa66a-#b8431f", "(", "-size", "1280x720", "xc:none", "-fill", "#ffffff22", "-draw", "circle 1050,120 1050,420", "-fill", "#ffffff14", "-draw", "circle 1180,560 1180,780", "-blur", "0x40", ")", "-compose", "over", "-composite"])
                .arg(&wall)
                .status();
        }
    }
    // 바탕화면 아이콘: 설치된 앱만 왼쪽 위부터 차례로 놓습니다. (작업 표시줄은 열린 창과 시계만 보여줍니다)
    let ob = data.join("ob");
    let cfg = ob.join(format!("cfg_{dnum}"));
    let desk = ob.join(format!("desktop_{dnum}"));
    let _ = std::fs::remove_dir_all(&desk);
    let _ = std::fs::create_dir_all(&desk);
    let _ = std::fs::create_dir_all(cfg.join("pcmanfm/default"));
    let _ = std::fs::create_dir_all(cfg.join("libfm"));
    let _ = std::fs::create_dir_all(cfg.join("gtk-3.0"));
    let pick = |c: &[&str]| c.iter().find(|p| Path::new(p).exists()).map(|p| p.to_string());
    let mut order = 0u64;
    let mut add = |name: &str, exec: String, icon: Option<String>| {
        order += 1;
        let f = desk.join(format!("{order:02}.desktop"));
        let body = format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\nIcon={}\nTerminal=false\n", icon.unwrap_or_else(|| "application-x-executable".into()));
        if std::fs::write(&f, body).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755));
            }
            if let Ok(h) = std::fs::OpenOptions::new().write(true).open(&f) {
                let _ = h.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + order * 10));
            }
        }
    };
    if let (Some(prof), Some(exe)) = (browser_profile, chromium_exe()) {
        let icon = pick(&["/opt/google/chrome/product_logo_48.png", "/usr/share/icons/Humanity/apps/48/internet-web-browser.svg"]);
        add("브라우저", format!("\"{}\" --no-sandbox --no-first-run --password-store=basic --use-mock-keychain --lang=ko \"--user-data-dir={}\"", exe.display(), prof.display()), icon);
    }
    for (name, bin, icons) in DESK_APPS.iter() {
        if which(&[bin]).is_some() {
            add(name, bin.to_string(), pick(icons));
        }
    }
    let _ = std::fs::write(cfg.join("user-dirs.dirs"), format!("XDG_DESKTOP_DIR=\"{}\"\n", desk.display()));
    let _ = std::fs::write(cfg.join("libfm/libfm.conf"), "[config]\nquick_exec=1\nsingle_click=1\n\n[ui]\nbig_icon_size=56\n");
    let theme = if Path::new("/usr/share/icons/Humanity").exists() { "Humanity" } else { "Adwaita" };
    let _ = std::fs::write(cfg.join("gtk-3.0/settings.ini"), format!("[Settings]\ngtk-icon-theme-name={theme}\ngtk-font-name=Sans 10\n"));
    let _ = std::fs::write(
        cfg.join("pcmanfm/default/desktop-items-0.conf"),
        format!("[*]\nwallpaper_mode=crop\nwallpaper_common=1\nwallpaper={}\ndesktop_bg=#c9683a\ndesktop_fg=#ffffff\ndesktop_shadow=#000000\ndesktop_font=Sans 10\nshow_wm_menu=0\nsort=mtime;ascending;\nshow_documents=0\nshow_trash=0\nshow_mounts=0\n", wall.display()),
    );
    let rc = data.join(format!("tint2rc_{dnum}"));
    let _ = std::fs::write(&rc, TINT_RC);
    // openbox 테마 + 설정
    let tdir = ob.join("themes/ClaudeOrange/openbox-3");
    let _ = std::fs::create_dir_all(&tdir);
    let _ = std::fs::write(tdir.join("themerc"), OB_THEMERC);
    for (n, bits) in OB_XBM.iter() {
        let _ = std::fs::write(tdir.join(format!("{n}.xbm")), format!("#define {n}_width 8\n#define {n}_height 8\nstatic unsigned char {n}_bits[] = {{\n {bits} }};\n"));
    }
    let mut ob_cfg: Option<PathBuf> = None;
    if let Ok(src) = std::fs::read_to_string("/etc/xdg/openbox/rc.xml") {
        if let Some(i) = src.find("<theme>") {
            if let Some(j) = src[i..].find("<name>").map(|x| x + i) {
                if let Some(k) = src[j..].find("</name>").map(|x| x + j) {
                    let mut out = format!("{}<name>ClaudeOrange</name>{}", &src[..j], &src[k + 7..]);
                    if let (Some(x), Some(y)) = (out.find("<titleLayout>"), out.find("</titleLayout>")) {
                        out = format!("{}<titleLayout>NLIMC</titleLayout>{}", &out[..x], &out[y + 14..]);
                    }
                    let f = ob.join("rc.xml");
                    if std::fs::write(&f, out).is_ok() {
                        ob_cfg = Some(f);
                    }
                }
            }
        }
    }
    let quiet = |c: &mut Command| { ko_env(c); dbus_env(c, disp); c.env("DISPLAY", disp).env("XDG_DATA_HOME", &ob).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() };
    if which(&["openbox"]).is_some() {
        let mut c = Command::new("openbox");
        if let Some(f) = &ob_cfg {
            c.arg("--config-file").arg(f);
        }
        let _ = quiet(&mut c);
        std::thread::sleep(Duration::from_millis(500));
    }
    if wall.exists() && which(&["feh"]).is_some() {
        let _ = Command::new("feh").env("DISPLAY", disp).arg("--bg-fill").arg(&wall).status();
    } else if which(&["xsetroot"]).is_some() {
        let _ = Command::new("xsetroot").env("DISPLAY", disp).args(["-solid", "#c9683a"]).status();
    }
    if which(&["pcmanfm"]).is_some() {
        let mut c = Command::new("pcmanfm");
        c.args(["--desktop", "--profile", "default"]).env("XDG_CONFIG_HOME", &cfg);
        let _ = quiet(&mut c);
        std::thread::sleep(Duration::from_millis(1200));
    }
    if which(&["tint2"]).is_some() {
        let _ = quiet(Command::new("tint2").arg("-c").arg(&rc));
    }
}

fn random_code() -> String {
    use std::hash::{BuildHasher, Hasher};
    let alphabet = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut out = String::new();
    for i in 0..12u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(now() ^ i);
        out.push(alphabet[(h.finish() % alphabet.len() as u64) as usize] as char);
        if i == 3 || i == 7 {
            out.push('-');
        }
    }
    out
}

fn kill_tree(pid: u32) {
    if cfg!(windows) {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).output();
    } else {
        let _ = Command::new("kill").args(["-9", "--", &format!("-{pid}")]).output();
    }
}

fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > n {
        format!("{}…", one.chars().take(n).collect::<String>())
    } else {
        one
    }
}

/// Pulls "@@TO name ... @@END" blocks out of an agent's reply.
fn split_handoffs(text: &str) -> (String, Vec<(String, String)>) {
    let mut clean = String::new();
    let mut out = Vec::new();
    let mut rest = text;
    let find_to = |r: &str| -> Option<usize> {
        let mut from = 0;
        while let Some(j) = r[from..].find("@@TO") {
            let i = from + j;
            // "@@TODO" 같은 다른 신호와 헷갈리지 않게, @@TO 바로 뒤는 공백이어야 합니다.
            if r[i + 4..].chars().next().map_or(true, |c| c.is_whitespace()) {
                return Some(i);
            }
            from = i + 4;
        }
        None
    };
    while let Some(i) = find_to(rest) {
        clean.push_str(&rest[..i]);
        let after = &rest[i + 4..];
        let nl = after.find('\n').unwrap_or(after.len());
        let name = after[..nl].trim().trim_end_matches([':', '：']).trim().to_string();
        let body_all = &after[nl..];
        let (body, next) = match body_all.find("@@END") {
            Some(e) => (&body_all[..e], &body_all[e + 5..]),
            None => (body_all, ""),
        };
        if !name.is_empty() && !body.trim().is_empty() {
            out.push((name, body.trim().to_string()));
        }
        rest = next;
    }
    clean.push_str(rest);
    (clean.trim().to_string(), out)
}

/// 웹 검색·웹 페이지 결과에서 출처 링크(제목, 주소)를 뽑습니다.
fn extract_sources(name: &str, input: &Value, text: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut push = |url: &str, title: &str, out: &mut Vec<Value>| {
        let u = url.trim().trim_end_matches(['.', ',', ')', ']', '>', '"', '\'', ';']);
        if !(u.starts_with("http://") || u.starts_with("https://")) || u.len() > 600 {
            return;
        }
        if out.iter().any(|o| o["url"] == u) {
            return;
        }
        out.push(json!({"url": u, "title": title.trim().chars().take(120).collect::<String>()}));
    };
    if name == "WebFetch" {
        if let Some(u) = input["url"].as_str() {
            push(u, "", &mut out);
        }
    }
    if let Some(i) = text.find("Links: [") {
        let mut de = serde_json::Deserializer::from_str(&text[i + 7..]).into_iter::<Value>();
        if let Some(Ok(Value::Array(arr))) = de.next() {
            for it in arr {
                if let Some(u) = it["url"].as_str() {
                    push(u, it["title"].as_str().unwrap_or(""), &mut out);
                }
            }
        }
    }
    if out.len() < 2 {
        for tok in text.split(|c: char| c.is_whitespace() || c == '"' || c == '(' || c == '<') {
            if tok.starts_with("http://") || tok.starts_with("https://") {
                push(tok, "", &mut out);
            }
        }
    }
    out.truncate(15);
    out
}

impl App {
    fn emit(&self, g: &mut Inner, ev: Value) {
        g.seq += 1;
        let seq = g.seq;
        g.events.push_back((seq, ev));
        while g.events.len() > 3000 {
            g.events.pop_front();
        }
        self.cv.notify_all();
    }

    fn save(&self, g: &Inner) {
        if let Ok(txt) = serde_json::to_string(&g.s) {
            let tmp = self.data.join("state.json.tmp");
            if std::fs::write(&tmp, txt).is_ok() {
                let _ = std::fs::rename(&tmp, self.data.join("state.json"));
            }
        }
    }

    fn add_msg(&self, g: &mut Inner, agent: &str, from: &str, kind: &str, text: &str) {
        self.add_msg_d(g, agent, from, kind, text, Value::Null);
    }

    fn add_msg_d(&self, g: &mut Inner, agent: &str, from: &str, kind: &str, text: &str, data: Value) -> u64 {
        g.s.next_id += 1;
        let id = g.s.next_id;
        let m = Msg { id, agent: agent.into(), from: from.into(), kind: kind.into(), text: text.into(), time: now(), data };
        let list = g.s.msgs.entry(agent.to_string()).or_default();
        list.push(m.clone());
        if list.len() > MAX_MSGS {
            let cut = list.len() - MAX_MSGS;
            // 화면에는 최근 것만 두지만, 잘려 나가는 대화는 버리지 않고 기록 파일에 보관합니다.
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.data.join("history.jsonl")) {
                for m in list.iter().take(cut) {
                    let _ = writeln!(f, "{}", serde_json::to_string(m).unwrap_or_default());
                }
            }
            list.drain(..cut);
        }
        self.emit(g, json!({"type": "msg", "m": m}));
        id
    }

    fn upd_msg<F: FnOnce(&mut Msg)>(&self, g: &mut Inner, agent: &str, pick: impl Fn(&Msg) -> bool, f: F) {
        let found = g.s.msgs.get_mut(agent).and_then(|l| l.iter_mut().rev().find(|m| pick(m))).map(|m| {
            f(m);
            m.clone()
        });
        if let Some(m) = found {
            self.emit(g, json!({"type": "upd", "m": m}));
        }
    }

    /// Streams buffered text into a live message (creating it on first use).
    fn flush_live(&self, agent: &str, kind: &str, live: &mut Option<u64>, buf: &mut String) {
        if buf.is_empty() {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        match *live {
            None => *live = Some(self.add_msg_d(&mut g, agent, "agent", kind, buf, json!({"live": true}))),
            Some(id) => {
                if let Some(m) = g.s.msgs.get_mut(agent).and_then(|l| l.iter_mut().rev().find(|m| m.id == id)) {
                    m.text.push_str(buf);
                }
                self.emit(&mut g, json!({"type": "delta", "agent": agent, "id": id, "add": buf.as_str()}));
            }
        }
        buf.clear();
    }

    fn emit_agent(&self, g: &mut Inner, id: &str) {
        if let Some(a) = g.s.agents.iter().find(|a| a.id == id).cloned() {
            self.emit(g, json!({"type": "agent", "a": a}));
        }
    }

    fn set_agent<F: FnOnce(&mut Agent)>(&self, id: &str, f: F) {
        let mut g = self.inner.lock().unwrap();
        if let Some(a) = g.s.agents.iter_mut().find(|a| a.id == id) {
            f(a);
        }
        self.emit_agent(&mut g, id);
    }

    fn new_agent(&self, g: &mut Inner, name: &str, role: &str, mode: String, browser: bool, model: &str, effort: &str, project: &str) -> Agent {
        g.s.next_id += 1;
        let id = format!("a{}", g.s.next_id);
        let safe: String = name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        let display = if self.screens { g.s.agents.iter().map(|a| a.display).max().unwrap_or(20).max(20) + 1 } else { 0 };
        let dir = match g.s.projects.iter().find(|p| p.id == project) {
            Some(p) => PathBuf::from(&p.dir).join(&safe),
            None => self.work.join(format!("{id}_{safe}")),
        };
        let a = Agent {
            id, name: name.to_string(), role: role.trim().to_string(), mode, browser,
            model: model.to_string(), effort: effort.to_string(), actual_model: String::new(),
            session: String::new(), status: "idle".into(), display, queue: 0, project: project.to_string(),
            instructions: String::new(), style: String::new(), allowed: String::new(), mcp: String::new(), skills: true, denied: String::new(), imported: false,
            dir: dir.to_string_lossy().to_string(), created: now(),
        };
        g.s.agents.push(a.clone());
        a
    }

    /// Each agent's folder gets a CLAUDE.md describing its role and teammates.
    fn write_briefs(&self, g: &Inner) {
        for a in &g.s.agents {
            if a.imported {
                continue;
            }
            let mut t = format!("# 당신은 '{}' 입니다\n\n## 역할\n{}\n\n", a.name, if a.role.trim().is_empty() { "범용 도우미" } else { a.role.trim() });
            if !a.instructions.trim().is_empty() {
                t.push_str(&format!("## 일하는 방식 (사용자가 정한 지침)\n{}\n\n", a.instructions.trim()));
            }
            match a.style.as_str() {
                "concise" => t.push_str("## 응답 스타일\n아주 간결하게 답하세요. 결론과 꼭 필요한 내용만 적습니다.\n\n"),
                "detailed" => t.push_str("## 응답 스타일\n근거와 과정을 충분히 자세히 설명하세요.\n\n"),
                _ => {}
            }
            t.push_str("## 팀\n당신은 여러 에이전트로 이루어진 팀의 한 명입니다. 사용자가 각 에이전트에게 채팅으로 일을 맡깁니다.\n\n");
            let proj = g.s.projects.iter().find(|p| p.id == a.project);
            if let Some(p) = proj {
                t.push_str(&format!("## 프로젝트\n이 팀은 프로젝트 '{}' 안에서만 일합니다. 프로젝트 폴더: {}\n- 공용 폴더: {}/shared (팀원끼리 주고받을 파일은 여기에 두세요)\n- 이 프로젝트 폴더 밖의 파일은 읽거나 바꾸지 마세요. 다른 프로젝트의 폴더는 더더욱 건드리지 마세요.\n\n", p.name, p.dir, p.dir));
                if !p.instructions.trim().is_empty() {
                    t.push_str(&format!("## 프로젝트 지침 (모든 팀원이 따를 것)\n{}\n\n", p.instructions.trim()));
                }
            }
            let mates: Vec<&Agent> = g.s.agents.iter().filter(|b| b.id != a.id && b.project == a.project).collect();
            if mates.is_empty() {
                t.push_str("지금은 다른 팀원이 없습니다.\n\n");
            } else {
                for b in &mates {
                    t.push_str(&format!("- {}: {}\n", b.name, clip(&b.role, 120)));
                }
                t.push_str("\n팀원 모두에게 한 번에 알리려면 이름 자리에 `전체` 를 쓰세요.\n");
                t.push_str("\n### 팀원에게 일 넘기기\n다른 팀원의 전문 분야이거나 나눠서 하는 편이 나을 때만, 답변 안에 아래 블록을 그대로 쓰세요. 블록은 팀원에게 전달되고 사용자 화면에는 전달 표시로 보입니다.\n\n```\n@@TO 팀원이름\n전달할 내용 (필요한 맥락을 모두 포함)\n@@END\n```\n\n팀원이 보낸 메시지에 답할 때도 같은 블록으로 회신합니다. 불필요하게 주고받지 마세요.\n\n");
            }
            t.push_str("## 작업 규칙\n- 이 폴더가 당신의 작업 공간입니다. 결과물은 이 폴더에 저장하세요.\n- 사용자에게는 한국어로 짧고 분명하게 보고하세요.\n- 되돌릴 수 없는 일(삭제, 결제, 외부로 메시지 전송)은 하기 전에 사용자에게 먼저 물어보세요.\n");
            t.push_str("\n## 출처\n웹 검색이나 웹 페이지에서 얻은 내용을 답에 쓸 때는, 답변 끝에 `출처` 목록을 `- [제목](주소)` 형태로 달아 주세요. 확인하지 못한 내용은 확인하지 못했다고 쓰세요.\n");
            t.push_str(assist::BRIEF);
            if a.display > 0 && self.screens {
                t.push_str("\n## 로그인이 필요할 때\n웹사이트가 로그인을 요구하면 비밀번호를 묻거나 대신 입력하려 하지 말고, 답변에 `@@LOGIN 사이트이름` 한 줄만 쓰고 멈추세요. 사용자가 앱에서 직접 로그인한 뒤 다시 알려 줍니다.\n");
                t.push_str(&format!("\n## 내 화면\n당신에게는 전용 가상 화면 DISPLAY=:{} (1280x720)이 있습니다. GUI 프로그램은 이 화면에서 실행됩니다. 화면을 보려면 `import -window root /tmp/screen_{}.png` 로 캡처한 뒤 그 파일을 읽으세요. xdotool이 설치되어 있으면 마우스와 키보드를 조작할 수 있습니다. 한국어 팩이 설치되어 있으면 다음 앱을 한글로 쓸 수 있습니다: 파일 `pcmanfm`, 메모장 `mousepad`, 계산기 `gnome-calculator`, 달력 `gnome-calendar`, 날씨 `gnome-weather`, 설정 `xfce4-settings-manager`, 사진 보기 `ristretto`, 문서(PDF) 보기 `evince`, 터미널 `xfce4-terminal`. 실행할 때는 뒤에 `&` 를 붙여 백그라운드로 여세요.\n", a.display, a.id));
            }
            let dir = PathBuf::from(&a.dir);
            let _ = std::fs::create_dir_all(&dir);
            if let Some(p) = proj { let _ = std::fs::create_dir_all(PathBuf::from(&p.dir).join("shared")); }
            let _ = std::fs::write(dir.join("CLAUDE.md"), t);
        }
    }

    fn enqueue(self: &Arc<Self>, g: &mut Inner, id: &str, job: Job) -> bool {
        if !g.senders.contains_key(id) {
            let (tx, rx) = mpsc::channel::<Job>();
            g.senders.insert(id.to_string(), tx);
            let app = self.clone();
            let aid = id.to_string();
            std::thread::spawn(move || {
                app.start_screen(&aid);
                for job in rx {
                    app.run_job(&aid, job);
                }
            });
        }
        if let Some(a) = g.s.agents.iter_mut().find(|a| a.id == id) {
            a.queue += 1;
        }
        let ok = g.senders.get(id).map(|tx| tx.send(job).is_ok()).unwrap_or(false);
        self.emit_agent(g, id);
        ok
    }

    fn start_screen(&self, id: &str) {
        if !self.screens {
            return;
        }
        let d = self.inner.lock().unwrap().s.agents.iter().find(|a| a.id == id).map(|a| a.display).unwrap_or(0);
        if d == 0 {
            return;
        }
        let disp = format!(":{d}");
        let _ = Command::new("Xvfb").args([&disp, "-screen", "0", "1280x720x24", "-nolisten", "tcp"])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        std::thread::sleep(Duration::from_millis(600));
        decorate_display(&self.data, &disp, Some(&browser_base(&self.data).join(format!("browser_{id}"))));
    }

    fn run_job(self: &Arc<Self>, id: &str, job: Job) {
        let agent = {
            let mut g = self.inner.lock().unwrap();
            g.stopped.remove(id);
            match g.s.agents.iter_mut().find(|a| a.id == id) {
                Some(a) => {
                    a.status = "working".into();
                    a.queue = a.queue.saturating_sub(1);
                    let c = a.clone();
                    self.emit_agent(&mut g, id);
                    c
                }
                None => return,
            }
        };
        let prompt = if job.from.is_empty() {
            job.text.clone()
        } else {
            format!("[팀원 '{}' 이(가) 보낸 메시지입니다. 회신이 필요하면 @@TO 블록을 쓰세요.]\n\n{}", job.from, job.text)
        };
        let prompt = format!("{}{}", assist::prompt_extra(self), prompt);
        assist::inflight_set(self, id, &job.text);
        let mut result = self.run_claude(&agent, &prompt, job.hops, true);
        if result == RunResult::ResumeFailed {
            self.set_agent(id, |a| a.session.clear());
            let mut fresh = agent.clone();
            fresh.session.clear();
            result = self.run_claude(&fresh, &prompt, job.hops, false);
        }
        assist::inflight_clear(self, id);
        let mut g = self.inner.lock().unwrap();
        let stopped = g.stopped.remove(id);
        if stopped {
            self.add_msg(&mut g, id, "system", "system", "작업을 중지했습니다.");
        }
        if let Some(a) = g.s.agents.iter_mut().find(|a| a.id == id) {
            a.status = if result == RunResult::Ok || stopped { "idle".into() } else { "error".into() };
        }
        self.emit_agent(&mut g, id);
        self.save(&g);
        drop(g);
        // 일이 끝난 직후 바로 백업합니다 (45초 주기를 기다리지 않고).
        if !self.backup.lock().unwrap().url.is_empty() {
            let me = self.clone();
            std::thread::spawn(move || {
                let _ = bk_push(&me);
            });
        }
    }

    fn run_claude(self: &Arc<Self>, agent: &Agent, prompt: &str, hops: u32, allow_retry: bool) -> RunResult {
        let id = agent.id.as_str();
        let fail = |text: &str| {
            let mut g = self.inner.lock().unwrap();
            self.add_msg(&mut g, id, "system", "error", text);
            RunResult::Failed
        };
        let Some(claude) = self.claude.clone() else {
            return fail("이 컴퓨터에서 Claude Code(claude 명령)를 찾지 못했습니다. 설치한 뒤 실행기를 다시 시작하세요.");
        };
        let _ = std::fs::create_dir_all(&agent.dir);
        let mut cmd = Command::new(&claude);
        cmd.current_dir(&agent.dir)
            .args(["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages"])
            .args(["--permission-mode", &agent.mode]);
        if !agent.model.is_empty() {
            cmd.args(["--model", &agent.model]);
        }
        if !agent.effort.is_empty() {
            cmd.args(["--effort", &agent.effort]);
        }
        if !agent.project.is_empty() {
            if let Some(p) = self.inner.lock().unwrap().s.projects.iter().find(|p| p.id == agent.project).cloned() {
                cmd.arg("--add-dir").arg(PathBuf::from(&p.dir).join("shared"));
            }
        }
        cmd.arg("--add-dir").arg(assist::md_dir(self));
        let mut allowed = String::from("WebSearch,WebFetch");
        if agent.browser {
            let profile = browser_base(&self.data).join(format!("browser_{id}"));
            sync_login_profile(self, &profile, &agent.project);
            let mut args = vec![json!("-y"), json!("@playwright/mcp@latest"), json!("--user-data-dir"), json!(profile.to_string_lossy()), json!("--browser"), json!("chromium"), json!("--viewport-size"), json!("1280,720")];
            if cfg!(unix) { args.push(json!("--no-sandbox")); }
            let command = if cfg!(windows) {
                let mut a = vec![json!("/c"), json!("npx")];
                a.append(&mut args);
                args = a;
                "cmd"
            } else {
                "npx"
            };
            let cfg = json!({"mcpServers": {"playwright": {"command": command, "args": args}}});
            let path = self.data.join(format!("mcp_{id}.json"));
            let _ = std::fs::write(&path, cfg.to_string());
            cmd.arg("--mcp-config").arg(&path);
            allowed.push_str(",mcp__playwright");
        }
        if agent.mode != "bypassPermissions" {
            if agent.skills {
                allowed.push_str(",Skill");
            }
            for n in agent.mcp.split(',').map(|t| t.trim()).filter(|t| !t.is_empty()) {
                allowed.push_str(&format!(",mcp__{}", mcp_norm(n)));
            }
            for t in agent.allowed.split(',').map(|t| t.trim()).filter(|t| !t.is_empty()) {
                allowed.push(',');
                allowed.push_str(t);
            }
            cmd.args(["--allowedTools", &allowed]);
        }
        {
            let mut dn = agent.denied.trim().to_string();
            if !dn.is_empty() {
                dn.push(',');
            }
            dn.push_str("AskUserQuestion");
            cmd.args(["--disallowedTools", &dn]);
        }
        if !agent.session.is_empty() {
            cmd.args(["--resume", &agent.session]);
        }
        if agent.display > 0 && self.screens {
            cmd.env("DISPLAY", format!(":{}", agent.display));
            ko_env(&mut cmd);
            dbus_env(&mut cmd, &format!(":{}", agent.display));
        }
        cmd.env_remove("CLAUDECODE").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return fail(&format!("Claude Code를 실행하지 못했습니다: {e}")),
        };
        self.inner.lock().unwrap().pids.insert(id.to_string(), child.id());
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(prompt.as_bytes());
        }
        let mut stderr = child.stderr.take().unwrap();
        let err_thread = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        let mut said_anything = false;
        let mut result_error = String::new();
        let (mut live_text, mut live_think): (Option<u64>, Option<u64>) = (None, None);
        let (mut buf_text, mut buf_think) = (String::new(), String::new());
        let mut last_text: Option<u64> = None;
        let mut turn_src: Vec<Value> = Vec::new();
        let mut last_flush = Instant::now();
        let reader = BufReader::new(child.stdout.take().unwrap());
        for line in reader.split(b'\n') {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
            if let Some(sid) = v["session_id"].as_str() {
                if !sid.is_empty() && sid != agent.session {
                    let mut g = self.inner.lock().unwrap();
                    if let Some(a) = g.s.agents.iter_mut().find(|a| a.id == id) {
                        a.session = sid.to_string();
                    }
                }
            }
            if v["parent_tool_use_id"].is_string() {
                continue; // inner chatter of a sub-agent
            }
            match v["type"].as_str().unwrap_or("") {
                "rate_limit_event" => {
                    // 구독 요금제 한도 알림(상태, 초기화 시각, 가능하면 사용률)을 마지막 값만 저장해 둡니다.
                    if v["rate_limit_info"].is_object() {
                        let mut prev: Value = std::fs::read_to_string(self.data.join("ratelimit.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!({}));
                        let mut windows = prev["windows"].as_object().cloned().unwrap_or_default();
                        let info = &v["rate_limit_info"];
                        if let Some(uw) = info["unifiedWindows"].as_object() {
                            for (k, w) in uw {
                                windows.insert(k.clone(), w.clone());
                            }
                        } else if let (Some(t), Some(u)) = (info["rateLimitType"].as_str(), info["utilization"].as_f64()) {
                            windows.insert(t.to_string(), json!({"utilization": u, "resetsAt": info["resetsAt"]}));
                        }
                        prev = json!({"info": info, "windows": windows, "at": now()});
                        let _ = std::fs::write(self.data.join("ratelimit.json"), prev.to_string());
                    }
                }
                "system" => {
                    if let Some(model) = v["model"].as_str() {
                        self.set_agent(id, |a| a.actual_model = model.to_string());
                    }
                }
                "stream_event" => {
                    let ev = &v["event"];
                    match ev["type"].as_str().unwrap_or("") {
                        "content_block_delta" => {
                            if let Some(t) = ev["delta"]["text"].as_str() {
                                buf_text.push_str(t);
                            }
                            if let Some(t) = ev["delta"]["thinking"].as_str() {
                                buf_think.push_str(t);
                            }
                            if last_flush.elapsed() > Duration::from_millis(80) {
                                self.flush_live(id, "thinking", &mut live_think, &mut buf_think);
                                self.flush_live(id, "text", &mut live_text, &mut buf_text);
                                last_flush = Instant::now();
                            }
                        }
                        "content_block_stop" => {
                            self.flush_live(id, "thinking", &mut live_think, &mut buf_think);
                            self.flush_live(id, "text", &mut live_text, &mut buf_text);
                        }
                        _ => {}
                    }
                }
                "assistant" => {
                    for block in v["message"]["content"].as_array().cloned().unwrap_or_default() {
                        said_anything = true;
                        match block["type"].as_str().unwrap_or("") {
                            "text" => {
                                buf_text.clear();
                                let (clean, handoffs) = split_handoffs(block["text"].as_str().unwrap_or(""));
                                let (clean, sigs) = assist::split_signals(&clean);
                                let mut g = self.inner.lock().unwrap();
                                match live_text.take() {
                                    Some(mid) => {
                                        self.upd_msg(&mut g, id, |m| m.id == mid, |m| {
                                            m.text = clean.clone();
                                            m.data = Value::Null;
                                        });
                                        last_text = Some(mid);
                                    }
                                    None if !clean.is_empty() => {
                                        last_text = Some(self.add_msg_d(&mut g, id, "agent", "text", &clean, Value::Null));
                                    }
                                    None => {}
                                }
                                for sg in sigs {
                                    assist::on_signal(self, &mut g, agent, sg);
                                }
                                for (to, body) in handoffs {
                                    self.handoff(&mut g, agent, &to, &body, hops);
                                }
                            }
                            "thinking" => {
                                buf_think.clear();
                                let text = block["thinking"].as_str().unwrap_or("").to_string();
                                let mut g = self.inner.lock().unwrap();
                                match live_think.take() {
                                    Some(mid) => self.upd_msg(&mut g, id, |m| m.id == mid, |m| {
                                        if !text.is_empty() {
                                            m.text = text.clone();
                                        }
                                        m.data = Value::Null;
                                    }),
                                    None if !text.trim().is_empty() => self.add_msg(&mut g, id, "agent", "thinking", &text),
                                    None => {}
                                }
                            }
                            "tool_use" => {
                                let name = block["name"].as_str().unwrap_or("tool");
                                let data = json!({"id": block["id"], "name": name, "input": shrink(&block["input"], 6000)});
                                let mut g = self.inner.lock().unwrap();
                                self.add_msg_d(&mut g, id, "agent", "tool", name, data);
                            }
                            _ => {}
                        }
                    }
                }
                "user" => {
                    for block in v["message"]["content"].as_array().cloned().unwrap_or_default() {
                        if block["type"] != "tool_result" {
                            continue;
                        }
                        let tid = block["tool_use_id"].clone();
                        let text = match &block["content"] {
                            Value::String(t) => t.clone(),
                            Value::Array(parts) => parts.iter().map(|p| p["text"].as_str().unwrap_or(if p["type"] == "image" { "[이미지]" } else { "" })).collect::<Vec<_>>().join("\n"),
                            _ => String::new(),
                        };
                        let is_err = block["is_error"].as_bool().unwrap_or(false);
                        let mut g = self.inner.lock().unwrap();
                        let mut found: Vec<Value> = Vec::new();
                        self.upd_msg(&mut g, id, |m| m.kind == "tool" && m.data["id"] == tid, |m| {
                            m.data["result"] = json!(trunc(&text, 6000));
                            m.data["is_error"] = json!(is_err);
                            let nm = m.data["name"].as_str().unwrap_or("").to_string();
                            if !is_err && (nm == "WebSearch" || nm == "WebFetch") {
                                found = extract_sources(&nm, &m.data["input"], &text);
                                if !found.is_empty() {
                                    m.data["sources"] = json!(found.clone());
                                }
                            }
                        });
                        for f in found {
                            if !turn_src.iter().any(|o| o["url"] == f["url"]) && turn_src.len() < 20 {
                                turn_src.push(f);
                            }
                        }
                    }
                }
                "result" => {
                    if v["is_error"].as_bool().unwrap_or(false) {
                        result_error = v["result"].as_str().unwrap_or("알 수 없는 오류").to_string();
                    } else {
                        let data = json!({"duration_ms": v["duration_ms"], "cost": v["total_cost_usd"], "turns": v["num_turns"]});
                        let mut g = self.inner.lock().unwrap();
                        self.add_msg_d(&mut g, id, "agent", "meta", "", data);
                    }
                }
                _ => {}
            }
        }
        self.flush_live(id, "thinking", &mut live_think, &mut buf_think);
        self.flush_live(id, "text", &mut live_text, &mut buf_text);
        {
            let mut g = self.inner.lock().unwrap();
            for mid in [live_text, live_think].into_iter().flatten() {
                self.upd_msg(&mut g, id, |m| m.id == mid, |m| m.data = Value::Null);
            }
        }
        if let (Some(mid), false) = (last_text, turn_src.is_empty()) {
            let mut g = self.inner.lock().unwrap();
            let src = turn_src.clone();
            self.upd_msg(&mut g, id, |m| m.id == mid, |m| m.data = json!({"sources": src}));
        }
        let status = child.wait();
        let err_text = err_thread.join().unwrap_or_default();
        let stopped = {
            let mut g = self.inner.lock().unwrap();
            g.pids.remove(id);
            g.stopped.contains(id)
        };
        if stopped {
            return RunResult::Ok;
        }
        let ok = status.map(|s| s.success()).unwrap_or(false) && result_error.is_empty();
        if ok {
            return RunResult::Ok;
        }
        if allow_retry && !agent.session.is_empty() && !said_anything {
            return RunResult::ResumeFailed;
        }
        let detail = if !result_error.is_empty() { result_error } else { err_text.trim().to_string() };
        let tail: String = detail.chars().rev().take(700).collect::<Vec<_>>().into_iter().rev().collect();
        fail(&format!("작업이 오류로 끝났습니다.\n{}", if tail.is_empty() { "(출력 없음) 터미널에서 claude 를 한 번 실행해 로그인되어 있는지 확인하세요.".to_string() } else { tail }))
    }

    fn handoff(self: &Arc<Self>, g: &mut Inner, from: &Agent, to: &str, body: &str, hops: u32) {
        let want = to.trim().to_lowercase();
        let everyone = ["전체", "모두", "all", "everyone"].contains(&want.as_str());
        let targets: Vec<Agent> = g.s.agents.iter()
            .filter(|a| a.id != from.id && a.project == from.project && (everyone || a.name.trim().to_lowercase() == want)).cloned().collect();
        if targets.is_empty() {
            return self.add_msg(g, &from.id, "system", "system", &format!("'{to}' 팀원을 찾지 못해 전달하지 못했습니다."));
        }
        if hops >= MAX_HOPS {
            return self.add_msg(g, &from.id, "system", "system", "팀원 사이에 주고받기가 너무 길어져 여기서 멈췄습니다. 이어가려면 직접 지시해 주세요.");
        }
        let label = if everyone { "팀 전체".to_string() } else { targets[0].name.clone() };
        self.add_msg(g, &from.id, &label, "handoff", body);
        for t in targets {
            self.add_msg(g, &t.id, &from.name, "peer", body);
            self.enqueue(g, &t.id, Job { text: body.to_string(), from: from.name.clone(), hops: hops + 1 });
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
}

fn read_json(p: &Path) -> Value {
    std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok()).unwrap_or(Value::Null)
}

fn str_list(v: &Value, cap: usize) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.chars().take(120).collect())).take(cap).collect()).unwrap_or_default()
}

/// Reads the signed-in user's own Claude Code settings. Secrets (env values, MCP args, tokens) are never sent.
// `claude mcp list` 로 내 Claude 에 실제로 연결된 MCP(계정 커넥터 포함)와 연결 상태를 읽습니다.
fn mcp_live(claude: &Path) -> Value {
    let mut child = match Command::new(claude).args(["mcp", "list"]).env_remove("CLAUDECODE").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("claude 를 실행하지 못했습니다: {e}"), "servers": []}),
    };
    let mut so = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut t = String::new();
        let _ = so.read_to_string(&mut t);
        let _ = tx.send(t);
    });
    let out = match rx.recv_timeout(Duration::from_secs(45)) {
        Ok(t) => t,
        Err(_) => {
            let _ = child.kill();
            return json!({"error": "45초 안에 끝나지 않았습니다 (연결 확인이 느린 서버가 있을 수 있습니다).", "servers": []});
        }
    };
    let _ = child.wait();
    let mut servers = Vec::new();
    for line in out.lines() {
        let Some((name, rest)) = line.split_once(": ") else { continue };
        if name.trim().is_empty() || name.contains(' ') && !name.contains('.') && name.len() > 60 {
            continue;
        }
        let status = if rest.contains("Connected") { "ok" } else if rest.to_lowercase().contains("auth") { "auth" } else if rest.contains("Fail") || rest.contains('✗') { "fail" } else { "unknown" };
        servers.push(json!({"name": name.trim(), "tool_prefix": format!("mcp__{}", mcp_norm(name.trim())), "status": status, "detail": rest.chars().take(100).collect::<String>()}));
    }
    json!({"servers": servers})
}

fn claude_settings(version: &str) -> Value {
    let Some(home) = home_dir() else { return json!({"found": false}) };
    let dir = home.join(".claude");
    let st = read_json(&dir.join("settings.json"));
    let top = read_json(&home.join(".claude.json"));
    let mcp: Vec<Value> = top["mcpServers"].as_object().map(|m| m.iter().take(40).map(|(k, v)| json!({"name": k, "type": v["type"].as_str().unwrap_or("stdio")})).collect()).unwrap_or_default();
    let plugins: Vec<String> = st["enabledPlugins"].as_object().map(|m| m.iter().filter(|(_, v)| v.as_bool().unwrap_or(false)).map(|(k, _)| k.clone()).take(40).collect()).unwrap_or_default();
    let keys = |v: &Value| -> Vec<String> { v.as_object().map(|m| m.keys().take(40).cloned().collect()).unwrap_or_default() };
    let mut skills = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir.join("skills")) {
        for e in rd.flatten().take(60) {
            let md = e.path().join("SKILL.md");
            if let Ok(t) = std::fs::read_to_string(&md) {
                let desc = t.lines().take(25).find(|l| l.trim_start().starts_with("description:")).map(|l| l.trim_start()["description:".len()..].trim().trim_matches('"').chars().take(140).collect::<String>()).unwrap_or_default();
                skills.push(json!({"name": e.file_name().to_string_lossy(), "description": desc}));
            }
        }
    }
    let claude_md: String = std::fs::read_to_string(dir.join("CLAUDE.md")).map(|t| t.chars().take(2000).collect()).unwrap_or_default();
    json!({
        "found": dir.exists(), "dir": dir.to_string_lossy(), "version": version.lines().next().unwrap_or("").chars().take(40).collect::<String>(),
        "model": st["model"].as_str().unwrap_or(""),
        "default_mode": st["permissions"]["defaultMode"].as_str().unwrap_or(""),
        "allow": str_list(&st["permissions"]["allow"], 40), "deny": str_list(&st["permissions"]["deny"], 40),
        "plugins": plugins, "env_keys": keys(&st["env"]), "hooks": keys(&st["hooks"]),
        "mcp": mcp, "skills": skills, "claude_md": claude_md,
        "has_settings": !st.is_null()
    })
}

/// Tool rules such as `Bash(git status),WebSearch`: keeps only characters that make up a rule.
fn clean_tools(v: &Value) -> String {
    v.as_str().unwrap_or("").chars().filter(|c| c.is_alphanumeric() || " _-(),:*./@\\".contains(*c)).take(600).collect::<String>().trim().to_string()
}

fn clean_text(v: &Value, max: usize) -> String {
    v.as_str().unwrap_or("").chars().filter(|c| *c != '\0').take(max).collect()
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}\n… (이하 생략)", s.chars().take(n).collect::<String>())
    } else {
        s.to_string()
    }
}

fn shrink(v: &Value, n: usize) -> Value {
    match v {
        Value::String(s) => Value::String(trunc(s, n)),
        Value::Array(a) => Value::Array(a.iter().take(60).map(|x| shrink(x, n)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), shrink(x, n))).collect()),
        other => other.clone(),
    }
}

fn clean_mode(v: &Value) -> String {
    match v.as_str().unwrap_or("") {
        "plan" => "plan",
        "bypassPermissions" | "full" => "bypassPermissions",
        _ => "acceptEdits",
    }
    .to_string()
}

fn clean_word(v: &Value, max: usize) -> String {
    v.as_str().unwrap_or("").trim().chars().filter(|c| c.is_ascii_alphanumeric() || "-_.[]:/".contains(*c)).take(max).collect()
}

#[derive(PartialEq)]
enum RunResult {
    Ok,
    Failed,
    ResumeFailed,
}

// ---------- HTTP ----------

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = hex {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query(url: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Some((_, q)) = url.split_once('?') {
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            m.insert(pct_decode(k), pct_decode(v));
        }
    }
    m
}

fn hdr(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn respond(req: Request, code: u16, ctype: &str, body: Vec<u8>) {
    let resp = Response::from_data(body)
        .with_status_code(code)
        .with_header(hdr("Content-Type", ctype))
        .with_header(hdr("Access-Control-Allow-Origin", "*"))
        .with_header(hdr("Access-Control-Allow-Headers", "authorization, content-type"))
        .with_header(hdr("Access-Control-Allow-Methods", "GET, POST, DELETE, OPTIONS"))
        .with_header(hdr("Cache-Control", "no-store"));
    let _ = req.respond(resp);
}

fn send_json(req: Request, code: u16, v: Value) {
    respond(req, code, "application/json; charset=utf-8", v.to_string().into_bytes());
}

fn list_files(root: &Path, dir: &Path, depth: u32, out: &mut Vec<Value>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if out.len() >= 300 {
            return;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "CLAUDE.md" || name == "node_modules" {
            continue;
        }
        let p = e.path();
        let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            out.push(json!({"path": rel, "dir": true}));
            if depth < 3 {
                list_files(root, &p, depth + 1, out);
            }
        } else {
            let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0);
            out.push(json!({"path": rel, "size": md.len(), "time": mtime}));
        }
    }
}

const PAGE: &str = include_str!("../../app/src/index.html");

fn handle(app: Arc<App>, mut req: Request) {
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let q = query(&url);
    let method = req.method().clone();
    if method == Method::Options {
        return respond(req, 204, "text/plain", vec![]);
    }
    // 실행기가 채팅 화면도 직접 내줍니다 (태블릿처럼 앱 exe가 없는 기기용). 화면 파일 자체에는 비밀이 없습니다.
    if method == Method::Get && (path == "/" || path == "/index.html") {
        return respond(req, 200, "text/html; charset=utf-8", PAGE.as_bytes().to_vec());
    }
    if method == Method::Get && path == "/app/conn" {
        return send_json(req, 200, json!({"self": true, "url": "", "token": "", "reachable": true, "remote": false, "note": ""}));
    }
    let bearer = req.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.as_str().trim_start_matches("Bearer ").trim().to_string());
    let given = bearer.or_else(|| q.get("t").cloned()).unwrap_or_default();
    if given.replace('-', "").to_uppercase() != app.token.replace('-', "") {
        return send_json(req, 401, json!({"error": "연결 코드가 맞지 않습니다."}));
    }
    let mut body = String::new();
    if method == Method::Post {
        let cap = if path == "/api/charimg" { 18_000_000 } else { 2_000_000 };
        let _ = req.as_reader().take(cap).read_to_string(&mut body);
    }
    let input: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();

    if method == Method::Get && parts.len() == 3 && parts[0] == "api" && parts[1] == "charimg" {
        return match assist::char_file(&app, parts[2]) {
            Some((ct, bytes)) => respond(req, 200, &ct, bytes),
            None => send_json(req, 404, json!({"error": "이미지를 찾지 못했습니다."})),
        };
    }
    if let Some((code, v)) = assist::route(&app, &method, parts.as_slice(), &input, &q) {
        return send_json(req, code, v);
    }
    match (&method, parts.as_slice()) {
        (Method::Get, ["api", "cc", "sessions"]) => {
            let mut linked: HashSet<String> = app.inner.lock().unwrap().s.agents.iter().map(|a| a.session.clone()).filter(|x| !x.is_empty()).collect();
            linked.extend(cc_auto_load(&app.data).known);
            send_json(req, 200, json!({"sessions": cc_sessions(&linked, 60), "auto": cc_auto_view(&app.data)}))
        }
        (Method::Post, ["api", "cc", "import"]) => {
            let sid = input["id"].as_str().unwrap_or("").to_string();
            match cc_import(&app, &sid, input["name"].as_str().unwrap_or("")) {
                Ok(a) => send_json(req, 200, json!({"agent": a})),
                Err(e) => send_json(req, 400, json!({"error": e})),
            }
        }
        (Method::Post, ["api", "cc", "update"]) => {
            let (imported, refreshed) = cc_update(&app);
            send_json(req, 200, json!({"imported": imported, "refreshed": refreshed, "auto": cc_auto_view(&app.data)}))
        }
        (Method::Get, ["api", "rc", id]) => send_json(req, 200, rc_view(id)),
        (Method::Post, ["api", "rc", id]) => {
            if input["action"] == "stop" {
                rc_stop(id);
                return send_json(req, 200, rc_view(id));
            }
            let agent = app.inner.lock().unwrap().s.agents.iter().find(|a| a.id == *id).cloned();
            let Some(agent) = agent else { return send_json(req, 404, json!({"error": "에이전트가 없습니다."})) };
            match rc_start(&app, &agent) {
                Ok(_) => send_json(req, 200, rc_view(id)),
                Err(e) => send_json(req, 400, json!({"error": e})),
            }
        }
        (Method::Get, ["api", "ratelimit"]) => {
            let v: Value = std::fs::read_to_string(app.data.join("ratelimit.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!({"info": null}));
            send_json(req, 200, v)
        }
        (Method::Get, ["api", "health"]) => send_json(req, 200, json!({
            "ok": true, "claude": app.claude.is_some(), "version": app.version, "app_version": APP_VERSION, "os": std::env::consts::OS,
            "screens": app.screens, "work": app.work.to_string_lossy(), "vnc": vnc_ok(), "vnc_port": VNC_PORT.load(std::sync::atomic::Ordering::Relaxed)
        })),
        (Method::Get, ["api", "mcp-live"]) => match &app.claude {
            Some(c) => {
                let c = c.clone();
                send_json(req, 200, mcp_live(&c))
            }
            None => send_json(req, 200, json!({"error": "Claude Code 를 찾지 못했습니다.", "servers": []})),
        },
        (Method::Get, ["api", "claude-settings"]) => send_json(req, 200, claude_settings(&app.version)),
        (Method::Get, ["api", "state"]) => {
            let g = app.inner.lock().unwrap();
            send_json(req, 200, json!({"agents": g.s.agents, "projects": g.s.projects, "msgs": g.s.msgs, "seq": g.seq}))
        }
        (Method::Get, ["api", "backup"]) => send_json(req, 200, bk_status(&app)),
        (Method::Post, ["api", "backup"]) => {
            let url_in = input["url"].as_str().unwrap_or("").trim().to_string();
            if url_in.is_empty() {
                let mut b = app.backup.lock().unwrap();
                b.url.clear();
                b.last_err.clear();
                b.last_hash = 0;
                bk_store(&app.data, &b);
                drop(b);
                return send_json(req, 200, bk_status(&app));
            }
            let url = match bk_norm_url(&url_in) {
                Some(u) => u,
                None => return send_json(req, 400, json!({"error": "주소는 https://이름.firebaseio.com 또는 https://이름.이지역.firebasedatabase.app 형태여야 합니다 (뒤에 경로 없이)."})),
            };
            let key_in = input["key"].as_str().unwrap_or("").trim().to_string();
            let key = if key_in.is_empty() {
                let cur = app.backup.lock().unwrap().key.clone();
                if cur.is_empty() { bk_new_key() } else { cur }
            } else if bk_key_ok(&key_in) {
                key_in
            } else {
                return send_json(req, 400, json!({"error": "백업 키는 영문·숫자 16~64자여야 합니다."}));
            };
            {
                let mut b = app.backup.lock().unwrap();
                b.url = url;
                b.key = key;
                if let Some(a) = input["auth"].as_str() {
                    if !a.trim().is_empty() { b.auth = a.trim().to_string(); }
                    if input["clear_auth"].as_bool() == Some(true) { b.auth.clear(); }
                }
                b.last_hash = 0;
                b.last_err.clear();
                bk_store(&app.data, &b);
            }
            // 이미 있던 대화도 곧바로 올립니다 (복원 직후라면 건너뛰도록 restore 쪽에서 처리).
            if input["push"].as_bool() != Some(false) {
                if let Err(e) = bk_push(&app) {
                    let mut st = bk_status(&app);
                    st["error"] = json!(e);
                    return send_json(req, 200, st);
                }
            }
            send_json(req, 200, bk_status(&app))
        }
        (Method::Post, ["api", "backup", "now"]) => match bk_push(&app) {
            Ok(n) => {
                let mut st = bk_status(&app);
                st["chunks"] = json!(n);
                send_json(req, 200, st)
            }
            Err(e) => send_json(req, 400, json!({"error": e})),
        },
        (Method::Post, ["api", "backup", "restore"]) => {
            {
                let g = app.inner.lock().unwrap();
                if g.s.agents.iter().any(|a| a.status == "working" || a.queue > 0) {
                    return send_json(req, 400, json!({"error": "일하는 중인 에이전트가 있습니다. 모두 멈춘 뒤에 불러오세요."}));
                }
            }
            let mut sv = match bk_pull(&app) {
                Ok(v) => v,
                Err(e) => return send_json(req, 400, json!({"error": e})),
            };
            bk_normalize(&mut sv);
            bk_rebase(&app.work, &mut sv);
            bk_pull_hist(&app);
            let (na, np) = (sv.agents.len(), sv.projects.len());
            let nm: usize = sv.msgs.values().map(|v| v.len()).sum();
            let _ = std::fs::copy(app.data.join("state.json"), app.data.join("state.json.before-restore"));
            {
                let mut g = app.inner.lock().unwrap();
                g.s = sv;
                app.save(&g);
                app.write_briefs(&g);
            }
            {
                let mut b = app.backup.lock().unwrap();
                b.last_hash = 0;
            }
            send_json(req, 200, json!({"ok": true, "agents": na, "projects": np, "messages": nm}))
        }
        (Method::Get, ["api", "git"]) => send_json(req, 200, git_status(&app)),
        (Method::Post, ["api", "git"]) => {
            {
                let mut c = app.git.lock().unwrap();
                if let Some(e) = input["enabled"].as_bool() {
                    c.enabled = e;
                }
                git_store(&app.data, &c);
            }
            let mut st = git_status(&app);
            if input["enabled"].as_bool() == Some(true) {
                if let Err(e) = git_sync(&app) {
                    st = git_status(&app);
                    st["error"] = json!(e);
                } else {
                    st = git_status(&app);
                }
            }
            send_json(req, 200, st)
        }
        (Method::Post, ["api", "git", "now"]) => match git_sync(&app) {
            Ok(_) => send_json(req, 200, git_status(&app)),
            Err(e) => send_json(req, 400, json!({"error": e})),
        },
        (Method::Post, ["api", "agents", id, "input"]) => {
            let d = if *id == "login" { Some(LOGIN_DISPLAY) } else { app.inner.lock().unwrap().s.agents.iter().find(|a| a.id == *id).map(|a| a.display) };
            let d = match d {
                Some(d) if d > 0 && app.screens => d,
                _ => return send_json(req, 404, json!({"error": "이 에이전트에는 가상 화면이 없습니다."})),
            };
            if which(&["xdotool"]).is_none() {
                return send_json(req, 400, json!({"error": "xdotool 이 설치되어 있어야 합니다 (sudo apt-get install -y xdotool)."}));
            }
            let disp = format!(":{d}");
            let run = |args: Vec<String>| Command::new("xdotool").env("DISPLAY", &disp).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
            let ok = match input["type"].as_str().unwrap_or("") {
                "click" => {
                    let x = input["x"].as_i64().unwrap_or(-1).clamp(0, 1279);
                    let y = input["y"].as_i64().unwrap_or(-1).clamp(0, 719);
                    let btn = if input["button"].as_str() == Some("right") { "3" } else { "1" };
                    let dbl = input["double"].as_bool() == Some(true);
                    let mut a = vec!["mousemove".to_string(), x.to_string(), y.to_string(), "click".into()];
                    if dbl {
                        a.push("--repeat".into());
                        a.push("2".into());
                    }
                    a.push(btn.into());
                    run(a)
                }
                "scroll" => {
                    let up = input["dy"].as_i64().unwrap_or(0) < 0;
                    run(vec!["click".into(), "--repeat".into(), "3".into(), if up { "4" } else { "5" }.into()])
                }
                "type" => {
                    let t: String = input["text"].as_str().unwrap_or("").chars().take(500).collect();
                    !t.is_empty() && run(vec!["type".into(), "--delay".into(), "12".into(), "--".into(), t])
                }
                "key" => {
                    let k = input["key"].as_str().unwrap_or("");
                    let allowed = ["Return", "Tab", "Escape", "BackSpace", "Delete", "Up", "Down", "Left", "Right", "Home", "End", "Page_Up", "Page_Down", "ctrl+a", "ctrl+c", "ctrl+v", "ctrl+l", "ctrl+t", "ctrl+w", "alt+Left", "F5"];
                    allowed.contains(&k) && run(vec!["key".into(), k.into()])
                }
                _ => false,
            };
            if ok { send_json(req, 200, json!({"ok": true})) } else { send_json(req, 400, json!({"error": "입력을 보내지 못했습니다."})) }
        }
        (Method::Get, ["api", "term"]) => {
            let t = app.tools.lock().unwrap().clone();
            send_json(req, 200, json!({"enabled": t.term_enabled, "cwd": app.term_cwd.lock().unwrap().to_string_lossy(), "os": std::env::consts::OS}))
        }
        (Method::Post, ["api", "term", "enable"]) => {
            {
                let mut t = app.tools.lock().unwrap();
                t.term_enabled = input["enabled"].as_bool().unwrap_or(false);
                jstore(&app.data, "tools.json", &*t);
            }
            send_json(req, 200, json!({"enabled": app.tools.lock().unwrap().term_enabled}))
        }
        (Method::Post, ["api", "term", "run"]) => {
            if !app.tools.lock().unwrap().term_enabled {
                return send_json(req, 403, json!({"error": "터미널이 꺼져 있습니다. 설정에서 켜세요."}));
            }
            let cmd = input["cmd"].as_str().unwrap_or("").trim().to_string();
            if cmd.is_empty() || cmd.len() > 4000 {
                return send_json(req, 400, json!({"error": "명령을 입력하세요."}));
            }
            let cwd = app.term_cwd.lock().unwrap().clone();
            if let Some(arg) = cmd.strip_prefix("cd ").or(if cmd == "cd" { Some("") } else { None }) {
                let arg = arg.trim().trim_matches('"');
                let target = if arg.is_empty() { hdir() } else if Path::new(arg).is_absolute() { PathBuf::from(arg) } else { cwd.join(arg) };
                return match target.canonicalize() {
                    Ok(t) if t.is_dir() => {
                        *app.term_cwd.lock().unwrap() = t.clone();
                        send_json(req, 200, json!({"code": 0, "out": "", "cwd": t.to_string_lossy()}))
                    }
                    _ => send_json(req, 200, json!({"code": 1, "out": "그런 폴더가 없습니다.", "cwd": cwd.to_string_lossy()})),
                };
            }
            let (code, out) = term_exec(&cwd, &cmd);
            send_json(req, 200, json!({"code": code, "out": out, "cwd": cwd.to_string_lossy()}))
        }
        (Method::Get, ["api", "repo"]) => send_json(req, 200, repo_status(&app)),
        (Method::Post, ["api", "repo"]) => {
            let url = input["url"].as_str().unwrap_or("").trim().to_string();
            let dir = input["dir"].as_str().unwrap_or("").trim().to_string();
            if !url.is_empty() && !repo_url_ok(&url) {
                return send_json(req, 400, json!({"error": "주소는 https://github.com/주인/이름 형태여야 합니다."}));
            }
            if !dir.is_empty() && !Path::new(&dir).is_absolute() {
                return send_json(req, 400, json!({"error": "폴더는 전체 경로(예: C:\\Users\\나\\ClaudeBot-저장소)로 적어 주세요."}));
            }
            {
                let mut t = app.tools.lock().unwrap();
                t.repo_url = url;
                t.repo_dir = dir;
                jstore(&app.data, "tools.json", &*t);
            }
            send_json(req, 200, repo_status(&app))
        }
        (Method::Post, ["api", "repo", "pull"]) => match repo_pull(&app) {
            Ok(m) => {
                let mut st = repo_status(&app);
                st["note"] = json!(m);
                send_json(req, 200, st)
            }
            Err(e) => send_json(req, 400, json!({"error": e})),
        },
        (Method::Post, ["api", "repo", "push"]) => match repo_push(&app) {
            Ok(m) => {
                let mut st = repo_status(&app);
                st["note"] = json!(m);
                send_json(req, 200, st)
            }
            Err(e) => send_json(req, 400, json!({"error": e})),
        },
        (Method::Get, ["api", "kopack"]) => send_json(req, 200, kopack_status(&app)),
        (Method::Post, ["api", "kopack", "install"]) => match kopack_install(&app) {
            Ok(()) => send_json(req, 200, kopack_status(&app)),
            Err(e) => send_json(req, 400, json!({"error": e})),
        },
        (Method::Get, ["api", "update"]) => send_json(req, 200, upd_status(&app)),
        (Method::Post, ["api", "update"]) => {
            let r = input["repo"].as_str().unwrap_or("").trim().trim_start_matches("https://github.com/").trim_end_matches('/').to_string();
            if !r.is_empty() && !upd_repo_ok(&r) {
                return send_json(req, 400, json!({"error": "저장소는 주인/이름 형태로 적어 주세요. 예: dani/claudebot"}));
            }
            {
                let mut t = app.tools.lock().unwrap();
                t.upd_repo = r.clone();
                jstore(&app.data, "tools.json", &*t);
            }
            *app.upd.lock().unwrap() = UpdInfo::default();
            if !r.is_empty() {
                let _ = upd_check(&app);
            }
            send_json(req, 200, upd_status(&app))
        }
        (Method::Post, ["api", "update", "check"]) => match upd_check(&app) {
            Ok(()) => send_json(req, 200, upd_status(&app)),
            Err(e) => send_json(req, 400, json!({"error": e})),
        },
        (Method::Post, ["api", "update", "apply"]) => match upd_apply(&app) {
            Ok(m) => send_json(req, 200, json!({"ok": true, "message": m})),
            Err(e) => send_json(req, 400, json!({"error": e})),
        },
        (Method::Get, ["api", "login-browser"]) => send_json(req, 200, login_status(&app, q.get("scope").map(|s| s.as_str()).unwrap_or("all"))),
        (Method::Post, ["api", "login-browser", "open"]) => match login_open(&app, input["url"].as_str().unwrap_or(""), input["scope"].as_str().unwrap_or("all")) {
            Ok(()) => send_json(req, 200, login_status(&app, input["scope"].as_str().unwrap_or("all"))),
            Err(e) => send_json(req, 400, json!({"error": e})),
        },
        (Method::Post, ["api", "login-browser", "close"]) => {
            login_stop(&app);
            send_json(req, 200, login_status(&app, input["scope"].as_str().unwrap_or("all")))
        }
        (Method::Get, ["api", "events"]) => {
            let since: u64 = q.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
            let mut g = app.inner.lock().unwrap();
            if g.seq <= since {
                g = app.cv.wait_timeout(g, Duration::from_secs(25)).unwrap().0;
            }
            let oldest = g.events.front().map(|e| e.0).unwrap_or(g.seq + 1);
            if since > g.seq || (since + 1 < oldest && g.seq > since) {
                let seq = g.seq;
                drop(g);
                return send_json(req, 200, json!({"resync": true, "seq": seq}));
            }
            let evs: Vec<&Value> = g.events.iter().filter(|e| e.0 > since).map(|e| &e.1).collect();
            let out = json!({"events": evs, "seq": g.seq});
            drop(g);
            send_json(req, 200, out)
        }
        (Method::Post, ["api", "projects"]) => {
            let name = input["name"].as_str().unwrap_or("").trim().to_string();
            if name.is_empty() || name.chars().count() > 40 || name.contains('\n') {
                return send_json(req, 400, json!({"error": "프로젝트 이름은 1~40자로 적어 주세요."}));
            }
            let mut g = app.inner.lock().unwrap();
            if g.s.projects.iter().any(|p| p.name.eq_ignore_ascii_case(&name)) {
                return send_json(req, 400, json!({"error": "같은 이름의 프로젝트가 이미 있습니다."}));
            }
            g.s.next_id += 1;
            let pid = format!("p{}", g.s.next_id);
            let safe: String = name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
            let dir = app.work.join("projects").join(format!("{pid}_{safe}"));
            let _ = std::fs::create_dir_all(dir.join("shared"));
            let proj = Project { id: pid.clone(), name, dir: dir.to_string_lossy().to_string(), created: now(), instructions: clean_text(&input["instructions"], 8000) };
            g.s.projects.push(proj.clone());
            let mut made = Vec::new();
            if let Some(team) = input["team"].as_array() {
                for t in team.iter().take(8) {
                    let n = t["name"].as_str().unwrap_or("").trim().to_string();
                    if n.is_empty() { continue; }
                    let a = app.new_agent(&mut g, &n, t["role"].as_str().unwrap_or(""), clean_mode(&t["mode"]), t["browser"].as_bool().unwrap_or(false), "", "", &pid);
                    made.push(a);
                }
            }
            app.write_briefs(&g);
            app.emit(&mut g, json!({"type": "project", "p": proj}));
            for a in &made { app.emit_agent(&mut g, &a.id); }
            app.save(&g);
            send_json(req, 200, json!({"project": proj, "agents": made}))
        }
        (Method::Post, ["api", "projects", id, "update"]) => {
            let mut g = app.inner.lock().unwrap();
            let dup = input["name"].as_str().map_or(false, |n| g.s.projects.iter().any(|p| p.id != *id && p.name.eq_ignore_ascii_case(n.trim())));
            let Some(p) = g.s.projects.iter_mut().find(|p| p.id == *id) else {
                return send_json(req, 404, json!({"error": "프로젝트가 없습니다."}));
            };
            if input["instructions"].is_string() { p.instructions = clean_text(&input["instructions"], 8000); }
            if let Some(n) = input["name"].as_str() {
                let n = n.trim().to_string();
                if n.is_empty() || n.chars().count() > 40 || n.contains('\n') || dup {
                    return send_json(req, 400, json!({"error": "쓸 수 없는 프로젝트 이름입니다 (비었거나, 너무 길거나, 이미 있음)."}));
                }
                p.name = n;
            }
            let out = p.clone();
            app.write_briefs(&g);
            app.emit(&mut g, json!({"type": "project", "p": out}));
            app.save(&g);
            send_json(req, 200, json!({"project": out}))
        }
        (Method::Get, ["api", "projects", id, "files"]) => {
            let dir = app.inner.lock().unwrap().s.projects.iter().find(|p| p.id == *id).map(|p| PathBuf::from(&p.dir).join("shared"));
            let Some(dir) = dir else { return send_json(req, 404, json!({"error": "프로젝트가 없습니다."})) };
            let mut out = Vec::new();
            list_files(&dir, &dir, 0, &mut out);
            send_json(req, 200, json!({"dir": dir.to_string_lossy(), "files": out}))
        }
        (Method::Post, ["api", "projects", id, "files"]) => {
            let dir = app.inner.lock().unwrap().s.projects.iter().find(|p| p.id == *id).map(|p| PathBuf::from(&p.dir).join("shared"));
            let Some(dir) = dir else { return send_json(req, 404, json!({"error": "프로젝트가 없습니다."})) };
            let name: String = input["name"].as_str().unwrap_or("").trim().chars().map(|c| if c == '/' || c == '\\' || c == ':' || c.is_control() { '_' } else { c }).collect();
            if name.is_empty() || name.starts_with('.') || name.chars().count() > 80 {
                return send_json(req, 400, json!({"error": "파일 이름을 1~80자로 적어 주세요 (점으로 시작할 수 없습니다)."}));
            }
            let _ = std::fs::create_dir_all(&dir);
            match std::fs::write(dir.join(&name), clean_text(&input["content"], 200_000)) {
                Ok(_) => send_json(req, 200, json!({"ok": true, "name": name})),
                Err(e) => send_json(req, 500, json!({"error": format!("저장하지 못했습니다: {e}")})),
            }
        }
        (Method::Delete, ["api", "projects", id]) if q.get("keep").map_or(false, |v| v == "1") => {
            let mut g = app.inner.lock().unwrap();
            let moved: Vec<String> = g.s.agents.iter().filter(|a| a.project == *id).map(|a| a.id.clone()).collect();
            if g.s.agents.iter().any(|a| a.project == *id && (g.pids.contains_key(&a.id) || a.queue > 0)) {
                return send_json(req, 400, json!({"error": "작업 중인 에이전트가 있습니다. 먼저 중지해 주세요."}));
            }
            for a in g.s.agents.iter_mut().filter(|a| a.project == *id) { a.project.clear(); a.session.clear(); }
            g.s.projects.retain(|p| p.id != *id);
            app.emit(&mut g, json!({"type": "project_removed", "id": id}));
            for a in &moved { app.emit_agent(&mut g, a); }
            app.write_briefs(&g);
            app.save(&g);
            send_json(req, 200, json!({"ok": true, "kept": moved.len()}))
        }
        (Method::Delete, ["api", "projects", id]) => {
            let mut pids = Vec::new();
            {
                let mut g = app.inner.lock().unwrap();
                let ids: Vec<String> = g.s.agents.iter().filter(|a| a.project == *id).map(|a| a.id.clone()).collect();
                for a in &ids {
                    if let Some(pid) = g.pids.get(a).copied() { g.stopped.insert(a.clone()); pids.push(pid); }
                    g.s.msgs.remove(a);
                    g.senders.remove(a);
                    app.emit(&mut g, json!({"type": "removed", "id": a}));
                }
                g.s.agents.retain(|a| a.project != *id);
                g.s.projects.retain(|p| p.id != *id);
                app.emit(&mut g, json!({"type": "project_removed", "id": id}));
                app.write_briefs(&g);
                app.save(&g);
            }
            for pid in pids { kill_tree(pid); }
            send_json(req, 200, json!({"ok": true, "note": "폴더와 파일은 지우지 않았습니다."}))
        }
        (Method::Post, ["api", "agents"]) => {
            let name = input["name"].as_str().unwrap_or("").trim().to_string();
            if name.is_empty() || name.chars().count() > 30 || name.contains('\n') {
                return send_json(req, 400, json!({"error": "이름은 1~30자로 적어 주세요."}));
            }
            let project = input["project"].as_str().unwrap_or("").to_string();
            let mut g = app.inner.lock().unwrap();
            if !project.is_empty() && !g.s.projects.iter().any(|p| p.id == project) {
                return send_json(req, 404, json!({"error": "프로젝트가 없습니다."}));
            }
            if g.s.agents.iter().any(|a| a.project == project && a.name.eq_ignore_ascii_case(&name)) {
                return send_json(req, 400, json!({"error": "같은 이름의 에이전트가 이미 있습니다."}));
            }
            let a = app.new_agent(&mut g, &name, input["role"].as_str().unwrap_or(""), clean_mode(&input["mode"]),
                input["browser"].as_bool().unwrap_or(false), &clean_word(&input["model"], 80), &clean_word(&input["effort"], 10), &project);
            app.write_briefs(&g);
            app.emit_agent(&mut g, &a.id);
            app.save(&g);
            send_json(req, 200, json!({"agent": a}))
        }
        (Method::Post, ["api", "team", "messages"]) => {
            let text = input["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return send_json(req, 400, json!({"error": "내용이 비어 있습니다."}));
            }
            let mut g = app.inner.lock().unwrap();
            let want: Vec<String> = input["to"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            let proj = input["project"].as_str().map(String::from);
            let ids: Vec<String> = g.s.agents.iter().filter(|a| proj.as_ref().map_or(true, |p| &a.project == p) && (want.is_empty() || want.contains(&a.id))).map(|a| a.id.clone()).collect();
            if ids.is_empty() {
                return send_json(req, 400, json!({"error": "받을 에이전트가 없습니다."}));
            }
            let batch = now();
            let to_label = if ids.len() == g.s.agents.iter().filter(|a| proj.as_ref().map_or(true, |p| &a.project == p)).count() && ids.len() > 1 { "팀 전체".to_string() } else {
                g.s.agents.iter().filter(|a| ids.contains(&a.id)).map(|a| a.name.clone()).collect::<Vec<_>>().join(", ")
            };
            for id in &ids {
                app.add_msg_d(&mut g, id, "user", "text", &text, json!({"batch": batch, "team": true, "to": to_label}));
                app.enqueue(&mut g, id, Job { text: format!("[팀 채팅에서 사용자가 보낸 메시지입니다. 받는 팀원 수: {}]\n\n{}", ids.len(), text), from: String::new(), hops: 0 });
            }
            app.save(&g);
            send_json(req, 200, json!({"ok": true, "sent": ids.len()}))
        }
        (Method::Post, ["api", "agents", id, "update"]) => {
            let mut g = app.inner.lock().unwrap();
            if let Some(n) = input["name"].as_str() {
                let n = n.trim();
                if n.is_empty() || n.chars().count() > 30 || g.s.agents.iter().any(|a| a.id != *id && a.name.eq_ignore_ascii_case(n)) {
                    return send_json(req, 400, json!({"error": "쓸 수 없는 이름입니다 (비었거나, 너무 길거나, 이미 있음)."}));
                }
            }
            let Some(a) = g.s.agents.iter_mut().find(|a| a.id == *id) else {
                return send_json(req, 404, json!({"error": "에이전트가 없습니다."}));
            };
            if let Some(n) = input["name"].as_str() { a.name = n.trim().to_string(); }
            if let Some(r) = input["role"].as_str() { a.role = r.trim().to_string(); }
            if input["mode"].is_string() { a.mode = clean_mode(&input["mode"]); }
            if input["model"].is_string() { a.model = clean_word(&input["model"], 80); a.actual_model.clear(); }
            if input["effort"].is_string() { a.effort = clean_word(&input["effort"], 10); }
            if let Some(b) = input["browser"].as_bool() { a.browser = b; }
            if input["instructions"].is_string() { a.instructions = clean_text(&input["instructions"], 6000); }
            if let Some(st) = input["style"].as_str() { a.style = if ["concise", "detailed"].contains(&st) { st.to_string() } else { String::new() }; }
            if input["allowed"].is_string() { a.allowed = clean_tools(&input["allowed"]); }
            if let Some(arr) = input["mcp"].as_array() {
                let names: Vec<String> = arr.iter().filter_map(|x| x.as_str()).map(|x| x.chars().filter(|c| !c.is_control() && *c != ',').take(60).collect::<String>().trim().to_string()).filter(|x| !x.is_empty()).take(30).collect();
                a.mcp = names.join(",");
            }
            if let Some(b) = input["skills"].as_bool() { a.skills = b; }
            if input["denied"].is_string() { a.denied = clean_tools(&input["denied"]); }
            let out = a.clone();
            app.write_briefs(&g);
            app.emit_agent(&mut g, id);
            app.save(&g);
            send_json(req, 200, json!({"agent": out}))
        }
        (Method::Post, ["api", "agents", id, "messages"]) => {
            let text = input["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return send_json(req, 400, json!({"error": "내용이 비어 있습니다."}));
            }
            let mut g = app.inner.lock().unwrap();
            if !g.s.agents.iter().any(|a| a.id == *id) {
                return send_json(req, 404, json!({"error": "에이전트가 없습니다."}));
            }
            app.add_msg(&mut g, id, "user", "text", &text);
            app.enqueue(&mut g, id, Job { text, from: String::new(), hops: 0 });
            app.save(&g);
            send_json(req, 200, json!({"ok": true}))
        }
        (Method::Post, ["api", "agents", id, "move"]) => {
            let target = input["project"].as_str().unwrap_or("").to_string();
            let mut g = app.inner.lock().unwrap();
            let Some(a) = g.s.agents.iter().find(|a| a.id == *id).cloned() else {
                return send_json(req, 404, json!({"error": "에이전트가 없습니다."}));
            };
            if g.pids.contains_key(*id) || a.queue > 0 {
                return send_json(req, 400, json!({"error": "작업 중에는 옮길 수 없습니다. 먼저 중지하세요."}));
            }
            if a.project == target {
                return send_json(req, 400, json!({"error": "이미 그 프로젝트에 있습니다."}));
            }
            let tproj = g.s.projects.iter().find(|p| p.id == target).cloned();
            if !target.is_empty() && tproj.is_none() {
                return send_json(req, 404, json!({"error": "프로젝트가 없습니다."}));
            }
            if g.s.agents.iter().any(|b| b.id != a.id && b.project == target && b.name.eq_ignore_ascii_case(&a.name)) {
                return send_json(req, 400, json!({"error": "그 프로젝트에 같은 이름의 에이전트가 있습니다. 이름을 먼저 바꿔 주세요."}));
            }
            let safe: String = a.name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
            let newdir = match &tproj {
                Some(p) => PathBuf::from(&p.dir).join(&safe),
                None => app.work.join(format!("{}_{}", a.id, safe)),
            };
            if newdir.exists() {
                return send_json(req, 400, json!({"error": "옮길 위치에 같은 이름의 폴더가 이미 있습니다."}));
            }
            if let Some(parent) = newdir.parent() { let _ = std::fs::create_dir_all(parent); }
            if Path::new(&a.dir).exists() {
                if let Err(e) = std::fs::rename(&a.dir, &newdir) {
                    return send_json(req, 500, json!({"error": format!("폴더를 옮기지 못했습니다: {e}")}));
                }
            }
            if let Some(x) = g.s.agents.iter_mut().find(|x| x.id == *id) {
                x.project = target.clone();
                x.dir = newdir.to_string_lossy().to_string();
                x.session.clear();
            }
            let label = tproj.map(|p| format!("프로젝트 '{}'", p.name)).unwrap_or_else(|| "프로젝트 없음".to_string());
            app.add_msg(&mut g, id, "system", "system", &format!("{label}(으)로 옮겼습니다. 작업 폴더도 함께 옮겼고, 이전 대화 맥락은 이어지지 않습니다 (기록은 남아 있습니다)."));
            app.write_briefs(&g);
            app.emit_agent(&mut g, id);
            app.save(&g);
            let out = g.s.agents.iter().find(|x| x.id == *id).cloned();
            send_json(req, 200, json!({"agent": out}))
        }
        (Method::Post, ["api", "agents", id, "stop"]) => {
            let pid = {
                let mut g = app.inner.lock().unwrap();
                let pid = g.pids.get(*id).copied();
                if pid.is_some() {
                    g.stopped.insert(id.to_string());
                }
                pid
            };
            if let Some(pid) = pid {
                kill_tree(pid);
            }
            send_json(req, 200, json!({"ok": true, "running": pid.is_some()}))
        }
        (Method::Post, ["api", "agents", id, "reset"]) => {
            let mut g = app.inner.lock().unwrap();
            if g.pids.contains_key(*id) {
                return send_json(req, 400, json!({"error": "작업 중에는 초기화할 수 없습니다. 먼저 중지하세요."}));
            }
            if let Some(a) = g.s.agents.iter_mut().find(|a| a.id == *id) {
                a.session.clear();
                a.status = "idle".into();
            }
            g.s.msgs.remove(*id);
            app.emit(&mut g, json!({"type": "reset", "id": id}));
            app.emit_agent(&mut g, id);
            app.save(&g);
            send_json(req, 200, json!({"ok": true}))
        }
        (Method::Delete, ["api", "agents", id]) => {
            let pid = {
                let mut g = app.inner.lock().unwrap();
                let pid = g.pids.get(*id).copied();
                if pid.is_some() {
                    g.stopped.insert(id.to_string());
                }
                rc_stop(id);
                g.s.agents.retain(|a| a.id != *id);
                g.s.msgs.remove(*id);
                g.senders.remove(*id);
                app.write_briefs(&g);
                app.emit(&mut g, json!({"type": "removed", "id": id}));
                app.save(&g);
                pid
            };
            if let Some(pid) = pid {
                kill_tree(pid);
            }
            send_json(req, 200, json!({"ok": true}))
        }
        (Method::Get, ["api", "agents", "login", "screen"]) => {
            if !app.screens {
                return send_json(req, 404, json!({"error": "no-screen"}));
            }
            let out = Command::new("import").args(["-display", &format!(":{LOGIN_DISPLAY}"), "-window", "root", "png:-"]).output();
            match out {
                Ok(o) if o.status.success() && !o.stdout.is_empty() => respond(req, 200, "image/png", o.stdout),
                _ => send_json(req, 503, json!({"error": "화면을 아직 캡처할 수 없습니다."})),
            }
        }
        (Method::Delete, ["api", "agents", id, "file"]) => {
            let agent = app.inner.lock().unwrap().s.agents.iter().find(|a| a.id == *id).cloned();
            let Some(agent) = agent else { return send_json(req, 404, json!({"error": "에이전트가 없습니다."})) };
            let root = PathBuf::from(&agent.dir);
            let rel = q.get("path").cloned().unwrap_or_default();
            match (root.join(&rel).canonicalize(), root.canonicalize()) {
                (Ok(f), Ok(r)) if f != r && f.starts_with(&r) && f.is_file() => match std::fs::remove_file(&f) {
                    Ok(_) => send_json(req, 200, json!({"ok": true})),
                    Err(e) => send_json(req, 500, json!({"error": format!("지우지 못했습니다: {e}")})),
                },
                _ => send_json(req, 404, json!({"error": "파일이 없습니다."})),
            }
        }
        (Method::Get, ["api", "agents", id, what]) => {
            let agent = app.inner.lock().unwrap().s.agents.iter().find(|a| a.id == *id).cloned();
            let Some(agent) = agent else { return send_json(req, 404, json!({"error": "에이전트가 없습니다."})) };
            let root = PathBuf::from(&agent.dir);
            match *what {
                "files" => {
                    let mut out = Vec::new();
                    list_files(&root, &root, 0, &mut out);
                    send_json(req, 200, json!({"dir": agent.dir, "files": out}))
                }
                "file" => {
                    let rel = q.get("path").cloned().unwrap_or_default();
                    let full = root.join(&rel);
                    match (full.canonicalize(), root.canonicalize()) {
                        (Ok(f), Ok(r)) if f.starts_with(&r) && f.is_file() => match std::fs::read(&f) {
                            Ok(data) => {
                                let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                                let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
                                let inline_ct = if q.get("inline").map_or(false, |v| v == "1") {
                                    match ext.as_str() {
                                        "html" | "htm" => Some("text/html; charset=utf-8"),
                                        "txt" | "md" | "json" | "js" | "css" | "py" | "rs" | "toml" | "yml" | "yaml" | "csv" | "log" | "sh" | "xml" | "ts" | "c" | "h" | "cpp" | "java" | "go" => Some("text/plain; charset=utf-8"),
                                        "png" => Some("image/png"),
                                        "jpg" | "jpeg" => Some("image/jpeg"),
                                        "gif" => Some("image/gif"),
                                        "webp" => Some("image/webp"),
                                        "svg" => Some("image/svg+xml"),
                                        "pdf" => Some("application/pdf"),
                                        _ => None,
                                    }
                                } else {
                                    None
                                };
                                let mut resp = Response::from_data(data).with_header(hdr("Access-Control-Allow-Origin", "*"));
                                if let Some(ct) = inline_ct {
                                    // 에이전트가 만든 파일이 앱의 로그인 정보에 접근하지 못하도록 격리(sandbox)해서 보여 줍니다.
                                    resp = resp
                                        .with_header(hdr("Content-Type", ct))
                                        .with_header(hdr("Content-Security-Policy", "sandbox allow-scripts allow-forms allow-popups"))
                                        .with_header(hdr("X-Content-Type-Options", "nosniff"))
                                        .with_header(hdr("Content-Disposition", "inline"));
                                } else {
                                    resp = resp
                                        .with_header(hdr("Content-Type", "application/octet-stream"))
                                        .with_header(hdr("Content-Disposition", &format!("attachment; filename*=UTF-8''{}", pct_encode(&name))));
                                }
                                let _ = req.respond(resp);
                            }
                            Err(_) => send_json(req, 404, json!({"error": "파일을 읽지 못했습니다."})),
                        },
                        _ => send_json(req, 404, json!({"error": "파일이 없습니다."})),
                    }
                }
                "screen" => {
                    if !app.screens || agent.display == 0 {
                        return send_json(req, 404, json!({"error": "no-screen"}));
                    }
                    let out = Command::new("import").args(["-display", &format!(":{}", agent.display), "-window", "root", "png:-"]).output();
                    match out {
                        Ok(o) if o.status.success() && !o.stdout.is_empty() => respond(req, 200, "image/png", o.stdout),
                        _ => send_json(req, 503, json!({"error": "화면을 아직 캡처할 수 없습니다."})),
                    }
                }
                _ => send_json(req, 404, json!({"error": "not found"})),
            }
        }
        _ => send_json(req, 404, json!({"error": "not found"})),
    }
}

// ---- Claude Code 대화 가져오기 ----
fn cc_projects_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok()?;
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

fn cc_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// 세션 파일(jsonl)을 읽어 (작업 폴더, [(역할, 글)]) 을 돌려줍니다. 도구 호출·생각·시스템 안내문은 건너뜁니다.
fn cc_parse(path: &Path, byte_cap: u64, want_msgs: usize, first_only: bool) -> (String, Vec<(String, String)>) {
    use std::io::BufRead;
    let mut cwd = String::new();
    let mut msgs: Vec<(String, String)> = Vec::new();
    let Ok(f) = std::fs::File::open(path) else { return (cwd, msgs) };
    for line in std::io::BufReader::new(f.take(byte_cap)).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if cwd.is_empty() {
            if let Some(c) = v["cwd"].as_str() {
                cwd = c.to_string();
            }
        }
        if v["isSidechain"] == true || v["isMeta"] == true {
            continue;
        }
        let role = match v["type"].as_str() {
            Some("user") => "user",
            Some("assistant") => "agent",
            _ => continue,
        };
        let text = cc_text(&v["message"]["content"]);
        let t = text.trim();
        if t.is_empty() || t.starts_with('<') || t.starts_with("Caveat:") || t.starts_with("[Request interrupted") {
            continue;
        }
        let t: String = t.chars().take(6000).collect();
        msgs.push((role.to_string(), t));
        if first_only && !cwd.is_empty() && msgs.iter().any(|m| m.0 == "user") {
            break;
        }
        if msgs.len() > want_msgs * 2 {
            let cut = msgs.len() - want_msgs;
            msgs.drain(..cut);
        }
    }
    if msgs.len() > want_msgs {
        let cut = msgs.len() - want_msgs;
        msgs.drain(..cut);
    }
    (cwd, msgs)
}

fn cc_find(id: &str) -> Option<PathBuf> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let root = cc_projects_dir()?;
    for d in std::fs::read_dir(root).ok()?.flatten() {
        let p = d.path().join(format!("{id}.jsonl"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn cc_sessions(linked: &std::collections::HashSet<String>, cap: usize) -> Vec<Value> {
    let mut out: Vec<(u64, Value)> = Vec::new();
    let Some(root) = cc_projects_dir() else { return vec![] };
    let Ok(rd) = std::fs::read_dir(root) else { return vec![] };
    for d in rd.flatten() {
        let Ok(files) = std::fs::read_dir(d.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().map_or(true, |e| e != "jsonl") {
                continue;
            }
            let id = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let mtime = f.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0);
            let (cwd, msgs) = cc_parse(&p, 262_144, 4, true);
            let Some(first) = msgs.iter().find(|m| m.0 == "user") else { continue };
            let title: String = first.1.replace('\n', " ").chars().take(80).collect();
            let exists = !cwd.is_empty() && Path::new(&cwd).is_dir();
            out.push((mtime, json!({"id": id, "cwd": cwd, "title": title, "mtime": mtime, "exists": exists, "linked": linked.contains(&id)})));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().take(cap).map(|x| x.1).collect()
}

fn cc_import(app: &App, sid: &str, name: &str) -> Result<Agent, String> {
    let path = cc_find(sid).ok_or("그 대화 기록을 찾지 못했습니다.")?;
    let (cwd, msgs) = cc_parse(&path, 40_000_000, 200, false);
    if cwd.is_empty() || !Path::new(&cwd).is_dir() {
        return Err(format!("이 대화를 하던 폴더({cwd})가 이 컴퓨터에 없어서 이어갈 수 없습니다."));
    }
    let mut g = app.inner.lock().unwrap();
    if g.s.agents.iter().any(|a| a.session == sid) {
        return Err("이미 가져온 대화입니다.".into());
    }
    let base: String = Some(name.trim().to_string()).filter(|n| !n.is_empty())
        .unwrap_or_else(|| msgs.iter().find(|m| m.0 == "user").map(|m| m.1.replace('\n', " ").chars().take(14).collect::<String>().trim().to_string()).unwrap_or_default());
    let base: String = if base.is_empty() { "가져온 대화".to_string() } else { base.chars().take(26).collect() };
    let mut name = base.clone();
    let mut n = 2;
    while g.s.agents.iter().any(|a| a.project.is_empty() && a.name.eq_ignore_ascii_case(&name)) {
        name = format!("{base} {n}");
        n += 1;
    }
    let a = app.new_agent(&mut g, &name, "Claude Code 에서 가져온 대화", "acceptEdits".to_string(), false, "", "", "");
    let id = a.id.clone();
    if let Some(x) = g.s.agents.iter_mut().find(|x| x.id == id) {
        x.dir = cwd.clone();
        x.session = sid.to_string();
        x.imported = true;
    }
    app.add_msg(&mut g, &id, "system", "system", &format!("Claude Code 에서 가져온 대화입니다. 작업 폴더는 {cwd} 이고, 최근 {}개 메시지만 가져왔습니다. 같은 대화를 터미널과 동시에 쓰지는 마세요.", msgs.len()));
    for (role, text) in &msgs {
        app.add_msg(&mut g, &id, role, "text", text);
    }
    let a = g.s.agents.iter().find(|x| x.id == id).cloned().unwrap();
    app.emit_agent(&mut g, &id);
    app.save(&g);
    drop(g);
    let mut c = cc_auto_load(&app.data);
    if !c.known.iter().any(|k| k == sid) {
        c.known.push(sid.to_string());
        cc_auto_store(&app.data, &c);
    }
    Ok(a)
}

// ---- Claude Code 대화 자동 가져오기 (주기) ----
#[derive(Serialize, Deserialize, Default, Clone)]
struct CcAuto {
    #[serde(default)]
    on: bool,
    #[serde(default)]
    minutes: u64,
    #[serde(default)]
    since: u64,
    #[serde(default)]
    last: u64,
    #[serde(default)]
    count: u64,
    #[serde(default)]
    known: Vec<String>, // 이미 가져왔거나 이 앱이 만든 대화 번호 (다시 가져오지 않음)
}

fn cc_auto_load(data: &Path) -> CcAuto {
    let mut c: CcAuto = std::fs::read_to_string(data.join("ccauto.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    if c.minutes == 0 {
        c.minutes = 5;
    }
    c
}

fn cc_auto_store(data: &Path, c: &CcAuto) {
    let _ = std::fs::write(data.join("ccauto.json"), serde_json::to_string(c).unwrap_or_default());
}

fn cc_auto_view(data: &Path) -> Value {
    let c = cc_auto_load(data);
    json!({"on": c.on, "minutes": c.minutes, "since": c.since, "last": c.last, "count": c.count})
}

/// 20초마다 불립니다. 이 앱의 에이전트가 쓰는 대화 번호를 기억해 두고, 켜져 있으면 주기에 맞춰 새 대화를 가져옵니다.
fn cc_auto_tick(app: &App) {
    let mut c = cc_auto_load(&app.data);
    let (sessions, own_dirs, busy_dirs): (Vec<String>, Vec<PathBuf>, Vec<String>) = {
        let g = app.inner.lock().unwrap();
        (
            g.s.agents.iter().map(|a| a.session.clone()).filter(|x| !x.is_empty()).collect(),
            g.s.agents.iter().filter(|a| !a.imported).map(|a| PathBuf::from(&a.dir)).chain(g.s.projects.iter().map(|p| PathBuf::from(&p.dir))).collect(),
            g.s.agents.iter().filter(|a| a.status == "working" || a.queue > 0).map(|a| a.dir.clone()).collect(),
        )
    };
    let mut changed = false;
    for sid in sessions {
        if !c.known.contains(&sid) {
            c.known.push(sid);
            changed = true;
        }
    }
    if c.known.len() > 4000 {
        let cut = c.known.len() - 4000;
        c.known.drain(..cut);
    }
    if changed {
        cc_auto_store(&app.data, &c);
    }
    let _ = (own_dirs, busy_dirs);
}

/// "모든 대화 가져오기 / 업데이트" 버튼: 아직 안 가져온 대화를 모두 가져오고, 이미 가져온 대화에 터미널에서 더해진 메시지를 채팅에 덧붙입니다.
/// 돌려주는 값: (새로 가져온 대화 수, 덧붙인 메시지 수)
fn cc_update(app: &App) -> (u64, u64) {
    cc_auto_tick(app);
    let c = cc_auto_load(&app.data);
    let (own_dirs, busy, imported_agents): (Vec<PathBuf>, HashSet<String>, Vec<(String, String)>) = {
        let g = app.inner.lock().unwrap();
        (
            g.s.agents.iter().filter(|a| !a.imported).map(|a| PathBuf::from(&a.dir)).chain(g.s.projects.iter().map(|p| PathBuf::from(&p.dir))).collect(),
            g.s.agents.iter().filter(|a| a.status == "working" || a.queue > 0).map(|a| a.id.clone()).collect(),
            g.s.agents.iter().filter(|a| a.imported && !a.session.is_empty()).map(|a| (a.id.clone(), a.session.clone())).collect(),
        )
    };
    // 1) 이미 가져온 대화: 터미널에서 더해진 메시지 덧붙이기 (일하는 중인 에이전트는 건너뜀)
    let mut refreshed = 0u64;
    for (id, sid) in imported_agents {
        if busy.contains(&id) {
            continue;
        }
        let Some(path) = cc_find(&sid) else { continue };
        let (_, file) = cc_parse(&path, 40_000_000, 200, false);
        let mut g = app.inner.lock().unwrap();
        let have: Vec<String> = g.s.msgs.get(&id).map(|l| l.iter().filter(|m| m.kind == "text").map(|m| m.text.trim().to_string()).collect()).unwrap_or_default();
        // 채팅의 최근 메시지(뒤에서 10개까지)가 기록 파일의 어디에 있는지 찾아, 그 뒤의 것만 새 메시지로 봅니다. 못 찾으면 중복을 막기 위해 아무것도 더하지 않습니다.
        let mut start = None;
        for h in have.iter().rev().take(10) {
            if let Some(i) = file.iter().rposition(|m| m.1.trim() == h) {
                start = Some(i + 1);
                break;
            }
        }
        let Some(start) = start else { continue };
        if start >= file.len() {
            continue;
        }
        for (role, text) in &file[start..] {
            app.add_msg(&mut g, &id, role, "text", text);
            refreshed += 1;
        }
        app.save(&g);
    }
    // 2) 아직 안 가져온 대화 모두 가져오기 (이 앱이 스스로 만든 대화는 제외)
    let known: HashSet<String> = c.known.iter().cloned().collect();
    let mut imported = 0u64;
    for sv in cc_sessions(&known, 200) {
        let (sid, cwd) = (sv["id"].as_str().unwrap_or(""), sv["cwd"].as_str().unwrap_or(""));
        if sv["linked"] == true || sv["exists"] != true {
            continue;
        }
        let cp = Path::new(cwd);
        if cp.starts_with(&app.work) || cp.starts_with(&app.data) || own_dirs.iter().any(|d| cp.starts_with(d)) {
            continue;
        }
        if cc_import(app, sid, "").is_ok() {
            imported += 1;
        }
    }
    let mut c = cc_auto_load(&app.data);
    c.count += imported;
    c.last = now();
    cc_auto_store(&app.data, &c);
    (imported, refreshed)
}

// ---- 원격 제어(Remote Control): 에이전트 폴더를 Claude 앱(claude.ai/code)에서 열기 ----
struct RcProc {
    pid: u32,
    url: String,
    log: String,
    exited: bool,
    started: u64,
}

fn rc_map() -> &'static Mutex<HashMap<String, RcProc>> {
    static M: std::sync::OnceLock<Mutex<HashMap<String, RcProc>>> = std::sync::OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn rc_view(id: &str) -> Value {
    match rc_map().lock().unwrap().get(id) {
        Some(p) => json!({"running": !p.exited, "url": p.url, "log": p.log, "started": p.started}),
        None => json!({"running": false, "url": "", "log": ""}),
    }
}

fn rc_stop(id: &str) {
    let pid = rc_map().lock().unwrap().remove(id).filter(|p| !p.exited).map(|p| p.pid);
    if let Some(pid) = pid {
        kill_tree(pid);
    }
}

fn rc_clean(line: &str) -> String {
    // 터미널 색상 코드(ESC[...)를 걷어냅니다.
    let mut out = String::new();
    let mut it = line.chars().peekable();
    while let Some(ch) = it.next() {
        if ch == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for c2 in it.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if !ch.is_control() {
            out.push(ch);
        }
    }
    out
}

fn rc_start(app: &App, agent: &Agent) -> Result<(), String> {
    use std::io::BufRead;
    let claude = app.claude.clone().ok_or("Claude Code 를 찾지 못했습니다.")?;
    if rc_map().lock().unwrap().get(&agent.id).map_or(false, |p| !p.exited) {
        return Ok(());
    }
    let mut cmd = Command::new(&claude);
    cmd.args(["remote-control", "--name", &format!("ClaudeBot · {}", agent.name)])
        .current_dir(&agent.dir)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // 끌 때 자식 프로세스까지 함께 끝내기 위해 따로 묶습니다.
    }
    let mut child = cmd
        .spawn().map_err(|e| format!("원격 제어를 시작하지 못했습니다: {e}"))?;
    let pid = child.id();
    rc_map().lock().unwrap().insert(agent.id.clone(), RcProc { pid, url: String::new(), log: String::new(), exited: false, started: now() });
    let feed = |id: String, r: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(r).split(b'\n').map_while(Result::ok) {
                let line = rc_clean(&String::from_utf8_lossy(&line));
                let mut m = rc_map().lock().unwrap();
                let Some(p) = m.get_mut(&id) else { break };
                if p.pid != pid {
                    break;
                }
                if p.url.is_empty() {
                    if let Some(i) = line.find("https://claude.ai/") {
                        p.url = line[i..].split_whitespace().next().unwrap_or("").to_string();
                    }
                }
                if !line.trim().is_empty() {
                    p.log.push_str(line.trim());
                    p.log.push('\n');
                    if p.log.len() > 3000 {
                        let cut = p.log.len() - 3000;
                        let cut = (cut..p.log.len()).find(|i| p.log.is_char_boundary(*i)).unwrap_or(0);
                        p.log.drain(..cut);
                    }
                }
            }
        });
    };
    if let Some(o) = child.stdout.take() {
        feed(agent.id.clone(), Box::new(o));
    }
    if let Some(e) = child.stderr.take() {
        feed(agent.id.clone(), Box::new(e));
    }
    let id = agent.id.clone();
    std::thread::spawn(move || {
        let _ = child.wait();
        if let Some(p) = rc_map().lock().unwrap().get_mut(&id) {
            if p.pid == pid {
                p.exited = true;
            }
        }
    });
    Ok(())
}

fn pct_encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn arg_value(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    if let Some(code) = assist::cli_service() {
        std::process::exit(code);
    }
    let data = arg_value("--data").map(PathBuf::from).unwrap_or_else(data_dir);
    let work = arg_value("--work").map(PathBuf::from).unwrap_or_else(|| data.join("agents"));
    let listen = arg_value("--listen").unwrap_or_else(|| "127.0.0.1:7717".to_string());
    std::fs::create_dir_all(&data).expect("데이터 폴더를 만들 수 없습니다");
    std::fs::create_dir_all(&work).expect("작업 폴더를 만들 수 없습니다");

    let cfg_path = data.join("runner.json");
    let old: Value = std::fs::read_to_string(&cfg_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    let token = old["token"].as_str().map(String::from).unwrap_or_else(random_code);

    let server = match Server::http(&listen) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{listen} 에서 시작하지 못했습니다: {e}\n이미 실행기가 켜져 있는지 확인하세요.");
            std::thread::sleep(Duration::from_secs(6));
            std::process::exit(1);
        }
    };
    assist::set_listen(&listen);
    let port = listen.rsplit(':').next().unwrap_or("7717");
    let _ = std::fs::write(&cfg_path, json!({"url": format!("http://127.0.0.1:{port}"), "token": token}).to_string());

    let claude = find_claude();
    let version = claude.as_ref()
        .and_then(|c| Command::new(c).arg("--version").stdin(Stdio::null()).output().ok())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let screens = cfg!(unix) && which(&["Xvfb"]).is_some() && which(&["import"]).is_some();

    let mut saved: Saved = std::fs::read_to_string(data.join("state.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    for a in &mut saved.agents {
        a.status = "idle".into();
        a.queue = 0;
        a.mode = clean_mode(&json!(a.mode));
    }
    for list in saved.msgs.values_mut() {
        for m in list.iter_mut() {
            if m.data["live"] == true {
                m.data = Value::Null;
            }
        }
    }
    let data_for_git = data.clone();
    let app = Arc::new(App {
        inner: Mutex::new(Inner { s: saved, ..Default::default() }),
        cv: Condvar::new(), data, work: work.clone(), token: token.clone(), claude: claude.clone(), version: version.clone(), screens,
        backup: Mutex::new(Backup::default()), bk_busy: Mutex::new(()),
        git: Mutex::new(git_load(&data_for_git)), git_busy: Mutex::new(()),
        tools: Mutex::new(jload(&data_for_git, "tools.json")), upd: Mutex::new(UpdInfo::default()), term_cwd: Mutex::new(work.clone()), login_br: Mutex::new(None), extra: Mutex::new(assist::load(&data_for_git)),
    });
    *app.backup.lock().unwrap() = bk_load(&app.data);
    // Codespaces 비밀값 등으로 미리 넣어 두면, 새로 만든 환경에서도 자동으로 백업에 연결됩니다.
    if app.backup.lock().unwrap().url.is_empty() {
        if let (Ok(u), Ok(k)) = (std::env::var("CLAUDEBOT_FIREBASE_URL"), std::env::var("CLAUDEBOT_BACKUP_KEY")) {
            if let Some(u) = bk_norm_url(&u) {
                if bk_key_ok(k.trim()) {
                    let mut b = app.backup.lock().unwrap();
                    b.url = u;
                    b.key = k.trim().to_string();
                    b.auth = std::env::var("CLAUDEBOT_FIREBASE_AUTH").unwrap_or_default().trim().to_string();
                    bk_store(&app.data, &b);
                    println!("  백업 주소를 환경변수에서 가져왔습니다.");
                }
            }
        }
    }
    if app.inner.lock().unwrap().s.agents.is_empty() && !app.backup.lock().unwrap().url.is_empty() {
        if let Ok(mut sv) = bk_pull(&app) {
            if !sv.agents.is_empty() {
                bk_pull_hist(&app);
                bk_normalize(&mut sv);
                bk_rebase(&app.work, &mut sv);
                let mut g = app.inner.lock().unwrap();
                g.s = sv;
                app.save(&g);
                println!("  백업에서 대화를 불러왔습니다.");
            }
        }
    }
    app.write_briefs(&app.inner.lock().unwrap());
    {
        let app = app.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(45));
            let (on, last) = {
                let b = app.backup.lock().unwrap();
                (!b.url.is_empty() && !b.key.is_empty(), b.last_hash)
            };
            if !on {
                continue;
            }
            let h = {
                let g = app.inner.lock().unwrap();
                bk_hash(&serde_json::to_string(&g.s).unwrap_or_default())
            };
            if h != last {
                let _ = bk_push(&app);
            }
        });
    }

    {
        let app = app.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(20));
            cc_auto_tick(&app);
        });
    }
    {
        let app = app.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(60));
            if app.git.lock().unwrap().enabled {
                let _ = git_sync(&app);
            }
        });
    }

    if let Ok(me) = std::env::current_exe() {
        if let Some(d) = me.parent() {
            for n in ["claudebot-runner.exe.old", "claudebot.exe.old", "claudebot-runner.old", "claudebot.old"] {
                let _ = std::fs::remove_file(d.join(n));
            }
        }
    }
    {
        let app = app.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(20));
            if !app.tools.lock().unwrap().upd_repo.is_empty() {
                let _ = upd_check(&app);
            }
            std::thread::sleep(Duration::from_secs(6 * 3600));
        });
    }

    println!("==============================================");
    println!("  ClaudeBot 실행기");
    println!("==============================================");
    println!("  주소        : http://{listen}");
    println!("  연결 코드   : {token}");
    println!("  작업 폴더   : {}", work.display());
    match &claude {
        Some(p) => println!("  Claude Code : {} ({})", if version.is_empty() { "버전 확인 실패" } else { &version }, p.display()),
        None => println!("  Claude Code : 찾지 못함! 설치 후 이 창을 닫고 다시 실행하세요."),
    }
    println!("  가상 화면   : {}", if screens { "사용 (에이전트마다 전용 화면)" } else { "없음 (Linux + Xvfb 에서 사용 가능)" });
    println!("----------------------------------------------");
    println!("  이 창을 닫으면 에이전트가 모두 멈춥니다.");

    assist::startup(&app);
    if assist::exposed() {
        println!("  ⚠ 주의      : 이 컴퓨터 밖에서도 접속할 수 있게 열려 있고 연결은 암호화되지 않아요. Tailscale 같은 VPN 사용을 권해요.");
    }
    start_vnc_gateway(app.clone(), &listen);
    if VNC_PORT.load(std::sync::atomic::Ordering::Relaxed) > 0 {
        println!("  VNC 화면    : 사용 (포트 {})", VNC_PORT.load(std::sync::atomic::Ordering::Relaxed));
    }
    for req in server.incoming_requests() {
        let app = app.clone();
        std::thread::spawn(move || handle(app, req));
    }
}
