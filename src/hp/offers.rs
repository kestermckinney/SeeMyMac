//! AVConference media-stream offer construction and answer parsing.
//!
//! The 0x1c message carries one binary plist per media stream; each plist holds a
//! zlib-compressed protobuf "MediaBlob". Layout reverse-engineered by iShareScreen
//! from `-[AVCMediaStreamNegotiator createOffer]`.

use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use rand::RngCore;
use std::io::{Read, Write};

/// Which codec banks to offer. Apple's answer to each bank is the reverse of its
/// label: the bank carrying "HEVC" parameters (field 1 = 123) yields H.264.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecOffer {
    /// Apple's own offer (both banks); the Mac picks.
    Both,
    /// Only the bank that yields H.264 4:2:0.
    AvcOnly,
}

fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v > 0x7f {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn fv(field: u64, v: u64, out: &mut Vec<u8>) {
    varint(field << 3, out);
    varint(v, out);
}

fn fb(field: u64, b: &[u8], out: &mut Vec<u8>) {
    varint((field << 3) | 2, out);
    varint(b.len() as u64, out);
    out.extend_from_slice(b);
}

fn read_varint(d: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    while *pos < d.len() {
        let b = d[*pos];
        *pos += 1;
        if shift < 64 {
            v |= ((b & 0x7f) as u64) << shift;
        }
        shift += 7;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// Walk one protobuf message, calling `f(field, wire_type, varint_value, bytes)`.
fn walk(d: &[u8], mut f: impl FnMut(u64, u8, u64, &[u8])) {
    let mut p = 0;
    while p < d.len() {
        let Some(tag) = read_varint(d, &mut p) else { return };
        let (field, wt) = (tag >> 3, (tag & 7) as u8);
        match wt {
            0 => match read_varint(d, &mut p) {
                Some(v) => f(field, 0, v, &[]),
                None => return,
            },
            2 => {
                let Some(n) = read_varint(d, &mut p) else { return };
                let Some(b) = d.get(p..p.saturating_add(n as usize)) else { return };
                f(field, 2, 0, b);
                p += n as usize;
            }
            1 => p += 8,
            5 => p += 4,
            _ => return,
        }
    }
}

const AUDIO_TIERS: [(u64, u64, Option<u64>); 10] = [
    (0, 40_000_000, Some(12288)),
    (0, 6_000_000, Some(131072)),
    (4074, 0, Some(16384)),
    (16, 4100, None),
    (0, 75_000_000, Some(524288)),
    (0, 20_000_000, Some(98304)),
    (4, 6500, None),
    (0, 60_000_000, Some(262144)),
    (1, 299, None),
    (0, 100_000_000, Some(1048576)),
];

const HEVC_PARAMS: &[u8] = b"FLS;MS:-1;LF:-1;CABAC;POS:0;EOD:1;HTS:2;RR:3;AR:16/9,5/8;XR:16/9,5/8;";
const AVC_PARAMS: &[u8] = b"FLS;LF:-1;POS:5;EOD:1;HTS:2;RR:3;POSE:4;AR:16/9,5/8;XR:16/9,5/8;";
/// Apple's own client requests this audio bitrate; we negotiate audio (the Mac
/// requires the section) but discard it locally.
const AUDIO_BITRATE: u64 = 24191;

fn media_blob(video: bool, session_id: u32, timestamp: u64, codec: CodecOffer) -> Vec<u8> {
    let mut desc = vec![];
    let desc_field;
    if video {
        let res = |alt: u64| {
            let mut r = vec![];
            fv(1, 1, &mut r);
            fv(2, alt, &mut r);
            fv(3, 50115, &mut r);
            fv(4, 0, &mut r);
            r
        };
        let (r1, r2) = (res(1), res(2));
        // field1 = 123 carries the HEVC parameter string but yields H.264.
        let mut yields_h264 = vec![];
        fv(1, 123, &mut yields_h264);
        for r in [&r1, &r2, &r1, &r2] {
            fb(2, r, &mut yields_h264);
        }
        fb(3, HEVC_PARAMS, &mut yields_h264);
        fv(4, 1, &mut yields_h264);
        let mut yields_hevc = vec![];
        fv(1, 100, &mut yields_hevc);
        fb(2, &r1, &mut yields_hevc);
        fb(2, &r2, &mut yields_hevc);
        fb(3, AVC_PARAMS, &mut yields_hevc);
        fv(4, 14, &mut yields_hevc);

        fv(1, session_id as u64, &mut desc);
        fv(2, 0, &mut desc); // no long-term-reference acks (HEVC only)
        fb(3, &yields_h264, &mut desc);
        if codec == CodecOffer::Both {
            fb(3, &yields_hevc, &mut desc);
        }
        fv(6, 1, &mut desc); // one self-contained picture per frame
        fv(7, 0, &mut desc);
        fv(8, 63, &mut desc);
        fv(9, 1, &mut desc);
        fv(12, 1, &mut desc);
        desc_field = 5;
    } else {
        fv(1, session_id as u64, &mut desc);
        fv(2, 0, &mut desc);
        fv(3, 0, &mut desc);
        fv(4, AUDIO_BITRATE, &mut desc);
        fv(5, 0, &mut desc);
        fv(6, 0, &mut desc);
        desc_field = 3;
    }
    let mut out = vec![];
    fv(1, 1, &mut out);
    fv(2, 1, &mut out);
    fb(desc_field, &desc, &mut out);
    fb(6, format!("SeeMyMac {}", env!("CARGO_PKG_VERSION")).as_bytes(), &mut out);
    fv(8, 0, &mut out);
    for (f1, f2, f3) in AUDIO_TIERS {
        let mut e = vec![];
        fv(1, f1, &mut e);
        fv(2, f2, &mut e);
        if let Some(f3) = f3 {
            fv(3, f3, &mut e);
        }
        fb(9, &e, &mut out);
    }
    fv(13, timestamp, &mut out);
    fv(14, 2, &mut out);
    fv(16, 0, &mut out);
    fv(18, 1, &mut out);
    out
}

fn endpoint_info() -> Vec<u8> {
    let s = |tag: u8, v: &str| {
        let b = &v.as_bytes()[..v.len().min(127)];
        let mut o = vec![tag, b.len() as u8];
        o.extend_from_slice(b);
        o
    };
    let mut o = vec![0x08, 0x00, 0x10, 0x01];
    o.extend(s(0x1a, &format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)));
    o.extend(s(0x22, "1.0.0"));
    o.extend(s(0x2a, "0"));
    o
}

pub fn uuid_v4() -> [u8; 16] {
    let mut u = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut u);
    u[6] = (u[6] & 0x0f) | 0x40;
    u[8] = (u[8] & 0x3f) | 0x80;
    u
}

fn uuid_string(u: &[u8; 16]) -> String {
    let h: String = u.iter().map(|b| format!("{b:02X}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn plist_for(blob: &[u8], mode: i64) -> Vec<u8> {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
    z.write_all(blob).unwrap();
    let mut d = plist::Dictionary::new();
    d.insert("avcMediaStreamOptionRemoteEndpointInfo".into(), plist::Value::Data(endpoint_info()));
    d.insert("avcMediaStreamNegotiatorMode".into(), plist::Value::Integer(mode.into()));
    d.insert("avcMediaStreamNegotiatorMediaBlob".into(), plist::Value::Data(z.finish().unwrap()));
    d.insert("avcMediaStreamOptionCallID".into(), plist::Value::String(uuid_string(&uuid_v4())));
    let mut out = vec![];
    plist::Value::Dictionary(d).to_writer_binary(&mut out).unwrap();
    out
}

pub struct Offers {
    pub video: Vec<u8>,
    pub audio: Vec<u8>,
    /// The SSRCs we advertise (Apple only accepts RTCP/RTP from these).
    pub video_ssrc: u32,
    pub audio_ssrc: u32,
    /// A second video stream (empty when not offered), for a second virtual display.
    pub video2: Vec<u8>,
    pub video2_ssrc: u32,
}

/// `second_video`: also offer a second video stream (for two virtual displays).
pub fn create(codec: CodecOffer, second_video: bool) -> Offers {
    let mut r = rand::thread_rng();
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let (vs, as_) = (r.next_u32(), r.next_u32());
    let vs2 = r.next_u32();
    let video2 = if second_video { plist_for(&media_blob(true, vs2, ts, codec), 7) } else { vec![] };
    Offers { video: plist_for(&media_blob(true, vs, ts, codec), 7), audio: plist_for(&media_blob(false, as_, ts, codec), 8), video_ssrc: vs, audio_ssrc: as_, video2, video2_ssrc: vs2 }
}

/// Parse (canvas width, height, tiles) out of the Mac's 0x1c answer, which arrives
/// framed as a FramebufferUpdate containing an embedded binary plist.
pub fn canvas_dims(msg: &[u8]) -> (u32, u32, u32) {
    if msg.first() != Some(&0) {
        return (0, 0, 0);
    }
    let mut idx = 0;
    while let Some(off) = find(&msg[idx..], b"bplist") {
        let start = idx + off;
        idx = start + 6;
        // Find the end of this plist from its 32-byte trailer.
        for end in start + 40..=msg.len() {
            let t = &msg[end - 32..end];
            let (osz, rsz) = (t[6], t[7]);
            if t[..6] != [0; 6] || !(1..=8).contains(&osz) || !(1..=8).contains(&rsz) {
                continue;
            }
            let count = u64::from_be_bytes(t[8..16].try_into().unwrap());
            let table = u64::from_be_bytes(t[24..32].try_into().unwrap());
            if count == 0 || table < 8 || table.checked_add(count.saturating_mul(osz as u64)) != Some((end - start - 32) as u64) {
                continue;
            }
            let Ok(v) = plist::Value::from_reader(std::io::Cursor::new(&msg[start..end])) else { continue };
            let Some(blob) = v.as_dictionary().and_then(|d| d.get("avcMediaStreamNegotiatorMediaBlob")).and_then(|b| b.as_data()) else { break };
            let mut dec = vec![];
            if ZlibDecoder::new(blob).take(1 << 20).read_to_end(&mut dec).is_err() {
                break;
            }
            let (mut w, mut h, mut tiles) = (0, 0, 0);
            walk(&dec, |f, wt, _, b| {
                if f == 5 && wt == 2 {
                    walk(b, |sf, swt, v, _| {
                        if swt == 0 {
                            match sf {
                                4 => w = v as u32,
                                5 => h = v as u32,
                                6 => tiles = v as u32,
                                _ => {}
                            }
                        }
                    });
                }
            });
            if w > 0 && h > 0 {
                return (w, h, tiles);
            }
            break;
        }
    }
    (0, 0, 0)
}

pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob_of(plist_bytes: &[u8]) -> Vec<u8> {
        let v = plist::Value::from_reader(std::io::Cursor::new(plist_bytes)).unwrap();
        let d = v.as_dictionary().unwrap();
        assert_eq!(d.keys().next().unwrap(), "avcMediaStreamOptionRemoteEndpointInfo");
        let z = d.get("avcMediaStreamNegotiatorMediaBlob").unwrap().as_data().unwrap();
        let mut out = vec![];
        ZlibDecoder::new(z).read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn offers_advertise_their_ssrcs_and_codec_banks() {
        for (codec, banks) in [(CodecOffer::Both, 2), (CodecOffer::AvcOnly, 1)] {
            let o = create(codec, false);
            let v = blob_of(&o.video);
            let (mut ssrc, mut n_banks, mut tiles) = (None, 0, 0);
            walk(&v, |f, wt, _, b| {
                if f == 5 && wt == 2 {
                    walk(b, |sf, swt, val, _| match (sf, swt) {
                        (1, 0) => ssrc = Some(val as u32),
                        (3, 2) => n_banks += 1,
                        (6, 0) => tiles = val,
                        _ => {}
                    });
                }
            });
            assert_eq!((ssrc, n_banks, tiles), (Some(o.video_ssrc), banks, 1));
            let a = blob_of(&o.audio);
            let mut assrc = None;
            walk(&a, |f, wt, _, b| {
                if f == 3 && wt == 2 {
                    walk(b, |sf, _, val, _| {
                        if sf == 1 {
                            assrc = Some(val as u32)
                        }
                    });
                }
            });
            assert_eq!(assrc, Some(o.audio_ssrc));
        }
    }

    #[test]
    fn canvas_dims_are_read_from_an_embedded_answer() {
        let mut inner = vec![];
        fv(4, 1920, &mut inner);
        fv(5, 1080, &mut inner);
        fv(6, 1, &mut inner);
        let mut blob = vec![];
        fv(1, 1, &mut blob);
        fb(5, &inner, &mut blob);
        let pl = plist_for(&blob, 7);
        let mut msg = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0xf2];
        msg.extend_from_slice(&pl);
        msg.extend_from_slice(b"trailing section bytes");
        assert_eq!(canvas_dims(&msg), (1920, 1080, 1));
        assert_eq!(canvas_dims(&[0, 0, 0, 0]), (0, 0, 0));
        assert_eq!(canvas_dims(&[0x14, 0, 0, 0]), (0, 0, 0));
    }
}
