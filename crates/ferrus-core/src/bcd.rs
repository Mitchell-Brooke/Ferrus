//! Native BCD (Boot Configuration Data) generation.
//!
//! A BCD store is a Windows registry hive (`regf`) holding boot objects
//! under `\Objects\{guid}\Elements\<element-id>`. This module writes a
//! minimal hive from scratch — no external tools — so the helper can
//! produce a bootable `\EFI\Microsoft\Boot\BCD` on Linux.
//!
//! Every structure was verified byte-for-byte against genuine stores made
//! by `bcdedit.exe` (Win7 media store, Win8+ PXE store, freshly minted GPT
//! store) with systemd's parser (`src/boot/bcd.c`) as cross-check:
//!
//! * base block v1.3, checksum = XOR of dwords `[0x00, 0x1FC)`
//! * bins split into 4096-byte pages, each starting with an `hbin` header
//! * cell sizes are negative multiples of 8 covering `4 + payload`
//! * key nodes `nk`, value records `vk`, fast-leaf subkey lists `lf`,
//!   one shared security record `sk`
//! * strings REG_SZ / REG_MULTI_SZ UTF-16LE NUL-terminated; small integers
//!   inline in the vk data-offset field (size word bit 31 set); larger
//!   payloads in dedicated data cells

/// GUID of the well-known Windows Boot Manager object.
pub const BOOTMGR_GUID: &str = "{9dea862c-5cdd-4e70-acc1-f32b344d4795}";

const EPOCH_DIFF_SECS: u64 = 11_644_473_600;

// Self-relative security descriptor copied verbatim from the shared sk
// record of a bcdedit-created store (owner BUILTIN\Administrators, group
// SYSTEM, protected DACL).
const SECURITY_DESCRIPTOR: &[u8] = &[
    0x01, 0x00, 0x04, 0x80, 0x48, 0x00, 0x00, 0x00, 0x58, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0x02, 0x00, 0x34, 0x00,
    0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x00, 0x19, 0x00, 0x06, 0x00,
    0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x20, 0x00, 0x00, 0x00,
    0x20, 0x02, 0x00, 0x00, 0x00, 0x00, 0x14, 0x00, 0x3f, 0x00, 0x0f, 0x00,
    0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x12, 0x00, 0x00, 0x00,
    0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x20, 0x00, 0x00, 0x00,
    0x20, 0x02, 0x00, 0x00, 0x01, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05,
    0x15, 0x00, 0x00, 0x00, 0xd1, 0xf8, 0x51, 0xc0, 0x17, 0xa3, 0x1a, 0x0d,
    0x1b, 0x9c, 0x9a, 0x56, 0xe9, 0x03, 0x00, 0x00,
];

#[derive(Debug)]
pub enum BcdError {
    /// Entry GUID is not `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}`.
    BadGuid(String),
}

impl std::fmt::Display for BcdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BcdError::BadGuid(g) => write!(f, "invalid BCD entry GUID: {g}"),
        }
    }
}

impl std::error::Error for BcdError {}

/// Identifies a partition the way bootmgfw resolves it: by GPT partition
/// GUID plus containing-disk GUID. Both are raw 16 bytes exactly as they
/// appear in the GPT on disk (mixed-endian; do not reformat).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PartitionRef {
    pub partition_guid: [u8; 16],
    pub disk_guid: [u8; 16],
}

enum Val {
    Sz(String),
    MultiSz(Vec<String>),
    Dword(u32),
    Binary(Vec<u8>),
}

struct Key {
    name: String,
    values: Vec<(String, Val)>,
    children: Vec<Key>,
}

impl Key {
    /// Preserves the given case (the hive root is "NewStoreRoot").
    fn new(name: &str) -> Self {
        Key {
            name: name.to_string(),
            values: Vec::new(),
            children: Vec::new(),
        }
    }

    fn child(&mut self, name: &str) -> &mut Key {
        let lower = name.to_ascii_lowercase();
        debug_assert!(
            !self.children.iter().any(|c| c.name == lower),
            "duplicate child {name}"
        );
        self.children.push(Key {
            name: lower,
            values: Vec::new(),
            children: Vec::new(),
        });
        self.children.last_mut().unwrap()
    }

    fn set(&mut self, name: &str, val: Val) {
        self.values.push((name.to_string(), val));
    }
}

/// Hive cell allocator. Cells live in "bin space" — offsets relative to the
/// end of the base block — and never cross a page boundary.
struct Hive {
    pages: Vec<Box<[u8; 4096]>>,
    next_free: usize,
    /// Every allocated nk record, for deferred security-key patching.
    nks: Vec<u32>,
}

impl Hive {
    fn new() -> Self {
        Hive {
            pages: vec![Box::new([0u8; 4096])],
            next_free: 0x20, // skip the first hbin header
            nks: Vec::new(),
        }
    }

    /// Allocate a cell holding `payload`; returns its bin-space index.
    fn alloc(&mut self, payload: &[u8]) -> u32 {
        let need = ((payload.len() + 4 + 7) & !7).max(8);
        let page_base = self.next_free & !0xFFF;
        if self.next_free + need > page_base + 0x1000 {
            // Pad the tail of this page with a free cell, start a new page.
            let pad = page_base + 0x1000 - self.next_free;
            if pad >= 4 {
                let off = self.next_free % 0x1000;
                self.pages.last_mut().unwrap()[off..off + 4]
                    .copy_from_slice(&(pad as u32).to_le_bytes());
            }
            self.pages.push(Box::new([0u8; 4096]));
            self.next_free = page_base + 0x1000 + 0x20;
        }
        let idx = self.next_free;
        let off = idx % 0x1000;
        let page = self.pages.last_mut().unwrap();
        page[off..off + 4].copy_from_slice(&((need as i32).wrapping_neg()).to_le_bytes());
        page[off + 4..off + 4 + payload.len()].copy_from_slice(payload);
        self.next_free += need;
        idx as u32
    }

    fn patch_u32(&mut self, cell_idx: u32, rec_off: usize, value: u32) {
        let base = cell_idx as usize + 4 + rec_off;
        self.pages[base / 0x1000][base % 0x1000..base % 0x1000 + 4]
            .copy_from_slice(&value.to_le_bytes());
    }

    /// Serialise into a complete regf file (base block + hbins).
    fn finish(mut self, filetime: u64, root_cell: u32) -> Vec<u8> {
        let n = self.pages.len();
        for (i, page) in self.pages.iter_mut().enumerate() {
            page[0..4].copy_from_slice(b"hbin");
            page[4..8].copy_from_slice(&((i * 0x1000) as u32).to_le_bytes());
            // bcdedit writes the stride even on the last page; readers stop
            // at end-of-file, so mirror that behaviour exactly.
            page[8..12].copy_from_slice(&0x1000u32.to_le_bytes());
        }

        let mut out = vec![0u8; 4096];
        out[0..4].copy_from_slice(b"regf");
        out[4..8].copy_from_slice(&1u32.to_le_bytes());
        out[8..12].copy_from_slice(&1u32.to_le_bytes());
        out[12..20].copy_from_slice(&filetime.to_le_bytes());
        out[20..24].copy_from_slice(&1u32.to_le_bytes()); // major
        out[24..28].copy_from_slice(&3u32.to_le_bytes()); // minor — bootmgfw requires 1.3
        out[28..32].copy_from_slice(&0u32.to_le_bytes()); // type
        out[32..36].copy_from_slice(&1u32.to_le_bytes()); // format
        out[36..40].copy_from_slice(&root_cell.to_le_bytes());
        out[40..44].copy_from_slice(&((n * 0x1000) as u32).to_le_bytes());
        out[44..48].copy_from_slice(&1u32.to_le_bytes()); // cluster
        out[48..51].copy_from_slice(b"BCD");

        for page in &self.pages {
            out.extend_from_slice(page.as_slice());
        }

        let mut sum: u32 = 0;
        for i in (0..0x1FC).step_by(4) {
            sum ^= u32::from_le_bytes(out[i..i + 4].try_into().unwrap());
        }
        out[0x1FC..0x200].copy_from_slice(&sum.to_le_bytes());
        out
    }
}

fn utf16_nul(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
    v.extend_from_slice(&[0, 0]);
    v
}

fn multi_sz(items: &[String]) -> Vec<u8> {
    let mut v = Vec::new();
    for s in items {
        v.extend(utf16_nul(s));
    }
    v.extend_from_slice(&[0, 0]); // final terminator
    v
}

fn filetime_now() -> u64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (secs + EPOCH_DIFF_SECS) * 10_000_000
}

fn val_data(val: &Val) -> (u32, Vec<u8>, Option<u32>) {
    match val {
        Val::Sz(s) => (1, utf16_nul(s), None),
        Val::MultiSz(items) => (7, multi_sz(items), None),
        Val::Dword(v) => (4, Vec::new(), Some(*v)),
        Val::Binary(b) => (3, b.clone(), None),
    }
}

fn val_data_len(val: &Val) -> usize {
    match val {
        Val::Dword(_) => 4,
        Val::Binary(b) => b.len(),
        Val::Sz(s) => s.encode_utf16().count() * 2 + 2,
        Val::MultiSz(items) => items
            .iter()
            .map(|s| s.encode_utf16().count() * 2 + 2)
            .sum::<usize>()
            + 2,
    }
}

/// Serialise `key` and its subtree; returns the nk cell index.
fn write_key(hive: &mut Hive, sk_idx: u32, key: &Key, is_root: bool) -> u32 {
    let mut sorted_children: Vec<&Key> = key.children.iter().collect();
    sorted_children.sort_by(|a, b| a.name.cmp(&b.name));

    let child_cells: Vec<(Vec<u8>, u32)> = sorted_children
        .iter()
        .map(|c| {
            let idx = write_key(hive, sk_idx, c, false);
            let nb = c.name.as_bytes();
            let mut hint = [0u8; 4];
            hint[..nb.len().min(4)].copy_from_slice(&nb[..nb.len().min(4)]);
            (hint.to_vec(), idx)
        })
        .collect();

    let sublist: i32 = if child_cells.is_empty() {
        -1
    } else {
        let mut payload = Vec::with_capacity(4 + 8 * child_cells.len());
        payload.extend_from_slice(b"lf");
        payload.extend_from_slice(&(child_cells.len() as u16).to_le_bytes());
        for (hint, idx) in &child_cells {
            payload.extend_from_slice(&idx.to_le_bytes());
            payload.extend_from_slice(hint);
        }
        hive.alloc(&payload) as i32
    };

    let mut max_val_name = 0usize;
    let mut max_val_data = 0usize;
    let mut vk_cells = Vec::with_capacity(key.values.len());
    for (vname, val) in &key.values {
        max_val_name = max_val_name.max(vname.len());
        max_val_data = max_val_data.max(val_data_len(val));
        let (raw_type, data, inline) = val_data(val);
        let data_len = if inline.is_some() { 4 } else { data.len() };
        let data_cell = inline.unwrap_or_else(|| hive.alloc(&data));
        let drawsize = if inline.is_some() {
            0x8000_0000u32 | data_len as u32
        } else {
            data_len as u32
        };

        let mut rec = Vec::with_capacity(20 + vname.len());
        rec.extend_from_slice(b"vk");
        rec.extend_from_slice(&(vname.len() as u16).to_le_bytes());
        rec.extend_from_slice(&drawsize.to_le_bytes());
        rec.extend_from_slice(&data_cell.to_le_bytes());
        rec.extend_from_slice(&raw_type.to_le_bytes());
        rec.extend_from_slice(&1u16.to_le_bytes()); // ASCII name flag
        rec.extend_from_slice(&0u16.to_le_bytes()); // spare
        rec.extend_from_slice(vname.as_bytes());
        vk_cells.push(hive.alloc(&rec));
    }

    let vlist: i32 = if vk_cells.is_empty() {
        -1
    } else {
        let mut payload = Vec::with_capacity(4 * vk_cells.len());
        for c in &vk_cells {
            payload.extend_from_slice(&c.to_le_bytes());
        }
        hive.alloc(&payload) as i32
    };

    let max_sub_name = key
        .children
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0);
    let flags: u16 = if is_root { 0x2C } else { 0x20 };
    let mut rec = Vec::with_capacity(76 + key.name.len());
    rec.extend_from_slice(b"nk");
    rec.extend_from_slice(&flags.to_le_bytes());
    rec.extend_from_slice(&filetime_now().to_le_bytes());
    rec.extend_from_slice(&0u32.to_le_bytes()); // spare/access
    rec.extend_from_slice(&0u32.to_le_bytes()); // parent — patched below
    rec.extend_from_slice(&(child_cells.len() as u32).to_le_bytes());
    rec.extend_from_slice(&0u32.to_le_bytes()); // volatile count
    rec.extend_from_slice(&sublist.to_le_bytes());
    rec.extend_from_slice(&(-1i32).to_le_bytes()); // volatile list
    rec.extend_from_slice(&(vk_cells.len() as u32).to_le_bytes());
    rec.extend_from_slice(&vlist.to_le_bytes());
    rec.extend_from_slice(&sk_idx.to_le_bytes());
    rec.extend_from_slice(&(-1i32).to_le_bytes()); // class
    rec.extend_from_slice(&(max_sub_name as u32).to_le_bytes());
    rec.extend_from_slice(&0u32.to_le_bytes()); // max class len
    rec.extend_from_slice(&(max_val_name as u32).to_le_bytes());
    rec.extend_from_slice(&(max_val_data as u32).to_le_bytes());
    rec.extend_from_slice(&0u32.to_le_bytes()); // work var
    rec.extend_from_slice(&(key.name.len() as u16).to_le_bytes());
    rec.extend_from_slice(&0u16.to_le_bytes()); // class len
    rec.extend_from_slice(key.name.as_bytes());

    let my_idx = hive.alloc(&rec);
    hive.nks.push(my_idx);
    // Children were allocated before us; patch their nk parent (+0x10).
    for (_, idx) in &child_cells {
        hive.patch_u32(*idx, 0x10, my_idx);
    }
    my_idx
}

/// Device-element blob for a GPT partition reference (88 bytes), reverse-
/// engineered from a store created by `bcdedit /set ... partition=C:` on a
/// GPT disk:
/// `zeros[16] | type=6 | 0 | opts_size=72 | 0 | part_guid | 0u64 |
///  disk_guid | zeros[16]`.
pub fn gpt_partition_device(part: &PartitionRef) -> Vec<u8> {
    let mut b = Vec::with_capacity(88);
    b.extend_from_slice(&[0u8; 16]); // no options object
    b.extend_from_slice(&6u32.to_le_bytes()); // PartitionDevice
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&72u32.to_le_bytes()); // additional options size
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&part.partition_guid);
    b.extend_from_slice(&0u64.to_le_bytes());
    b.extend_from_slice(&part.disk_guid);
    b.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(b.len(), 88);
    b
}

/// VHD-device element for WinToGo-from-VHDX (type 8).
///
/// Produces the 192 + 2*(path_chars+1) byte blob reverse-engineered from
/// real `bcdedit /set device vhd=[C:]\path.vhdx` stores. The layout is:
///   +0x00  16B reserved
///   +0x10  u64=8  (device type VHD)
///   +0x18  u64 = blob_len - 16
///   +0x20  u32=0, u32=locate_id (0x12000002 device / 0x22000002 osdevice)
///   +0x28  u64=30
///   +0x30  6B zero
///   +0x36  u16 = 146 + 2*path_chars
///   +0x38  6B zero
///   +0x3E  u16=6 (parent partition device type)
///   +0x40  10B zero
///   +0x4E  u16 = 106 + 2*path_chars
///   +0x50  22B zero
///   +0x66  u16=5
///   +0x68  u16=0, u16=1
///   +0x6C  u16=0
///   +0x6E  u16 = 86 + 2*path_chars
///   +0x70  u16=0
///   +0x72  u16=5
///   +0x74  u16=6
///   +0x76  8B zero
///   +0x7E  u16=72
///   +0x80  6B zero
///   +0x86  disk_guid (16B)
///   +0x96  8B zero
///   +0x9E  part_guid (16B)
///   +0xAE  16B zero
///   +0xBE  UTF-16LE `path` + NUL (no extra padding)
///
/// `path` must include the leading backslash, e.g. `r"\windows.vhdx"`.
pub fn vhd_device(
    path: &str,
    disk_guid: &[u8; 16],
    part_guid: &[u8; 16],
    for_osdevice: bool,
) -> Vec<u8> {
    let path_chars = path.chars().count();
    let locate_id: u32 = if for_osdevice { 0x2200_0002 } else { 0x1200_0002 };
    let blob_len = 190 + 2 * (path_chars + 1);
    let mut b = Vec::with_capacity(blob_len);
    b.extend_from_slice(&[0u8; 16]);                           // +0x00 reserved
    b.extend_from_slice(&8u64.to_le_bytes());                  // +0x10 type=8
    b.extend_from_slice(&((blob_len - 16) as u64).to_le_bytes()); // +0x18 len-16
    b.extend_from_slice(&0u32.to_le_bytes());                  // +0x20 flags
    b.extend_from_slice(&locate_id.to_le_bytes());             // +0x24 locate id
    b.extend_from_slice(&30u64.to_le_bytes());                 // +0x28 constant 30
    b.extend_from_slice(&[0u8; 6]);                            // +0x30 pad
    b.extend_from_slice(&(146 + 2 * path_chars as u16).to_le_bytes()); // +0x36
    b.extend_from_slice(&[0u8; 6]);                            // +0x38 pad
    b.extend_from_slice(&6u16.to_le_bytes());                  // +0x3E parent type=6
    b.extend_from_slice(&[0u8; 14]);                           // +0x40 pad (14 bytes to reach +0x4E)
    b.extend_from_slice(&(106 + 2 * path_chars as u16).to_le_bytes()); // +0x4E
    b.extend_from_slice(&[0u8; 22]);                           // +0x50 pad
    b.extend_from_slice(&5u16.to_le_bytes());                  // +0x66
    b.extend_from_slice(&0u16.to_le_bytes());                  // +0x68
    b.extend_from_slice(&1u16.to_le_bytes());                  // +0x6A
    b.extend_from_slice(&0u16.to_le_bytes());                  // +0x6C
    b.extend_from_slice(&(86 + 2 * path_chars as u16).to_le_bytes());  // +0x6E
    b.extend_from_slice(&0u16.to_le_bytes());                  // +0x70
    b.extend_from_slice(&5u16.to_le_bytes());                  // +0x72
    b.extend_from_slice(&6u16.to_le_bytes());                  // +0x74
    b.extend_from_slice(&[0u8; 8]);                            // +0x76 pad
    b.extend_from_slice(&72u16.to_le_bytes());                 // +0x7E
    b.extend_from_slice(&[0u8; 6]);                            // +0x80 pad
    b.extend_from_slice(disk_guid);                            // +0x86 disk GUID
    b.extend_from_slice(&[0u8; 8]);                            // +0x96 pad
    b.extend_from_slice(part_guid);                            // +0x9E partition GUID
    b.extend_from_slice(&[0u8; 16]);                           // +0xAE pad
    // UTF-16LE path + NUL
    for ch in path.encode_utf16() {
        b.extend_from_slice(&ch.to_le_bytes());
    }
    b.extend_from_slice(&[0u8, 0u8]); // NUL terminator
    debug_assert_eq!(b.len(), blob_len);
    b
}

/// Device element meaning "the volume we were booted from" (bcdedit's
/// `device boot`, type 5); used by Windows install media.
#[allow(dead_code)]
pub fn boot_device() -> Vec<u8> {
    let mut b = vec![0u8; 88];
    b[16..20].copy_from_slice(&5u32.to_le_bytes());
    b[24..28].copy_from_slice(&72u32.to_le_bytes());
    b
}

fn validate_guid_str(s: &str) -> Result<(), BcdError> {
    let hex = |c: char| c.is_ascii_hexdigit();
    let ok = s.len() == 38
        && s.starts_with('{')
        && s.ends_with('}')
        && s[1..37]
            .chars()
            .enumerate()
            .all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { hex(c) });
    if ok {
        Ok(())
    } else {
        Err(BcdError::BadGuid(s.to_string()))
    }
}

/// Build a complete UEFI-boot BCD store for a Windows installation.
///
/// * `entry_guid` — lowercase `{guid}` for the osloader object (any unique
///   UUID works)
/// * `description` — menu title shown by bootmgfw
/// * `esp` — EFI system partition hosting this BCD and `bootmgfw.efi`
/// * `win` — partition carrying the applied `\Windows` tree
/// * `timeout_secs` — boot menu countdown
///
/// The osloader path is fixed to `\Windows\system32\winload.efi`.
pub fn generate_uefi_bcd(
    entry_guid: &str,
    description: &str,
    esp: &PartitionRef,
    win: &PartitionRef,
    timeout_secs: u32,
) -> Result<Vec<u8>, BcdError> {
    validate_guid_str(entry_guid)?;

    let mut root = Key::new("NewStoreRoot");
    {
        let desc = root.child("Description");
        desc.set("KeyName", Val::Sz("BCD00000000".into()));
        desc.set("FirmwareModified", Val::Dword(1));
    }
    let objects = root.child("Objects");

    let bm = objects.child(BOOTMGR_GUID);
    bm.child("Description")
        .set("Type", Val::Dword(0x1010_0002)); // boot manager application
    let elems = bm.child("Elements");
    // Each element lives in its own subkey holding a single "Element" value
    // — the exact layout bcdedit writes and bootmgfw expects.
    elems
        .child("11000001")
        .set("Element", Val::Binary(gpt_partition_device(esp)));
    elems
        .child("12000004")
        .set("Element", Val::Sz("Windows Boot Manager".into()));
    elems.child("24000001").set(
        "Element",
        Val::MultiSz(vec![entry_guid.to_ascii_lowercase()]),
    ); // displayorder
    elems.child("25000004").set(
        "Element",
        Val::Binary((timeout_secs as u64).to_le_bytes().to_vec()),
    ); // timeout (REG_BINARY u64)

    let os = objects.child(entry_guid);
    os.child("Description")
        .set("Type", Val::Dword(0x1020_0003)); // osloader application
    let elems = os.child("Elements");
    elems
        .child("11000001")
        .set("Element", Val::Binary(gpt_partition_device(win))); // device
    elems.child("12000002").set(
        "Element",
        Val::Sz("\\Windows\\system32\\winload.efi".into()),
    ); // path
    elems
        .child("12000004")
        .set("Element", Val::Sz(description.into())); // description
    elems
        .child("21000001")
        .set("Element", Val::Binary(gpt_partition_device(win))); // osdevice
    elems
        .child("22000002")
        .set("Element", Val::Sz("\\Windows".into())); // systemroot

    let mut hive = Hive::new();

    // Root nk first so it occupies cell 0x20 like bcdedit's stores; the
    // security-key index is patched into every record afterwards.
    let root_idx = write_key(&mut hive, 0, &root, true);

    // Shared security record. flink/blink point at itself; refcount covers
    // every key in the tree.
    let n_keys = hive.nks.len();
    let sk_cell = hive.alloc(&{
        let mut p = Vec::new();
        p.extend_from_slice(b"sk");
        p.extend_from_slice(&0u16.to_le_bytes());
        p.extend_from_slice(&[0xff; 4]); // flink/blink patched below
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(&(n_keys as u32).to_le_bytes());
        p.extend_from_slice(&(SECURITY_DESCRIPTOR.len() as u32).to_le_bytes());
        p.extend_from_slice(SECURITY_DESCRIPTOR);
        p
    });
    hive.patch_u32(sk_cell, 0x04, sk_cell);
    hive.patch_u32(sk_cell, 0x08, sk_cell);
    let nk_list = std::mem::take(&mut hive.nks);
    for nk in nk_list {
        hive.patch_u32(nk, 44, sk_cell);
    }

Ok(hive.finish(filetime_now(), root_idx))
}

/// Build a UEFI-boot BCD store for Windows To Go from VHDX.
///
/// Similar to `generate_uefi_bcd` but the `device` and `osdevice` elements
/// reference a VHDX file on `win` partition instead of the partition directly.
///
/// * `entry_guid` — osloader GUID (lowercase `{...}`)
/// * `description` — boot menu title
/// * `esp` — EFI system partition (holds BCD + bootmgfw.efi)
/// * `win` — partition carrying the `\windows.vhdx` file
/// * `vhdx_rel_path` — path to VHDX relative to `win` root, e.g. `r"\windows.vhdx"`
/// * `timeout_secs` — boot menu countdown
pub fn generate_uefi_bcd_vhdx(
    entry_guid: &str,
    description: &str,
    esp: &PartitionRef,
    win: &PartitionRef,
    vhdx_rel_path: &str,
    timeout_secs: u32,
) -> Result<Vec<u8>, BcdError> {
    validate_guid_str(entry_guid)?;

    let mut root = Key::new("NewStoreRoot");
    {
        let desc = root.child("Description");
        desc.set("KeyName", Val::Sz("BCD00000000".into()));
        desc.set("FirmwareModified", Val::Dword(1));
    }
    let objects = root.child("Objects");

    let bm = objects.child(BOOTMGR_GUID);
    bm.child("Description")
        .set("Type", Val::Dword(0x1010_0002));
    let elems = bm.child("Elements");
    elems
        .child("11000001")
        .set("Element", Val::Binary(gpt_partition_device(esp)));
    elems
        .child("12000004")
        .set("Element", Val::Sz("Windows Boot Manager".into()));
    elems.child("24000001").set(
        "Element",
        Val::MultiSz(vec![entry_guid.to_ascii_lowercase()]),
    );
    elems.child("25000004").set(
        "Element",
        Val::Binary((timeout_secs as u64).to_le_bytes().to_vec()),
    );

    let os = objects.child(entry_guid);
    os.child("Description")
        .set("Type", Val::Dword(0x1020_0003));
    let elems = os.child("Elements");
    // VHD device elements (type 8) referencing the VHDX file on `win`
    elems
        .child("11000001")
        .set("Element", Val::Binary(vhd_device(vhdx_rel_path, &win.disk_guid, &win.partition_guid, false)));
    elems.child("12000002").set(
        "Element",
        Val::Sz("\\Windows\\system32\\winload.efi".into()),
    );
    elems
        .child("12000004")
        .set("Element", Val::Sz(description.into()));
    elems
        .child("21000001")
        .set("Element", Val::Binary(vhd_device(vhdx_rel_path, &win.disk_guid, &win.partition_guid, true)));
    elems
        .child("22000002")
        .set("Element", Val::Sz("\\Windows".into()));

    let mut hive = Hive::new();
    let root_idx = write_key(&mut hive, 0, &root, true);

    let n_keys = hive.nks.len();
    let sk_cell = hive.alloc(&{
        let mut p = Vec::new();
        p.extend_from_slice(b"sk");
        p.extend_from_slice(&0u16.to_le_bytes());
        p.extend_from_slice(&[0xff; 4]);
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(&(n_keys as u32).to_le_bytes());
        p.extend_from_slice(&(SECURITY_DESCRIPTOR.len() as u32).to_le_bytes());
        p.extend_from_slice(SECURITY_DESCRIPTOR);
        p
    });
    hive.patch_u32(sk_cell, 0x04, sk_cell);
    hive.patch_u32(sk_cell, 0x08, sk_cell);
    let nk_list = std::mem::take(&mut hive.nks);
    for nk in nk_list {
        hive.patch_u32(nk, 44, sk_cell);
    }

    Ok(hive.finish(filetime_now(), root_idx))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- minimal regf reader (test-side oracle) ------------------------

    /// All accessors derive from `data` alone, so lifetimes stay tied to the
    /// store buffer rather than to any borrowed reader.
    #[derive(Clone, Copy)]
    struct TestKey<'a> {
        data: &'a [u8],
        rec: &'a [u8],
    }

    impl<'a> TestKey<'a> {
        fn hive_root(data: &'a [u8], root_cell: u32) -> TestKey<'a> {
            let probe = TestKey {
                data,
                rec: &data[0..0],
            };
            probe.key(root_cell)
        }
        fn rec_u32(&self, rec_off: usize) -> u32 {
            u32::from_le_bytes(self.rec[rec_off..rec_off + 4].try_into().unwrap())
        }
        fn cell(&self, idx: u32) -> &'a [u8] {
            let base = 4096 + idx as usize;
            let size = i32::from_le_bytes(
                self.data[base..base + 4]
                    .try_into()
                    .unwrap_or_else(|_| panic!("cell {idx:#x} size oob")),
            )
            .unsigned_abs() as usize;
            self.data
                .get(base + 4..base + size)
                .unwrap_or_else(|| panic!("cell {idx:#x} span {size} oob"))
        }
        fn key(&self, idx: u32) -> TestKey<'a> {
            let k = TestKey {
                data: self.data,
                rec: self.cell(idx),
            };
            assert_eq!(&k.rec[0..2], b"nk", "not an nk at {idx}");
            k
        }
        fn name(&self) -> &'a str {
            let len = u16::from_le_bytes(self.rec[72..74].try_into().unwrap()) as usize;
            std::str::from_utf8(&self.rec[76..76 + len]).unwrap()
        }
        fn child(&self, name: &str) -> Option<TestKey<'a>> {
            if self.rec[28..32] == [0xFF; 4] {
                return None;
            }
            let list = self.cell(self.rec_u32(28));
            assert_eq!(&list[0..2], b"lf");
            let count = u16::from_le_bytes(list[2..4].try_into().unwrap());
            for i in 0..count as usize {
                let cell = u32::from_le_bytes(list[4 + i * 8..8 + i * 8].try_into().unwrap());
                let k = self.key(cell);
                if k.name().eq_ignore_ascii_case(name) {
                    return Some(k);
                }
            }
            None
        }
        /// Walk down a path of key names.
        fn path(&self, names: &[&str]) -> TestKey<'a> {
            let mut cur = self.child(names[0]).expect("missing key");
            for n in &names[1..] {
                cur = cur.child(n).expect("missing key");
            }
            cur
        }
        /// Value payload by name; returns (type, bytes).
        fn value(&self, name: &str) -> (u32, Vec<u8>) {
            let nval = u32::from_le_bytes(self.rec[36..40].try_into().unwrap());
            assert!(nval > 0, "no values");
            let vlist = self.cell(self.rec_u32(40));
            for i in 0..nval as usize {
                let vk = u32::from_le_bytes(vlist[i * 4..i * 4 + 4].try_into().unwrap());
                let rec = self.cell(vk);
                assert_eq!(&rec[0..2], b"vk");
                let nlen = u16::from_le_bytes(rec[2..4].try_into().unwrap()) as usize;
                if rec[20..20 + nlen].eq_ignore_ascii_case(name.as_bytes()) {
                    let drawsize = u32::from_le_bytes(rec[4..8].try_into().unwrap());
                    let doff = u32::from_le_bytes(rec[8..12].try_into().unwrap());
                    let rtype = u32::from_le_bytes(rec[12..16].try_into().unwrap());
                    if drawsize & 0x8000_0000 != 0 {
                        let len = (drawsize & 0x7FFF_FFFF) as usize;
                        let mut v = vec![0u8; len];
                        let le = doff.to_le_bytes();
                        v[..le.len()].copy_from_slice(&le);
                        return (rtype, v);
                    }
                    return (
                        rtype,
                        self.cell(doff)[..drawsize as usize].to_vec(),
                    );
                }
            }
            panic!("value {name} not found");
        }
    }

    #[test]
    fn device_blob_gpt_matches_bcdedit_ground_truth() {
        // Captured verbatim from gpt.bcd created by elevated bcdedit on this
        // machine: partition={95a270a5-a91f-4abf-8a71-3cdf9367badc},
        // disk={26512254-9215-4eb8-ba3e-760e...}.
        let part_guid: [u8; 16] = [
            0xa5, 0x70, 0xa2, 0x95, 0x1f, 0xa9, 0xbf, 0x4a, 0x8a, 0x71, 0x3c, 0xdf, 0x93,
            0x67, 0xba, 0xdc,
        ];
        let disk_guid: [u8; 16] = [
            0x54, 0x22, 0x51, 0x26, 0x15, 0x92, 0xb8, 0x4e, 0xba, 0x3e, 0x7a, 0x60, 0xe7,
            0x4a, 0x84, 0xa1,
        ];
        let blob = gpt_partition_device(&PartitionRef {
            partition_guid: part_guid,
            disk_guid,
        });
        let mut expected: Vec<u8> = Vec::new();
        expected.extend_from_slice(&[0u8; 16]); // no options object
        expected.extend_from_slice(&6u32.to_le_bytes()); // PartitionDevice
        expected.extend_from_slice(&0u32.to_le_bytes());
        expected.extend_from_slice(&72u32.to_le_bytes()); // additional options size
        expected.extend_from_slice(&0u32.to_le_bytes());
        expected.extend_from_slice(&part_guid);
        expected.extend_from_slice(&0u64.to_le_bytes());
        expected.extend_from_slice(&disk_guid);
        expected.extend_from_slice(&[0u8; 16]);
        assert_eq!(&blob, &expected);
    }

    #[test]
    fn boot_device_blob_matches_media_store() {
        let b = boot_device();
        assert_eq!(b.len(), 88);
        assert_eq!(&b[16..20], &5u32.to_le_bytes());
        assert_eq!(&b[24..28], &72u32.to_le_bytes());
        assert!(b[28..].iter().all(|&v| v == 0));
    }

    #[test]
    fn generated_store_parses_like_systemd_expects() {
        let esp = PartitionRef {
            partition_guid: [1u8; 16],
            disk_guid: [2u8; 16],
        };
        let win = PartitionRef {
            partition_guid: [3u8; 16],
            disk_guid: [2u8; 16],
        };
        let entry = "{12345678-1234-1234-1234-123456789abc}";
        let store =
            generate_uefi_bcd(entry, "Ferrus Windows", &esp, &win, 9).unwrap();

        // Base block invariants (systemd's get_bcd_title checks these).
        assert_eq!(&store[0..4], b"regf");
        assert_eq!(store[20..24], 1u32.to_le_bytes());
        assert_eq!(store[24..28], 3u32.to_le_bytes());
        assert_eq!(store[28..32], 0u32.to_le_bytes());
        assert_eq!(store[4..8], store[8..12]);
        // Checksum recomputes.
        let mut sum: u32 = 0;
        for i in (0..0x1FC).step_by(4) {
            sum ^= u32::from_le_bytes(store[i..i + 4].try_into().unwrap());
        }
        assert_eq!(&store[0x1FC..0x200], sum.to_le_bytes().as_slice());

        let root_cell = u32::from_le_bytes(store[36..40].try_into().unwrap());
        let root = TestKey::hive_root(&store, root_cell);
        assert_eq!(root.name(), "NewStoreRoot");

        // Root description carries KeyName + FirmwareModified like real stores.
        let desc = root.child("Description").unwrap();
        let (t, v) = desc.value("KeyName");
        assert_eq!(t, 1); // REG_SZ
        assert_eq!(
            String::from_utf16_lossy(&bytemuck_wide(&v)).trim_end_matches('\0'),
            "BCD00000000"
        );

        let objects = root.child("Objects").unwrap();

        // {bootmgr}: type + displayorder + timeout + ESP device.
        let bm = objects.child(BOOTMGR_GUID).unwrap();
        let bm_desc = bm.path(&["Description"]);
        let (t, v) = bm_desc.value("Type");
        assert_eq!(t, 4); // inline REG_DWORD
        assert_eq!(u32::from_le_bytes(v[..4].try_into().unwrap()), 0x1010_0002);

        let elems = bm.path(&["Elements"]);
        let (t, dev) = elems
            .child("11000001")
            .unwrap()
            .value("Element");
        assert_eq!(t, 3); // REG_BINARY
        assert_eq!(dev.len(), 88);
        assert_eq!(&dev[16..20], &6u32.to_le_bytes());
        assert_eq!(&dev[32..48], &[1u8; 16]); // partition guid
        assert_eq!(&dev[56..72], &[2u8; 16]); // disk guid

        let (t, order) = elems
            .child("24000001")
            .unwrap()
            .value("Element");
        assert_eq!(t, 7); // REG_MULTI_SZ
        let order_str = String::from_utf16_lossy(&bytemuck_wide(&order));
        assert!(order_str.contains(entry), "{order_str}");

        let (_, to) = elems
            .child("25000004")
            .unwrap()
            .value("Element");
        assert_eq!(u64::from_le_bytes(to[..8].try_into().unwrap()), 9);

        // osloader entry.
        let os = objects.child(entry).unwrap();
        let os_desc = os.path(&["Description"]);
        let (t, v) = os_desc.value("Type");
        assert_eq!(u32::from_le_bytes(v[..4].try_into().unwrap()), 0x1020_0003);
        assert_eq!(t, 4);

        let os_elems = os.path(&["Elements"]);
        let (_, path) = os_elems
            .child("12000002")
            .unwrap()
            .value("Element");
        assert_eq!(
            String::from_utf16_lossy(&bytemuck_wide(&path)),
            "\\Windows\\system32\\winload.efi\0"
        );
        let (_, sr) = os_elems
            .child("22000002")
            .unwrap()
            .value("Element");
        assert_eq!(
            String::from_utf16_lossy(&bytemuck_wide(&sr)),
            "\\Windows\0"
        );
        let (_, dsc) = os_elems
            .child("12000004")
            .unwrap()
            .value("Element");
        assert_eq!(
            String::from_utf16_lossy(&bytemuck_wide(&dsc)),
            "Ferrus Windows\0"
        );
        let (_, osdev) = os_elems
            .child("21000001")
            .unwrap()
            .value("Element");
        assert_eq!(&osdev[32..48], &[3u8; 16]); // win partition guid

        // Every nk must be reachable and cells well-formed:
        // walk whole tree ensuring lf ordering.
        fn check_sorted(k: &TestKey) {
            if k.rec[28..32] == [0xFF; 4] {
                return;
            }
            let list = k.cell(k.rec_u32(28));
            let count = u16::from_le_bytes(list[2..4].try_into().unwrap());
            let mut prev = String::new();
            for i in 0..count as usize {
                let cell =
                    u32::from_le_bytes(list[4 + i * 8..8 + i * 8].try_into().unwrap());
                let child = k.key(cell);
                let nm = child.name();
                assert!(nm > prev.as_str(), "lf not sorted: {nm} after {prev}");
                prev = nm.to_string();
                check_sorted(&child);
            }
        }
        check_sorted(&root);
    }

    fn bytemuck_wide(v: &[u8]) -> Vec<u16> {
        (0..v.len() / 2)
            .map(|i| u16::from_le_bytes([v[2 * i], v[2 * i + 1]]))
            .collect()
    }

#[test]
    fn bad_guid_rejected() {
        let esp = PartitionRef::default();
        assert!(generate_uefi_bcd("not-a-guid", "x", &esp, &esp, 1).is_err());
        assert!(generate_uefi_bcd("{ZZZZZZZZ-1234-1234-1234-123456789abc}", "x", &esp, &esp, 1).is_err());
    }

    #[test]
    fn vhd_device_blob_matches_ground_truth() {
        // Ground-truth blobs from real bcdedit stores (S/M/L path lengths)
        let disk_guid = [0x95,0xa2,0x70,0xa5,0x1f,0xa9,0xbf,0x4a,0x8a,0x71,0x3c,0xdf,0x93,0x67,0xba,0xdc];
        let part_guid = [0x26,0x51,0x22,0x54,0x15,0x92,0xb8,0x4e,0xba,0x3e,0x7a,0x60,0xe7,0x4a,0x84,0xa1];
        // S: "\w.vhdx" (7 chars) -> 206 bytes
        let s = vhd_device(r"\w.vhdx", &disk_guid, &part_guid, false);
        assert_eq!(s.len(), 206);
        // M: "\ferrus-vhd\windows.vhdx" (24 chars) -> 240 bytes
        let m = vhd_device(r"\ferrus-vhd\windows.vhdx", &disk_guid, &part_guid, false);
        assert_eq!(m.len(), 240);
        // L: "\ferrus-vhd\subdir\windows Ten.vhdx" (35 chars) -> 262 bytes
        let l = vhd_device(r"\ferrus-vhd\subdir\windows Ten.vhdx", &disk_guid, &part_guid, false);
        assert_eq!(l.len(), 262);
        // Device vs osdevice differ only at +0x24 (locate_id)
        let mut dev = vhd_device(r"\windows.vhdx", &disk_guid, &part_guid, false);
        let osdev = vhd_device(r"\windows.vhdx", &disk_guid, &part_guid, true);
        assert_eq!(dev.len(), osdev.len());
        // locate_id at bytes 0x24..0x27: device=0x12000002, osdevice=0x22000002
        assert_eq!(&dev[0x24..0x28], &0x1200_0002u32.to_le_bytes());
        assert_eq!(&osdev[0x24..0x28], &0x2200_0002u32.to_le_bytes());
        // Rest identical
        dev[0x24..0x28].copy_from_slice(&osdev[0x24..0x28]);
        assert_eq!(dev, osdev);
    }
}
