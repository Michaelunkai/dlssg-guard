//! dlssg-guard: stop Streamline/DLSS-G "present failed" (DXGI_ERROR_INVALID_CALL)
//! crashes for every game on this machine.
//!
//! Two layers of protection:
//! 1. File layer: a game can only use DLSS Frame Generation if it ships the
//!    Streamline plugin `sl.dlss_g.dll` / NVIDIA's `nvngx_dlssg.dll`. Every such
//!    file (any case, any copy/backup variant) is renamed to `*.dlssg-disabled`
//!    (reversible) in every game folder on every drive.
//! 2. Config layer: Unreal Engine games get `r.Streamline.DLSSG.Enable=0` in
//!    their `Saved/Config/Windows*/Engine.ini` (idempotent).
//!
//! A scheduled task runs `watch` forever so game updates / new installs are
//! covered too.
//!
//! Commands: diag | scan | fix | restore | watch | install | uninstall | tail

use std::env;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

const SUFFIX: &str = ".dlssg-disabled";
const TASK: &str = "dlssg-guard";
const CVAR: &str = "r.Streamline.DLSSG.Enable=0";
const CVAR_KEY: &str = "r.streamline.dlssg.enable";
const MAX_DEPTH: u32 = 30;

static LOGGING: AtomicBool = AtomicBool::new(false);
static VERBOSE: AtomicBool = AtomicBool::new(true);

// ---------------------------------------------------------------- output ---

#[repr(C)]
struct SysTime {
    y: u16,
    mo: u16,
    dw: u16,
    d: u16,
    h: u16,
    mi: u16,
    s: u16,
    ms: u16,
}
extern "system" {
    fn GetLocalTime(t: *mut SysTime);
}

/// Local timestamp `YYYY-MM-DD HH:MM:SS`.
fn now() -> String {
    let mut t = SysTime { y: 0, mo: 0, dw: 0, d: 0, h: 0, mi: 0, s: 0, ms: 0 };
    unsafe { GetLocalTime(&mut t) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.y, t.mo, t.d, t.h, t.mi, t.s)
}

fn data_dir() -> PathBuf {
    PathBuf::from(env::var("ProgramData").unwrap_or("C:\\ProgramData".into())).join("dlssg-guard")
}

fn log_path() -> PathBuf {
    data_dir().join("guard.log")
}

/// Timestamped line to stdout (never panics: a scheduled task has no console)
/// and, in watcher mode, to the log file.
fn out(s: &str) {
    let line = format!("{} {}", now(), s);
    let _ = writeln!(io::stdout(), "{line}");
    if LOGGING.load(Ordering::Relaxed) {
        let dir = data_dir();
        let _ = fs::create_dir_all(&dir);
        let p = log_path();
        if fs::metadata(&p).map(|m| m.len() > 5_000_000).unwrap_or(false) {
            let _ = fs::rename(&p, dir.join("guard.log.1"));
        }
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = writeln!(f, "{line}");
        }
    }
}

macro_rules! say {
    ($($a:tt)*) => { out(&format!($($a)*)) };
}

// ------------------------------------------------------------- detection ---

/// OS / NVIDIA driver folders, never touched.
const SKIP_DIRS: [&str; 7] = [
    "$recycle.bin",
    "system volume information",
    "windows",
    "driverstore",
    "$windows.~bt",
    "winsxs",
    "nvidia corporation",
];

fn is_skipped(name: &str, full: &Path) -> bool {
    let l = name.to_ascii_lowercase();
    if SKIP_DIRS.contains(&l.as_str()) {
        return true;
    }
    let f = full.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
    f.ends_with("\\programdata\\nvidia") || f.ends_with("\\programdata\\dlssg-guard")
}

/// Any DLL-ish file for DLSS-G: `sl.dlss_g*` / `nvngx_dlssg*`, any case,
/// including `.bak` / ` - Copy` variants.
fn is_dlssg_name(l: &str) -> bool {
    (l.contains("sl.dlss_g") || l.contains("nvngx_dlssg")) && l.contains(".dll")
}

fn is_ue_engine_ini(l: &str, p: &Path) -> bool {
    if l != "engine.ini" {
        return false;
    }
    let dir = p
        .parent()
        .and_then(|d| d.file_name())
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let full = p.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
    dir.starts_with("windows") && full.contains("\\saved\\config\\")
}

#[derive(Default)]
struct Scan {
    live: Vec<PathBuf>,
    off: Vec<PathBuf>,
    inis: Vec<PathBuf>,
}

struct Prog {
    last: Instant,
    dirs: u64,
    files: u64,
}

impl Prog {
    fn tick(&mut self, cur: &Path, s: &Scan) {
        if VERBOSE.load(Ordering::Relaxed) && self.last.elapsed() >= Duration::from_secs(2) {
            say!(
                "  ... {} dirs, {} files scanned, {} exposed, {} neutralized, {} Engine.ini; now in {}",
                self.dirs,
                self.files,
                s.live.len(),
                s.off.len(),
                s.inis.len(),
                cur.display()
            );
            self.last = Instant::now();
        }
    }
}

fn walk(dir: &Path, depth: u32, s: &mut Scan, pr: &mut Prog) {
    if depth > MAX_DEPTH {
        return;
    }
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return, // denied / vanished: skip, never crash
    };
    pr.dirs += 1;
    pr.tick(dir, s);
    for e in rd.flatten() {
        let p = e.path();
        let ft = match e.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let name = e.file_name().to_string_lossy().to_string();
        if ft.is_dir() {
            if ft.is_symlink() || is_skipped(&name, &p) {
                continue;
            }
            walk(&p, depth + 1, s, pr);
        } else if ft.is_file() {
            pr.files += 1;
            let l = name.to_ascii_lowercase();
            if is_dlssg_name(&l) {
                if l.ends_with(SUFFIX) {
                    s.off.push(p);
                } else {
                    s.live.push(p);
                }
            } else if is_ue_engine_ini(&l, &p) {
                s.inis.push(p);
            }
        }
    }
}

fn drives() -> Vec<PathBuf> {
    (b'C'..=b'Z')
        .map(|c| PathBuf::from(format!("{}:\\", c as char)))
        .filter(|p| p.exists())
        .collect()
}

fn scan(roots: &[PathBuf]) -> Scan {
    let mut s = Scan::default();
    let n = roots.len();
    for (i, r) in roots.iter().enumerate() {
        let t = Instant::now();
        let mut pr = Prog { last: Instant::now(), dirs: 0, files: 0 };
        if VERBOSE.load(Ordering::Relaxed) {
            say!("[{}/{}] scanning {}", i + 1, n, r.display());
        }
        walk(r, 0, &mut s, &mut pr);
        if VERBOSE.load(Ordering::Relaxed) {
            say!(
                "[{}/{}] done {} in {:.1}s: {} dirs, {} files; totals exposed {}, neutralized {}, Engine.ini {}",
                i + 1,
                n,
                r.display(),
                t.elapsed().as_secs_f32(),
                pr.dirs,
                pr.files,
                s.live.len(),
                s.off.len(),
                s.inis.len()
            );
        }
    }
    s
}

// ---------------------------------------------------------------- actions ---

fn disabled_name(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(SUFFIX);
    PathBuf::from(s)
}

#[allow(clippy::permissions_set_readonly_false)]
fn clear_ro(p: &Path) {
    if let Ok(m) = fs::metadata(p) {
        let mut perm = m.permissions();
        if perm.readonly() {
            perm.set_readonly(false);
            let _ = fs::set_permissions(p, perm);
        }
    }
}

/// Retry with backoff (locked files, AV scanners, launchers closing handles).
fn retry<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut last = None;
    for ms in [0u64, 150, 450, 1350] {
        if ms > 0 {
            sleep(Duration::from_millis(ms));
        }
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap())
}

fn disable_one(p: &Path) -> io::Result<()> {
    clear_ro(p);
    let dst = disabled_name(p);
    if dst.exists() {
        clear_ro(&dst);
        retry(|| fs::remove_file(&dst))?;
    }
    retry(|| fs::rename(p, &dst))
}

/// Returns new text if the cvar is missing/wrong, `None` if already correct.
fn with_cvar(text: &str) -> Option<String> {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let (mut found, mut changed) = (false, false);
    for l in lines.iter_mut() {
        let t = l.trim().to_ascii_lowercase();
        if let Some(rest) = t.strip_prefix(CVAR_KEY) {
            if let Some(v) = rest.trim_start().strip_prefix('=') {
                found = true;
                if v.trim() != "0" {
                    *l = CVAR.to_string();
                    changed = true;
                }
            }
        }
    }
    if !found {
        match lines.iter().position(|l| l.trim().eq_ignore_ascii_case("[systemsettings]")) {
            Some(i) => lines.insert(i + 1, CVAR.to_string()),
            None => {
                if lines.last().map(|l| !l.trim().is_empty()).unwrap_or(false) {
                    lines.push(String::new());
                }
                lines.push("[SystemSettings]".to_string());
                lines.push(CVAR.to_string());
            }
        }
        changed = true;
    }
    if changed {
        Some(lines.join(nl) + nl)
    } else {
        None
    }
}

/// Ensure the cvar in one Engine.ini. Ok(true) = changed.
fn guard_ini(p: &Path) -> io::Result<bool> {
    let bytes = fs::read(p)?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    match with_cvar(&text) {
        None => Ok(false),
        Some(new) => {
            let was_ro = fs::metadata(p).map(|m| m.permissions().readonly()).unwrap_or(false);
            clear_ro(p);
            retry(|| fs::write(p, new.as_bytes()))?;
            if was_ro {
                if let Ok(m) = fs::metadata(p) {
                    let mut perm = m.permissions();
                    perm.set_readonly(true);
                    let _ = fs::set_permissions(p, perm);
                }
            }
            Ok(true)
        }
    }
}

/// Neutralize every exposed dll and guard every UE Engine.ini. Returns (done, failed).
fn apply(s: &Scan) -> (usize, usize) {
    let (mut ok, mut bad) = (0, 0);
    let n = s.live.len();
    for (i, p) in s.live.iter().enumerate() {
        match disable_one(p) {
            Ok(_) => {
                say!("[{}/{}] disabled  {}", i + 1, n, p.display());
                ok += 1;
            }
            Err(e) => {
                say!("[{}/{}] FAILED    {} ({e}) - admin: {}", i + 1, n, p.display(), is_admin());
                bad += 1;
            }
        }
    }
    let m = s.inis.len();
    for (i, p) in s.inis.iter().enumerate() {
        match guard_ini(p) {
            Ok(true) => say!("[{}/{}] Engine.ini guarded {}", i + 1, m, p.display()),
            Ok(false) => {}
            Err(e) => {
                say!("[{}/{}] Engine.ini FAILED {} ({e})", i + 1, m, p.display());
                bad += 1;
            }
        }
    }
    (ok, bad)
}

fn fix(roots: &[PathBuf]) -> (usize, usize) {
    apply(&scan(roots))
}

fn restore(roots: &[PathBuf]) -> usize {
    let s = scan(roots);
    let mut n = 0;
    for p in &s.off {
        let st = p.to_string_lossy().to_string();
        let orig = PathBuf::from(&st[..st.len() - SUFFIX.len()]);
        if !orig.exists() && retry(|| fs::rename(p, &orig)).is_ok() {
            say!("restored  {}", orig.display());
            n += 1;
        }
    }
    n
}

fn run(cmd: &str, args: &[&str]) -> (bool, String) {
    match Command::new(cmd).args(args).output() {
        Ok(o) => (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        ),
        Err(e) => (false, e.to_string()),
    }
}

fn is_admin() -> bool {
    run("net", &["session"]).0
}

// ------------------------------------------------------------------- diag ---

/// Read-only: list files/dirs whose name contains `dlssg` under `dir`.
fn list_matching(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if e.file_name().to_string_lossy().to_ascii_lowercase().contains("dlssg") {
                out.push(p.clone());
            }
            if e.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false) {
                list_matching(&p, depth + 1, out);
            }
        }
    }
}

fn lock_open() -> io::Result<fs::File> {
    let _ = fs::create_dir_all(data_dir());
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(data_dir().join("watch.lock"))
}

fn versions(paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    let list: Vec<String> = paths
        .iter()
        .take(60)
        .map(|p| format!("'{}'", p.to_string_lossy().replace('\'', "''")))
        .collect();
    let ps = format!(
        "foreach($f in @({})){{ try{{$i=Get-Item -LiteralPath $f; '   {{0}} | version {{1}} | {{2}} bytes | {{3}}' -f $f,$i.VersionInfo.FileVersion,$i.Length,$i.LastWriteTime}}catch{{'   '+$f+' | unreadable'}} }}",
        list.join(",")
    );
    let (_, o) = run("powershell", &["-NoProfile", "-Command", &ps]);
    for l in o.lines().filter(|l| !l.trim().is_empty()) {
        say!("{l}");
    }
}

fn diag() {
    let (_, g) = run(
        "nvidia-smi",
        &["--query-gpu=name,driver_version", "--format=csv,noheader"],
    );
    say!("GPU/driver : {}", g.trim());
    let (_, h) = run(
        "reg",
        &["query", r"HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers", "/v", "HwSchMode"],
    );
    let hags = h.lines().find(|l| l.contains("HwSchMode")).unwrap_or("? (value absent = driver default)");
    say!("HAGS (HwSchMode 2=on 1=off) : {}", hags.trim());
    say!("running as admin : {}", is_admin());

    let mut ds = Vec::new();
    list_matching(&PathBuf::from(r"C:\Windows\System32\DriverStore\FileRepository"), 0, &mut ds);
    say!("driver-store dlssg files (read-only, never touched): {}", ds.len());
    for p in ds.iter().take(10) {
        say!("   {}", p.display());
    }
    let mut ngx = Vec::new();
    let pd = env::var("ProgramData").unwrap_or("C:\\ProgramData".into());
    list_matching(&PathBuf::from(&pd).join("NVIDIA").join("NGX"), 0, &mut ngx);
    say!("NGX model dir dlssg entries (ProgramData\\NVIDIA\\NGX): {}", ngx.len());
    for p in ngx.iter().take(10) {
        say!("   {}", p.display());
    }

    let s = scan(&drives());
    say!("exposed DLSS-G dlls in game folders : {}", s.live.len());
    for p in &s.live {
        say!("   {}", p.display());
    }
    say!("already neutralized                 : {}", s.off.len());
    say!("Streamline/DLSS-G dll versions in games:");
    let mut all = s.live.clone();
    all.extend(s.off.iter().cloned());
    versions(&all);
    let unguarded = s
        .inis
        .iter()
        .filter(|p| fs::read_to_string(p).ok().and_then(|t| with_cvar(&t)).is_some())
        .count();
    say!("UE Engine.ini found: {}, missing {CVAR}: {}", s.inis.len(), unguarded);
    for p in &s.inis {
        say!("   {}", p.display());
    }
    let (ok, o) = run("schtasks", &["/Query", "/TN", TASK, "/V", "/FO", "LIST"]);
    say!("permanent guard task installed      : {ok}");
    for l in o.lines() {
        let t = l.trim();
        for k in ["Status:", "Scheduled Task State:", "Run As User:", "Last Run Time:", "Last Result:", "Task To Run:"] {
            if t.starts_with(k) {
                say!("   {t}");
            }
        }
    }
    let watcher = lock_open().is_err();
    say!("watcher process running             : {watcher}");
    say!("log file: {}", log_path().display());
}

// ------------------------------------------------------------------ watch ---

fn watch() {
    LOGGING.store(true, Ordering::Relaxed);
    VERBOSE.store(false, Ordering::Relaxed);
    let _lock = match lock_open() {
        Ok(f) => f,
        Err(_) => {
            say!("another watcher is already running; exiting");
            return;
        }
    };
    say!("watcher started pid {} admin {}", std::process::id(), is_admin());
    let mut cycles: u64 = 0;
    loop {
        let r = std::panic::catch_unwind(|| {
            let s = scan(&drives());
            let (ok, bad) = apply(&s);
            if ok + bad > 0 {
                say!("cycle: neutralized {ok}, failed {bad}");
            }
        });
        if r.is_err() {
            say!("PANIC in watch cycle, continuing");
        }
        cycles += 1;
        if cycles % 30 == 1 {
            say!("heartbeat: cycle {cycles} ok");
        }
        sleep(Duration::from_secs(60));
    }
}

fn tail(args: &[String]) {
    let follow = args.iter().any(|a| a == "-f");
    let n: usize = args.iter().find_map(|a| a.parse().ok()).unwrap_or(20);
    let p = log_path();
    let text = fs::read_to_string(&p).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    for l in &lines[lines.len().saturating_sub(n)..] {
        let _ = writeln!(io::stdout(), "{l}");
    }
    if !follow {
        return;
    }
    let mut pos = text.len() as u64;
    loop {
        sleep(Duration::from_millis(500));
        if let Ok(mut f) = fs::File::open(&p) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < pos {
                pos = 0; // rotated
            }
            if len > pos && f.seek(SeekFrom::Start(pos)).is_ok() {
                let mut buf = String::new();
                if f.read_to_string(&mut buf).is_ok() {
                    let _ = write!(io::stdout(), "{buf}");
                    let _ = io::stdout().flush();
                    pos = len;
                }
            }
        }
    }
}

fn install() -> bool {
    let exe = match env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            say!("cannot find own exe: {e}");
            return false;
        }
    };
    // stable copy so deleting the build dir does not break the task
    let dir = data_dir();
    let _ = fs::create_dir_all(&dir);
    let dest = dir.join("dlssg-guard.exe");
    if exe != dest {
        // watcher may be running from dest: stop task, copy with retry
        let _ = run("schtasks", &["/End", "/TN", TASK]);
        match retry(|| fs::copy(&exe, &dest)) {
            Ok(_) => say!("copied exe to {}", dest.display()),
            Err(e) => say!("copy to {} failed: {e} (task keeps old exe)", dest.display()),
        }
    }
    let tr = format!("\"{}\" watch", dest.display());
    for rl in ["HIGHEST", "LIMITED"] {
        for sc in ["ONSTART", "ONLOGON"] {
            let (ok, out) = run(
                "schtasks",
                &["/Create", "/F", "/TN", TASK, "/SC", sc, "/RL", rl, "/TR", &tr],
            );
            if ok {
                say!("task '{TASK}' installed ({sc}, {rl}) -> {tr}");
                let _ = run("schtasks", &["/Run", "/TN", TASK]);
                return true;
            } else {
                say!("schtasks {sc}/{rl}: {}", out.trim());
            }
        }
    }
    false
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("fix");
    let roots: Vec<PathBuf> = if args.len() > 2 && cmd != "tail" {
        args[2..].iter().map(PathBuf::from).collect()
    } else {
        drives()
    };
    match cmd {
        "diag" => diag(),
        "scan" => {
            let s = scan(&roots);
            for p in &s.live {
                say!("{}", p.display());
            }
            for p in &s.inis {
                say!("Engine.ini {}", p.display());
            }
            say!("exposed: {}", s.live.len());
        }
        "fix" => {
            say!("admin: {}", is_admin());
            let (ok, bad) = fix(&roots);
            let left = scan(&roots).live.len();
            say!("fixed {ok}, failed {bad}, remaining exposed {left}");
            std::process::exit(if left == 0 { 0 } else { 1 });
        }
        "restore" => say!("restored {}", restore(&roots)),
        "watch" => watch(),
        "tail" => tail(&args[2..]),
        "install" => {
            say!("[1/2] fix");
            fix(&roots);
            say!("[2/2] install scheduled task");
            std::process::exit(if install() { 0 } else { 1 });
        }
        "uninstall" => {
            let (ok, o) = run("schtasks", &["/Delete", "/F", "/TN", TASK]);
            say!("{ok} {}", o.trim());
        }
        _ => say!("usage: dlssg-guard diag|scan|fix|restore|watch|tail [-f] [n]|install|uninstall [roots...]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let b = env::temp_dir().join(format!("dlssg-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&b);
        fs::create_dir_all(&b).unwrap();
        VERBOSE.store(false, Ordering::Relaxed);
        b
    }

    #[test]
    fn fix_and_restore_roundtrip() {
        let base = tmp("rt");
        let game = base.join("Game").join("Plugins").join("Streamline");
        fs::create_dir_all(&game).unwrap();
        fs::write(game.join("sl.dlss_g.dll"), b"x").unwrap();
        fs::write(game.join("nvngx_dlssg.dll"), b"x").unwrap();
        fs::write(game.join("sl.interposer.dll"), b"keep").unwrap();
        let roots = vec![base.clone()];
        assert_eq!(scan(&roots).live.len(), 2);
        assert_eq!(fix(&roots), (2, 0));
        assert_eq!(scan(&roots).live.len(), 0);
        assert!(game.join("sl.interposer.dll").exists());
        assert_eq!(fix(&roots), (0, 0)); // idempotent
        fs::write(game.join("sl.dlss_g.dll"), b"y").unwrap(); // game update re-adds
        assert_eq!(fix(&roots), (1, 0));
        assert_eq!(restore(&roots), 2);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn readonly_file_is_neutralized() {
        let base = tmp("ro");
        let f = base.join("sl.dlss_g.dll");
        fs::write(&f, b"x").unwrap();
        let mut perm = fs::metadata(&f).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&f, perm).unwrap();
        // stale read-only disabled copy too
        let d = disabled_name(&f);
        fs::write(&d, b"old").unwrap();
        let mut perm = fs::metadata(&d).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&d, perm).unwrap();
        assert_eq!(fix(std::slice::from_ref(&base)), (1, 0));
        assert!(!f.exists() && d.exists());
        clear_ro(&d);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn variant_names_detected() {
        let base = tmp("var");
        let deep = base.join("a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p");
        fs::create_dir_all(&deep).unwrap();
        for n in [
            "SL.DLSS_G.DLL",
            "sl.dlss_g.dll.bak",
            "Nvngx_DlssG.dll",
            "nvngx_dlssg - Copy.dll",
            "sl.dlss_g_v2.dll",
        ] {
            fs::write(deep.join(n), b"x").unwrap();
        }
        fs::write(deep.join("sl.dlss_g.dll.dlssg-disabled"), b"x").unwrap();
        fs::write(deep.join("sl.dlss.dll"), b"keep").unwrap(); // DLSS SR, not frame gen
        let s = scan(std::slice::from_ref(&base));
        assert_eq!(s.live.len(), 5);
        assert_eq!(s.off.len(), 1);
        assert_eq!(fix(std::slice::from_ref(&base)), (5, 0));
        assert!(deep.join("sl.dlss.dll").exists());
        assert_eq!(scan(std::slice::from_ref(&base)).live.len(), 0);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn engine_ini_guard_is_idempotent() {
        let base = tmp("ini");
        let cfg = base.join("Game/Saved/Config/WindowsNoEditor");
        fs::create_dir_all(&cfg).unwrap();
        let ini = cfg.join("Engine.ini");
        fs::write(&ini, "[SystemSettings]\r\nr.Foo=1\r\n").unwrap();
        let roots = vec![base.clone()];
        fix(&roots);
        fix(&roots);
        let t = fs::read_to_string(&ini).unwrap();
        assert_eq!(t.matches(CVAR).count(), 1);
        assert!(t.contains("r.Foo=1"));
        // wrong value gets corrected, no duplicate
        fs::write(&ini, "[SystemSettings]\nr.Streamline.DLSSG.Enable=1\n").unwrap();
        fix(&roots);
        let t = fs::read_to_string(&ini).unwrap();
        assert_eq!(t.matches("Enable").count(), 1);
        assert!(t.contains(CVAR));
        // no section at all
        assert!(with_cvar("").unwrap().contains("[SystemSettings]"));
        assert!(with_cvar(CVAR).is_none());
        let _ = fs::remove_dir_all(&base);
    }
}
