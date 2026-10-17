//! OS process handling for the Hy-Memory sidecar. Outcomes are fed back into
//! `model::step` by the engine; nothing here decides a phase.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use super::model::{python_candidates, sidecar_env, RuntimeConfig, SidecarFailure};

/// First import compiles bytecode for chromadb, kuzu, and openai; that can be slow.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(60);
const TERMINATE_GRACE: Duration = Duration::from_secs(3);
const STDERR_LINES: usize = 40;
const STDERR_TAIL_CHARS: usize = 1_500;
const STDERR_DRAIN: Duration = Duration::from_millis(500);
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct SidecarProcess {
    child: Child,
    stderr: Arc<Mutex<VecDeque<String>>>,
    reader: Option<JoinHandle<()>>,
    pid_file: PathBuf,
}

impl SidecarProcess {
    pub fn try_exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    fn stderr_tail(&self) -> String {
        let lines = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
        let joined = lines.iter().map(String::as_str).collect::<Vec<_>>().join("\n");
        let skip = joined.chars().count().saturating_sub(STDERR_TAIL_CHARS);
        joined.chars().skip(skip).collect()
    }

    /// hy-memory 2.0.3's SIGTERM handler deadlocks (it calls `server.shutdown()` on the
    /// thread running `serve_forever`), so this ends in a kill. The grace still lets
    /// in-flight request threads finish their write.
    pub fn terminate(mut self) {
        terminate_child(&mut self.child);
        let _ = fs::remove_file(&self.pid_file);
    }

    /// For a process that already exited. The traceback is usually the last thing
    /// written, so give the reader a moment to drain the closed pipe.
    pub fn into_exit_report(mut self) -> String {
        if let Some(reader) = self.reader.take() {
            let deadline = Instant::now() + STDERR_DRAIN;
            while !reader.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = fs::remove_file(&self.pid_file);
        self.stderr_tail()
    }
}

/// Reap a stale sidecar, find an interpreter that can import `hy_memory`, pick a
/// free loopback port, and start `python -m hy_memory.server` in the store dir.
pub fn spawn(cfg: &RuntimeConfig, store_dir: &Path, pid_file: &Path) -> Result<(SidecarProcess, u16), SidecarFailure> {
    reap_stale(pid_file);
    let python = find_python(cfg)?;
    let start_error = |e: std::io::Error| SidecarFailure::ExitedOnStart { code: None, stderr_tail: e.to_string() };

    fs::create_dir_all(store_dir.join("hy")).map_err(start_error)?;
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .map_err(start_error)?;

    let plan = sidecar_env(cfg, port, store_dir);
    let mut cmd = python_command(&python, plan.remove_prefixes, std::env::vars_os().map(|(k, _)| k));
    cmd.args(["-m", "hy_memory.server", "--host", "127.0.0.1", "--port", &port.to_string()])
        .envs(plan.set)
        .current_dir(&plan.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(start_error)?;

    let stderr = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_LINES)));
    let reader = child.stderr.take().map(|pipe| {
        let sink = stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let mut lines = sink.lock().unwrap_or_else(|e| e.into_inner());
                if lines.len() == STDERR_LINES {
                    lines.pop_front();
                }
                lines.push_back(line);
            }
        })
    });
    if let Some(dir) = pid_file.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(pid_file, child.id().to_string());
    Ok((SidecarProcess { child, stderr, reader, pid_file: pid_file.to_path_buf() }, port))
}

enum Preflight {
    Imports,
    Missing,
    NotFound,
    TimedOut,
}

fn find_python(cfg: &RuntimeConfig) -> Result<Vec<String>, SidecarFailure> {
    let mut tried = Vec::new();
    let mut without_module = None;
    for argv in python_candidates(&cfg.python, cfg!(windows)) {
        let label = argv.join(" ");
        tried.push(label.clone());
        match preflight(&argv) {
            Preflight::Imports => return Ok(argv),
            Preflight::Missing => {
                without_module.get_or_insert(label);
            }
            Preflight::NotFound => {}
            Preflight::TimedOut => {
                return Err(SidecarFailure::HealthTimeout { after_ms: PREFLIGHT_TIMEOUT.as_millis() as u64 })
            }
        }
    }
    Err(match without_module {
        Some(python) => SidecarFailure::ModuleMissing { python },
        None => SidecarFailure::PythonNotFound { tried },
    })
}

fn preflight(argv: &[String]) -> Preflight {
    let mut cmd = python_command(argv, &[], std::iter::empty());
    cmd.args(["-c", "import hy_memory"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return Preflight::NotFound;
    };
    let deadline = Instant::now() + PREFLIGHT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Preflight::Imports,
            Ok(Some(_)) => return Preflight::Missing,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Preflight::TimedOut;
            }
        }
    }
}

/// `inherited` is the parent's variable names; any matching a stripped prefix is
/// removed so Hy-Memory cannot fall back to the user's shell configuration.
fn python_command(argv: &[String], strip: &[&str], inherited: impl Iterator<Item = OsString>) -> Command {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    for key in inherited {
        let upper = key.to_string_lossy().to_ascii_uppercase();
        if strip.iter().any(|prefix| upper.starts_with(prefix)) {
            cmd.env_remove(&key);
        }
    }
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

fn terminate_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill").args(["-TERM", &child.id().to_string()]).status();
        let deadline = Instant::now() + TERMINATE_GRACE;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// A crashed AnyChat can leave a sidecar holding Kuzu's single-writer lock on the
/// store dir. Kill it only if the pid is alive and its command line is ours, so a
/// reused pid is never touched. Windows has no cheap command-line lookup; there the
/// stale file is only removed.
fn reap_stale(pid_file: &Path) {
    let pid = fs::read_to_string(pid_file).ok().and_then(|s| s.trim().parse::<u32>().ok());
    #[cfg(unix)]
    if let Some(pid) = pid {
        reap_unix(pid);
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = fs::remove_file(pid_file);
}

#[cfg(unix)]
fn reap_unix(pid: u32) {
    let is_ours = || {
        Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .is_ok_and(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).contains("hy_memory.server"))
    };
    if !is_ours() {
        return;
    }
    let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
    let deadline = Instant::now() + TERMINATE_GRACE;
    while Instant::now() < deadline {
        if !is_ours() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::memory::model::{desired_from, Desired, RawMemorySettings, INHERITED_ENV_TO_STRIP};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "anychat-sidecar-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_python(dir: &Path, body: &str) -> String {
        let path = dir.join("fake-python");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn config(python: &str) -> RuntimeConfig {
        let raw: RawMemorySettings = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "generation": { "baseUrl": "http://127.0.0.1:9/v1", "apiKey": "sk-test", "model": "gpt-test" },
            "embedding": { "source": { "kind": "reuse-generation" }, "model": "emb-test", "dims": 8 },
            "pythonCommand": python,
        }))
        .unwrap();
        match desired_from(&raw, &[]) {
            Desired::On { config, .. } => config,
            other => panic!("expected a valid config, got {other:?}"),
        }
    }

    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn python_command_strips_only_prefixed_inherited_vars() {
        let inherited = ["MEMORY_LLM_API_KEY", "openai_api_key", "HY_MEMORY_SERVER_PORT", "PATH", "HOME"]
            .into_iter()
            .map(OsString::from);
        let cmd = python_command(&["python3".to_string(), "-X".to_string()], INHERITED_ENV_TO_STRIP, inherited);
        let mut removed: Vec<String> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        removed.sort();
        assert_eq!(removed, vec!["HY_MEMORY_SERVER_PORT", "MEMORY_LLM_API_KEY", "openai_api_key"]);
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), vec!["-X"]);
    }

    #[test]
    fn find_python_reports_missing_interpreter_and_missing_module() {
        let dir = temp_dir("find");
        let absent = dir.join("no-such-python").to_string_lossy().into_owned();
        assert_eq!(
            find_python(&config(&absent)).unwrap_err(),
            SidecarFailure::PythonNotFound { tried: vec![absent] }
        );
        let without_module = fake_python(&dir, "exit 1");
        assert_eq!(
            find_python(&config(&without_module)).unwrap_err(),
            SidecarFailure::ModuleMissing { python: without_module }
        );
        let with_module = fake_python(&dir, "[ \"$1\" = \"-c\" ] && [ \"$2\" = \"import hy_memory\" ] && exit 0; exit 1");
        assert_eq!(find_python(&config(&with_module)).unwrap(), vec![with_module]);
    }

    #[test]
    fn spawn_runs_the_server_in_the_store_dir_with_a_clean_env() {
        let dir = temp_dir("spawn");
        let store = dir.join("store");
        let pid_file = dir.join("sidecar.pid");
        let python = fake_python(&dir, "[ \"$1\" = \"-c\" ] && exit 0\necho \"$@\" > args.txt\nenv > env.txt\nexec sleep 30");

        let (mut process, port) = spawn(&config(&python), &store, &pid_file).unwrap();
        wait_for("env dump", || fs::read_to_string(store.join("env.txt")).is_ok_and(|e| e.contains("MEMORY_MODE")));
        let args = fs::read_to_string(store.join("args.txt")).unwrap();
        assert_eq!(args.trim(), format!("-m hy_memory.server --host 127.0.0.1 --port {port}"));
        let env = fs::read_to_string(store.join("env.txt")).unwrap();
        assert!(env.contains(&format!("MEMORY_DATA_DIR={}", store.join("hy").display())));
        assert!(env.contains(&format!("HY_MEMORY_SERVER_PORT={port}")));
        assert!(env.contains("MEMORY_LLM_API_KEY=sk-test"));
        assert!(store.join("hy").is_dir());
        assert_eq!(fs::read_to_string(&pid_file).unwrap(), process.child.id().to_string());
        assert!(process.try_exited().is_none());

        process.terminate();
        assert!(!pid_file.exists());
    }

    #[test]
    fn exit_status_and_stderr_tail_are_captured() {
        let dir = temp_dir("exit");
        let python = fake_python(&dir, "[ \"$1\" = \"-c\" ] && exit 0\necho 'Traceback: kuzu lock held' >&2\nexit 3");
        let (mut process, _) = spawn(&config(&python), &dir.join("store"), &dir.join("sidecar.pid")).unwrap();
        wait_for("exit", || process.try_exited().is_some());
        assert_eq!(process.try_exited().and_then(|s| s.code()), Some(3));
        assert!(process.into_exit_report().contains("Traceback: kuzu lock held"));
        assert!(!dir.join("sidecar.pid").exists());
    }

    #[test]
    fn reap_stale_leaves_foreign_pids_alone_and_kills_our_server() {
        let dir = temp_dir("reap");
        let pid_file = dir.join("sidecar.pid");

        let mut foreign = Command::new("sleep").arg("30").spawn().unwrap();
        fs::write(&pid_file, foreign.id().to_string()).unwrap();
        reap_stale(&pid_file);
        assert!(foreign.try_wait().unwrap().is_none());
        assert!(!pid_file.exists());
        foreign.kill().unwrap();
        foreign.wait().unwrap();

        let mut ours = Command::new("bash")
            .args(["-c", "exec -a 'python -m hy_memory.server --port 1' sleep 30"])
            .spawn()
            .unwrap();
        wait_for("renamed process", || {
            Command::new("ps")
                .args(["-p", &ours.id().to_string(), "-o", "command="])
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("hy_memory.server"))
        });
        fs::write(&pid_file, ours.id().to_string()).unwrap();
        reap_stale(&pid_file);
        wait_for("reaped", || ours.try_wait().unwrap().is_some());
        assert!(!pid_file.exists());
    }
}
