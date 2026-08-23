//! Headless flasher for testing: flashes an image onto a device through the
//! real helper protocol, auto-selecting the plan like the GUI does.
//!
//! Usage:
//!   flash-dev <device> <image> [--no-verify] [--plan raw|fat32|fat32-split|ntfs]
//!             [--persist MiB] [--opt-hw] [--opt-account] [--opt-bitlocker]
//!
//! Requires FERRUS_HELPER_PATH (or helper next to the example binary) and,
//! for loop devices, FERRUS_ALLOW_LOOP=1.
use ferrus_core::client::{self, ClientEvent};
use ferrus_core::protocol::{FlashPlan, WinOptions};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let verify = !args.iter().any(|a| a == "--no-verify");
    args.retain(|a| a != "--no-verify");

    let mut take_flag = |name: &str| -> bool {
        let hit = args.iter().any(|a| a == name);
        args.retain(|a| a != name);
        hit
    };
    let opt_hw = take_flag("--opt-hw");
    let opt_account = take_flag("--opt-account");
    let opt_bitlocker = take_flag("--opt-bitlocker");
    let win_options = WinOptions {
        hw_bypass: opt_hw,
        no_online_account: opt_account,
        no_bitlocker: opt_bitlocker,
    };

    // Edition listing: print the install.wim/esd image names and exit.
    // Must run while take_flag is still the only args borrower.
    if take_flag("--list-editions") {
        let iso = match args.first() {
            Some(p) => std::path::PathBuf::from(p),
            None => {
                eprintln!("usage: flash-dev --list-editions <image.iso>");
                std::process::exit(2);
            }
        };
        match ferrus_core::iso::windows_editions(&iso) {
            list if !list.is_empty() => {
                for (i, name) in list.iter().enumerate() {
                    println!("{}: {name}", i + 1);
                }
                return;
            }
            _ => {
                eprintln!("no edition names found in {}", iso.display());
                std::process::exit(1);
            }
        }
    }

    let mut persistence_mb: u64 = 0;
    if let Some(pos) = args.iter().position(|a| a == "--persist") {
        if pos + 1 < args.len() {
            persistence_mb = args.remove(pos + 1).trim().parse().unwrap_or(0);
            args.remove(pos);
        }
    }

    let mut plan_kind: Option<String> = None;
    if let Some(pos) = args.iter().position(|a| a == "--plan") {
        if pos + 1 < args.len() {
            plan_kind = Some(args.remove(pos + 1));
            args.remove(pos);
        }
    }

    let mut wim_index: u32 = 0;
    if let Some(pos) = args.iter().position(|a| a == "--wim-index") {
        if pos + 1 < args.len() {
            wim_index = args.remove(pos + 1).trim().parse().unwrap_or(0);
            args.remove(pos);
        }
    }

    let mut wtg_persist: u64 = 0;
    if let Some(pos) = args.iter().position(|a| a == "--wtg-persist") {
        if pos + 1 < args.len() {
            wtg_persist = args.remove(pos + 1).trim().parse().unwrap_or(0);
            args.remove(pos);
        }
    }

    let (device, image) = match (args.first(), args.get(1)) {
        (Some(d), Some(i)) => (d.clone(), i.clone()),
        _ => {
            eprintln!(
                "usage: flash-dev <device> <image> [--no-verify] [--plan raw|fat32|fat32-split|ntfs|wtg]\n\
                 \x20             [--wim-index N] [--wtg-persist MiB] [--persist MiB] [--opt-hw]\n\
                 \x20             [--opt-account] [--opt-bitlocker] | --list-editions <image.iso>"
            );
            std::process::exit(2);
        }
    };

    let force_plan = plan_kind.map(|kind| match kind.as_str() {
        "raw" => FlashPlan::RawDd {
            image: image.clone(),
            verify,
            persistence_mb,
            persistence_label: None,
        },
        "fat32" => FlashPlan::WinFat32 {
            image: image.clone(),
            split_wim: false,
            scheme: Default::default(),
            options: win_options,
        },
        "fat32-split" => FlashPlan::WinFat32 {
            image: image.clone(),
            split_wim: true,
            scheme: Default::default(),
            options: win_options,
        },
        "ntfs" => FlashPlan::WinUefiNtfs {
            image: image.clone(),
            scheme: Default::default(),
            options: win_options,
        },
        "wtg" => FlashPlan::WinToGo {
            image: image.clone(),
            wim_index,
            persist_mib: wtg_persist,
            scheme: Default::default(),
            options: win_options,
        },
        other => {
            eprintln!("unknown --plan '{other}' (raw|fat32|fat32-split|ntfs|wtg)");
            std::process::exit(2);
        }
    });

    let helper = client::resolve_helper_path().unwrap_or_else(|| {
        eprintln!("helper not found; set FERRUS_HELPER_PATH");
        std::process::exit(2);
    });

    let plan = match force_plan {
        Some(p) => p,
        None => {
            match client::decide_plan(&helper, std::path::Path::new(&image)) {
                Ok((mut p, m)) => {
                    println!(
                        "probe: windows={} total={} max_file={} oversized_wim={:?} flavor={:?}",
                        m.is_windows, m.total_size, m.max_file_size, m.oversized_wim, m.linux_flavor
                    );
                    // Persistence rides on the auto-selected raw plan.
                    if let FlashPlan::RawDd {
                        persistence_mb: pm,
                        persistence_label,
                        ..
                    } = &mut p
                    {
                        *pm = persistence_mb;
                        *persistence_label = m.linux_flavor.clone();
                    } else if persistence_mb > 0 {
                        eprintln!("warning: --persist ignored (image is not raw-DD)");
                    }
                    println!("plan:  {}", p.describe());
                    p
                }
                Err(e) => {
                    eprintln!("probe failed ({e:#}); falling back to raw DD");
                    FlashPlan::RawDd {
                        image: image.clone(),
                        verify,
                        persistence_mb,
                        persistence_label: None,
                    }
                }
            }
        }
    };

    let helper = client::resolve_helper_path().unwrap_or_else(|| {
        eprintln!("helper not found; set FERRUS_HELPER_PATH");
        std::process::exit(2);
    });

    let (rx, handle) = match client::spawn_flash_plan(&helper, &device, plan, None) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("spawn failed: {e:#}");
            std::process::exit(1);
        }
    };

    let mut last_line = String::new();
    for ev in rx {
        match ev {
            ClientEvent::Progress {
                done,
                total,
                verifying,
                phase,
            } => {
                let line = match done
                    .checked_mul(100)
                    .and_then(|pct| pct.checked_div(total))
                {
                    Some(pct) => format!(
                        "{pct:>3}%  {done}/{total} {}{}",
                        phase.as_deref().unwrap_or(""),
                        if verifying { " verifying" } else { "" }
                    ),
                    None => phase.as_deref().unwrap_or("working").to_string(),
                };
                if line != last_line {
                    last_line = line.clone();
                    println!("{line}");
                }
            }
            ClientEvent::Done(Ok(msg)) => {
                println!("SUCCESS: {msg}");
                return;
            }
            ClientEvent::Done(Err(e)) => {
                eprintln!("FAILED: {e}");
                if handle.is_cancel_requested() {
                    std::process::exit(130);
                }
                std::process::exit(1);
            }
        }
    }
}
