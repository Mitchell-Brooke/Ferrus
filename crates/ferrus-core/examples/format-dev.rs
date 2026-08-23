//! Headless formatter for testing: partitions + formats a device through the
//! real helper protocol (Rufus's "Non bootable" path).
//!
//! Usage:
//!   format-dev <device> <fs> <label> [gpt|mbr] [cluster_bytes] [badblocks:fast|thorough] [align63]
//!
//! Requires FERRUS_HELPER_PATH and, for loop devices, FERRUS_ALLOW_LOOP=1.
use ferrus_core::client::{self, ClientEvent};
use ferrus_core::protocol::{BadBlocks, FlashPlan, PartitionScheme};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 3 {
        eprintln!(
            "usage: format-dev <device> <fs> <label> [gpt|mbr] [cluster_bytes] [fast|thorough] [align63]"
        );
        std::process::exit(2);
    }
    let device = &a[0];
    let fs = a[1].clone();
    let label = a[2].clone();

    let mut scheme = PartitionScheme::Gpt;
    let mut cluster_bytes = None;
    let mut bad_blocks = None;
    let mut old_bios_align = false;
    for extra in &a[3..] {
        match extra.as_str() {
            "gpt" => scheme = PartitionScheme::Gpt,
            "mbr" => scheme = PartitionScheme::Mbr,
            "fast" => bad_blocks = Some(BadBlocks::Fast),
            "thorough" => bad_blocks = Some(BadBlocks::Thorough),
            "align63" => old_bios_align = true,
            other => match other.parse::<u64>() {
                Ok(b) => cluster_bytes = Some(b),
                Err(_) => {
                    eprintln!("unknown option '{other}'");
                    std::process::exit(2);
                }
            },
        }
    }

    let plan = FlashPlan::FormatDevice {
        scheme,
        fs,
        label,
        cluster_bytes,
        old_bios_align,
    };

    let helper = client::resolve_helper_path().unwrap_or_else(|| {
        eprintln!("helper not found; set FERRUS_HELPER_PATH");
        std::process::exit(2);
    });

    let (rx, handle) = match client::spawn_flash_plan(&helper, device, plan, bad_blocks) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("spawn failed: {e:#}");
            std::process::exit(1);
        }
    };

    let mut last_line = String::new();
    for ev in rx {
        match ev {
            ClientEvent::Progress { done, total, verifying, phase } => {
                let line = match done
                    .checked_mul(100)
                    .and_then(|pct| pct.checked_div(total))
                {
                    Some(pct) => format!("{pct:>3}%  {done}/{total} {}",
                                        phase.as_deref().unwrap_or("")),
                    None => phase.unwrap_or_else(|| "working".into()),
                };
                if line != last_line && !verifying {
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
