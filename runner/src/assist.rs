// 비서 기능: 예약 작업, 할 일, 사용자 노트, 승인 카드, 이어하기, 요약, 서비스 등록, 자가 진단.
// 이 파일의 데이터는 데이터 폴더의 extra.json 에 저장됩니다.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

pub static BEAT: AtomicU64 = AtomicU64::new(0);
static LISTEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn set_listen(s: &str) {
    let _ = LISTEN.set(s.to_string());
}

fn listen() -> String {
    LISTEN.get().cloned().unwrap_or_default()
}

/// 실행기가 이 컴퓨터 밖(다른 기기)에도 열려 있는지.
pub fn exposed() -> bool {
    let l = listen();
    let host = l.rsplit_once(':').map(|x| x.0).unwrap_or(&l);
    // Tailscale 전용 주소(100.64.0.0/10)는 내 기기끼리만 닿으므로 열려 있다고 보지 않습니다.
    let tailscale = host.strip_prefix("100.").and_then(|r| r.split('.').next()).and_then(|x| x.parse::<u8>().ok()).map_or(false, |x| (64..=127).contains(&x));
    !(host.is_empty() || host == "127.0.0.1" || host == "localhost" || host == "[::1]" || tailscale)
}

pub const BRIEF: &str = "\n## 비서 기능 (사용자를 도울 때)\n- 사용자가 놓치기 쉬운 할 일이나 마감이 보이면 답변에 `@@TODO 할 일 | 2026-11-20 09:00` 한 줄을 쓰세요. 마감을 모르면 `|` 뒤를 비웁니다. 이미 열려 있는 할 일과 같은 것은 쓰지 마세요.\n- 사용자에 대해 오래 쓸 새 사실을 알게 되면 `@@NOTE 내용` 한 줄로 제안하세요. 사용자가 승인해야 노트에 들어갑니다. 비밀번호·카드번호 같은 민감한 정보는 제안하지 마세요.\n- 되돌릴 수 없는 일(메일·메시지 보내기, 결제, 삭제, 외부 게시)은 하기 직전에 멈추고 `@@ASK 하려는 일을 한 문장으로` 한 줄만 쓰세요. 사용자가 승인하면 다시 알려 줍니다.\n- 사용자가 만든 참고 문서(MD) 목록이 요청 앞에 붙어 오면, 관련 있을 때만 그 파일을 열어 읽으세요. 내용을 매번 읽을 필요는 없습니다.\n- 작업 중 꼭 물어봐야 할 게 있으면(방향이 갈리거나 정보가 없을 때) `@@QUESTION 질문 | 선택지1 | 선택지2` 한 줄을 쓰고 거기서 멈추세요. 선택지는 없어도 되고 최대 6개입니다. 사소한 건 묻지 말고 스스로 정하세요. 질문 도구(AskUserQuestion)는 쓰지 마세요.\n- `@@` 로 시작하는 줄은 앱이 읽는 신호입니다. 설명할 때는 그 줄을 따옴표 없이 쓰지 마세요.\n";

// ---------- 저장 구조 ----------
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Sched {
    pub id: u64,
    pub agent: String,
    pub text: String,
    pub kind: String, // daily | once | interval
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub time: String, // daily: "HH:MM"
    #[serde(default)]
    pub days: Vec<u8>, // 1=월 .. 7=일, 비면 매일
    #[serde(default)]
    pub at: u64, // once: 실행할 시각(ms)
    #[serde(default)]
    pub every: u64, // interval: 분
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub last: u64,
    #[serde(default)]
    pub last_day: i64,
    #[serde(default)]
    pub created: u64,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Todo {
    pub id: u64,
    pub text: String,
    #[serde(default)]
    pub due: u64,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub notified: bool,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Sugg {
    pub id: u64,
    pub user: String,
    pub text: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub time: u64,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub agent: String,
    pub text: String,
    #[serde(default)]
    pub time: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Extra {
    #[serde(default)]
    pub scheds: Vec<Sched>,
    #[serde(default)]
    pub todos: Vec<Todo>,
    #[serde(default)]
    pub notes: HashMap<String, String>,
    #[serde(default)]
    pub sugg: Vec<Sugg>,
    #[serde(default)]
    pub inflight: HashMap<String, Pending>,
    #[serde(default)]
    pub next: u64,
    #[serde(default)]
    pub tz: i64, // 분 단위, 동쪽이 +
    #[serde(default)]
    pub seen: u64,
}

pub fn load(data: &Path) -> Extra {
    jload(data, "extra.json")
}

fn store(app: &App, e: &Extra) {
    jstore(&app.data, "extra.json", e);
}

// ---------- 시간 계산 (지역 시각) ----------
fn local_parts(ms: u64, tz: i64) -> (i64, i64, u8) {
    let s = (ms / 1000) as i64 + tz * 60;
    let day = s.div_euclid(86400);
    let sec = s.rem_euclid(86400);
    let wd = ((day + 3).rem_euclid(7) + 1) as u8; // 1970-01-01 은 목요일
    (day, sec, wd)
}

fn hm(t: &str) -> i64 {
    let mut it = t.trim().split(':').filter_map(|x| x.trim().parse::<i64>().ok());
    let h = it.next().unwrap_or(9).clamp(0, 23);
    let m = it.next().unwrap_or(0).clamp(0, 59);
    h * 3600 + m * 60
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// "2026-11-20 09:00" 또는 "2026-11-20" 을 ms 로. 못 읽으면 0.
fn parse_due(s: &str, tz: i64) -> u64 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }
    let (d, t) = s.split_once(|c: char| c == ' ' || c == 'T').unwrap_or((s, "09:00"));
    let dp: Vec<i64> = d.split('-').filter_map(|x| x.trim().parse().ok()).collect();
    if dp.len() != 3 || dp[1] < 1 || dp[1] > 12 || dp[2] < 1 || dp[2] > 31 {
        return 0;
    }
    let secs = days_from_civil(dp[0], dp[1], dp[2]) * 86400 + hm(t) - tz * 60;
    if secs <= 0 { 0 } else { (secs * 1000) as u64 }
}

// ---------- 알림 (앱 안 알림 + 브라우저 알림). 나중에 카카오톡 같은 외부 알림은 여기에 붙입니다. ----------
pub fn notify(app: &App, g: &mut Inner, title: &str, text: &str) {
    app.emit(g, json!({"type": "reminder", "title": title, "text": text}));
}

// ---------- 에이전트 답변 속 신호 ----------
pub enum Sig {
    Login(String),
    Todo(String, String),
    Note(String),
    Ask(String),
    Question(String, Vec<String>),
}

pub fn split_signals(text: &str) -> (String, Vec<Sig>) {
    let mut sigs = Vec::new();
    let mut keep = Vec::new();
    let mut has_login = false;
    for line in text.lines() {
        let t = line.trim();
        if let Some(r) = t.strip_prefix("@@LOGIN") {
            if !has_login {
                has_login = true;
                let n = r.trim();
                sigs.push(Sig::Login(if n.is_empty() { "웹사이트".to_string() } else { n.chars().take(80).collect() }));
            }
        } else if let Some(r) = t.strip_prefix("@@TODO") {
            let (a, b) = r.split_once('|').unwrap_or((r, ""));
            let a = a.trim();
            if !a.is_empty() {
                sigs.push(Sig::Todo(a.chars().take(200).collect(), b.trim().to_string()));
            }
        } else if let Some(r) = t.strip_prefix("@@NOTE") {
            let a = r.trim();
            if !a.is_empty() {
                sigs.push(Sig::Note(a.chars().take(300).collect()));
            }
        } else if let Some(r) = t.strip_prefix("@@QUESTION") {
            let mut it = r.split('|').map(|x| x.trim()).filter(|x| !x.is_empty());
            if let Some(q) = it.next() {
                let opts: Vec<String> = it.take(6).map(|x| x.chars().take(80).collect()).collect();
                sigs.push(Sig::Question(q.chars().take(400).collect(), opts));
            }
        } else if let Some(r) = t.strip_prefix("@@ASK") {
            let a = r.trim();
            if !a.is_empty() {
                sigs.push(Sig::Ask(a.chars().take(300).collect()));
            }
        } else {
            keep.push(line);
        }
    }
    (keep.join("\n").trim().to_string(), sigs)
}

/// 신호 처리. g 를 잡은 채로 부르며, extra 는 항상 g 다음에 잠급니다.
pub fn on_signal(app: &App, g: &mut Inner, agent: &Agent, sig: Sig) {
    match sig {
        Sig::Login(site) => {
            app.add_msg_d(g, &agent.id, "agent", "login", &site, json!({}));
        }
        Sig::Ask(what) => {
            app.add_msg_d(g, &agent.id, "agent", "ask", &what, json!({"done": ""}));
        }
        Sig::Question(q, opts) => {
            app.add_msg_d(g, &agent.id, "agent", "question", &q, json!({"done": "", "opts": opts}));
        }
        Sig::Todo(text, due) => {
            let mut e = app.extra.lock().unwrap();
            if e.todos.iter().any(|t| !t.done && t.text == text) {
                return;
            }
            e.next += 1;
            let id = e.next;
            let d = parse_due(&due, e.tz);
            e.todos.push(Todo { id, text, due: d, created: now(), source: agent.name.clone(), user: "owner".into(), ..Default::default() });
            store(app, &e);
            drop(e);
            app.emit(g, json!({"type": "assist"}));
        }
        Sig::Note(text) => {
            let mut e = app.extra.lock().unwrap();
            let have = e.notes.get("owner").map(|n| n.contains(&text)).unwrap_or(false);
            if have || e.sugg.iter().any(|s| s.text == text) {
                return;
            }
            e.next += 1;
            let id = e.next;
            e.sugg.push(Sugg { id, user: "owner".into(), text, agent: agent.name.clone(), time: now() });
            store(app, &e);
            drop(e);
            app.emit(g, json!({"type": "assist"}));
        }
    }
}

/// 모든 에이전트가 일을 시작할 때 앞에 붙는 사용자 노트와 열린 할 일.
pub fn prompt_extra(app: &App) -> String {
    let e = app.extra.lock().unwrap();
    let mut out = String::new();
    if let Some(n) = e.notes.get("owner") {
        if !n.trim().is_empty() {
            out.push_str(&format!("[사용자 노트 — 참고만 하세요]\n{}\n\n", n.trim().chars().take(2500).collect::<String>()));
        }
    }
    let docs = md_list(app);
    if !docs.is_empty() {
        out.push_str("[사용자가 만든 참고 문서 — 관련 있을 때만 Read 로 열어 보세요]\n");
        for d in docs.iter().take(30) {
            out.push_str(&format!("- {} : {}\n", d["path"].as_str().unwrap_or(""), d["title"].as_str().unwrap_or("")));
        }
        out.push('\n');
    }
    let open: Vec<&Todo> = e.todos.iter().filter(|t| !t.done).take(10).collect();
    if !open.is_empty() {
        out.push_str("[사용자의 열린 할 일]\n");
        for t in open {
            out.push_str(&format!("- {}\n", clip(&t.text, 120)));
        }
        out.push('\n');
    }
    if out.is_empty() { out } else { format!("{out}---\n") }
}

// ---------- 내 캐릭터 이미지 (사용자가 직접 올린 파일, 데이터 폴더에만 저장) ----------
pub fn char_dir(app: &App) -> PathBuf {
    let d = app.data.join("char");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn b64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => continue,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

pub fn char_file(app: &App, name: &str) -> Option<(String, Vec<u8>)> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return None;
    }
    let bytes = std::fs::read(char_dir(app).join(name)).ok()?;
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    let ct = match ext.as_str() { "gif" => "image/gif", "png" => "image/png", "webp" => "image/webp", "jpg" | "jpeg" => "image/jpeg", _ => return None };
    Some((ct.to_string(), bytes))
}

// ---------- 간단 MD 문서 ----------
pub fn md_dir(app: &App) -> PathBuf {
    let d = app.data.join("md");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn md_name(s: &str) -> String {
    let n: String = s.trim().chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else if c == ' ' { '-' } else { '_' }).take(40).collect();
    let n = n.trim_matches(|c| c == '-' || c == '_').to_string();
    if n.is_empty() { "문서".to_string() } else { n }
}

fn md_list(app: &App) -> Vec<Value> {
    let dir = md_dir(app);
    let mut v: Vec<(u64, Value)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            let title = text.lines().map(|l| l.trim().trim_start_matches('#').trim()).find(|l| !l.is_empty()).unwrap_or("").chars().take(60).collect::<String>();
            let name = p.file_stem().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
            let mt = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0);
            v.push((mt, json!({"name": name, "title": title, "path": p.to_string_lossy(), "size": text.len(), "time": mt})));
        }
    }
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.into_iter().map(|x| x.1).collect()
}

// ---------- 이어하기 표시 ----------
pub fn inflight_set(app: &App, agent: &str, text: &str) {
    let mut e = app.extra.lock().unwrap();
    e.inflight.insert(agent.to_string(), Pending { agent: agent.to_string(), text: clip(text, 1500), time: now() });
    store(app, &e);
}

pub fn inflight_clear(app: &App, agent: &str) {
    let mut e = app.extra.lock().unwrap();
    if e.inflight.remove(agent).is_some() {
        store(app, &e);
    }
}

// ---------- 시작: 끊긴 작업을 이어하기 카드로, 예약 스케줄러 시작 ----------
pub fn startup(app: &Arc<App>) {
    let pend: Vec<Pending> = {
        let mut e = app.extra.lock().unwrap();
        let v: Vec<Pending> = e.inflight.drain().map(|x| x.1).collect();
        if !v.is_empty() {
            store(app, &e);
        }
        v
    };
    if !pend.is_empty() {
        let mut g = app.inner.lock().unwrap();
        for p in pend {
            if g.s.agents.iter().any(|a| a.id == p.agent) {
                app.add_msg_d(&mut g, &p.agent, "system", "resume", &p.text, json!({"done": ""}));
            }
        }
        app.save(&g);
    }
    let app = app.clone();
    std::thread::spawn(move || loop {
        BEAT.store(now(), Ordering::Relaxed);
        tick(&app);
        std::thread::sleep(Duration::from_secs(15));
    });
}

fn fire(app: &Arc<App>, s: &Sched) {
    let mut g = app.inner.lock().unwrap();
    if !g.s.agents.iter().any(|a| a.id == s.agent) {
        return;
    }
    let label = if s.name.is_empty() { "예약 작업".to_string() } else { s.name.clone() };
    app.add_msg_d(&mut g, &s.agent, "user", "text", &format!("⏰ {}\n{}", label, s.text), json!({"sched": s.id}));
    app.enqueue(&mut g, &s.agent, Job { text: s.text.clone(), from: String::new(), hops: 0 });
    app.save(&g);
}

fn tick(app: &Arc<App>) {
    let t = now();
    let mut fires: Vec<Sched> = Vec::new();
    let mut remind: Vec<Todo> = Vec::new();
    {
        let mut e = app.extra.lock().unwrap();
        let (day, sec, wd) = local_parts(t, e.tz);
        let mut dirty = false;
        for s in e.scheds.iter_mut() {
            if !s.enabled {
                continue;
            }
            let go = match s.kind.as_str() {
                "daily" => {
                    let ts = hm(&s.time);
                    (s.days.is_empty() || s.days.contains(&wd)) && sec >= ts && sec < ts + 3 * 3600 && s.last_day != day
                }
                "once" => {
                    if s.at > 0 && t >= s.at + 6 * 3600 * 1000 && s.last == 0 {
                        s.enabled = false; // 너무 오래 지났으면 실행하지 않고 끕니다
                        dirty = true;
                    }
                    s.enabled && s.at > 0 && t >= s.at && s.last == 0
                }
                "interval" => s.every > 0 && t >= s.last.max(s.created) + s.every * 60_000,
                _ => false,
            };
            if go {
                s.last = t;
                s.last_day = day;
                if s.kind == "once" {
                    s.enabled = false;
                }
                fires.push(s.clone());
                dirty = true;
            }
        }
        for td in e.todos.iter_mut() {
            if !td.done && td.due > 0 && td.due <= t && !td.notified {
                td.notified = true;
                remind.push(td.clone());
                dirty = true;
            }
        }
        if dirty {
            store(app, &e);
        }
    }
    for s in &fires {
        fire(app, s);
    }
    if !fires.is_empty() || !remind.is_empty() {
        let mut g = app.inner.lock().unwrap();
        for td in &remind {
            notify(app, &mut g, "할 일 시간이에요", &td.text);
        }
        app.emit(&mut g, json!({"type": "assist"}));
    }
}

// ---------- 서비스 등록 ----------
fn cmd_out(c: &mut Command) -> (bool, String) {
    match c.stdin(Stdio::null()).output() {
        Ok(o) => (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)).trim().to_string()),
        Err(e) => (false, e.to_string()),
    }
}

fn is_root() -> bool {
    cfg!(unix) && cmd_out(Command::new("id").arg("-u")).1 == "0"
}

fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn service_file() -> PathBuf {
    if cfg!(windows) {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home());
        base.join("Microsoft").join("Windows").join("Start Menu").join("Programs").join("Startup").join("ClaudeBot.bat")
    } else if is_root() {
        PathBuf::from("/etc/systemd/system/claudebot.service")
    } else {
        home().join(".config").join("systemd").join("user").join("claudebot.service")
    }
}

pub fn service_on() -> bool {
    service_file().is_file()
}

fn run_args() -> Vec<String> {
    let mut a = vec!["--listen".to_string(), {
        let l = listen();
        if l.is_empty() { "127.0.0.1:7717".to_string() } else { l }
    }];
    for k in ["--data", "--work"] {
        if let Some(v) = arg_value(k) {
            a.push(k.to_string());
            a.push(v);
        }
    }
    a
}

pub fn service_install() -> (bool, String) {
    let Ok(exe) = std::env::current_exe() else { return (false, "실행 파일 위치를 알 수 없습니다.".into()) };
    let args = run_args();
    let f = service_file();
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if cfg!(windows) {
        let line = args.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(" ");
        let bat = format!(
            "@echo off\r\nif not \"%1\"==\"min\" (\r\n  start \"\" /min \"%~f0\" min\r\n  exit /b\r\n)\r\n:loop\r\n\"{}\" {}\r\ntimeout /t 5 /nobreak >nul\r\ngoto loop\r\n",
            exe.display(), line
        );
        return match std::fs::write(&f, bat) {
            Ok(_) => (true, format!("등록했어요. 다음에 Windows 에 로그인할 때부터 실행기가 자동으로 켜져요. ({})", f.display())),
            Err(e) => (false, format!("등록하지 못했어요: {e}")),
        };
    }
    let root = is_root();
    let path_env = std::env::var("PATH").unwrap_or_default();
    let unit = format!(
        "[Unit]\nDescription=ClaudeBot runner\nAfter=network-online.target\n\n[Service]\nExecStart={} {}\nRestart=always\nRestartSec=5\nWorkingDirectory={}\nEnvironment=\"PATH={}\"\nEnvironment=\"HOME={}\"\n\n[Install]\nWantedBy={}\n",
        exe.display(), args.join(" "), home().display(), path_env, home().display(),
        if root { "multi-user.target" } else { "default.target" }
    );
    if let Err(e) = std::fs::write(&f, unit) {
        return (false, format!("등록 파일을 쓰지 못했어요: {e}"));
    }
    let sc = |args: &[&str]| {
        let mut c = Command::new("systemctl");
        if !root {
            c.arg("--user");
        }
        c.args(args);
        cmd_out(&mut c)
    };
    let (ok1, m1) = sc(&["daemon-reload"]);
    let (ok2, m2) = sc(&["enable", "claudebot.service"]);
    if !(ok1 && ok2) {
        return (false, format!("systemd 에 등록하지 못했어요. {} {}", m1, m2).trim().to_string());
    }
    let mut note = String::new();
    if !root {
        let user = std::env::var("USER").unwrap_or_default();
        let (ok, m) = cmd_out(Command::new("loginctl").args(["enable-linger", &user]));
        if !ok {
            note = format!(" (로그인하지 않아도 켜지게 하려면 `sudo loginctl enable-linger {user}` 를 한 번 실행하세요. {m})");
        }
    }
    (true, format!("등록했어요. 지금 켜져 있는 실행기는 그대로 두고, 컴퓨터를 다시 켤 때부터 자동으로 시작돼요.{note}"))
}

pub fn service_uninstall() -> (bool, String) {
    let f = service_file();
    if !cfg!(windows) {
        let root = is_root();
        let mut c = Command::new("systemctl");
        if !root {
            c.arg("--user");
        }
        let _ = cmd_out(c.args(["disable", "claudebot.service"]));
    }
    if !cfg!(windows) {
        // enable 이 남긴 연결 파일도 지웁니다.
        for d in ["/etc/systemd/system/multi-user.target.wants/claudebot.service"] {
            let _ = std::fs::remove_file(d);
        }
        let _ = std::fs::remove_file(home().join(".config/systemd/user/default.target.wants/claudebot.service"));
    }
    match std::fs::remove_file(&f) {
        Ok(_) => (true, "등록을 해제했어요.".into()),
        Err(e) => (false, format!("해제하지 못했어요: {e}")),
    }
}

/// `--install-service` / `--uninstall-service` 로 실행했을 때. 처리했으면 종료 코드.
pub fn cli_service() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    let inst = args.iter().any(|a| a == "--install-service");
    let uninst = args.iter().any(|a| a == "--uninstall-service");
    if !inst && !uninst {
        return None;
    }
    if let Some(l) = arg_value("--listen") {
        set_listen(&l);
    }
    let (ok, m) = if inst { service_install() } else { service_uninstall() };
    println!("{m}");
    Some(if ok { 0 } else { 1 })
}

// ---------- 자가 진단 ----------
fn chk(id: &str, name: &str, ok: bool, detail: &str, fix: &str) -> Value {
    json!({"id": id, "name": name, "ok": ok, "detail": detail, "fix": if ok { "" } else { fix }})
}

pub fn selftest(app: &Arc<App>) -> Value {
    let mut v: Vec<Value> = Vec::new();
    v.push(chk("claude", "Claude Code", app.claude.is_some(), &if app.version.is_empty() { "찾지 못함".to_string() } else { clip(app.version.lines().next().unwrap_or(""), 60) },
        "터미널에서 `claude` 를 한 번 실행해 로그인하고, 실행기를 다시 시작하세요."));
    let wr = {
        let p = app.data.join(".write_test");
        let ok = std::fs::write(&p, b"1").is_ok();
        let _ = std::fs::remove_file(&p);
        ok
    };
    v.push(chk("data", "데이터 폴더 쓰기", wr, &app.data.to_string_lossy(), "데이터 폴더의 권한이나 남은 용량을 확인하세요."));
    v.push(chk("screens", "가상 화면 (Xvfb)", app.screens, if app.screens { "사용 중" } else { "없음" },
        "Linux 에서 `sudo apt install xvfb imagemagick` 를 실행하고 실행기를 다시 시작하세요."));
    let br = which(&["chromium", "chromium-browser", "google-chrome", "google-chrome-stable", "chrome"]).is_some();
    v.push(chk("browser", "브라우저 (Chromium)", br, if br { "설치됨" } else { "찾지 못함" },
        "`sudo apt install chromium` 또는 Chrome 을 설치하세요. (에이전트의 브라우저 기능에 필요)"));
    let node = which(&["node", "node.exe", "npx", "npx.cmd"]).is_some();
    v.push(chk("node", "Node.js (브라우저 도구용)", node, if node { "설치됨" } else { "찾지 못함" },
        "Node.js 를 설치하세요. 에이전트의 브라우저 도구(playwright)가 npx 로 실행돼요."));
    let vnc = vnc_ok();
    v.push(chk("vnc", "실시간 화면 (VNC)", vnc, if vnc { "사용 중" } else { "꺼짐" },
        "`x11vnc` 와 `novnc` 가 필요해요. 설정 > 일반 의 한국어 팩 설치로 함께 설치돼요."));
    let ko = kopack_status(app);
    let ko_ok = ko["locale"].as_bool().unwrap_or(false) && ko["font"].as_bool().unwrap_or(false);
    v.push(chk("ko", "한국어 팩 (글꼴·언어)", ko_ok, if ko_ok { "설치됨" } else { "일부 없음" },
        "설정 > 일반 의 '한국어 팩 설치' 를 누르세요."));
    let bk = !app.backup.lock().unwrap().url.is_empty();
    v.push(chk("backup", "백업 (Firebase)", bk, if bk { "연결됨" } else { "연결 안 됨" },
        "설정 > 백업 에서 Firebase 주소를 연결하세요. (없어도 동작하지만 기기가 바뀌면 대화가 사라져요)"));
    let gt = app.git.lock().unwrap().enabled;
    v.push(chk("git", "GitHub 자동 기록", gt, if gt { "켜짐" } else { "꺼짐" }, "설정 > GitHub 에서 켜 두면 작업 기록이 남아요. (선택)"));
    let beat = BEAT.load(Ordering::Relaxed);
    let alive = beat > 0 && now().saturating_sub(beat) < 60_000;
    v.push(chk("sched", "예약 작업 시계", alive, if alive { "작동 중" } else { "멈춤" }, "실행기를 다시 시작하세요."));
    let sv = service_on();
    v.push(chk("service", "자동 시작 (서비스 등록)", sv, if sv { "등록됨" } else { "등록 안 됨" },
        "설정 > 진단 의 '서비스로 등록' 을 누르세요. 컴퓨터를 켜면 실행기가 자동으로 시작돼요."));
    let ex = exposed();
    v.push(chk("secure", "접속 보안", !ex, &if ex { format!("{} 로 열려 있음 (암호화 없음)", listen()) } else { "이 컴퓨터에서만 접속".to_string() },
        "다른 기기에서 쓰려면 --listen 0.0.0.0 대신 Tailscale(무료 VPN)로 연결하세요. 도움말 참고."));
    let ok = v.iter().filter(|c| c["ok"] == true).count();
    json!({"checks": v, "ok": ok, "total": v.len()})
}

// ---------- 요약 ----------
fn digest(app: &Arc<App>, since: u64) -> Value {
    let t = now();
    let g = app.inner.lock().unwrap();
    let e = app.extra.lock().unwrap();
    let mut agents: Vec<Value> = Vec::new();
    let mut blocked: Vec<Value> = Vec::new();
    for a in &g.s.agents {
        let list = g.s.msgs.get(&a.id).cloned().unwrap_or_default();
        let recent: Vec<&Msg> = list.iter().filter(|m| m.time > since).collect();
        let texts = recent.iter().filter(|m| m.kind == "text" && m.from == "agent").count();
        let asked = recent.iter().filter(|m| m.kind == "text" && m.from == "user").count();
        let errs = recent.iter().filter(|m| m.kind == "error").count();
        let last = recent.iter().rev().find(|m| m.kind == "text" && m.from == "agent").map(|m| clip(m.text.trim(), 160)).unwrap_or_default();
        if !recent.is_empty() {
            agents.push(json!({"id": a.id, "name": a.name, "answers": texts, "asked": asked, "errors": errs, "last": last, "status": a.status}));
        }
        for m in list.iter().rev().take(80) {
            if (m.kind == "ask" || m.kind == "resume" || m.kind == "question") && m.data["done"] == "" {
                blocked.push(json!({"agent": a.id, "name": a.name, "kind": m.kind, "text": clip(&m.text, 120), "mid": m.id}));
            }
        }
        if let Some(m) = list.iter().rev().find(|m| m.kind != "meta" && m.kind != "thinking") {
            if m.kind == "login" {
                blocked.push(json!({"agent": a.id, "name": a.name, "kind": "login", "text": m.text, "mid": m.id}));
            } else if m.kind == "error" {
                blocked.push(json!({"agent": a.id, "name": a.name, "kind": "error", "text": clip(&m.text, 120), "mid": m.id}));
            }
        }
    }
    let open: Vec<&Todo> = e.todos.iter().filter(|x| !x.done).collect();
    let overdue: Vec<Value> = open.iter().filter(|x| x.due > 0 && x.due < t).map(|x| json!({"id": x.id, "text": x.text, "due": x.due})).collect();
    let soon: Vec<Value> = open.iter().filter(|x| x.due >= t && x.due < t + 24 * 3600 * 1000).map(|x| json!({"id": x.id, "text": x.text, "due": x.due})).collect();
    let sugg: Vec<Value> = e.sugg.iter().filter(|s| s.user == "owner").map(|s| json!({"id": s.id, "text": s.text, "agent": s.agent})).collect();
    json!({"since": since, "now": t, "agents": agents, "blocked": blocked, "overdue": overdue, "soon": soon, "open": open.len(), "sugg": sugg,
        "empty": agents.is_empty() && blocked.is_empty() && overdue.is_empty() && soon.is_empty() && sugg.is_empty()})
}

fn assist_state(app: &Arc<App>, user: &str) -> Value {
    let e = app.extra.lock().unwrap();
    json!({
        "scheds": e.scheds, "todos": e.todos.iter().filter(|t| t.user == user || t.user.is_empty()).collect::<Vec<_>>(),
        "notes": e.notes.get(user).cloned().unwrap_or_default(),
        "sugg": e.sugg.iter().filter(|s| s.user == user).collect::<Vec<_>>(),
        "tz": e.tz, "seen": e.seen, "now": now(), "listen": listen(), "exposed": exposed(), "service": service_on(),
        "user": user,
    })
}

// ---------- HTTP 경로 ----------
pub fn route(app: &Arc<App>, method: &Method, parts: &[&str], input: &Value, q: &HashMap<String, String>) -> Option<(u16, Value)> {
    let user = q.get("user").cloned().filter(|u| !u.is_empty()).unwrap_or_else(|| "owner".into());
    let bad = |m: &str| Some((400, json!({"error": m})));
    match (method, parts) {
        (Method::Get, ["api", "assist"]) => {
            if let Some(tz) = q.get("tz").and_then(|x| x.parse::<i64>().ok()) {
                let mut e = app.extra.lock().unwrap();
                if e.tz != tz && tz.abs() <= 14 * 60 {
                    e.tz = tz;
                    store(app, &e);
                }
            }
            Some((200, assist_state(app, &user)))
        }
        (Method::Get, ["api", "digest"]) => {
            let since = q.get("since").and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
            Some((200, digest(app, since)))
        }
        (Method::Post, ["api", "digest-seen"]) => {
            let mut e = app.extra.lock().unwrap();
            e.seen = now();
            store(app, &e);
            Some((200, json!({"ok": true, "seen": e.seen})))
        }
        (Method::Get, ["api", "selftest"]) => Some((200, selftest(app))),
        (Method::Post, ["api", "service"]) => {
            let (ok, msg) = match input["action"].as_str().unwrap_or("") {
                "install" => service_install(),
                "uninstall" => service_uninstall(),
                _ => return bad("알 수 없는 동작입니다."),
            };
            Some((200, json!({"ok": ok, "message": msg, "service": service_on()})))
        }
        // ----- 내 캐릭터 이미지 -----
        (Method::Post, ["api", "charimg"]) => {
            let name = input["name"].as_str().unwrap_or("");
            let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
            if !["gif", "png", "webp", "jpg", "jpeg"].contains(&ext.as_str()) {
                return bad("gif, png, webp, jpg 이미지만 올릴 수 있어요.");
            }
            let bytes = b64_decode(input["data"].as_str().unwrap_or(""));
            if bytes.is_empty() || bytes.len() > 12_000_000 {
                return bad("파일이 비었거나 12MB 보다 커요.");
            }
            let stem: String = name.rsplit_once('.').map(|x| x.0).unwrap_or(name).chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).take(30).collect();
            let file = format!("{}_{}.{}", stem, now() % 100000, ext);
            match std::fs::write(char_dir(app).join(&file), bytes) {
                Ok(_) => Some((200, json!({"ok": true, "url": format!("/api/charimg/{file}")}))),
                Err(e) => Some((500, json!({"error": format!("저장하지 못했어요: {e}")}))),
            }
        }
        // ----- 간단 MD 문서 -----
        (Method::Get, ["api", "md"]) => Some((200, json!({"docs": md_list(app)}))),
        (Method::Get, ["api", "md", name]) => {
            let p = md_dir(app).join(format!("{}.md", md_name(name)));
            match std::fs::read_to_string(&p) {
                Ok(t) => Some((200, json!({"name": md_name(name), "text": t}))),
                Err(_) => Some((404, json!({"error": "문서를 찾지 못했습니다."}))),
            }
        }
        (Method::Post, ["api", "md"]) => {
            let text = input["text"].as_str().unwrap_or("").to_string();
            if text.trim().is_empty() {
                return bad("내용이 비어 있습니다.");
            }
            let first = text.lines().map(|l| l.trim().trim_start_matches('#').trim()).find(|l| !l.is_empty()).unwrap_or("문서").to_string();
            let name = md_name(input["name"].as_str().filter(|n| !n.trim().is_empty()).unwrap_or(&first));
            let t: String = text.chars().take(60_000).collect();
            match std::fs::write(md_dir(app).join(format!("{name}.md")), t) {
                Ok(_) => Some((200, json!({"ok": true, "name": name}))),
                Err(e) => Some((500, json!({"error": format!("저장하지 못했어요: {e}")}))),
            }
        }
        (Method::Post, ["api", "md", name, "delete"]) => {
            let _ = std::fs::remove_file(md_dir(app).join(format!("{}.md", md_name(name))));
            Some((200, json!({"ok": true})))
        }
        // ----- 예약 -----
        (Method::Post, ["api", "sched"]) => {
            let agent = input["agent"].as_str().unwrap_or("").to_string();
            let text = input["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return bad("할 일 내용이 비어 있습니다.");
            }
            if !app.inner.lock().unwrap().s.agents.iter().any(|a| a.id == agent) {
                return bad("에이전트를 고르세요.");
            }
            let kind = input["kind"].as_str().unwrap_or("daily").to_string();
            let mut s = Sched { agent, text, kind: kind.clone(), name: input["name"].as_str().unwrap_or("").chars().take(40).collect(), enabled: true, created: now(), ..Default::default() };
            match kind.as_str() {
                "daily" => {
                    s.time = input["time"].as_str().unwrap_or("09:00").to_string();
                    s.days = input["days"].as_array().map(|a| a.iter().filter_map(|x| x.as_u64()).filter(|x| (1..=7).contains(x)).map(|x| x as u8).collect()).unwrap_or_default();
                }
                "once" => {
                    s.at = input["at"].as_u64().unwrap_or(0);
                    if s.at <= now() {
                        return bad("지난 시각이에요. 앞으로의 시각을 고르세요.");
                    }
                }
                "interval" => {
                    s.every = input["every"].as_u64().unwrap_or(0);
                    if s.every < 1 {
                        return bad("간격(분)을 정하세요.");
                    }
                }
                _ => return bad("종류를 알 수 없습니다."),
            }
            let mut e = app.extra.lock().unwrap();
            e.next += 1;
            s.id = e.next;
            e.scheds.push(s);
            store(app, &e);
            Some((200, json!({"ok": true})))
        }
        (Method::Post, ["api", "sched", id, act]) => {
            let id: u64 = id.parse().unwrap_or(0);
            let mut run: Option<Sched> = None;
            {
                let mut e = app.extra.lock().unwrap();
                match *act {
                    "toggle" => {
                        if let Some(s) = e.scheds.iter_mut().find(|s| s.id == id) {
                            s.enabled = !s.enabled;
                            if s.enabled {
                                s.last = if s.kind == "once" { 0 } else { s.last };
                            }
                        }
                    }
                    "delete" => e.scheds.retain(|s| s.id != id),
                    "run" => run = e.scheds.iter().find(|s| s.id == id).cloned(),
                    _ => return bad("알 수 없는 동작입니다."),
                }
                store(app, &e);
            }
            if let Some(s) = run {
                fire(app, &s);
            }
            Some((200, json!({"ok": true})))
        }
        // ----- 할 일 -----
        (Method::Post, ["api", "todo"]) => {
            let text = input["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return bad("내용이 비어 있습니다.");
            }
            let mut e = app.extra.lock().unwrap();
            e.next += 1;
            let id = e.next;
            e.todos.push(Todo { id, text: text.chars().take(200).collect(), due: input["due"].as_u64().unwrap_or(0), created: now(), source: "사용자".into(), user: user.clone(), ..Default::default() });
            store(app, &e);
            Some((200, json!({"ok": true})))
        }
        (Method::Post, ["api", "todo", id, act]) => {
            let id: u64 = id.parse().unwrap_or(0);
            let mut e = app.extra.lock().unwrap();
            match *act {
                "toggle" => {
                    if let Some(t) = e.todos.iter_mut().find(|t| t.id == id) {
                        t.done = !t.done;
                    }
                }
                "delete" => e.todos.retain(|t| t.id != id),
                _ => return bad("알 수 없는 동작입니다."),
            }
            store(app, &e);
            Some((200, json!({"ok": true})))
        }
        // ----- 노트 -----
        (Method::Post, ["api", "notes"]) => {
            let who = input["user"].as_str().filter(|u| !u.is_empty()).unwrap_or(&user).to_string();
            let text = input["text"].as_str().unwrap_or("").chars().take(6000).collect::<String>();
            let mut e = app.extra.lock().unwrap();
            e.notes.insert(who, text);
            store(app, &e);
            Some((200, json!({"ok": true})))
        }
        (Method::Post, ["api", "note-sugg", id]) => {
            let id: u64 = id.parse().unwrap_or(0);
            let mut e = app.extra.lock().unwrap();
            if let Some(i) = e.sugg.iter().position(|s| s.id == id) {
                let s = e.sugg.remove(i);
                if input["action"] == "add" {
                    let n = e.notes.entry(s.user.clone()).or_default();
                    if !n.is_empty() && !n.ends_with('\n') {
                        n.push('\n');
                    }
                    n.push_str(&format!("- {}\n", s.text));
                }
                store(app, &e);
            }
            Some((200, json!({"ok": true})))
        }
        // ----- 승인 카드 / 이어하기 -----
        (Method::Post, ["api", "agents", id, kind @ ("ask" | "resume" | "question")]) => {
            let mid = input["mid"].as_u64().unwrap_or(0);
            let action = input["action"].as_str().unwrap_or("").to_string();
            let mut g = app.inner.lock().unwrap();
            let Some(m) = g.s.msgs.get(*id).and_then(|l| l.iter().find(|m| m.id == mid && m.kind == *kind)).cloned() else {
                return Some((404, json!({"error": "카드를 찾지 못했습니다."})));
            };
            if m.data["done"] != "" {
                return Some((200, json!({"ok": true, "already": true})));
            }
            let (done, text) = match (*kind, action.as_str()) {
                ("ask", "yes") => ("yes", format!("승인합니다. 진행하세요: {}", m.text)),
                ("ask", "no") => ("no", format!("거절합니다. 다음 일은 하지 마세요: {}", m.text)),
                ("resume", "go") => ("go", format!("[이전 작업이 중간에 끊겼습니다. 아래 요청을 이어서 마무리하세요. 이미 끝낸 부분은 반복하지 마세요.]\n\n{}", m.text)),
                ("resume", "drop") => ("drop", String::new()),
                ("question", "answer") => {
                    let ans = input["text"].as_str().unwrap_or("").trim().chars().take(2000).collect::<String>();
                    if ans.is_empty() {
                        return bad("답을 입력하거나 고르세요.");
                    }
                    app.upd_msg(&mut g, id, |x| x.id == mid, |x| x.data = json!({"done": "answered", "answer": ans, "opts": m.data["opts"].clone()}));
                    let text = format!("[질문에 대한 사용자의 답입니다. 이어서 진행하세요.]\n질문: {}\n답: {}", m.text, ans);
                    app.add_msg(&mut g, id, "user", "text", &ans);
                    app.enqueue(&mut g, id, Job { text, from: String::new(), hops: 0 });
                    app.save(&g);
                    return Some((200, json!({"ok": true})));
                }
                ("question", "skip") => ("skipped", "[사용자가 질문에 답하지 않기로 했습니다. 가장 무난한 쪽으로 정하고, 어떻게 정했는지 한 줄로 알려 주세요.]".to_string()),
                _ => return bad("알 수 없는 동작입니다."),
            };
            app.upd_msg(&mut g, id, |x| x.id == mid, |x| x.data = json!({"done": done}));
            if !text.is_empty() {
                let shown = if *kind == "resume" { "이어서 해 주세요.".to_string() } else { text.clone() };
                app.add_msg(&mut g, id, "user", "text", &shown);
                app.enqueue(&mut g, id, Job { text, from: String::new(), hops: 0 });
            }
            app.save(&g);
            Some((200, json!({"ok": true})))
        }
        _ => None,
    }
}
