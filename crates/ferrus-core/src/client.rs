//! Client-side driver for ferrus-helper: spawn, request, stream progress.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use anyhow::Context;
use crate::iso::{self, ImageManifest};
use crate::protocol::{BadBlocks, FlashPlan, Request, Response};

/// Events delivered to the caller while a flash job runs.
#[derive(Debug, Clone)]
pub enum ClientEvent {
    Progress {
        done: u64,
        total: u64,
        verifying: bool,
        phase: Option<String>,
    },
    Done(Result<String, String>),
}

/// Handle kept by the UI to cancel a running job.
pub struct FlashHandle {
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    cancel_flag: Arc<AtomicBool>,
}

impl FlashHandle {
    /// True after a local cancel has been requested.
    pub fn is_cancel_requested(&self) -> bool {
        self.cancel_flag.load(Ordering::Relaxed)
    }

    /// Ask helper to stop at the next chunk boundary.
    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.stdin.lock() {
            if let Some(s) = guard.as_mut() {
                let req = serde_json::to_string(&Request::Cancel).unwrap_or_default();
                let _ = writeln!(s, "{req}");
                let _ = s.flush();
            }
        }
    }
}

fn helper_cmd(helper: &Path) -> Command {
    let mut cmd = Command::new(helper);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0);
    cmd
}

fn spawn_helper(helper: &Path) -> anyhow::Result<Child> {
    // Privileged launch: as root run directly; otherwise route through
    // pkexec so a polkit policy can govern the elevation prompt. Set
    // FERRUS_NO_PKEXEC=1 for unprivileged dev runs that skip elevation.
    if unsafe { libc::geteuid() } != 0 && std::env::var_os("FERRUS_NO_PKEXEC").is_none() {
        match Command::new("pkexec")
            .arg(helper)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No pkexec on this system; fall through to a direct launch.
            }
            Err(e) => return Err(anyhow::Error::new(e).context("launch helper via pkexec")),
        }
    }

    helper_cmd(helper)
        .spawn()
        .with_context(|| format!("launch helper {}", helper.display()))
}

fn write_request(stdin: &mut ChildStdin, req: &Request) -> anyhow::Result<()> {
    let line = serde_json::to_string(req).context("encode request")?;
    writeln!(stdin, "{line}").context("write request")?;
    stdin.flush().context("flush request")
}

fn read_response(reader: &mut BufReader<std::process::ChildStdout>) -> anyhow::Result<Response> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).context("read response")?;
    if n == 0 {
        anyhow::bail!("helper closed the connection unexpectedly");
    }
    serde_json::from_str::<Response>(line.trim()).context("decode response")
}

fn expect_ok(resp: Response, what: &str) -> Result<Option<String>, String> {
    match resp {
        Response::Ok(data) => Ok(data),
        other => Err(format!("{what} failed: {other:?}")),
    }
}

/// Locate the helper binary: `FERRUS_HELPER_PATH` env var wins, otherwise
/// look next to the current executable (dev layout / same-prefix install).
pub fn resolve_helper_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FERRUS_HELPER_PATH") {
        return Some(PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?;
    let sibling = exe.parent()?.join("ferrus-helper");
    sibling.exists().then_some(sibling)
}

// ------------------------------------------------------------------ probing

/// Loop-mount `image` in a throwaway privileged session and classify it.
pub fn probe_image(helper: &Path, image: &Path) -> anyhow::Result<ImageManifest> {
    let mut child = spawn_helper(helper)?;
    let mut sin = child.stdin.take().context("no stdin")?;
    let mut sout = BufReader::new(child.stdout.take().context("no stdout")?);

    let result = (|| {
        write_request(
            &mut sin,
            &Request::ProbeImage {
                image: image.to_string_lossy().into_owned(),
            },
        )?;
        // Close stdin so the helper exits after answering.
        drop(sin);
        match read_response(&mut sout)? {
            Response::Ok(Some(json)) => {
                serde_json::from_str::<ImageManifest>(&json)
                    .context("decode image manifest")
            }
            Response::Ok(None) => anyhow::bail!("helper returned empty manifest"),
            Response::Error(e) => anyhow::bail!("{e}"),
            other => anyhow::bail!("unexpected probe response: {other:?}"),
        }
    })();

    let _ = child.kill();
    let _ = child.wait();
    result
}

/// Probe an image and pick the flashing plan Rufus would pick.
pub fn decide_plan(helper: &Path, image: &Path) -> anyhow::Result<(FlashPlan, ImageManifest)> {
    let manifest = probe_image(helper, image)?;
    let plan = iso::choose_plan(&manifest, image);
    Ok((plan, manifest))
}

// ------------------------------------------------------------------ flashing

enum Op {
    Write { image: String, verify: bool },
    Apply(FlashPlan, Option<BadBlocks>),
}

fn spawn_session(
    helper: &Path,
    device: &str,
    op: Op,
) -> anyhow::Result<(mpsc::Receiver<ClientEvent>, FlashHandle)> {
    let mut child = spawn_helper(helper)?;

    let sin = child.stdin.take().context("no stdin")?;
    let mut sout = BufReader::new(child.stdout.take().context("no stdout")?);

    let (tx, rx) = mpsc::channel::<ClientEvent>();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let shared_stdin: Arc<Mutex<Option<ChildStdin>>> = Arc::new(Mutex::new(Some(sin)));

    let handle = FlashHandle {
        stdin: Arc::clone(&shared_stdin),
        cancel_flag: Arc::clone(&cancel_flag),
    };

    let device = device.to_string();

    std::thread::spawn(move || {
        let send_req = |req: &Request| -> anyhow::Result<()> {
            let mut guard = shared_stdin.lock().unwrap();
            let s = guard.as_mut().context("helper stdin already closed")?;
            write_request(s, req)
        };
        let mut finish = |tx: mpsc::Sender<ClientEvent>, result: Result<String, String>| {
            let _ = tx.send(ClientEvent::Done(result));
            drop(shared_stdin.lock().unwrap().take());
            let _ = child.kill();
            let _ = child.wait();
        };

        if let Err(e) = send_req(&Request::AcquireDevice {
            device: device.clone(),
        }) {
            finish(tx, Err(format!("{e:#}")));
            return;
        }
        match read_response(&mut sout) {
            Ok(r) => {
                if let Err(msg) = expect_ok(r, "acquire") {
                    finish(tx, Err(msg));
                    return;
                }
            }
            Err(e) => {
                finish(tx, Err(format!("{e:#}")));
                return;
            }
        }

        let req = match op {
            Op::Write { image, verify } => Request::WriteImage {
                device: device.clone(),
                image,
                verify,
            },
            Op::Apply(plan, bad_blocks) => Request::ApplyPlan {
                device: device.clone(),
                plan,
                bad_blocks,
            },
        };
        if let Err(e) = send_req(&req) {
            finish(tx, Err(format!("{e:#}")));
            return;
        }

        loop {
            match read_response(&mut sout) {
                Ok(Response::Progress {
                    done,
                    total,
                    verifying,
                    phase,
                }) => {
                    let _ = tx.send(ClientEvent::Progress {
                        done,
                        total,
                        verifying,
                        phase,
                    });
                }
                Ok(Response::Cancelled) => {}
                Ok(Response::Accepted) => {}
                Ok(Response::Ok(data)) => {
                    let _ = send_req(&Request::ReleaseDevice {
                        device: device.clone(),
                    });
                    finish(tx, Ok(data.unwrap_or_else(|| "done".into())));
                    return;
                }
                Ok(Response::Error(e)) => {
                    let _ = send_req(&Request::ReleaseDevice {
                        device: device.clone(),
                    });
                    finish(tx, Err(e));
                    return;
                }
                Err(e) => {
                    finish(tx, Err(format!("{e:#}")));
                    return;
                }
            }
        }
    });

    Ok((rx, handle))
}

/// Flash with an explicit plan (raw DD, a Windows layout, or format-only),
/// optionally preceded by a bad-block sector scan.
pub fn spawn_flash_plan(
    helper: &Path,
    device: &str,
    plan: FlashPlan,
    bad_blocks: Option<BadBlocks>,
) -> anyhow::Result<(mpsc::Receiver<ClientEvent>, FlashHandle)> {
    spawn_session(helper, device, Op::Apply(plan, bad_blocks))
}

/// Plain sector-copy flash of a hybrid ISO.
pub fn spawn_flash(
    helper: &Path,
    device: &str,
    image: &str,
    verify: bool,
) -> anyhow::Result<(mpsc::Receiver<ClientEvent>, FlashHandle)> {
    spawn_session(
        helper,
        device,
        Op::Write {
            image: image.to_string(),
            verify,
        },
    )
}
