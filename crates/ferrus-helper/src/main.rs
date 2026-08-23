//! ferrus-helper: privileged worker for Ferrus.
//!
//! Speaks newline-delimited JSON (ferrus_core::protocol) on stdin/stdout.
//! Launched unattended via `pkexec` by the GUI; every request is validated
//! before any privileged action is taken. Long-running writes execute on a
//! background thread and stream `progress` responses.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use ferrus_core::protocol::{Request, Response};
use ferrus_core::write::{self, WriteOutcome};

mod ops;

type SharedOut = Arc<Mutex<io::Stdout>>;
type JobSlot = Arc<Mutex<Option<Arc<AtomicBool>>>>;

fn send(out: &SharedOut, resp: &Response) {
    let mut o = out.lock().unwrap();
    if serde_json::to_writer(&mut *o, resp).is_ok() {
        let _ = o.write_all(b"\n");
        let _ = o.flush();
    }
}

fn validate_device_path(raw: &str) -> anyhow::Result<PathBuf> {
    let path = Path::new(raw);
    let canon = path
        .canonicalize()
        .with_context(|| format!("cannot resolve {raw}"))?;

    let meta = canon
        .metadata()
        .with_context(|| format!("cannot stat {raw}"))?;
    if !meta.file_type().is_block_device() {
        anyhow::bail!("{raw} is not a block device");
    }

    // Must live directly under /dev and be a whole disk (no digit suffix).
    let parent = canon
        .parent()
        .and_then(|p| p.to_str())
        .context("bad device path")?;
    if parent != "/dev" {
        anyhow::bail!("refusing non-/dev device {raw}");
    }
    let name = canon
        .file_name()
        .and_then(|n| n.to_str())
        .context("bad device name")?;

    let allow_loop = std::env::var_os("FERRUS_ALLOW_LOOP").as_deref() == Some(std::ffi::OsStr::new("1"));
    let is_loop = name.starts_with("loop");
    if is_loop && !allow_loop {
        anyhow::bail!("refusing loop device {raw} (set FERRUS_ALLOW_LOOP=1 to enable loop-device testing)");
    }
    if name.chars().last().is_some_and(|c| c.is_ascii_digit()) && !(is_loop && allow_loop) {
        anyhow::bail!("refusing partition node {raw}");
    }
    Ok(canon)
}

fn validate_image_path(raw: &str) -> anyhow::Result<PathBuf> {
    let canon = Path::new(raw)
        .canonicalize()
        .with_context(|| format!("cannot resolve image {raw}"))?;
    let meta = canon.metadata().with_context(|| format!("cannot stat {raw}"))?;
    if !meta.is_file() {
        anyhow::bail!("{raw} is not a regular file");
    }
    Ok(canon)
}

fn handle(req: Request, state: &mut State) -> Response {
    match req {
        Request::Ping => Response::Ok(Some("pong".into())),
        Request::Version => Response::Ok(Some(env!("CARGO_PKG_VERSION").into())),

        Request::ProbeImage { image } => {
            let path = match validate_image_path(&image) {
                Ok(p) => p,
                Err(e) => return Response::Error(format!("{e:#}")),
            };
            match ops::probe_manifest(&path) {
                Ok(manifest) => match serde_json::to_string(&manifest) {
                    Ok(json) => Response::Ok(Some(json)),
                    Err(e) => Response::Error(format!("encode manifest: {e}")),
                },
                Err(e) => Response::Error(format!("{e:#}")),
            }
        }

        Request::AcquireDevice { device } => {
            let node = match validate_device_path(&device) {
                Ok(p) => p,
                Err(e) => return Response::Error(format!("{e:#}")),
            };
            if state.handles.contains_key(&device) {
                return Response::Ok(Some("acquired".into())); // already ours
            }
            match OpenOptions::new()
                .read(true)
                .write(true)
                .open(&node)
            {
                Ok(file) => {
                    // NOTE: no O_EXCL / flock here. An O_EXCL whole-disk
                    // claim blocks exclusive opens of partition nodes
                    // (breaking mkfs during plan execution) and block-dev
                    // flock is unreliable across kernels (WSL2). Mutual
                    // exclusion is best-effort, as with most Linux media
                    // writers; sfdisk/mount still fail naturally if the
                    // disk is genuinely busy.
                    state.handles.insert(device, file);
                    Response::Ok(Some("acquired".into()))
                }
                Err(e) => Response::Error(format!(
                    "cannot acquire {}: {e} (busy or read-only?)",
                    node.display()
                )),
            }
        }

        Request::ReleaseDevice { device } => {
            if state.job_slot.lock().unwrap().is_some() {
                return Response::Error("a write job is active; cancel it first".into());
            }
            match state.handles.remove(&device) {
                Some(_) => Response::Ok(Some("released".into())),
                None => Response::Error(format!("device {device} was not acquired")),
            }
        }

        Request::WriteImage {
            device,
            image,
            verify,
        } => {
            if state.job_slot.lock().unwrap().is_some() {
                return Response::Error("another write job is already running".into());
            }
            let Some(file) = state.handles.get(&device) else {
                return Response::Error(format!(
                    "device {device} not acquired; send acquire_device first"
                ));
            };
            let src = match validate_image_path(&image) {
                Ok(p) => p,
                Err(e) => return Response::Error(format!("{e:#}")),
            };
            let mut dst = match file.try_clone() {
                Ok(f) => f,
                Err(e) => return Response::Error(format!("cannot clone device handle: {e}")),
            };

            let flag = Arc::new(AtomicBool::new(false));
            *state.job_slot.lock().unwrap() = Some(flag.clone());
            let out = state.out.clone();
            let job_slot = state.job_slot.clone();

            std::thread::spawn(move || {
                let result = write::write_image_to_device(
                    &src,
                    &mut dst,
                    verify,
                    &|p| {
                        send(
                            &out,
                            &Response::Progress {
                                done: p.bytes_done,
                                total: p.total,
                                verifying: p.verifying,
                                phase: None,
                            },
                        );
                    },
                    &flag,
                );
                job_slot.lock().unwrap().take();
                match result {
                    Ok(WriteOutcome::Completed) => {
                        let msg = if verify {
                            format!("image written and verified to {device}")
                        } else {
                            format!("image written to {device}")
                        };
                        send(&out, &Response::Ok(Some(msg)));
                    }
                    Ok(WriteOutcome::Cancelled) => {
                        send(&out, &Response::Error("cancelled by user".into()));
                    }
                    Err(e) => send(&out, &Response::Error(format!("{e:#}"))),
                }
            });

            Response::Accepted
        }

        Request::ApplyPlan {
            device,
            plan,
            bad_blocks,
        } => {
            if state.job_slot.lock().unwrap().is_some() {
                return Response::Error("another write job is already running".into());
            }
            // Validate the plan's image up front for fast feedback.
            if let Some(img) = plan.image_path() {
                if let Err(e) = validate_image_path(img) {
                    return Response::Error(format!("{e:#}"));
                }
            }
            let Some(file) = state.handles.get(&device) else {
                return Response::Error(format!(
                    "device {device} not acquired; send acquire_device first"
                ));
            };
            let node = match validate_device_path(&device) {
                Ok(p) => p,
                Err(e) => return Response::Error(format!("{e:#}")),
            };
            let mut dst = match file.try_clone() {
                Ok(f) => f,
                Err(e) => return Response::Error(format!("cannot clone device handle: {e}")),
            };

            let flag = Arc::new(AtomicBool::new(false));
            *state.job_slot.lock().unwrap() = Some(flag.clone());
            let out = state.out.clone();
            let job_slot = state.job_slot.clone();
            let pids = state.pids.clone();

            std::thread::spawn(move || {
                let result = ops::execute_plan(
                    &node,
                    &mut dst,
                    &plan,
                    bad_blocks,
                    &flag,
                    &pids,
                    &|resp| send(&out, &resp),
                );
                pids.lock().unwrap().clear();
                job_slot.lock().unwrap().take();
                match result {
                    Ok(msg) => send(&out, &Response::Ok(Some(msg))),
                    Err(e) => {
                        let msg = format!("{e:#}");
                        if msg == "cancelled" {
                            send(&out, &Response::Error("cancelled by user".into()));
                        } else {
                            send(&out, &Response::Error(msg));
                        }
                    }
                }
            });

            Response::Accepted
        }

        Request::Cancel => match state.job_slot.lock().unwrap().as_ref() {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                for pid in state.pids.lock().unwrap().drain(..) {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGTERM);
                    }
                }
                Response::Cancelled
            }
            None => Response::Error("no active job".into()),
        },
    }
}

struct State {
    handles: HashMap<String, File>,
    out: SharedOut,
    job_slot: JobSlot,
    pids: ops::Pids,
}

fn main() -> anyhow::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("warning: ferrus-helper not running as root; privileged ops will fail");
    }

    let stdin = io::stdin();
    let stdout: SharedOut = Arc::new(Mutex::new(io::stdout()));
    let mut state = State {
        handles: HashMap::new(),
        out: stdout.clone(),
        job_slot: Arc::new(Mutex::new(None)),
        pids: Arc::new(Mutex::new(Vec::new())),
    };

    for line in stdin.lock().lines() {
        let line = line.context("reading request")?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle(req, &mut state),
            Err(e) => Response::Error(format!("malformed request: {e}")),
        };
        send(&stdout, &response);
    }

    // Client went away: stop any running job so the helper cannot outlive
    // its parent and keep a device locked.
    if let Some(flag) = state.job_slot.lock().unwrap().take() {
        flag.store(true, Ordering::Relaxed);
    }
    for pid in state.pids.lock().unwrap().drain(..) {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    Ok(())
}
