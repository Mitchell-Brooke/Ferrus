//! Fixed VHDX (Virtual Hard Disk v2) writer.
//!
//! Implements a minimal FIXED-type VHDX suitable for Windows To Go.
//! Layout (all offsets 1 MB aligned):
//!   0x000000: File Type Identifier (64 KB slot) — "vhdxfile" + creator
//!   0x010000: Header #1 (4 KB in 64 KB slot) — current, seq=2
//!   0x020000: Header #2 (4 KB in 64 KB slot) — seq=1
//!   0x030000: Region Table #1 (64 KB slot) — 1 entry: Metadata
//!   0x040000: Region Table #2 (64 KB slot) — same
//!   0x100000: Metadata Region (1 MB) — metadata table + 5 items
//!   0x200000: Payload (virtual_size bytes, 1 MB aligned end)
//!
//! No BAT region (fixed disk), no log, no parent locator.
//! CRC-32C (Castagnoli) for headers and region tables.

use anyhow::{bail, Result};
use std::io::{Cursor, Write};
use uuid::Uuid;

const MB: u64 = 1_048_576;
const KB64: u64 = 65_536;
const KB4: u64 = 4_096;

// CRC-32C (Castagnoli, 0x11EDC6F41 reflected -> 0x82F63B78)
const CRC32C_POLY: u32 = 0x82F63B78;
static CRC32C_TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();

fn crc32c_table() -> &'static [u32; 256] {
    CRC32C_TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for i in 0..256 {
            let mut c = i as u32;
            for _ in 0..8 {
                if c & 1 != 0 {
                    c = (c >> 1) ^ CRC32C_POLY;
                } else {
                    c >>= 1;
                }
            }
            t[i] = c;
        }
        t
    })
}

fn crc32c(data: &[u8]) -> u32 {
    let tab = crc32c_table();
    let mut c = 0xFFFFFFFFu32;
    for &b in data {
        c = tab[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// Build a fixed VHDX file containing a raw NTFS payload.
///
/// * `payload` — raw bytes of the NTFS volume (must be multiple of 512)
/// * `block_size` — payload block size in bytes (power of 2, 1 MB..256 MB)
/// * `creator` — UTF-16LE creator string (optional, truncated to 512 bytes)
/// * `logical_sector_size` — 512 or 4096
/// * `physical_sector_size` — 512 or 4096
///
/// Returns the complete VHDX file bytes.
pub fn build_fixed_vhdx(
    payload: &[u8],
    block_size: u32,
    creator: Option<&str>,
    logical_sector_size: u32,
    physical_sector_size: u32,
) -> Result<Vec<u8>> {
    if payload.len() % 512 != 0 {
        bail!("payload size must be multiple of 512");
    }
    if !block_size.is_power_of_two() || block_size < MB as u32 || block_size > 256 * MB as u32 {
        bail!("block_size must be power of 2 between 1 MB and 256 MB");
    }

    let virtual_size = payload.len() as u64;
    let payload_blocks = (virtual_size + block_size as u64 - 1) / block_size as u64;

    // Layout offsets (all 1 MB aligned)
    let header_sec_offset = KB64;           // 0x10000
    let header_sec2_offset = 2 * KB64;      // 0x20000
    let region_tbl1_offset = 3 * KB64;      // 0x30000
    let region_tbl2_offset = 4 * KB64;      // 0x40000
    let metadata_offset = MB;               // 0x100000
    let metadata_len = MB;                  // 1 MB
    let payload_offset = 2 * MB;            // 0x200000
    let payload_len = virtual_size;

    // File size: payload + payload_offset, rounded to 1 MB
    let file_size = ((payload_offset + payload_len + MB - 1) / MB) * MB;

    let mut w = Cursor::new(Vec::with_capacity(file_size as usize));

    // ==== 1. File Type Identifier (64 KB slot) ====
    write_fti(&mut w, creator)?;
    pad_to(&mut w, KB64)?;

    // ==== 2. Header #1 ====
    let hdr1 = build_header(
        2, // seq=2 (current)
        &[0u8; 16], // FileWriteGuid - will fill later
        &[0u8; 16], // DataWriteGuid
        &[0u8; 16], // LogGuid = 0 (no log)
        0, // log_version
        1, // version
        0, // log_length
        0, // log_offset
    );
    w.write_all(&hdr1)?;
    pad_to(&mut w, KB64)?; // up to 128 KB

    // ==== 3. Header #2 ====
    let hdr2 = build_header(
        1, // seq=1
        &[0u8; 16], &[0u8; 16], &[0u8; 16],
        0, 1, 0, 0,
    );
    w.write_all(&hdr2)?;
    pad_to(&mut w, KB64)?; // up to 192 KB

    // ==== 4. Region Table #1 ====
    let rt1 = build_region_table(&[RegionEntry {
        guid: METADATA_REGION_GUID,
        file_offset: metadata_offset,
        length: metadata_len,
        required: 1,
    }]);
    w.write_all(&rt1)?;
    pad_to(&mut w, KB64)?; // up to 256 KB

    // ==== 5. Region Table #2 ====
    let rt2 = build_region_table(&[RegionEntry {
        guid: METADATA_REGION_GUID,
        file_offset: metadata_offset,
        length: metadata_len,
        required: 1,
    }]);
    w.write_all(&rt2)?;
    pad_to(&mut w, MB)?; // up to 1 MB

    // ==== 6. Metadata Region (1 MB) ====
    let meta = build_metadata_region(
        block_size,
        virtual_size,
        logical_sector_size,
        physical_sector_size,
    )?;
    w.write_all(&meta)?;
    pad_to(&mut w, metadata_offset + metadata_len)?;

    // ==== 7. Payload (NTFS raw data) ====
    w.write_all(payload)?;

    // ==== Pad to file size ====
    let current = w.position();
    if current < file_size {
        w.write_all(&vec![0u8; (file_size - current) as usize])?;
    }

    // ==== Post-write: patch Header GUIDs and CRC ====
    // We need to patch the FileWriteGuid/DataWriteGuid in both headers with a generated GUID.
    // Since the headers are small and at known offsets, we can do this in the buffer.
    let mut buf = w.into_inner();
    let file_write_guid = Uuid::new_v4().to_bytes_le();
    let data_write_guid = Uuid::new_v4().to_bytes_le();

    for hdr_off in [header_sec_offset as usize, header_sec2_offset as usize] {
        // FileWriteGuid at hdr_off + 0x10
        buf[hdr_off + 0x10..hdr_off + 0x20].copy_from_slice(&file_write_guid);
        // DataWriteGuid at hdr_off + 0x20
        buf[hdr_off + 0x20..hdr_off + 0x30].copy_from_slice(&data_write_guid);
        // CRC at hdr_off + 4 (zeroed during calc)
        buf[hdr_off + 4..hdr_off + 8].copy_from_slice(&0u32.to_le_bytes());
        let crc = crc32c(&buf[hdr_off..hdr_off + KB4 as usize]);
        buf[hdr_off + 4..hdr_off + 8].copy_from_slice(&crc.to_le_bytes());
    }

    // Patch Region Table CRCs
    for rt_off in [region_tbl1_offset as usize, region_tbl2_offset as usize] {
        buf[rt_off + 4..rt_off + 8].copy_from_slice(&0u32.to_le_bytes());
        let crc = crc32c(&buf[rt_off..rt_off + KB64 as usize]);
        buf[rt_off + 4..rt_off + 8].copy_from_slice(&crc.to_le_bytes());
    }

    std::fs::write("/tmp/ferrus-vhdx-debug.txt", format!("build_fixed_vhdx returning {} bytes\n", buf.len())).ok();
    if buf.is_empty() {
        std::fs::write("/tmp/ferrus-vhdx-debug.txt", "ERROR: buf is EMPTY!\n").ok();
    }

    Ok(buf)
}

/// Write File Type Identifier structure (at offset 0).
fn write_fti(w: &mut Cursor<Vec<u8>>, creator: Option<&str>) -> Result<()> {
    // Signature "vhdxfile" (8 bytes)
    w.write_all(b"vhdxfile")?;
    // Reserved (8 bytes)
    w.write_all(&[0u8; 8])?;
    // Creator: UTF-16LE string, max 512 bytes (256 chars)
    if let Some(c) = creator {
        let mut creator_bytes = Vec::new();
        for ch in c.encode_utf16() {
            creator_bytes.extend_from_slice(&ch.to_le_bytes());
        }
        creator_bytes.truncate(512);
        w.write_all(&creator_bytes)?;
        // Pad to 512 bytes
        if creator_bytes.len() < 512 {
            w.write_all(&vec![0u8; 512 - creator_bytes.len()])?;
        }
    } else {
        w.write_all(&[0u8; 512])?;
    }
    Ok(())
}

/// Header structure (4 KB), stored at 64 KB and 128 KB.
fn build_header(
    sequence_number: u64,
    file_write_guid: &[u8; 16],
    data_write_guid: &[u8; 16],
    log_guid: &[u8; 16],
    log_version: u16,
    version: u16,
    log_length: u32,
    log_offset: u64,
) -> Vec<u8> {
    let mut h = Vec::with_capacity(KB4 as usize);
    h.extend_from_slice(b"head");                    // +0x00 Signature
    h.extend_from_slice(&[0u8; 4]);                  // +0x04 Checksum (placeholder)
    h.extend_from_slice(&sequence_number.to_le_bytes()); // +0x08 SequenceNumber
    h.extend_from_slice(file_write_guid);            // +0x10 FileWriteGuid
    h.extend_from_slice(data_write_guid);            // +0x20 DataWriteGuid
    h.extend_from_slice(log_guid);                   // +0x30 LogGuid
    h.extend_from_slice(&log_version.to_le_bytes()); // +0x40 LogVersion
    h.extend_from_slice(&version.to_le_bytes());     // +0x42 Version
    h.extend_from_slice(&log_length.to_le_bytes());  // +0x44 LogLength
    h.extend_from_slice(&log_offset.to_le_bytes());  // +0x48 LogOffset
    // Reserved to 4 KB (4016 bytes from 0x50)
    h.extend_from_slice(&[0u8; 4016]);
    debug_assert_eq!(h.len(), KB4 as usize);
    h
}

const METADATA_REGION_GUID: [u8; 16] = [
    0x06, 0xa2, 0x7c, 0x8b, 0x90, 0x47, 0x9a, 0x4b,
    0xb8, 0xfe, 0x57, 0x5f, 0x05, 0x0f, 0x88, 0x6e,
];

#[derive(Copy, Clone)]
struct RegionEntry {
    guid: [u8; 16],
    file_offset: u64,
    length: u64,
    required: u32,
}

fn build_region_table(entries: &[RegionEntry]) -> Vec<u8> {
    let mut rt = Vec::with_capacity(KB64 as usize);
    rt.extend_from_slice(b"regi");        // +0x00 Signature
    rt.extend_from_slice(&[0u8; 4]);      // +0x04 Checksum (placeholder)
    rt.extend_from_slice(&(entries.len() as u32).to_le_bytes()); // +0x08 EntryCount
    rt.extend_from_slice(&[0u8; 4]);      // +0x0C Reserved

    for e in entries {
        rt.extend_from_slice(&e.guid);
        rt.extend_from_slice(&e.file_offset.to_le_bytes());
        rt.extend_from_slice(&e.length.to_le_bytes());
        rt.extend_from_slice(&e.required.to_le_bytes());
    }

    // Pad to 64 KB
    rt.resize(KB64 as usize, 0);
    debug_assert_eq!(rt.len(), KB64 as usize);
    rt
}

fn build_metadata_region(
    block_size: u32,
    virtual_size: u64,
    logical_sector_size: u32,
    physical_sector_size: u32,
) -> Result<Vec<u8>> {
    let mut m = Vec::with_capacity(MB as usize);

    // Metadata Table Header (32 bytes)
    m.extend_from_slice(b"metadata");  // +0x00 Signature (8)
    m.extend_from_slice(&[0u8; 2]);    // +0x08 Reserved
    m.extend_from_slice(&5u16.to_le_bytes()); // +0x0A EntryCount = 5
    m.extend_from_slice(&[0u8; 20]);   // +0x0C Reserved2
    // Total 32 bytes
    debug_assert_eq!(m.len(), 32);

    // We'll place items sequentially starting at offset 0x10000 (64 KB) within metadata region
    // (spec requires Offset >= 64 KB). We'll use 64 KB, 64 KB+align...
    let mut item_offset = 0x10000u32;
    let mut items_data = Vec::new();

    // 1. File Parameters (GUID CAA16737-FA36-4D43-B3B6-33F0AA44E76B)
    //    IsUser=0, IsVirtualDisk=0, IsRequired=1
    let file_params_guid = [
        0x37, 0x67, 0xa1, 0xca, 0x36, 0xfa, 0x43, 0x4d,
        0xb3, 0xb6, 0x33, 0xf0, 0xaa, 0x44, 0xe7, 0x6b,
    ];
    // BlockSize u32 + Flags u32 (A=LeaveBlockAllocated bit0, B=HasParent bit1)
    let fp_data: Vec<u8> = [
        block_size.to_le_bytes().to_vec(),
        1u32.to_le_bytes().to_vec(), // LeaveBlockAllocated=1 for fixed
    ].concat();
    let item1_off = item_offset;
    let item1_len = fp_data.len() as u32;
    items_data.extend_from_slice(&fp_data);
    item_offset = (item_offset + item1_len + 7) & !7; // 8-byte align

    // 2. Virtual Disk Size (GUID 2FA54224-CD1B-4876-B211-5DBED83BF4B8)
    //    IsUser=0, IsVirtualDisk=1, IsRequired=1
    let vdsize_guid = [
        0x24, 0x42, 0xa5, 0x2f, 0x1b, 0xcd, 0x76, 0x48,
        0xb2, 0x11, 0x5d, 0xbe, 0xd8, 0x3b, 0xf4, 0xb8,
    ];
    let vdsize_data = virtual_size.to_le_bytes().to_vec();
    let item2_off = item_offset;
    let item2_len = vdsize_data.len() as u32;
    items_data.extend_from_slice(&vdsize_data);
    item_offset = (item_offset + item2_len + 7) & !7;

    // 3. Virtual Disk ID (GUID BECA12AB-B2E6-4523-93EF-C309E000C746)
    let vdid_guid = [
        0xab, 0x12, 0xca, 0xbe, 0xe6, 0xb2, 0x23, 0x45,
        0x93, 0xef, 0xc3, 0x09, 0xe0, 0x00, 0xc7, 0x46,
    ];
    let vdid = Uuid::new_v4().to_bytes_le();
    let item3_off = item_offset;
    let item3_len = 16;
    items_data.extend_from_slice(&vdid);
    item_offset = (item_offset + 16 + 7) & !7;

    // 4. Logical Sector Size (GUID 8141BF1D-A96F-4709-BA47-F233A8FAAB5F)
    let logsec_guid = [
        0x1d, 0xbf, 0x41, 0x81, 0x6f, 0xa9, 0x09, 0x47,
        0xba, 0x47, 0xf2, 0x33, 0xa8, 0xfa, 0xab, 0x5f,
    ];
    let logsec_data = logical_sector_size.to_le_bytes().to_vec();
    let item4_off = item_offset;
    let item4_len = 4;
    items_data.extend_from_slice(&logsec_data);
    item_offset = (item_offset + 4 + 7) & !7;

    // 5. Physical Sector Size (GUID CDA348C7-445D-4471-9CC9-E9885251C556)
    let physsec_guid = [
        0xc7, 0x48, 0xa3, 0xcd, 0x5d, 0x44, 0x71, 0x44,
        0x9c, 0xc9, 0xe9, 0x88, 0x52, 0x51, 0xc5, 0x56,
    ];
    let physsec_data = physical_sector_size.to_le_bytes().to_vec();
    let item5_off = item_offset;
    let item5_len = 4;
    items_data.extend_from_slice(&physsec_data);
    // No alignment needed after last

    // Now write the 5 entries (32 bytes each)
    let entries = [
        (file_params_guid, item1_off, item1_len, 0u32 | 0x01 | 0x04), // IsRequired=1 (bit2), IsVirtualDisk=0, IsUser=0
        (vdsize_guid, item2_off, item2_len, 0u32 | 0x01 | 0x02 | 0x04), // IsVirtualDisk=1 (bit1), IsRequired=1
        (vdid_guid, item3_off, item3_len, 0u32 | 0x01 | 0x02 | 0x04),
        (logsec_guid, item4_off, item4_len, 0u32 | 0x01 | 0x02 | 0x04),
        (physsec_guid, item5_off, item5_len, 0u32 | 0x01 | 0x02 | 0x04),
    ];

    for (guid, off, len, flags) in entries {
        m.extend_from_slice(&guid);         // ItemID (16)
        m.extend_from_slice(&off.to_le_bytes());  // Offset (4)
        m.extend_from_slice(&len.to_le_bytes());  // Length (4)
        m.extend_from_slice(&flags.to_le_bytes()); // A|B|C|Reserved (4)
        m.extend_from_slice(&[0u8; 4]);     // Reserved2 (4)
        // Total 32 bytes per entry
    }

    // Pad to 64 KB boundary before items data
    let header_end = m.len();
    let items_start = ((header_end + 0xFFFF) & !0xFFFF) as u32; // 64 KB aligned
    let pad = items_start as usize - header_end;
    m.extend_from_slice(&vec![0u8; pad]);

    // Write items data
    m.extend_from_slice(&items_data);

    // Pad to 1 MB
    m.resize(MB as usize, 0);
    debug_assert_eq!(m.len(), MB as usize);
    Ok(m)
}

fn pad_to(w: &mut Cursor<Vec<u8>>, align: u64) -> Result<()> {
    let pos = w.position();
    let target = ((pos + align - 1) / align) * align;
    if target > pos {
        w.write_all(&vec![0u8; (target - pos) as usize])?;
    }
    Ok(())
}