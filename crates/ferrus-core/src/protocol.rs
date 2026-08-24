//! Wire protocol between the unprivileged GUI and the root helper.
//!
//! Newline-delimited JSON on the helper's stdin/stdout. Long-running
//! operations (image flashing) emit multiple `progress` responses before a
//! terminal `ok`/`error`.

use serde::{Deserialize, Serialize};

/// Partition table style for plans that create their own layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartitionScheme {
    /// GPT — the default; UEFI booting.
    #[default]
    Gpt,
    /// MBR — legacy BIOS / UEFI-CSM targets.
    Mbr,
}

impl PartitionScheme {
    pub fn describe(&self) -> &'static str {
        match self {
            PartitionScheme::Gpt => "GPT",
            PartitionScheme::Mbr => "MBR",
        }
    }
}

/// How hard to scan a device for failing sectors before touching it.
/// Mirrors Rufus's "Check device for bad blocks" combo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BadBlocks {
    /// One read-only pass.
    Fast,
    /// Destructive write-pattern pass (device is erased anyway).
    Thorough,
}

/// Rufus-style Windows user-experience options, injected as an answer file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinOptions {
    /// Add LabConfig bypasses (TPM/SecureBoot/RAM/CPU/storage checks).
    #[serde(default)]
    pub hw_bypass: bool,
    /// Hide the online-account OOBE screens so a local account works.
    #[serde(default)]
    pub no_online_account: bool,
    /// Disable BitLocker automatic device encryption at first logon.
    #[serde(default)]
    pub no_bitlocker: bool,
}

impl WinOptions {
    pub fn any(&self) -> bool {
        self.hw_bypass || self.no_online_account || self.no_bitlocker
    }
}

fn default_true() -> bool {
    true
}

/// How a bootable USB stick is laid out for a given image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlashPlan {
    /// Sector-copy the image verbatim — every Linux hybrid ISO. Optionally
    /// followed by a `persistence_mb` ext4 partition (Debian/Ubuntu live);
    /// `persistence_label` comes from image detection (`casper-rw` /
    /// `persistence`).
    RawDd {
        image: String,
        verify: bool,
        #[serde(default)]
        persistence_mb: u64,
        #[serde(default)]
        persistence_label: Option<String>,
    },
    /// One partition holding the ISO tree; oversized WIM/ESD split into
    /// 3.8 GiB `.swm` parts when requested.
    WinFat32 {
        image: String,
        split_wim: bool,
        #[serde(default)]
        scheme: PartitionScheme,
        #[serde(default)]
        options: WinOptions,
    },
    /// uefi:ntfs boot partition (FAT) + NTFS partition with the full ISO
    /// tree — for images whose files exceed FAT32 limits.
    WinUefiNtfs {
        image: String,
        #[serde(default)]
        scheme: PartitionScheme,
        #[serde(default)]
        options: WinOptions,
    },
    /// Windows To Go: bootable Windows from the stick itself. ESP (FAT32)
    /// carries `bootmgfw.efi` + a generated BCD; the NTFS partition gets a
    /// full `wimlib`-applied Windows tree with an optional answer file.
    /// `wim_index` selects the image inside a multi-edition install.wim/esd
    /// (`/index:N`, default 1). `persist_mib` appends an extra storage
    /// partition behind the Windows one.
    WinToGo {
        image: String,
        /// 1-based index into the WIM; 0 = auto (first).
        #[serde(default)]
        wim_index: u32,
        /// Extra data partition size in MiB; 0 = none.
        #[serde(default)]
        persist_mib: u64,
        #[serde(default)]
        scheme: PartitionScheme,
        #[serde(default)]
        options: WinOptions,
    },
/// Windows To Go from VHDX partition: ESP (FAT32) holds `bootmgfw.efi` + BCD
/// referencing a dedicated raw VHDX partition. The VHDX partition contains
/// the full Windows installation (applied via `wimlib`). `vhdx_size_mib`
/// sets the VHDX capacity (default 64 GiB or image size rounded up).
WinToGoVhdx {
    image: String,
    #[serde(default)]
    wim_index: u32,
    /// VHDX size in MiB; 0 = auto (image size + 25% headroom, min 64 GiB).
    #[serde(default)]
    vhdx_size_mib: u64,
    /// Extra data partition size in MiB; 0 = none.
    #[serde(default)]
    persist_mib: u64,
    #[serde(default)]
    scheme: PartitionScheme,
    #[serde(default)]
    options: WinOptions,
},
    /// Non-hybrid Linux ISO: Rufus-style extraction. The ISO tree is copied
    /// onto a single FAT32 partition instead of being sector-copied, and
    /// SYSLINUX is installed for BIOS booting (UEFI machines boot from the
    /// image's own `EFI/` tree). Used when an ISO carries no MBR boot code.
    IsoExtract {
        image: String,
        #[serde(default)]
        scheme: PartitionScheme,
        /// Install SYSLINUX bootsector + MBR (BIOS boot path).
        #[serde(default = "default_true")]
        syslinux_bios: bool,
    },
    /// No image: just partition + format the device (Rufus "Non bootable").
    FormatDevice {
        #[serde(default)]
        scheme: PartitionScheme,
        /// FAT32 | NTFS | exFAT | UDF | ext2 | ext3 | ext4
        fs: String,
        label: String,
        /// Requested cluster size in bytes; None = filesystem default.
        cluster_bytes: Option<u64>,
        /// MBR only: start the first partition at sector 63 for very old
        /// BIOSes that choke on aligned starts.
        old_bios_align: bool,
    },
}

impl FlashPlan {
    /// Short human-readable name for status lines.
    pub fn describe(&self) -> String {
        match self {
            FlashPlan::RawDd { persistence_mb, .. } => {
                if *persistence_mb > 0 {
                    format!("raw DD image + {persistence_mb} MiB persistence")
                } else {
                    "raw DD image".into()
                }
            }
            FlashPlan::WinFat32 {
                split_wim: true,
                scheme,
                ..
            } => format!("Windows · FAT32 + WIM split ({})", scheme.describe()),
            FlashPlan::WinFat32 { split_wim: false, scheme, .. } => {
                format!("Windows · FAT32 ({})", scheme.describe())
            }
            FlashPlan::WinUefiNtfs { scheme, .. } => {
                format!("Windows · UEFI:NTFS ({})", scheme.describe())
            }
            FlashPlan::WinToGo {
                wim_index,
                scheme,
                persist_mib,
                ..
            } => {
                let mut s = if *wim_index > 0 {
                    format!(
                        "Windows To Go · edition {} ({})",
                        wim_index,
                        scheme.describe()
                    )
                } else {
                    format!("Windows To Go ({})", scheme.describe())
                };
                if *persist_mib > 0 {
                    s.push_str(&format!(", +{persist_mib} MiB storage"));
                }
                s
            }
            FlashPlan::IsoExtract { scheme, syslinux_bios, .. } => {
                if *syslinux_bios {
                    format!("Linux · extracted + SYSLINUX ({})", scheme.describe())
                } else {
                    format!("Linux · extracted ({})", scheme.describe())
                }
            }
            FlashPlan::WinToGoVhdx {
                wim_index,
                scheme,
                vhdx_size_mib,
                ..
            } => {
                let mut s = if *wim_index > 0 {
                    format!(
                        "Windows To Go (VHDX) · edition {} ({})",
                        wim_index,
                        scheme.describe()
                    )
                } else {
                    format!("Windows To Go (VHDX) ({})", scheme.describe())
                };
                if *vhdx_size_mib > 0 {
                    s.push_str(&format!(", VHDX {vhdx_size_mib} MiB"));
                }
                s
            }
            FlashPlan::FormatDevice { scheme, fs, .. } => {
                format!("No bootable · {fs} ({})", scheme.describe())
            }
        }
    }

    pub fn image_path(&self) -> Option<&str> {
        match self {
            FlashPlan::RawDd { image, .. }
            | FlashPlan::WinFat32 { image, .. }
            | FlashPlan::WinUefiNtfs { image, .. }
            | FlashPlan::IsoExtract { image, .. }
            | FlashPlan::WinToGo { image, .. }
            | FlashPlan::WinToGoVhdx { image, .. } => Some(image),
            FlashPlan::FormatDevice { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "args", rename_all = "snake_case")]
pub enum Request {
    /// Liveness check.
    Ping,
    /// Ask the helper for its version.
    Version,
    /// Loop-mount an image read-only and return an ImageManifest JSON
    /// (ferrus_core::iso::ImageManifest) in the `ok` payload.
    ProbeImage { image: String },
    /// Open a block device exclusively (O_EXCL), holding it until release.
    AcquireDevice { device: String },
    /// Close a previously acquired device.
    ReleaseDevice { device: String },
    /// Begin writing `image` to an already-acquired `device` in a background
    /// job. Replies `accepted` immediately; progress follows as separate
    /// responses until a terminal `ok`/`error`.
    WriteImage {
        device: String,
        image: String,
        verify: bool,
    },
    /// Execute a full flashing plan against an already-acquired device.
    /// Same acknowledgement/progress/terminal semantics as write_image.
    ApplyPlan {
        device: String,
        plan: FlashPlan,
        /// Optional pre-flight sector scan (Rufus's bad-block check).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bad_blocks: Option<BadBlocks>,
    },
    /// Ask the active job to stop at the next chunk boundary.
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "data", rename_all = "snake_case")]
pub enum Response {
    /// Success; payload carries optional textual data (e.g. version string).
    Ok(Option<String>),
    Error(String),
    /// Acknowledgement of a cancel request; a terminal `error` ("cancelled
    /// by user") still follows once the job has actually stopped.
    Cancelled,
    /// Acknowledgement that a job was accepted and started; progress
    /// responses and a terminal `ok`/`error` follow.
    Accepted,
    Progress {
        done: u64,
        total: u64,
        verifying: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_requests() {
        for req in [
            Request::Ping,
            Request::Version,
            Request::ProbeImage {
                image: "/tmp/x.iso".into(),
            },
            Request::AcquireDevice {
                device: "/dev/sda".into(),
            },
            Request::ReleaseDevice {
                device: "/dev/sda".into(),
            },
            Request::WriteImage {
                device: "/dev/sda".into(),
                image: "/tmp/x.iso".into(),
                verify: true,
            },
            Request::ApplyPlan {
                device: "/dev/sda".into(),
                plan: FlashPlan::WinFat32 {
                    image: "/tmp/win.iso".into(),
                    split_wim: true,
                    scheme: PartitionScheme::Gpt,
                    options: WinOptions::default(),
                },
                bad_blocks: Some(BadBlocks::Fast),
            },
            Request::ApplyPlan {
                device: "/dev/sda".into(),
                plan: FlashPlan::WinToGo {
                    image: "/tmp/win.iso".into(),
                    wim_index: 2,
                    persist_mib: 0,
                    scheme: PartitionScheme::Gpt,
                    options: WinOptions::default(),
                },
                bad_blocks: Some(BadBlocks::Fast),
            },
            Request::Cancel,
        ] {
            let json = serde_json::to_string(&req).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);
        }
    }

    #[test]
    fn iso_extract_roundtrip_and_defaults() {
        let full = FlashPlan::IsoExtract {
            image: "/x.iso".into(),
            scheme: PartitionScheme::Mbr,
            syslinux_bios: true,
        };
        let json = serde_json::to_string(&full).unwrap();
        assert!(json.contains("\"kind\":\"iso_extract\""), "{json}");
        assert_eq!(serde_json::from_str::<FlashPlan>(&json).unwrap(), full);
        assert_eq!(
            full.describe(),
            "Linux · extracted + SYSLINUX (MBR)"
        );

        // Legacy JSON without the flag keeps BIOS install enabled.
        let legacy = r#"{"kind":"iso_extract","image":"/x.iso"}"#;
        let p: FlashPlan = serde_json::from_str(legacy).unwrap();
        match &p {
            FlashPlan::IsoExtract { syslinux_bios, .. } => assert!(*syslinux_bios),
            other => panic!("wrong plan {other:?}"),
        }
        assert_eq!(p.describe(), "Linux · extracted + SYSLINUX (GPT)");
    }

    #[test]
    fn wtg_vhdx_roundtrip() {
        let full = FlashPlan::WinToGoVhdx {
            image: "/win.iso".into(),
            wim_index: 1,
            vhdx_size_mib: 65536,
            persist_mib: 0,
            scheme: PartitionScheme::Gpt,
            options: WinOptions::default(),
        };
        let json = serde_json::to_string(&full).unwrap();
        assert!(json.contains("\"kind\":\"win_to_go_vhdx\""), "{json}");
        assert_eq!(serde_json::from_str::<FlashPlan>(&json).unwrap(), full);
        assert_eq!(
            full.describe(),
            "Windows To Go (VHDX) · edition 1 (GPT), VHDX 65536 MiB"
        );
    }

    #[test]
    fn roundtrip_responses() {
        for progress in [
            Response::Progress {
                done: 123,
                total: 456,
                verifying: true,
                phase: None,
            },
            Response::Progress {
                done: 1,
                total: 2,
                verifying: false,
                phase: Some("copying files".into()),
            },
        ] {
            let json = serde_json::to_string(&progress).unwrap();
            assert!(json.contains("\"status\":\"progress\""));
            assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), progress);
        }

        let resp = Response::Ok(Some("pong".into()));
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), resp);

        assert_eq!(
            serde_json::to_string(&Response::Cancelled).unwrap(),
            "{\"status\":\"cancelled\"}"
        );
        assert_eq!(
            serde_json::to_string(&Response::Accepted).unwrap(),
            "{\"status\":\"accepted\"}"
        );
    }

    #[test]
    fn flash_plan_wire_format() {
        let p = FlashPlan::WinUefiNtfs {
            image: "/tmp/w.iso".into(),
            scheme: PartitionScheme::Mbr,
            options: WinOptions {
                hw_bypass: true,
                ..Default::default()
            },
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"kind\":\"win_uefi_ntfs\""), "{json}");
        assert!(json.contains("\"scheme\":\"mbr\""), "{json}");
        assert_eq!(serde_json::from_str::<FlashPlan>(&json).unwrap(), p);

        // Legacy JSON without a scheme field still parses (defaults to GPT).
        let legacy = r#"{"kind":"win_fat32","image":"/x","split_wim":false}"#;
        let plan = serde_json::from_str::<FlashPlan>(legacy).unwrap();
        assert_eq!(
            plan,
            FlashPlan::WinFat32 {
                image: "/x".into(),
                split_wim: false,
                scheme: PartitionScheme::Gpt,
                options: WinOptions::default()
            }
        );

        // RawDd with persistence round-trips and describes itself.
        let raw = FlashPlan::RawDd {
            image: "/x".into(),
            verify: false,
            persistence_mb: 4096,
            persistence_label: Some("casper-rw".into()),
        };
        assert_eq!(
            serde_json::from_str::<FlashPlan>(&serde_json::to_string(&raw).unwrap()).unwrap(),
            raw
        );
        assert_eq!(raw.describe(), "raw DD image + 4096 MiB persistence");

        // WinToGo: wim_index/persist_mib default to 0 for legacy JSON.
        let legacy_wtg = r#"{"kind":"win_to_go","image":"/x"}"#;
        let wtg = FlashPlan::WinToGo {
            image: "/x".into(),
            wim_index: 0,
            persist_mib: 0,
            scheme: PartitionScheme::Gpt,
            options: WinOptions::default(),
        };
        assert_eq!(serde_json::from_str::<FlashPlan>(legacy_wtg).unwrap(), wtg);
        assert_eq!(wtg.describe(), "Windows To Go (GPT)");
        let picked = FlashPlan::WinToGo {
            image: "/x".into(),
            wim_index: 3,
            persist_mib: 8192,
            scheme: PartitionScheme::Gpt,
            options: WinOptions::default(),
        };
        assert_eq!(
            picked.describe(),
            "Windows To Go · edition 3 (GPT), +8192 MiB storage"
        );

        assert_eq!(
            describe_of(legacy),
            "Windows · FAT32 (GPT)"
        );

        assert_eq!(
            FlashPlan::FormatDevice {
                scheme: PartitionScheme::Mbr,
                fs: "FAT32".into(),
                label: "USBDRIVE".into(),
                cluster_bytes: None,
                old_bios_align: true,
            }
            .describe(),
            "No bootable · FAT32 (MBR)"
        );
    }

    fn describe_of(json: &str) -> String {
        serde_json::from_str::<FlashPlan>(json).unwrap().describe()
    }
}
