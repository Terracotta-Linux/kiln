//! Spawning, the wall clock, and the shim log.

use crate::spec::SandboxSpec;
use crate::{Error, Outcome, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Write the shim wrappers into `dir`, which the caller binds at
/// `spec::SHIM_DIR`. Returns the host path of the log the shims append to, so
/// the parent can read back what was called.
pub fn materialize_shims(spec: &SandboxSpec, dir: &Path) -> Result<Option<PathBuf>> {
    if spec.shims.is_empty() {
        return Ok(None);
    }
    let io = |doing: &'static str, path: &Path| {
        let path = path.to_path_buf();
        move |source| Error::Io {
            doing,
            path: path.clone(),
            source,
        }
    };
    std::fs::create_dir_all(dir).map_err(io("creating the shim directory", dir))?;

    for shim in &spec.shims {
        let at = dir.join(&shim.name);
        std::fs::write(&at, shim.script()).map_err(io("writing a shim", &at))?;
        set_executable(&at)?;
    }

    // The log lives beside the shims rather than inside the image: it is a fact
    // about the build, and anything written into the staging root would have to
    // be cleaned up again before the commit.
    let log = dir.join("calls.log");
    std::fs::write(&log, "").map_err(io("creating the shim log", &log))?;
    Ok(Some(log))
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|source| {
        Error::Io {
            doing: "making a shim executable",
            path: path.to_path_buf(),
            source,
        }
    })
}

/// Run `argv`, enforcing `spec.limits.wall` in the parent so that every backend
/// honours a timeout whether or not it has one of its own.
pub fn spawn(spec: &SandboxSpec, argv: &[String], shim_log: Option<&Path>) -> Result<Outcome> {
    let pretty = spec.command.join(" ");
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        // The sandbox's own environment is cleared by the backend's flags; this
        // clears what the *backend process* inherits, so nothing leaks in
        // through a bwrap or nspawn variable either.
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::Io {
            doing: "starting the sandbox",
            path: PathBuf::from(&argv[0]),
            source,
        })?;

    // Both pipes are drained on their own threads, *while* the child runs.
    //
    // Waiting first and reading afterwards deadlocks the moment a command
    // writes more than a pipe buffer — 64 KiB — because the child blocks on a
    // full pipe that nobody is emptying and the parent blocks waiting for a
    // child that will never exit. It survives every small command and then
    // hangs forever on the first big one: `lsinitrd` over a 22 MB initramfs
    // lists thousands of files, and the build simply stops, with no output and
    // no error.
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let reading_out = std::thread::spawn(move || drain(out_pipe));
    let reading_err = std::thread::spawn(move || drain(err_pipe));

    let waited = match spec.limits.wall {
        None => Ok(child.wait().ok()),
        Some(limit) => wait_with_timeout(&mut child, limit, &pretty),
    };

    // Joined either way. On a timeout the child has already been killed, so the
    // readers see EOF and finish; leaving them running would keep the pipe ends
    // — and the threads — alive past the call.
    let stdout = reading_out.join().unwrap_or_default();
    let stderr = reading_err.join().unwrap_or_default();
    let status = waited?;
    let shimmed = shim_log
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default();

    // *the full log is always written*. Here rather than in the caller,
    // because a failing run is turned into `Error::Failed` carrying forty lines
    // and the rest is gone by the time anyone else could write it.
    if let Some(path) = &spec.log {
        write_log(path, &pretty, &stdout, &stderr);
    }

    Ok(Outcome {
        status: status.and_then(|s| s.code()).unwrap_or(-1),
        stdout,
        stderr,
        shimmed,
    })
}

/// Write a run's combined output where the log path promised it would be.
///
/// Best-effort: a build that succeeded must not be turned into a failure
/// because `/var/lib/kiln/logs` was not writable, and a build that failed has
/// already said so through its exit status. The path is still printed, so a
/// missing file is visible rather than silent.
fn write_log(path: &Path, command: &str, stdout: &str, stderr: &str) {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Appended, not truncated: the two phases of a build share one log
    // — the whole story of one `build_key` in one file — and the fetch that
    // preceded a failing build is usually part of the explanation.
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(file, "$ {command}");
    for (stream, text) in [("stdout", stdout), ("stderr", stderr)] {
        if !text.trim().is_empty() {
            let _ = writeln!(file, "--- {stream} ---\n{}", text.trim_end());
        }
    }
    let _ = writeln!(file);
}

/// Read a pipe to end, or produce nothing. Errors are not reported: a command
/// whose output could not be read has already told us what matters through its
/// exit status, and a read error here would mask it.
fn drain(pipe: Option<impl Read>) -> String {
    let mut text = String::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_string(&mut text);
    }
    text
}

/// Poll rather than block, so the timeout needs no signal handling and no
/// extra thread. A build step is measured in seconds at least; a 50 ms poll
/// costs nothing and keeps this readable.
fn wait_with_timeout(
    child: &mut std::process::Child,
    limit: Duration,
    pretty: &str,
) -> Result<Option<std::process::ExitStatus>> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) => {}
            Err(source) => {
                return Err(Error::Io {
                    doing: "waiting for the sandbox",
                    path: PathBuf::new(),
                    source,
                })
            }
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::TimedOut {
                command: pretty.to_string(),
                after: limit,
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The last `n` non-empty lines. A failing build step's useful output is at the
/// end; puts the figure at forty.
pub fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
