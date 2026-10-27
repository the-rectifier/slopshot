//! Canon CR3 reader. A CR3 is an ISO base media file (the MP4 container): the
//! metadata sits in a Canon `uuid` box inside `moov` as small self-contained TIFF
//! blocks (CMT1 = IFD0, CMT2 = Exif IFD, CMT3 = Canon MakerNote) next to a 160px
//! thumbnail (THMB). A ~1620px preview has its own top-level box (PRVW), the
//! in-camera star rating lives in the XMP box, and track 1 is a full-size camera
//! JPEG stored in `mdat`. Box layout per https://github.com/lclevy/canon_cr3.
//!
//! Like the ARW path, this never touches the raw data: the boxes before `mdat`
//! come in with the initial read, and only the chosen JPEG is read after that.

use crate::raw::{Exif, Focus, MAX_JPEG, Pick, RawMeta, Tiff, Walk, pick_jpeg, walk};

const CANON_UUID: [u8; 16] = *b"\x85\xc0\xb6\x87\x82\x0f\x11\xe0\x81\x11\xf4\xce\x46\x2b\x6a\x48";
const XMP_UUID: [u8; 16] = *b"\xbe\x7a\xcf\xcb\x97\xa9\x42\xe8\x9c\x71\x99\x94\x91\xe3\xaf\xac";
const PREVIEW_UUID: [u8; 16] = *b"\xea\xf4\x2b\x5e\x1c\x98\x4b\x88\xb9\xfb\xb7\xdc\x40\x6e\x4d\x16";

/// Largest box read whole (moov, XMP). Real ones are around 100 KB.
const MAX_BOX: u64 = 16 << 20;
/// Bytes past the end of `moov` pulled in with it: covers the XMP box and the
/// PRVW header that follow it, so they cost no extra read.
const TAIL: u64 = 64 << 10;

fn be16(b: &[u8]) -> u32 {
    u16::from_be_bytes([b[0], b[1]]) as u32
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}

/// One box inside a buffer: its type and the byte range of its payload.
struct Bx {
    typ: [u8; 4],
    body: usize,
    end: usize,
}

/// Boxes laid end to end in `buf[start..end]`, stopping at anything malformed.
fn boxes(buf: &[u8], start: usize, end: usize) -> Vec<Bx> {
    let end = end.min(buf.len());
    let mut out = Vec::new();
    let mut off = start;
    while off + 8 <= end && out.len() < 256 {
        let room = (end - off) as u64;
        let (hdr, size) = match be32(&buf[off..]) {
            0 => (8, room),
            1 if off + 16 <= end => (16, be64(&buf[off + 8..])),
            1 => break,
            s => (8, s as u64),
        };
        if size < hdr as u64 || size > room {
            break;
        }
        let typ = buf[off + 4..off + 8].try_into().unwrap();
        out.push(Bx { typ, body: off + hdr, end: off + size as usize });
        off += size as usize;
    }
    out
}

/// Payload range of the first child box of type `typ`.
fn child(buf: &[u8], start: usize, end: usize, typ: &[u8; 4]) -> Option<(usize, usize)> {
    boxes(buf, start, end).into_iter().find(|b| &b.typ == typ).map(|b| (b.body, b.end))
}

/// Start of a JPEG (FF D8 FF) within `b[from..to]`.
fn find_soi(b: &[u8], from: usize, to: usize) -> Option<usize> {
    let to = to.min(b.len());
    (from..to.saturating_sub(2)).find(|&i| b[i..i + 3] == [0xFF, 0xD8, 0xFF])
}

/// An embedded JPEG: file offset, length, width, height.
type Jpeg = (u64, u64, u32, u32);

pub(crate) fn read(t: &mut Tiff, pick: Pick) -> Option<RawMeta> {
    if t.bytes(8, 4)? != b"crx " {
        return None; // some other MP4-family file
    }

    // Top-level boxes: ftyp, moov, uuid (XMP), uuid (PRVW), mdat.
    let (mut moov, mut xmp, mut prvw) = (None, None, None);
    let mut off = 0u64;
    for _ in 0..32 {
        let Some(h) = t.bytes(off, 32) else { break };
        let (hdr, size) = match be32(&h) {
            0 => break, // runs to EOF: that's mdat, nothing we need after it
            1 => (16, be64(&h[8..])),
            s => (8, s as u64),
        };
        if size < hdr + 8 {
            break;
        }
        match &h[4..8] {
            b"moov" => {
                moov = Some((off + hdr, size - hdr));
                // `size` is straight from the file (up to 64 bits), so every sum saturates.
                t.extend_head(off.saturating_add(size).saturating_add(TAIL).min(MAX_BOX) as usize);
            }
            b"uuid" if size >= hdr + 16 => {
                let id = &h[hdr as usize..hdr as usize + 16];
                let body = (off + hdr + 16, size - hdr - 16);
                if id == XMP_UUID {
                    xmp = Some(body);
                } else if id == PREVIEW_UUID {
                    prvw = Some(body);
                }
            }
            b"mdat" => break,
            _ => {}
        }
        off = off.checked_add(size)?;
    }

    let (moov_off, moov_len) = moov.filter(|m| m.1 <= MAX_BOX)?;
    let m = t.bytes(moov_off, moov_len as usize)?;
    let mut st = Walk::default();
    let mut jpegs: Vec<Jpeg> = Vec::new();
    let mut tracks = Vec::new();
    for b in boxes(&m, 0, m.len()) {
        match &b.typ {
            b"uuid" if m.get(b.body..b.body + 16) == Some(&CANON_UUID[..]) => {
                for c in boxes(&m, b.body + 16, b.end) {
                    let block = &m[c.body..c.end];
                    match &c.typ {
                        b"CMT1" | b"CMT2" | b"CMT3" => {
                            let mut tiff = Tiff::mem(block.to_vec());
                            let Some(ifd) = tiff.header() else { continue };
                            match &c.typ {
                                b"CMT1" => walk(&mut tiff, ifd, true, false, &mut st),
                                b"CMT2" => walk(&mut tiff, ifd, false, false, &mut st),
                                _ => _ = canon_makernote(&mut tiff, ifd, &mut st.exif),
                            }
                        }
                        b"THMB" => {
                            if let Some((at, len, w, h)) = thmb(block) {
                                jpegs.push((moov_off + (c.body + at) as u64, len, w, h));
                            }
                        }
                        _ => {}
                    }
                }
            }
            b"trak" => tracks.extend(track(&m, b.body, b.end)),
            _ => {}
        }
    }

    // Full-size JPEG track(s): tagged by a JPEG box in their sample description.
    // If no track is tagged, sniff the first bytes of each (raw tracks aren't JPEG).
    let tagged = tracks.iter().any(|tr| tr.4);
    for (off, len, w, h, is_jpeg) in tracks {
        if is_jpeg || (!tagged && t.bytes(off, 3).is_some_and(|b| b == [0xFF, 0xD8, 0xFF])) {
            jpegs.push((off, len, w, h));
        }
    }
    if let Some((body, len)) = prvw {
        jpegs.extend(prvw_jpeg(t, body, len));
    }
    for j in jpegs.iter_mut().filter(|j| j.2 == 0 || j.3 == 0) {
        (j.2, j.3) = t.jpeg_dims(j.0).unwrap_or((0, 0));
    }

    // Size shown at 100% before the full JPEG loads: IFD0 says the output size,
    // the full-size JPEG agrees; take the larger in case one is missing.
    let e = &mut st.exif;
    if let Some(&(_, _, w, h)) = jpegs.iter().max_by_key(|j| j.2 as u64 * j.3 as u64)
        && w as u64 * h as u64 > e.width as u64 * e.height as u64
    {
        (e.width, e.height) = (w, h);
    }
    let xmp_rating = xmp.filter(|x| x.1 <= MAX_BOX).and_then(|(off, len)| rating(&t.bytes(off, len as usize)?));
    e.rating = xmp_rating.or(st.rating.filter(|r| (1..=5).contains(r)).map(|r| r as u8));

    let jpegs = jpegs.into_iter().filter(|j| j.1 < MAX_JPEG).map(|(off, len, w, _)| (off, len, w)).collect();
    let (chosen, full) = pick_jpeg(jpegs, pick);
    st.exif.full_jpeg = full;
    let preview = chosen.and_then(|(off, len)| t.bytes(off, len as usize));
    Some(st.finish(preview))
}

/// THMB: version + flags, u16 width, u16 height, u32 JPEG size, 4 more bytes, JPEG.
/// Returns (JPEG start within the block, length, width, height).
fn thmb(b: &[u8]) -> Option<(usize, u64, u32, u32)> {
    let (w, h, declared) = (be16(b.get(4..6)?), be16(b.get(6..8)?), be32(b.get(8..12)?) as usize);
    let at = find_soi(b, 12, 48)?;
    let room = b.len() - at;
    let len = if declared > 0 && declared <= room { declared } else { room };
    Some((at, len as u64, w, h))
}

/// The preview uuid holds a PRVW box: u32, u16, u16 width, u16 height, u16,
/// u32 JPEG size, JPEG. `body`/`len` is the uuid payload after the 16-byte id.
fn prvw_jpeg(t: &mut Tiff, body: u64, len: u64) -> Option<Jpeg> {
    let b = t.bytes(body, len.min(64) as usize)?;
    let tag = b.windows(4).position(|w| w == b"PRVW")?;
    let (w, h) = (be16(b.get(tag + 10..)?), be16(b.get(tag + 12..)?));
    let declared = be32(b.get(tag + 16..tag + 20)?) as u64;
    let at = if b.get(tag + 20..tag + 23) == Some(&[0xFF, 0xD8, 0xFF]) { tag + 20 } else { find_soi(&b, tag, b.len())? };
    let room = len - at as u64;
    let jlen = if declared > 0 && declared <= room { declared } else { room };
    Some((body + at as u64, jlen, w, h))
}

/// A track's first sample as (file offset, size, width, height, tagged JPEG),
/// from trak/mdia/minf/stbl: stsd (sample description), stsz and co64/stco.
fn track(m: &[u8], body: usize, end: usize) -> Option<(u64, u64, u32, u32, bool)> {
    let (b, e) = child(m, body, end, b"mdia")?;
    let (b, e) = child(m, b, e, b"minf")?;
    let (b, e) = child(m, b, e, b"stbl")?;

    // stsd: version/flags, entry count, then the first sample entry. Canon's CRAW
    // entry is a VisualSampleEntry (width/height at +32/+34) with child boxes
    // from +90: JPEG on the full-size JPEG track, CMP1/CDI1 on the raw tracks.
    let (sd, sd_end) = child(m, b, e, b"stsd")?;
    let entry = sd + 8;
    let entry_end = entry.checked_add(be32(m.get(entry..entry + 4)?) as usize)?.min(sd_end);
    let (mut w, mut h, mut is_jpeg) = (0, 0, false);
    if m.get(entry + 4..entry + 8) == Some(b"CRAW") && entry + 36 <= entry_end {
        (w, h) = (be16(&m[entry + 32..]), be16(&m[entry + 34..]));
        is_jpeg = boxes(m, entry + 90, entry_end).iter().any(|c| &c.typ == b"JPEG");
    }

    // stsz: version/flags, uniform sample size (0 = per-sample table follows), count.
    let (sz, _) = child(m, b, e, b"stsz")?;
    let size = match be32(m.get(sz + 4..sz + 8)?) {
        0 => be32(m.get(sz + 12..sz + 16)?),
        s => s,
    } as u64;
    // co64 / stco: version/flags, count, chunk offsets.
    let off = match child(m, b, e, b"co64") {
        Some((co, _)) => be64(m.get(co + 8..co + 16)?),
        None => be32(m.get(child(m, b, e, b"stco")?.0 + 8..)?.get(..4)?) as u64,
    };
    (off > 0 && size > 0).then_some((off, size, w, h, is_jpeg))
}

/// `xmp:Rating` from an XMP packet, as an element or an attribute. Only 1–5 count:
/// 0 is unrated and -1 is some apps' "rejected".
fn rating(xmp: &[u8]) -> Option<u8> {
    const KEY: &str = "xmp:Rating";
    let s = String::from_utf8_lossy(xmp);
    let mut rest = &*s;
    while let Some(i) = rest.find(KEY) {
        rest = &rest[i + KEY.len()..];
        let v = rest.trim_start();
        let v = match v.as_bytes().first() {
            Some(b'>') => &v[1..],
            Some(b'=') => v[1..].trim_start().trim_start_matches(['"', '\'']),
            _ => continue, // e.g. a longer property name
        };
        let end = v.find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.')).unwrap_or(v.len());
        let n = v[..end].parse::<f32>().ok()?;
        return (1.0..=5.0).contains(&n).then_some(n as u8);
    }
    None
}

/// Canon's MakerNote is a plain IFD, here in its own TIFF block (CMT3) so offsets
/// are relative to that. Only the AF info is read.
fn canon_makernote(t: &mut Tiff, off: u64, exif: &mut Exif) -> Option<()> {
    let nb = t.bytes(off, 2)?;
    let n = t.u16(&nb) as usize;
    if n == 0 || n > 1000 {
        return None;
    }
    let block = t.bytes(off + 2, n * 12)?;
    let (mut af2, mut af3) = (None, None);
    for e in block.chunks_exact(12) {
        let (tag, typ, count) = (t.u16(&e[0..2]), t.u16(&e[2..4]), t.u32(&e[4..8]) as usize);
        // int16 arrays, always longer than 4 bytes so stored at an offset.
        if typ != 3 || !(8..=8192).contains(&count) {
            continue;
        }
        let read = |t: &mut Tiff| {
            let b = t.bytes(t.u32(&e[8..12]) as u64, count * 2)?;
            Some(b.chunks_exact(2).map(|c| t.u16(c)).collect::<Vec<u16>>())
        };
        match tag {
            0x0026 => af2 = read(t),
            0x003C => af3 = read(t), // AFInfo3: same layout, replaces AFInfo2 where present
            _ => {}
        }
    }
    exif.focus = af3.or(af2).and_then(|v| af_focus(&v));
    Some(())
}

/// Canon AFInfo2: size, AF area mode, NumAFPoints (N), ValidAFPoints, image
/// width/height, AF image width/height, then N widths, N heights, N X and N Y
/// positions (int16, relative to the image centre, +Y up on EOS bodies), then the
/// in-focus points as a bitmask (point i = bit i%16 of word i/16).
///
/// Face/eye tracking reports one valid point, the face box; zone and whole-area
/// AF report a grid with the in-focus points flagged. The focus frame is the
/// bounding box of the in-focus points (or the only point, if there's just one).
fn af_focus(v: &[u16]) -> Option<Focus> {
    let (mode, n, valid) = (*v.get(1)?, *v.get(2)? as usize, *v.get(3)? as usize);
    let (mut iw, mut ih) = (*v.get(6)? as f32, *v.get(7)? as f32);
    if iw == 0.0 || ih == 0.0 {
        (iw, ih) = (*v.get(4)? as f32, *v.get(5)? as f32);
    }
    if mode == 0 || n == 0 || iw == 0.0 || ih == 0.0 {
        return None; // manual focus, or no AF data
    }
    let arr = |k: usize| v.get(8 + k * n..8 + (k + 1) * n);
    let (ws, hs, xs, ys) = (arr(0)?, arr(1)?, arr(2)?, arr(3)?);
    let bits = v.get(8 + 4 * n..8 + 4 * n + n.div_ceil(16))?;
    let valid = valid.min(n);
    let mut pts: Vec<usize> = (0..valid).filter(|&i| (bits[i / 16] >> (i % 16)) & 1 == 1).collect();
    if pts.is_empty() && valid == 1 {
        pts.push(0);
    }
    let signed = |x: u16| x as i16 as f32;
    pts.retain(|&i| signed(ws[i]) > 0.0 && signed(hs[i]) > 0.0);
    if pts.is_empty() {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for i in pts {
        let (cx, cy) = (iw / 2.0 + signed(xs[i]), ih / 2.0 - signed(ys[i]));
        let (hw, hh) = (signed(ws[i]) / 2.0, signed(hs[i]) / 2.0);
        (x0, y0, x1, y1) = (x0.min(cx - hw), y0.min(cy - hh), x1.max(cx + hw), y1.max(cy + hh));
    }
    Some(Focus { sensor: (iw, ih), at: ((x0 + x1) / 2.0, (y0 + y1) / 2.0), frame: Some((x1 - x0, y1 - y0)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn walks_boxes() {
        let mut buf = bx(b"aaaa", &[1, 2, 3]);
        buf.extend(bx(b"bbbb", &[]));
        buf.extend([0, 0, 0, 99, b'c', b'c']); // truncated: ignored
        let b = boxes(&buf, 0, buf.len());
        assert_eq!(b.len(), 2);
        assert_eq!((&b[0].typ, b[0].body, b[0].end), (b"aaaa", 8, 11));
        assert_eq!((&b[1].typ, b[1].body, b[1].end), (b"bbbb", 19, 19));
    }

    #[test]
    fn xmp_rating_forms() {
        assert_eq!(rating(b"<rdf:Description><xmp:Rating>3</xmp:Rating>"), Some(3));
        assert_eq!(rating(b"<rdf:Description xmp:Rating=\"5\" xmp:Label=''/>"), Some(5));
        assert_eq!(rating(b"<xmp:Rating>0</xmp:Rating>"), None);
        assert_eq!(rating(b"<xmp:Rating>-1</xmp:Rating>"), None);
        assert_eq!(rating(b"<foo:RatingPercent>80</foo:RatingPercent>"), None);
    }

    /// AFInfo2 from ExifTool's EOS M50 CR3 sample (t/images/CanonRaw.cr3): Face +
    /// Tracking, 143 points of which 1 valid: a 337x299 face box at (+392, -327).
    #[test]
    fn af_face_box() {
        let n = 143;
        let mut v = vec![1216, 6, n as u16, 1, 6000, 4000, 6000, 4000];
        let arr = |first: i16| {
            let mut a = vec![0u16; n];
            a[0] = first as u16;
            a
        };
        let (w, h, x, y) = (arr(337), arr(299), arr(392), arr(-327));
        v.extend(w.iter().chain(&h).chain(&x).chain(&y));
        let mut bits = vec![0u16; 9];
        bits[0] = 1;
        v.extend(&bits); // in focus
        v.extend(&bits); // selected
        let f = af_focus(&v).unwrap();
        assert_eq!(f.sensor, (6000.0, 4000.0));
        assert_eq!(f.at, (3392.0, 2327.0));
        assert_eq!(f.frame, Some((337.0, 299.0)));
    }

    /// Grid AF (EOS RP layout, 13x11 points): the frame spans the in-focus points.
    #[test]
    fn af_grid_union() {
        let (cols, rows) = (13usize, 11usize);
        let n = cols * rows;
        let mut v = vec![0, 6, n as u16, n as u16, 6240, 4160, 6240, 4160];
        v.extend(std::iter::repeat_n(378u16, n));
        v.extend(std::iter::repeat_n(324u16, n));
        v.extend((0..n).map(|i| ((i % cols) as i16 * 420 - 2520) as u16));
        v.extend((0..n).map(|i| (1806 - (i / cols) as i16 * 361) as u16));
        let mut bits = vec![0u16; n.div_ceil(16)];
        for i in [0, 1, 13, 14] {
            bits[i / 16] |= 1 << (i % 16); // top-left 2x2
        }
        v.extend(&bits);
        let f = af_focus(&v).unwrap();
        // Points at x = -2520/-2100, y = 1806/1445 → pixels 600..1020, 274..635, ±half size.
        let (x0, x1) = (3120.0 - 2520.0 - 189.0, 3120.0 - 2100.0 + 189.0);
        let (y0, y1) = (2080.0 - 1806.0 - 162.0, 2080.0 - 1445.0 + 162.0);
        assert_eq!(f.at, ((x0 + x1) / 2.0, (y0 + y1) / 2.0));
        assert_eq!(f.frame, Some((x1 - x0, y1 - y0)));
    }

    #[test]
    fn af_manual_focus_or_nothing_in_focus() {
        let mut v = vec![0, 0, 1, 1, 100, 100, 100, 100, 10, 10, 0, 0, 0];
        assert!(af_focus(&v).is_none()); // MF
        v[1] = 7;
        assert_eq!(af_focus(&v).unwrap().at, (50.0, 50.0)); // a lone point counts even if not flagged
        let mut grid = vec![0, 7, 2, 2, 100, 100, 100, 100, 10, 10, 10, 10, 0, 0, 0, 0, 0];
        assert!(af_focus(&grid).is_none());
        grid[16] = 0b10;
        assert_eq!(af_focus(&grid).unwrap().frame, Some((10.0, 10.0)));
    }

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 50)
            .encode(&vec![90u8; (w * h * 3) as usize], w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    /// A TIFF block with SHORT/ASCII entries, as in CMT1.
    fn tiff(entries: &[(u16, &[u8])]) -> Vec<u8> {
        let mut v = b"II*\0\x08\0\0\0".to_vec();
        let data_at = 8 + 2 + entries.len() * 12 + 4;
        let mut data: Vec<u8> = Vec::new();
        v.extend((entries.len() as u16).to_le_bytes());
        for &(tag, val) in entries {
            let (typ, count) = if val.len() == 2 { (3u16, 1u32) } else { (2, val.len() as u32) };
            v.extend(tag.to_le_bytes());
            v.extend(typ.to_le_bytes());
            v.extend(count.to_le_bytes());
            if val.len() <= 4 {
                v.extend(val);
                v.extend(std::iter::repeat_n(0, 4 - val.len()));
            } else {
                v.extend(((data_at + data.len()) as u32).to_le_bytes());
                data.extend(val);
            }
        }
        v.extend([0; 4]);
        v.extend(data);
        v
    }

    /// One track: trak/mdia/minf/stbl with a CRAW sample entry (tagged JPEG or
    /// not), sample size and chunk offset.
    fn trak(w: u16, h: u16, jpeg_tag: bool, size: u32, off: u64) -> Vec<u8> {
        let mut entry = vec![0u8; 24];
        entry.extend(w.to_be_bytes());
        entry.extend(h.to_be_bytes());
        entry.extend([0u8; 54]); // child boxes start 90 bytes into the entry
        entry.extend(bx(if jpeg_tag { b"JPEG" } else { b"CMP1" }, &[0; 4]));
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend(bx(b"CRAW", &entry));
        let stsz = [&[0u8; 4][..], &size.to_be_bytes(), &1u32.to_be_bytes()].concat();
        let co64 = [&[0u8; 4][..], &1u32.to_be_bytes(), &off.to_be_bytes()].concat();
        let stbl = [bx(b"stsd", &stsd), bx(b"stsz", &stsz), bx(b"co64", &co64)].concat();
        bx(b"trak", &bx(b"mdia", &bx(b"minf", &bx(b"stbl", &stbl))))
    }

    /// A small but complete CR3: ftyp, moov (Canon uuid with CMT1 + a raw track
    /// + the full-size JPEG track), XMP uuid, PRVW uuid, mdat.
    fn fake_cr3(prvw: &[u8], full: &[u8], tag_full: bool) -> Vec<u8> {
        let ftyp = bx(b"ftyp", b"crx \0\0\0\x01crx isom");
        let cmt1 = tiff(&[(0x010F, b"Canon\0"), (0x0110, b"Canon EOS RP\0"), (0x0112, &6u16.to_le_bytes())]);
        let canon = bx(b"uuid", &[&CANON_UUID[..], &bx(b"CMT1", &cmt1)].concat());
        let xmp = bx(b"uuid", &[&XMP_UUID[..], b"<rdf:Description><xmp:Rating>3</xmp:Rating>"].concat());
        let mut pv = b"\0\0\0\0\0\0\0\x01".to_vec();
        let mut body = vec![0, 0, 0, 0, 0, 1];
        body.extend(1620u16.to_be_bytes());
        body.extend(1080u16.to_be_bytes());
        body.extend([0, 1]);
        body.extend((prvw.len() as u32).to_be_bytes());
        body.extend(prvw);
        pv.extend(bx(b"PRVW", &body));
        let prvw_box = bx(b"uuid", &[&PREVIEW_UUID[..], &pv].concat());
        // moov's size doesn't depend on the offsets, so lay it out once to learn where mdat lands.
        let moov = |raw_off: u64, full_off: u64| {
            let tracks = [trak(6000, 4000, false, 64, raw_off), trak(3000, 2000, tag_full, full.len() as u32, full_off)];
            bx(b"moov", &[canon.clone(), tracks.concat()].concat())
        };
        let mdat_body = ftyp.len() + moov(0, 0).len() + xmp.len() + prvw_box.len() + 8;
        let (raw_off, full_off) = (mdat_body as u64, mdat_body as u64 + 64);
        let mdat = bx(b"mdat", &[&[0x42u8; 64][..], full].concat()); // 64 bytes of "raw data", then the JPEG
        [ftyp, moov(raw_off, full_off), xmp, prvw_box, mdat].concat()
    }

    fn read_bytes(name: &str, bytes: &[u8], pick: Pick) -> Option<RawMeta> {
        let path = std::env::temp_dir().join(format!("slopshot-test-{}-{name}.CR3", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let m = crate::raw::read(&path, pick);
        let _ = std::fs::remove_file(&path);
        m
    }

    #[test]
    fn reads_a_whole_cr3() {
        let (prvw, full) = (jpeg(1620, 1080), jpeg(3000, 2000));
        for tagged in [true, false] {
            let file = fake_cr3(&prvw, &full, tagged);
            let m = read_bytes(&format!("whole-{tagged}"), &file, Pick::Preview).unwrap();
            assert_eq!(m.preview.as_deref(), Some(&prvw[..]), "browsing uses PRVW");
            assert_eq!((m.exif.width, m.exif.height, m.orientation), (3000, 2000, 6));
            assert_eq!(m.exif.camera().as_deref(), Some("Canon EOS RP"));
            assert_eq!((m.exif.rating, m.exif.full_jpeg), (Some(3), true));
            let m = read_bytes(&format!("full-{tagged}"), &file, Pick::FullSize).unwrap();
            assert_eq!(m.preview.as_deref(), Some(&full[..]), "zoom uses the full-size track (tagged: {tagged})");
        }
    }

    /// From the review: a moov with a 64-bit size near u64::MAX used to overflow
    /// `off + size` (a panic with debug overflow checks, i.e. in `cargo test`).
    #[test]
    fn cursed_box_size_is_rejected_not_a_panic() {
        let mut file = bx(b"ftyp", b"crx \0\0\0\x01crx isom");
        file.extend(1u32.to_be_bytes());
        file.extend(b"moov");
        file.extend(0xFFFF_FFFF_FFFF_FFF0u64.to_be_bytes());
        file.resize(120, 0);
        for pick in [Pick::Preview, Pick::FullSize] {
            assert!(read_bytes("cursed", &file, pick).is_none());
        }
    }
}
