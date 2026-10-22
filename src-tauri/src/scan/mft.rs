//! Fast scanner: reads the NTFS Master File Table straight from the volume.
//!
//! Needs administrator rights (opening `\\.\C:`). The layout follows the
//! public NTFS on-disk format: boot sector → $MFT data runs (through record
//! 0's $ATTRIBUTE_LIST when $MFT is fragmented) → FILE records (usually
//! 1 KiB) → $STANDARD_INFORMATION / $FILE_NAME / $DATA / $REPARSE_POINT.
//!
//! The volume is live while it is read, so every field from disk is checked:
//! a damaged or half-written record is skipped, never a panic.

use crate::index::{RawEntry, FLAG_DIR, FLAG_HARDLINK, FLAG_HIDDEN, FLAG_LINKED, FLAG_NOT_LINK, FLAG_REPARSE, FLAG_SYSTEM};
use rayon::prelude::*;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::windows::fs::OpenOptionsExt;

pub const ROOT_RECORD: u64 = 5;

const FILE_SHARE_READ: u32 = 1;
const FILE_SHARE_WRITE: u32 = 2;
const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;
const REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// $FILE_NAME namespace of an 8.3 alias that sits next to a long name.
const NS_DOS: u8 = 2;
/// Reparse tags of files Windows shows as ordinary files.
const IO_REPARSE_TAG_WOF: u32 = 0x8000_0017;
const IO_REPARSE_TAG_DEDUP: u32 = 0x8000_0013;
/// Never a record number, so `Index::build` parks the entry as an orphan.
const NO_PARENT: u64 = u64::MAX;
/// NTFS protects records in 512-byte strides whatever the disk's sector size.
const USA_STRIDE: usize = 512;
const CHUNK: usize = 8 << 20;

/// Volume layout from the boot sector.
struct Geometry {
    sector: usize,
    cluster: u64,
    record: usize,
    /// Volume size in clusters; every run must lie inside it.
    clusters: u64,
    mft_lcn: u64,
}

/// One $FILE_NAME: parent reference (record number and sequence number),
/// namespace and name.
#[derive(Clone)]
struct Link {
    parent: u64,
    ns: u8,
    name: String,
}

#[derive(Default, Clone)]
struct Rec {
    in_use: bool,
    is_dir: bool,
    /// Header sequence number; references to this record must carry it.
    seq: u16,
    attrs: u32,
    /// Tag from $REPARSE_POINT, 0 when none was seen.
    reparse: u32,
    /// The record's own name. Further long names (hard links) are kept apart.
    name: Option<Link>,
    size: u64,
    /// Clusters of the main stream plus the WofCompressedData stream.
    alloc: u64,
    modified: i64,
}

/// Scans an NTFS drive such as `C:`.
pub fn scan(drive: &str) -> io::Result<Vec<RawEntry>> {
    let mut disk = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(format!("\\\\.\\{drive}"))?;
    scan_volume(&mut disk, CHUNK)
}

/// Reads the MFT of a volume (the real one, or an image in tests).
fn scan_volume<R: Read + Seek>(disk: &mut R, chunk: usize) -> io::Result<Vec<RawEntry>> {
    // Volume reads must be whole sectors; 4 KiB covers 512-byte and 4Kn disks.
    let mut boot = vec![0u8; 4096];
    disk.seek(SeekFrom::Start(0))?;
    disk.read_exact(&mut boot)?;
    let geo = parse_boot(&boot)?;
    let (runs, bytes) = mft_layout(disk, &geo)?;
    let per = geo.record;
    let mut recs = vec![Rec::default(); (bytes / per as u64) as usize];
    // Extension records, whose attributes belong to another (base) record.
    let mut extra: Vec<(u64, Rec, Vec<Link>)> = Vec::new();
    // Extra hard links: (record number, link).
    let mut links: Vec<(usize, Link)> = Vec::new();

    read_records(disk, &geo, &runs, bytes, chunk, |first, batch| {
        let parsed: Vec<(usize, Parsed)> = batch
            .par_chunks_mut(per)
            .enumerate()
            .filter_map(|(k, raw)| parse_record(raw).map(|p| (first + k, p)))
            .collect();
        for (no, (base, rec, more)) in parsed {
            match base {
                None => {
                    if let Some(slot) = recs.get_mut(no) {
                        *slot = rec;
                        links.extend(more.into_iter().map(|l| (no, l)));
                    }
                }
                Some(b) => extra.push((b, rec, more)),
            }
        }
    })?;
    let mut found = Vec::new();
    for (base, rec, more) in extra {
        let no = (base & REF_MASK) as usize;
        // The base must still be the record this extension was written for.
        let Some(target) = recs.get_mut(no).filter(|t| t.in_use && seq_matches(t.seq, base)) else {
            continue;
        };
        merge(target, rec, more, &mut found);
        links.extend(found.drain(..).map(|l| (no, l)));
    }
    Ok(entries(recs, links))
}

/// Reads the volume layout from the boot sector, refusing values NTFS never writes.
fn parse_boot(boot: &[u8]) -> io::Result<Geometry> {
    if boot.len() < 512 || &boot[3..11] != b"NTFS    " {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "not an NTFS volume"));
    }
    let unsupported = || bad("unsupported NTFS boot sector");
    let sector = u16_at(boot, 0x0B) as u64;
    if !(512..=4096).contains(&sector) || !sector.is_power_of_two() {
        return Err(unsupported());
    }
    // Clusters above 64 KiB store a negative power of two: 0xF8 means 2^8 sectors.
    let spc = boot[0x0D];
    let per_cluster = if spc <= 0x80 { spc as u64 } else { 1u64.checked_shl(256 - spc as u32).unwrap_or(0) };
    if !per_cluster.is_power_of_two() || per_cluster > (2 << 20) / sector {
        return Err(unsupported());
    }
    let cluster = sector * per_cluster;
    let total_sectors = u64_at(boot, 0x28);
    if !total_sectors.checked_mul(sector).is_some_and(|b| b > 0 && b <= 1 << 60) {
        return Err(unsupported());
    }
    // Record size: a negative value is a power of two in bytes, otherwise clusters.
    let raw = boot[0x40] as i8;
    let record = if raw < 0 { 1u64.checked_shl(-(raw as i32) as u32).unwrap_or(0) } else { raw as u64 * cluster };
    if !record.is_power_of_two() || !(USA_STRIDE as u64..=64 << 10).contains(&record) {
        return Err(unsupported());
    }
    let clusters = total_sectors / per_cluster;
    let mft_lcn = u64_at(boot, 0x30);
    if mft_lcn >= clusters {
        return Err(unsupported());
    }
    Ok(Geometry { sector: sector as usize, cluster, record: record as usize, clusters, mft_lcn })
}

/// One extent of $MFT's unnamed $DATA: the VCNs it maps and their runs.
struct Extent {
    first: u64,
    last: u64,
    runs: Vec<(u64, u64)>,
}

/// Finds $MFT's runs in VCN order and how many bytes of it hold records.
/// A fragmented $MFT keeps later extents in extension records, which record
/// 0's $ATTRIBUTE_LIST names; they are read through the runs known so far.
fn mft_layout<R: Read + Seek>(disk: &mut R, geo: &Geometry) -> io::Result<(Vec<(u64, u64)>, u64)> {
    let mut rec = read_at(disk, geo, geo.mft_lcn * geo.cluster, geo.record)?;
    if !fixup(&mut rec) {
        return Err(bad("bad $MFT record"));
    }
    let mut extents = Vec::new();
    let mut sizes = None;
    let mut list = None;
    for (kind, a) in attrs(&rec) {
        match kind {
            ATTR_DATA if a[8] != 0 && a[9] == 0 => {
                let e = extent(a, geo)?;
                // Only the extent that starts at VCN 0 carries the stream sizes.
                if e.first == 0 {
                    sizes = Some((u64_at(a, 0x30), u64_at(a, 0x38)));
                }
                extents.push(e);
            }
            ATTR_ATTRIBUTE_LIST => list = Some(attribute_list(disk, geo, a)?),
            _ => {}
        }
    }
    let (data_size, init_size) = sizes.ok_or_else(|| bad("$MFT has no data runs"))?;
    // Records past the initialized size were never formatted and may hold
    // stale FILE records from an earlier file system.
    let record = geo.record as u64;
    let bytes = data_size.min(init_size) / record * record;
    let need = bytes.div_ceil(geo.cluster);

    let mut runs = chain(&mut extents)?;
    if total(&runs) < need {
        let mut listed = list.as_deref().map(listed_data).unwrap_or_default();
        listed.sort_unstable();
        let mut read = Vec::new();
        for (_, no) in listed {
            if no == 0 || read.contains(&no) {
                continue;
            }
            read.push(no);
            let pos = no.checked_mul(record).ok_or_else(|| bad("bad $MFT attribute list"))?;
            let mut ext = read_mapped(disk, geo, &runs, pos, geo.record)?;
            if !fixup(&mut ext) {
                return Err(bad("bad $MFT extension record"));
            }
            for (kind, a) in attrs(&ext) {
                if kind == ATTR_DATA && a[8] != 0 && a[9] == 0 {
                    let e = extent(a, geo)?;
                    if !extents.iter().any(|x| x.first == e.first) {
                        extents.push(e);
                    }
                }
            }
            runs = chain(&mut extents)?;
        }
    }
    // Rather an error (and the folder walk) than an index that misses records.
    if total(&runs) < need {
        return Err(bad("$MFT run list is incomplete"));
    }
    if bytes / record > u32::MAX as u64 {
        return Err(bad("$MFT is too large"));
    }
    Ok((runs, bytes))
}

/// Decodes one non-resident $MFT $DATA header and checks that its runs map
/// exactly the VCNs it names, inside the volume.
fn extent(a: &[u8], geo: &Geometry) -> io::Result<Extent> {
    let damaged = || bad("damaged $MFT run list");
    if a.len() < 0x40 {
        return Err(damaged());
    }
    let (first, last) = (u64_at(a, 0x10), u64_at(a, 0x18));
    let runs = a.get(u16_at(a, 0x20) as usize..).and_then(decode_runs).filter(|r| in_volume(r, geo)).ok_or_else(damaged)?;
    if last < first || last >= geo.clusters || total(&runs) != last - first + 1 {
        return Err(damaged());
    }
    Ok(Extent { first, last, runs })
}

/// Joins the extents into one run list, checking they cover VCN 0 onward
/// with no gap or overlap.
fn chain(extents: &mut [Extent]) -> io::Result<Vec<(u64, u64)>> {
    extents.sort_unstable_by_key(|e| e.first);
    let mut runs = Vec::new();
    let mut next = 0;
    for e in extents.iter() {
        if e.first != next {
            return Err(bad("$MFT extents do not line up"));
        }
        runs.extend_from_slice(&e.runs);
        next = e.last + 1;
    }
    Ok(runs)
}

/// The value of record 0's $ATTRIBUTE_LIST, resident or not.
fn attribute_list<R: Read + Seek>(disk: &mut R, geo: &Geometry, a: &[u8]) -> io::Result<Vec<u8>> {
    let damaged = || bad("damaged $MFT attribute list");
    if a[8] == 0 {
        return resident(a).map(<[u8]>::to_vec).ok_or_else(damaged);
    }
    if a.len() < 0x40 || u64_at(a, 0x10) != 0 {
        return Err(damaged());
    }
    let size = u64_at(a, 0x30);
    let runs = a.get(u16_at(a, 0x20) as usize..).and_then(decode_runs).filter(|r| in_volume(r, geo)).ok_or_else(damaged)?;
    // Real lists stay far below this.
    if size > 4 << 20 {
        return Err(damaged());
    }
    read_mapped(disk, geo, &runs, 0, size as usize)
}

/// The unnamed $DATA extents an $ATTRIBUTE_LIST names: (first VCN, record number).
fn listed_data(list: &[u8]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut off = 0;
    while let Some(e) = list.get(off..off + 0x1A) {
        let len = u16_at(e, 4) as usize;
        if len < 0x1A {
            break;
        }
        if u32_at(e, 0) == ATTR_DATA && e[6] == 0 {
            out.push((u64_at(e, 8), u64_at(e, 0x10) & REF_MASK));
        }
        off += len;
    }
    out
}

/// Decodes mapping pairs into (LCN, clusters). None for anything $MFT and
/// its attribute list never hold: sparse runs, oversized fields, LCNs below 0.
fn decode_runs(mut b: &[u8]) -> Option<Vec<(u64, u64)>> {
    let mut runs = Vec::new();
    let mut lcn: i64 = 0;
    while let Some(&head) = b.first() {
        if head == 0 {
            break;
        }
        let len_size = (head & 0x0F) as usize;
        let off_size = (head >> 4) as usize;
        if !(1..=8).contains(&len_size) || !(1..=8).contains(&off_size) {
            return None;
        }
        let field = b.get(1..1 + len_size + off_size)?;
        let length = read_uint(&field[..len_size]);
        lcn = lcn.checked_add(read_int(&field[len_size..]))?;
        if lcn < 0 || length == 0 {
            return None;
        }
        runs.push((lcn as u64, length));
        b = &b[1 + len_size + off_size..];
    }
    Some(runs)
}

fn in_volume(runs: &[(u64, u64)], geo: &Geometry) -> bool {
    runs.iter().all(|&(lcn, len)| lcn.checked_add(len).is_some_and(|end| end <= geo.clusters))
}

/// Clusters covered by a run list.
fn total(runs: &[(u64, u64)]) -> u64 {
    runs.iter().fold(0u64, |acc, r| acc.saturating_add(r.1))
}

fn read_uint(b: &[u8]) -> u64 {
    b.iter().rev().fold(0u64, |acc, &x| (acc << 8) | x as u64)
}

fn read_int(b: &[u8]) -> i64 {
    let v = read_uint(b) as i64;
    let bits = b.len() * 8;
    if bits < 64 && v & (1 << (bits - 1)) != 0 { v - (1 << bits) } else { v }
}

/// Reads `len` bytes at byte `pos` of the volume, widened to whole sectors.
fn read_at<R: Read + Seek>(disk: &mut R, geo: &Geometry, pos: u64, len: usize) -> io::Result<Vec<u8>> {
    let sector = geo.sector as u64;
    let start = pos / sector * sector;
    let skip = (pos - start) as usize;
    let mut buf = vec![0u8; (skip + len).next_multiple_of(geo.sector)];
    disk.seek(SeekFrom::Start(start))?;
    disk.read_exact(&mut buf)?;
    buf.drain(..skip);
    buf.truncate(len);
    Ok(buf)
}

/// Reads `len` bytes at offset `pos` of a non-resident stream laid out by `runs`.
fn read_mapped<R: Read + Seek>(disk: &mut R, geo: &Geometry, runs: &[(u64, u64)], pos: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    let mut run_start = 0u64;
    for &(lcn, clusters) in runs {
        let run_end = run_start.saturating_add(clusters * geo.cluster);
        let at = pos.saturating_add(out.len() as u64);
        // Earlier runs were skipped or used up, so `at >= run_start` here.
        if out.len() < len && at < run_end {
            let take = (run_end - at).min((len - out.len()) as u64) as usize;
            out.extend(read_at(disk, geo, lcn * geo.cluster + (at - run_start), take)?);
        }
        run_start = run_end;
    }
    if out.len() < len {
        return Err(bad("read past the end of a run list"));
    }
    Ok(out)
}

/// Reads the first `bytes` of the $MFT stream through `runs` and hands each
/// batch of whole records to `batch`, with the number of its first record.
/// The stream is one VCN-ordered whole: a record that straddles two runs
/// (records larger than clusters) is carried over and completed from the next.
fn read_records<R: Read + Seek>(
    disk: &mut R,
    geo: &Geometry,
    runs: &[(u64, u64)],
    bytes: u64,
    chunk: usize,
    mut batch: impl FnMut(usize, &mut [u8]),
) -> io::Result<()> {
    let per = geo.record;
    let chunk = chunk.max(geo.sector) / geo.sector * geo.sector;
    // buf[per - carry..per] holds the start of a record cut off by the last read.
    let mut buf = vec![0u8; per + chunk];
    let mut carry = 0;
    let mut record_no = 0;
    let mut wanted = bytes;
    for &(lcn, clusters) in runs {
        if wanted == 0 {
            break;
        }
        let take = (clusters * geo.cluster).min(wanted);
        wanted -= take;
        let mut offset = lcn * geo.cluster;
        let mut remaining = take;
        while remaining > 0 {
            let len = remaining.min(chunk as u64) as usize;
            // Only the very last read can be short, and runs end on a sector,
            // so rounding up keeps reads whole sectors inside the run.
            let read_len = len.next_multiple_of(geo.sector);
            disk.seek(SeekFrom::Start(offset))?;
            disk.read_exact(&mut buf[per..per + read_len])?;
            let start = per - carry;
            let whole = (carry + len) / per * per;
            batch(record_no, &mut buf[start..start + whole]);
            record_no += whole / per;
            carry = carry + len - whole;
            buf.copy_within(start + whole..per + len, per - carry);
            offset += len as u64;
            remaining -= len as u64;
        }
    }
    Ok(())
}

/// Applies the update-sequence fixup that NTFS uses to detect torn writes.
/// The array holds a check word plus the real last word of every 512-byte
/// stride; anything that does not fit the record exactly is refused.
fn fixup(rec: &mut [u8]) -> bool {
    if rec.len() < USA_STRIDE || !rec.len().is_multiple_of(USA_STRIDE) || &rec[0..4] != b"FILE" {
        return false;
    }
    let usa_off = u16_at(rec, 4) as usize;
    let usa_count = u16_at(rec, 6) as usize;
    if !usa_off.is_multiple_of(2) || usa_count != rec.len() / USA_STRIDE + 1 || usa_off + usa_count * 2 > USA_STRIDE - 2 {
        return false;
    }
    let check = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_count {
        let end = i * USA_STRIDE;
        if rec[end - 2..end] != check {
            return false;
        }
        rec[end - 2] = rec[usa_off + i * 2];
        rec[end - 1] = rec[usa_off + i * 2 + 1];
    }
    true
}

/// Walks the attributes of a fixed-up record as (type, whole attribute).
/// Every attribute is at least a resident header (0x18 bytes) long and lies
/// inside the record; the walk stops at the first one that does not.
fn attrs(rec: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    let mut off = u16_at(rec, 0x14) as usize;
    std::iter::from_fn(move || {
        let head = rec.get(off..off + 8)?;
        let kind = u32_at(head, 0);
        let len = u32_at(head, 4) as usize;
        if kind == ATTR_END || len < 0x18 || len > rec.len() - off {
            return None;
        }
        let a = &rec[off..off + len];
        off += len;
        Some((kind, a))
    })
}

/// A parsed FILE record: the base record reference for extension records
/// (whose attributes belong to that base record), the record, and its long
/// names after the first (extra hard links).
type Parsed = (Option<u64>, Rec, Vec<Link>);

/// Parses one FILE record.
fn parse_record(rec: &mut [u8]) -> Option<Parsed> {
    if !fixup(rec) {
        return None;
    }
    let flags = u16_at(rec, 0x16);
    if flags & 1 == 0 {
        return None;
    }
    let base = u64_at(rec, 0x20);
    let mut r = Rec { in_use: true, is_dir: flags & 2 != 0, seq: u16_at(rec, 0x10), ..Rec::default() };
    let mut more = Vec::new();
    for (kind, a) in attrs(rec) {
        let non_resident = a[8] != 0;
        match kind {
            ATTR_STANDARD_INFORMATION if !non_resident => {
                if let Some(c) = resident(a).filter(|c| c.len() >= 0x24) {
                    r.modified = u64_at(c, 0x08) as i64;
                    r.attrs = u32_at(c, 0x20);
                }
            }
            ATTR_FILE_NAME if !non_resident => {
                let Some(c) = resident(a).filter(|c| c.len() >= 0x42) else { continue };
                let ns = c[0x41];
                if ns == NS_DOS && r.name.is_some() {
                    continue;
                }
                let Some(units) = c.get(0x42..0x42 + c[0x40] as usize * 2) else { continue };
                if r.modified == 0 {
                    r.modified = u64_at(c, 0x10) as i64;
                }
                add_name(&mut r.name, Link { parent: u64_at(c, 0), ns, name: utf16(units) }, &mut more);
            }
            // The unnamed (main) stream gives the size. A WOF-compressed file's
            // main stream is sparse; its clusters are in WofCompressedData.
            ATTR_DATA => {
                let Some(name) = attr_name(a) else { continue };
                let main = name.is_empty();
                if !main && !utf16_is(name, "WofCompressedData") {
                    continue;
                }
                if non_resident {
                    // Sizes live in the extent that starts at VCN 0.
                    if a.len() >= 0x40 && u64_at(a, 0x10) == 0 {
                        // Compressed or sparse: clusters really used are at 0x40.
                        let packed = u16_at(a, 0x0C) & 0x8001 != 0;
                        let alloc = if packed && a.len() >= 0x48 { u64_at(a, 0x40) } else { u64_at(a, 0x28) };
                        r.alloc = r.alloc.saturating_add(alloc);
                        if main {
                            r.size = u64_at(a, 0x30);
                        }
                    }
                } else if main && let Some(c) = resident(a) {
                    r.size = c.len() as u64;
                }
            }
            ATTR_REPARSE_POINT if !non_resident => {
                if let Some(c) = resident(a).filter(|c| c.len() >= 4) {
                    r.reparse = u32_at(c, 0);
                }
            }
            _ => {}
        }
    }
    Some(((base != 0).then_some(base), r, more))
}

/// Keeps the first long (Win32 / POSIX) name as the record's own and every
/// further long name as an extra hard link. An 8.3 DOS alias only stands in
/// until a long name turns up.
fn add_name(name: &mut Option<Link>, link: Link, extra: &mut Vec<Link>) {
    let long = |l: &Link| l.ns != NS_DOS;
    match name {
        Some(old) if long(old) => {
            if long(&link) {
                extra.push(link);
            }
        }
        Some(_) if !long(&link) => {}
        _ => *name = Some(link),
    }
}

/// The value of a resident attribute, if it lies inside the attribute.
fn resident(a: &[u8]) -> Option<&[u8]> {
    let len = u32::from_le_bytes(a.get(0x10..0x14)?.try_into().ok()?) as usize;
    let off = u16::from_le_bytes(a.get(0x14..0x16)?.try_into().ok()?) as usize;
    a.get(off..off + len)
}

/// The attribute's name as raw UTF-16LE (empty when unnamed), if it fits.
fn attr_name(a: &[u8]) -> Option<&[u8]> {
    let units = *a.get(9)? as usize;
    if units == 0 {
        return Some(&[]);
    }
    let off = u16::from_le_bytes(a.get(0x0A..0x0C)?.try_into().ok()?) as usize;
    a.get(off..off + units * 2)
}

/// Folds an extension record into its base record. Long names beyond the
/// base's own go to `links`.
fn merge(target: &mut Rec, src: Rec, more: Vec<Link>, links: &mut Vec<Link>) {
    for link in src.name.into_iter().chain(more) {
        add_name(&mut target.name, link, links);
    }
    if src.size != 0 {
        target.size = src.size;
    }
    // A stream's VCN-0 extent sits in exactly one record, so adding counts it once.
    target.alloc = target.alloc.saturating_add(src.alloc);
    if src.reparse != 0 {
        target.reparse = src.reparse;
    }
}

/// True when `reference` names this sequence number. 0 means unchecked.
fn seq_matches(seq: u16, reference: u64) -> bool {
    let want = (reference >> 48) as u16;
    want == 0 || want == seq
}

/// The parent's record number, or NO_PARENT when that record is no longer
/// the folder the reference was made for (freed or reused while the MFT was
/// being read, which could otherwise even link folders into a cycle).
fn resolve(recs: &[Rec], reference: u64) -> u64 {
    let no = reference & REF_MASK;
    match recs.get(no as usize) {
        Some(p) if p.in_use && seq_matches(p.seq, reference) => no,
        _ => NO_PARENT,
    }
}

/// Turns the merged records into index entries. A file gets one entry per
/// hard link, each in its own folder; extra links have ids `record | k << 48`
/// and FLAG_HARDLINK, so folder totals count the file once.
fn entries(mut recs: Vec<Rec>, mut links: Vec<(usize, Link)>) -> Vec<RawEntry> {
    links.sort_by_key(|l| l.0);
    let mut out = Vec::with_capacity(recs.len() + links.len());
    let mut links = links.into_iter().peekable();
    for no in 0..recs.len() {
        let Some(own) = recs[no].name.take() else { continue };
        let r = &recs[no];
        let flags = flags_of(r);
        let entry = |id: u64, parent: u64, name: String, flags: u32| RawEntry {
            id,
            parent,
            name,
            flags,
            size: r.size,
            alloc: r.alloc,
            modified: r.modified,
        };
        let (mut parent, mut name) = (resolve(&recs, own.parent), own.name);
        // Links of records skipped above have nothing to attach to.
        while links.next_if(|l| l.0 < no).is_some() {}
        let mut k = 0u64;
        // Extra links in a live folder: the bytes counted through this entry are there too.
        let mut linked = false;
        while let Some((_, link)) = links.next_if(|l| l.0 == no) {
            // Folders cannot be hard-linked.
            if r.is_dir {
                continue;
            }
            let (mut link_parent, mut link_name) = (resolve(&recs, link.parent), link.name);
            // The first link in a live folder is the one counted in totals.
            if parent == NO_PARENT && link_parent != NO_PARENT {
                std::mem::swap(&mut parent, &mut link_parent);
                std::mem::swap(&mut name, &mut link_name);
            }
            k += 1;
            if k <= 0xFFFF {
                linked |= link_parent != NO_PARENT;
                out.push(entry(no as u64 | k << 48, link_parent, link_name, flags | FLAG_HARDLINK));
            }
        }
        out.push(entry(no as u64, parent, name, if linked { flags | FLAG_LINKED } else { flags }));
    }
    out
}

fn flags_of(r: &Rec) -> u32 {
    let mut flags = 0;
    if r.is_dir {
        flags |= FLAG_DIR;
    }
    if r.attrs & 0x2 != 0 {
        flags |= FLAG_HIDDEN;
    }
    if r.attrs & 0x4 != 0 {
        flags |= FLAG_SYSTEM;
    }
    // WOF-compressed and deduplicated files carry a reparse point that Windows
    // hides from normal listings: they are ordinary files, not links.
    if r.attrs & 0x400 != 0 && !matches!(r.reparse, IO_REPARSE_TAG_WOF | IO_REPARSE_TAG_DEDUP) {
        flags |= FLAG_REPARSE;
        // A cloud-files placeholder folder holds its own contents. Any other
        // tag (ProjFS, WCI, DFS, ...), or one that was not read (0), is taken
        // for a link, whose contents are not followed.
        if r.is_dir && super::is_cloud_tag(r.reparse) {
            flags |= FLAG_NOT_LINK;
        }
    }
    flags
}

fn utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|p| u16::from_le_bytes([p[0], p[1]])).collect();
    String::from_utf16_lossy(&units)
}

/// Compares raw UTF-16LE with an ASCII name.
fn utf16_is(b: &[u8], ascii: &str) -> bool {
    b.len() == ascii.len() * 2 && b.chunks_exact(2).zip(ascii.bytes()).all(|(p, c)| p[0] == c && p[1] == 0)
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

// Fixed-offset readers. Callers check the length first.
fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Index, ROOT};
    use std::io::Cursor;

    const USN: [u8; 2] = [0x34, 0x12];
    const IN_USE: u16 = 1;
    const DIR: u16 = 3;
    const REC: usize = 1024;

    fn put(b: &mut [u8], at: usize, v: &[u8]) {
        b[at..at + v.len()].copy_from_slice(v);
    }

    fn le16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// A file reference: record number plus sequence number.
    fn re(no: u64, seq: u16) -> u64 {
        no | (seq as u64) << 48
    }

    fn resident_attr(kind: u32, name: &str, value: &[u8]) -> Vec<u8> {
        let name = le16(name);
        let value_off = (0x18 + name.len()).next_multiple_of(8);
        let mut a = vec![0u8; (value_off + value.len()).next_multiple_of(8)];
        let len = a.len() as u32;
        put(&mut a, 0, &kind.to_le_bytes());
        put(&mut a, 4, &len.to_le_bytes());
        a[9] = (name.len() / 2) as u8;
        put(&mut a, 0x0A, &0x18u16.to_le_bytes());
        put(&mut a, 0x10, &(value.len() as u32).to_le_bytes());
        put(&mut a, 0x14, &(value_off as u16).to_le_bytes());
        put(&mut a, 0x18, &name);
        put(&mut a, value_off, value);
        a
    }

    /// Non-resident $DATA mapping VCNs `vcns`, with [allocated, data,
    /// initialized] sizes. `sparse_used` makes it a sparse stream.
    fn data_attr(name: &str, vcns: (u64, u64), sizes: [u64; 3], runs: &[u8], sparse_used: Option<u64>) -> Vec<u8> {
        let name = le16(name);
        let head = if sparse_used.is_some() { 0x48 } else { 0x40 };
        let runs_off = (head + name.len()).next_multiple_of(8);
        // The zeroed tail ends the mapping pairs.
        let mut a = vec![0u8; (runs_off + runs.len() + 1).next_multiple_of(8)];
        let len = a.len() as u32;
        put(&mut a, 0, &ATTR_DATA.to_le_bytes());
        put(&mut a, 4, &len.to_le_bytes());
        a[8] = 1;
        a[9] = (name.len() / 2) as u8;
        put(&mut a, 0x0A, &(head as u16).to_le_bytes());
        put(&mut a, 0x10, &vcns.0.to_le_bytes());
        put(&mut a, 0x18, &vcns.1.to_le_bytes());
        put(&mut a, 0x20, &(runs_off as u16).to_le_bytes());
        for (i, s) in sizes.iter().enumerate() {
            put(&mut a, 0x28 + i * 8, &s.to_le_bytes());
        }
        if let Some(used) = sparse_used {
            put(&mut a, 0x0C, &0x8000u16.to_le_bytes());
            put(&mut a, 0x40, &used.to_le_bytes());
        }
        put(&mut a, head, &name);
        put(&mut a, runs_off, runs);
        a
    }

    fn file_name(parent: u64, ns: u8, name: &str) -> Vec<u8> {
        let units = le16(name);
        let mut v = vec![0u8; 0x42 + units.len()];
        put(&mut v, 0, &parent.to_le_bytes());
        v[0x40] = (units.len() / 2) as u8;
        v[0x41] = ns;
        put(&mut v, 0x42, &units);
        resident_attr(ATTR_FILE_NAME, "", &v)
    }

    fn std_info(modified: i64, attrs: u32) -> Vec<u8> {
        let mut v = vec![0u8; 0x48];
        put(&mut v, 0x08, &modified.to_le_bytes());
        put(&mut v, 0x20, &attrs.to_le_bytes());
        resident_attr(ATTR_STANDARD_INFORMATION, "", &v)
    }

    /// Stamps the update sequence: saves each stride's last word in the array
    /// at 0x30 and puts the check value there instead.
    fn protect(r: &mut [u8]) {
        let count = r.len() / USA_STRIDE + 1;
        put(r, 0, b"FILE");
        put(r, 4, &0x30u16.to_le_bytes());
        put(r, 6, &(count as u16).to_le_bytes());
        put(r, 0x30, &USN);
        for i in 1..count {
            let end = i * USA_STRIDE;
            let saved = [r[end - 2], r[end - 1]];
            put(r, 0x30 + i * 2, &saved);
            put(r, end - 2, &USN);
        }
    }

    /// A FILE record as it lies on disk.
    fn record(size: usize, seq: u16, flags: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; size];
        let first = (0x30 + (size / USA_STRIDE + 1) * 2).next_multiple_of(8);
        put(&mut r, 0x10, &seq.to_le_bytes());
        put(&mut r, 0x14, &(first as u16).to_le_bytes());
        put(&mut r, 0x16, &flags.to_le_bytes());
        put(&mut r, 0x20, &base.to_le_bytes());
        let mut off = first;
        for a in attrs {
            put(&mut r, off, a);
            off += a.len();
        }
        put(&mut r, off, &ATTR_END.to_le_bytes());
        put(&mut r, 0x18, &((off + 8) as u32).to_le_bytes());
        protect(&mut r);
        r
    }

    fn boot(sector: u16, spc: u8, record: i8, total_sectors: u64, mft_lcn: u64) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        put(&mut b, 3, b"NTFS    ");
        put(&mut b, 0x0B, &sector.to_le_bytes());
        b[0x0D] = spc;
        put(&mut b, 0x28, &total_sectors.to_le_bytes());
        put(&mut b, 0x30, &mft_lcn.to_le_bytes());
        b[0x40] = record as u8;
        b
    }

    /// A volume image with 512-byte sectors and clusters and 1 KiB records, so
    /// records are larger than clusters. $MFT has two extents: VCN 0-4 at LCN
    /// 100 (in record 0) and VCN 5-15 at LCN 300 (in extension record 1, which
    /// record 0's $ATTRIBUTE_LIST names). Record 2 straddles the two runs.
    fn fake_volume(with_list: bool) -> Vec<u8> {
        let mut disk = vec![0u8; 400 * 512];
        put(&mut disk, 0, &boot(512, 1, -10, 400, 100));

        let mut list = Vec::new();
        for (vcn, rec) in [(0u64, re(0, 1)), (5, re(1, 1))] {
            let mut e = vec![0u8; 0x20];
            put(&mut e, 0, &ATTR_DATA.to_le_bytes());
            put(&mut e, 4, &0x20u16.to_le_bytes());
            e[7] = 0x1A;
            put(&mut e, 8, &vcn.to_le_bytes());
            put(&mut e, 0x10, &rec.to_le_bytes());
            list.extend(e);
        }
        let mut rec0 = vec![std_info(1, 0x6)];
        if with_list {
            rec0.push(resident_attr(ATTR_ATTRIBUTE_LIST, "", &list));
        }
        rec0.push(file_name(re(5, 5), 3, "$MFT"));
        // 8 records allocated, 7 initialized.
        rec0.push(data_attr("", (0, 4), [8 * REC as u64, 7 * REC as u64, 7 * REC as u64], &[0x11, 5, 100], None));
        let records = [
            record(REC, 1, IN_USE, 0, &rec0),
            record(REC, 1, IN_USE, re(0, 1), &[data_attr("", (5, 15), [0; 3], &[0x21, 11, 0x2C, 0x01], None)]),
            record(REC, 2, DIR, 0, &[std_info(2, 0), file_name(re(5, 5), 1, "dir")]),
            record(
                REC,
                3,
                IN_USE,
                0,
                &[
                    std_info(3, 0x20),
                    file_name(re(2, 2), 3, "a.bin"),
                    file_name(re(5, 5), 2, "LINK-T~1.BIN"),
                    file_name(re(5, 5), 1, "link-to-a.bin"),
                    file_name(re(2, 2), 0, "c.bin"),
                    data_attr("", (0, 1), [1024, 1000, 1000], &[0x11, 2, 0x50], None),
                ],
            ),
            record(
                REC,
                4,
                IN_USE,
                0,
                &[
                    std_info(4, 0x420),
                    file_name(re(5, 5), 3, "wof.dll"),
                    data_attr("", (0, 9), [5120, 5000, 5000], &[0x01, 10], Some(0)),
                    data_attr("WofCompressedData", (0, 3), [2048, 1900, 1900], &[0x11, 4, 0x60], None),
                    resident_attr(ATTR_REPARSE_POINT, "", &IO_REPARSE_TAG_WOF.to_le_bytes()),
                ],
            ),
            record(REC, 5, DIR, 0, &[std_info(5, 0x6), file_name(re(5, 5), 3, ".")]),
            // Its folder's record was reused since: sequence 9 is gone.
            record(REC, 6, IN_USE, 0, &[std_info(6, 0), file_name(re(2, 9), 3, "stale.txt")]),
            // Past the initialized size: never formatted, must be ignored.
            record(REC, 7, IN_USE, 0, &[std_info(7, 0), file_name(re(5, 5), 3, "ghost.txt")]),
        ];
        let stream = records.concat();
        put(&mut disk, 100 * 512, &stream[..5 * 512]);
        put(&mut disk, 300 * 512, &stream[5 * 512..]);
        disk
    }

    #[test]
    fn decodes_data_runs() {
        // 0x21: 1-byte length, 2-byte offset. Second run goes backwards (negative offset).
        let runs = decode_runs(&[0x21, 0x10, 0x00, 0x01, 0x21, 0x08, 0x00, 0xFF, 0x00]);
        assert_eq!(runs, Some(vec![(0x100, 0x10), (0x100 - 0x100, 0x08)]));
        // Sparse run, 9-byte length, LCN below zero, zero length.
        assert_eq!(decode_runs(&[0x01, 0x10, 0x00]), None);
        assert_eq!(decode_runs(&[0x19, 1, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0]), None);
        assert_eq!(decode_runs(&[0x11, 0x10, 0xFF, 0x00]), None);
        assert_eq!(decode_runs(&[0x11, 0x00, 0x10, 0x00]), None);
    }

    #[test]
    fn signed_offsets() {
        assert_eq!(read_int(&[0xFF]), -1);
        assert_eq!(read_int(&[0x00, 0x80]), -32768);
        assert_eq!(read_int(&[0x7F]), 127);
    }

    #[test]
    fn fixup_uses_512_byte_strides() {
        for size in [1024usize, 4096] {
            let plain: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let mut disk = plain.clone();
            protect(&mut disk);
            let usa_end = 0x30 + (size / USA_STRIDE + 1) * 2;
            let mut r = disk.clone();
            assert!(fixup(&mut r), "{size}");
            assert_eq!(r[usa_end..], plain[usa_end..], "{size}");

            // A torn last stride, an odd array offset, and an array sized for
            // a 4 KiB "sector" stride are all refused.
            let mut torn = disk.clone();
            torn[size - 1] ^= 1;
            assert!(!fixup(&mut torn));
            let mut odd = disk.clone();
            put(&mut odd, 4, &0x31u16.to_le_bytes());
            assert!(!fixup(&mut odd));
            let mut short = disk.clone();
            put(&mut short, 6, &2u16.to_le_bytes());
            assert!(!fixup(&mut short));
            let mut long = disk.clone();
            put(&mut long, 6, &(size as u16 / 512 + 2).to_le_bytes());
            assert!(!fixup(&mut long));
        }
        assert!(!fixup(&mut [0u8; 100]));
    }

    #[test]
    fn four_kib_records_keep_fields_across_strides() {
        // Two long names cross offsets 510 and 1022; $DATA lands past them.
        let long: String = (0..250).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let other: String = (0..240).map(|i| char::from(b'A' + (i % 26) as u8)).collect();
        let mut r = record(
            4096,
            7,
            IN_USE,
            0,
            &[
                std_info(9, 0),
                file_name(re(5, 5), 1, &long),
                file_name(re(5, 5), 1, &other),
                data_attr("", (0, 0), [4096, 4000, 4000], &[0x11, 1, 9], None),
            ],
        );
        let (base, rec, more) = parse_record(&mut r).unwrap();
        assert_eq!(base, None);
        assert_eq!(rec.seq, 7);
        assert_eq!(rec.name.unwrap().name, long);
        assert_eq!(more.len(), 1);
        assert_eq!(more[0].name, other);
        assert_eq!((rec.size, rec.alloc), (4000, 4096));
    }

    #[test]
    fn boot_sector_cluster_sizes() {
        let g = parse_boot(&boot(512, 8, -10, 1 << 20, 4)).unwrap();
        assert_eq!((g.sector, g.cluster, g.record, g.clusters), (512, 4096, 1024, 1 << 17));
        // Past 64 KiB the byte is a negative exponent: 0xF8 = 2^8 sectors.
        assert_eq!(parse_boot(&boot(512, 0xF8, -10, 1 << 20, 4)).unwrap().cluster, 128 << 10);
        assert_eq!(parse_boot(&boot(4096, 0xF7, -12, 1 << 20, 4)).unwrap().cluster, 2 << 20);
        assert_eq!(parse_boot(&boot(4096, 0x80, -12, 1 << 20, 4)).unwrap().cluster, 512 << 10);
        // A positive record size counts clusters.
        assert_eq!(parse_boot(&boot(4096, 1, 1, 1 << 20, 4)).unwrap().record, 4096);
        let bad_boots = [
            (0, 8, -10, 1 << 20, 4),
            (500, 8, -10, 1 << 20, 4),
            (8192, 1, -12, 1 << 20, 4),
            (512, 0, -10, 1 << 20, 4),
            (512, 3, -10, 1 << 20, 4),
            (512, 0x81, -10, 1 << 20, 4),
            (4096, 0xF6, -12, 1 << 20, 4),
            (512, 8, 0, 1 << 20, 4),
            (512, 8, -128, 1 << 20, 4),
            (512, 8, -8, 1 << 20, 4),
            (512, 8, 3, 1 << 20, 4),
            (512, 8, -10, 0, 0),
            (512, 8, -10, u64::MAX, 4),
            (512, 8, -10, 1 << 20, 1 << 17),
        ];
        for (sector, spc, record, total, lcn) in bad_boots {
            assert!(parse_boot(&boot(sector, spc, record, total, lcn)).is_err(), "{sector} {spc:#x} {record}");
        }
        let mut fat = boot(512, 8, -10, 1 << 20, 4);
        put(&mut fat, 3, b"MSDOS5.0");
        assert_eq!(parse_boot(&fat).err().unwrap().kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn reads_fragmented_mft_with_links_and_stale_parents() {
        let disk = fake_volume(true);
        for chunk in [512, 1536, CHUNK] {
            let mut raw = scan_volume(&mut Cursor::new(&disk[..]), chunk).unwrap();
            raw.sort_by_key(|e| e.id);
            let got: Vec<(u64, u64, &str, u32)> = raw.iter().map(|e| (e.id, e.parent, e.name.as_str(), e.flags)).collect();
            assert_eq!(
                got,
                [
                    (0, 5, "$MFT", FLAG_HIDDEN | FLAG_SYSTEM),
                    (2, 5, "dir", FLAG_DIR),
                    (3, 2, "a.bin", FLAG_LINKED),
                    (4, 5, "wof.dll", 0),
                    (5, 5, ".", FLAG_DIR | FLAG_HIDDEN | FLAG_SYSTEM),
                    (6, NO_PARENT, "stale.txt", 0),
                    (re(3, 1), 5, "link-to-a.bin", FLAG_HARDLINK),
                    (re(3, 2), 2, "c.bin", FLAG_HARDLINK),
                ],
                "chunk {chunk}"
            );
            let size_of = |id: u64| raw.iter().find(|e| e.id == id).map(|e| (e.size, e.alloc)).unwrap();
            assert_eq!(size_of(0), (7 * REC as u64, 8 * REC as u64));
            assert_eq!(size_of(3), (1000, 1024));
            assert_eq!(size_of(re(3, 2)), (1000, 1024));
            // The WOF stream's clusters are the file's size on disk.
            assert_eq!(size_of(4), (5000, 2048));

            let ix = Index::build("T:", "mft", 0, raw, ROOT_RECORD);
            let root = ix.nodes[ROOT as usize];
            // a.bin counted once, through its first link; stale.txt hidden.
            assert_eq!((root.size, root.files), (7 * REC as u64 + 1000 + 5000, 3));
            let mut top: Vec<String> = ix.list(ROOT).into_iter().map(|e| e.name).collect();
            top.sort();
            assert_eq!(top, ["$MFT", "dir", "link-to-a.bin", "wof.dll"]);
            let dir = ix.find("T:\\dir").unwrap();
            assert_eq!(ix.list(dir).len(), 2);
            assert_eq!((ix.nodes[dir as usize].size, ix.nodes[dir as usize].files), (1000, 1));
            assert_eq!(ix.search("stale", None, 10).0, 0);
            assert_eq!(ix.search("ghost", None, 10).0, 0);
        }
    }

    #[test]
    fn short_mft_run_list_is_an_error() {
        // Without the attribute list only VCN 0-4 is known, but 14 clusters are needed.
        let err = scan_volume(&mut Cursor::new(&fake_volume(false)[..]), CHUNK).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn primary_moves_to_a_link_with_a_live_folder() {
        let rec = |seq: u16, dir: bool, parent: u64, name: &str| Rec {
            in_use: true,
            is_dir: dir,
            seq,
            name: Some(Link { parent, ns: 1, name: name.into() }),
            size: 10,
            alloc: 10,
            ..Rec::default()
        };
        let mut recs = vec![Rec::default(); 8];
        recs[5] = rec(5, true, re(5, 5), ".");
        recs[6] = rec(6, false, re(2, 1), "gone.txt");
        recs[7] = rec(7, true, re(5, 5), "folder");
        let links = vec![(6, Link { parent: re(7, 7), ns: 1, name: "kept.txt".into() })];
        let raw = entries(recs, links);
        let primary = raw.iter().find(|e| e.id == 6).unwrap();
        // Its other link has no live folder, so no bytes are counted twice or lost.
        assert_eq!((primary.parent, primary.name.as_str(), primary.flags), (7, "kept.txt", 0));
        let extra = raw.iter().find(|e| e.id == re(6, 1)).unwrap();
        assert_eq!((extra.parent, extra.flags), (NO_PARENT, FLAG_HARDLINK));
    }

    #[test]
    fn placeholders_are_told_from_links_by_tag() {
        let folder = |reparse: u32| flags_of(&Rec { in_use: true, is_dir: true, attrs: 0x410, reparse, ..Rec::default() });
        // IO_REPARSE_TAG_CLOUD_6 and IO_REPARSE_TAG_MOUNT_POINT.
        assert_eq!(folder(0x9000_601A), FLAG_DIR | FLAG_REPARSE | FLAG_NOT_LINK);
        assert_eq!(folder(0xA000_0003), FLAG_DIR | FLAG_REPARSE);
        assert_eq!(folder(0), FLAG_DIR | FLAG_REPARSE);
        // Tags that are no links but not cloud files either: ProjFS, WCI, DFS.
        for tag in [0x9000_001C, 0x8000_0018, 0x8000_000A] {
            assert_eq!(folder(tag), FLAG_DIR | FLAG_REPARSE, "{tag:#x}");
        }
        // A placeholder file gets no FLAG_NOT_LINK, as in a walk: files are never followed.
        let file = flags_of(&Rec { in_use: true, attrs: 0x400, reparse: 0x9000_001A, ..Rec::default() });
        assert_eq!(file, FLAG_REPARSE);
    }

    #[test]
    fn corrupt_records_never_panic() {
        let long: String = "x".repeat(200);
        let samples = [
            record(
                REC,
                3,
                IN_USE,
                0,
                &[
                    std_info(3, 0x420),
                    file_name(re(2, 2), 3, "a.bin"),
                    file_name(re(5, 5), 1, "b.bin"),
                    data_attr("", (0, 1), [1024, 1000, 1000], &[0x11, 2, 0x50], Some(512)),
                    data_attr("WofCompressedData", (0, 3), [2048, 1900, 1900], &[0x11, 4, 0x60], None),
                    resident_attr(ATTR_REPARSE_POINT, "", &[1, 2, 3, 4]),
                ],
            ),
            record(4096, 1, IN_USE, re(9, 1), &[file_name(re(5, 5), 1, &long), resident_attr(ATTR_DATA, "", b"hello")]),
        ];
        for good in samples {
            assert!(parse_record(&mut good.clone()).is_some());
            for at in 0..good.len() {
                for v in [0x00, 0xFF, good[at] ^ 0x01, good[at] ^ 0x80] {
                    let mut b = good.clone();
                    b[at] = v;
                    parse_record(&mut b);
                    // Also as it would look after the fixup, so header damage gets past it.
                    let mut fixed = good.clone();
                    fixup(&mut fixed);
                    fixed[at] = v;
                    protect(&mut fixed);
                    parse_record(&mut fixed);
                }
            }
        }
    }

    #[test]
    fn corrupt_volume_never_panics() {
        let mut disk = fake_volume(true);
        // Boot sector fields, then every byte of the $MFT stream in both runs.
        let spots = (0..0x50).chain(100 * 512..105 * 512).chain(300 * 512..311 * 512);
        for at in spots {
            let old = disk[at];
            for v in [0x00, 0xFF, old ^ 0x01, old ^ 0x80] {
                disk[at] = v;
                if let Ok(raw) = scan_volume(&mut Cursor::new(&disk[..]), 1536) {
                    let ix = Index::build("T:", "mft", 0, raw, ROOT_RECORD);
                    ix.search("a", None, 10);
                }
            }
            disk[at] = old;
        }
    }
}
