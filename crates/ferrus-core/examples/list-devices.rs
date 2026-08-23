//! Prints block devices Ferrus can flash to.
//! cargo run -p ferrus-core --example list-devices
use ferrus_core::device::list_block_devices;

fn main() {
    match list_block_devices() {
        Ok(devices) => {
            println!("found {} candidate disk(s)", devices.len());
            for d in devices {
                println!(
                    "  {:<10} {:<28} size={:<9} removable={:<5} usb={:<5} ro={}",
                    d.name,
                    d.display_name(),
                    d.size_string(),
                    d.removable,
                    d.is_usb(),
                    d.read_only
                );
            }
        }
        Err(e) => {
            eprintln!("enumeration failed: {e:#}");
            std::process::exit(1);
        }
    }
}
