//! App side of a flash: launches the helper via osascript, tails progress, cancels.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

pub const EVENT: &str = "flash://progress";
const TICK: Duration = Duration::from_millis(200);
const RATE_WINDOW: Duration = Duration::from_secs(5);

/// Work dir of the running flash, if any.
#[derive(Default, Clone)]
pub struct FlashState(Arc<Mutex<Option<PathBuf>>>);

/// Escape a Rust string as an AppleScript string literal.
fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `do shell script` with `quoted form of` for every argument.
fn build_script(helper: &Path, image: &str, disk: &str, work_dir: &Path, env: &str) -> String {
    let q = |s: &str| format!("quoted form of {}", applescript_str(s));
    format!(
        "do shell script {env} & {} & \" --image \" & {} & \" --disk \" & {} & \" --work-dir \" & {} with administrator privileges",
        q(&helper.to_string_lossy()),
        q(image),
        q(disk),
        q(&work_dir.to_string_lossy()),
        env = applescript_str(env),
    )
}

/// Debug builds pass FLASHOS_ALLOW_VIRTUAL through (osascript drops the environment).
fn helper_env() -> &'static str {
    #[cfg(debug_assertions)]
    if std::env::var("FLASHOS_ALLOW_VIRTUAL").as_deref() == Ok("1") {
        return "FLASHOS_ALLOW_VIRTUAL=1 ";
    }
    ""
}

fn helper_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding the app executable")?;
    let p = exe
        .parent()
        .context("app executable has no parent dir")?
        .join("flashos-helper");
    if !p.is_file() {
        bail!("Helper not found at {}", p.display());
    }
    Ok(p)
}

fn make_work_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("flashos-{}", uuid::Uuid::new_v4()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    File::create(dir.join("progress.jsonl")).context("creating progress file")?;
    Ok(dir)
}

pub fn start(app: AppHandle, state: &FlashState, image: &str, disk: &str) -> Result<()> {
    let mut slot = state.0.lock().unwrap();
    if slot.is_some() {
        bail!("A flash is already running");
    }
    let helper = helper_path()?;
    let work_dir = make_work_dir()?;
    let script = build_script(&helper, image, disk, &work_dir, helper_env());
    let child = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => {
            let _ = fs::remove_dir_all(&work_dir);
            bail!("Starting osascript failed: {e}");
        }
    };
    *slot = Some(work_dir.clone());
    let state = state.clone();
    thread::spawn(move || {
        let last = monitor(&app, child, &work_dir);
        let _ = fs::remove_dir_all(&work_dir);
        *state.0.lock().unwrap() = None;
        let _ = app.emit(EVENT, last);
    });
    Ok(())
}

pub fn cancel(state: &FlashState) -> Result<()> {
    if let Some(dir) = state.0.lock().unwrap().as_ref() {
        File::create(dir.join("cancel")).context("requesting cancel")?;
    }
    Ok(())
}

fn is_terminal(v: &Value) -> bool {
    matches!(v["phase"].as_str(), Some("done" | "error" | "cancelled"))
}

/// Tails progress.jsonl until osascript exits; returns the final event.
fn monitor(app: &AppHandle, mut child: std::process::Child, work_dir: &Path) -> Value {
    let mut tail = Tail::new(work_dir.join("progress.jsonl"));
    let mut rate = Rate::default();
    let mut terminal: Option<Value> = None;
    let mut handle = |v: Value, rate: &mut Rate| {
        if is_terminal(&v) {
            terminal = Some(v);
        } else {
            let _ = app.emit(EVENT, rate.annotate(v));
        }
    };
    let status = loop {
        for v in tail.read() {
            handle(v, &mut rate);
        }
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => thread::sleep(TICK),
            Err(_) => break None,
        }
    };
    for v in tail.read() {
        handle(v, &mut rate);
    }
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    if stderr.contains("(-128)") {
        return json!({"phase": "cancelled"});
    }
    if let Some(v) = terminal {
        return v;
    }
    let msg = stderr.trim();
    let msg = if msg.is_empty() {
        format!("The helper exited unexpectedly ({status:?})")
    } else {
        msg.to_string()
    };
    json!({"phase": "error", "message": msg})
}

/// Reads complete new lines appended to a JSONL file.
struct Tail {
    path: PathBuf,
    pos: u64,
    partial: Vec<u8>,
}

impl Tail {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            pos: 0,
            partial: Vec::new(),
        }
    }

    fn read(&mut self) -> Vec<Value> {
        let Ok(mut f) = File::open(&self.path) else {
            return vec![];
        };
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(self.pos)).is_err() || f.read_to_end(&mut buf).is_err() {
            return vec![];
        }
        self.pos += buf.len() as u64;
        self.partial.extend_from_slice(&buf);
        let Some(end) = self.partial.iter().rposition(|&b| b == b'\n') else {
            return vec![];
        };
        let rest = self.partial.split_off(end + 1);
        let lines = std::mem::replace(&mut self.partial, rest);
        lines
            .split(|&b| b == b'\n')
            .filter_map(|l| serde_json::from_slice(l).ok())
            .collect()
    }
}

/// Adds `rateBps` and `etaSecs` from a sliding window of samples.
#[derive(Default)]
struct Rate {
    phase: String,
    samples: VecDeque<(Instant, u64, u64)>,
}

impl Rate {
    fn annotate(&mut self, mut v: Value) -> Value {
        let phase = v["phase"].as_str().unwrap_or_default().to_string();
        if phase != self.phase {
            self.phase = phase;
            self.samples.clear();
        }
        let done = v["done"].as_u64().unwrap_or(0);
        let src_done = v["srcDone"].as_u64().unwrap_or(done);
        let now = Instant::now();
        self.samples.push_back((now, done, src_done));
        while self.samples.len() > 2 && now - self.samples[0].0 > RATE_WINDOW {
            self.samples.pop_front();
        }
        let (t0, d0, s0) = self.samples[0];
        let secs = (now - t0).as_secs_f64();
        if secs < 0.5 {
            return v;
        }
        let rate = done.saturating_sub(d0) as f64 / secs;
        v["rateBps"] = json!(rate.round() as u64);
        // Percent for xz comes from compressed bytes, so ETA does too.
        let (left, speed) = match (v["total"].as_u64(), v["srcTotal"].as_u64()) {
            (Some(total), _) => (total.saturating_sub(done), rate),
            (None, Some(src_total)) => (
                src_total.saturating_sub(src_done),
                src_done.saturating_sub(s0) as f64 / secs,
            ),
            _ => return v,
        };
        if speed > 0.0 {
            v["etaSecs"] = json!((left as f64 / speed).round() as u64);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_quotes_everything() {
        let s = build_script(
            Path::new("/Apps/Flash OS.app/Contents/MacOS/flashos-helper"),
            "/Users/b/My \"odd\" \\ image's.img",
            "disk4",
            Path::new("/tmp/w"),
            "",
        );
        assert!(s.starts_with("do shell script \"\" & quoted form of \"/Apps/Flash OS.app/"));
        assert!(s.contains(r#"quoted form of "/Users/b/My \"odd\" \\ image's.img""#));
        assert!(s.contains("quoted form of \"disk4\""));
        assert!(s.ends_with("with administrator privileges"));
    }

    #[test]
    fn tail_reads_complete_lines() {
        let dir = std::env::temp_dir().join(format!("flashos-tail-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("progress.jsonl");
        fs::write(&p, "{\"phase\":\"writing\",\"done\":1}\n{\"phase\":\"wri").unwrap();
        let mut t = Tail::new(p.clone());
        let v = t.read();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["done"], 1);
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "ting\",\"done\":2}}").unwrap();
        let v = t.read();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["done"], 2);
        assert!(t.read().is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
