//! Ciphers used by Apple High Performance Screen Sharing.
//!
//! * enc1103: the AES-128-CBC record layer that wraps every RFB control message
//!   after authentication, with a plain SHA-1 tag over `u32 counter || record`.
//! * SRTP/SRTCP (RFC 3711) with AES-256-CTR and HMAC-SHA1-80, keyed from the
//!   46-byte blobs we put in the 0x1c media offer.

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use aes::{Aes128, Aes256};
use hmac::{Hmac, Mac};
use sha1::{Digest, Sha1};
use std::collections::HashMap;

pub fn aes128_ecb_encrypt(key: &[u8; 16], data: &mut [u8]) {
    let c = Aes128::new(GenericArray::from_slice(key));
    for b in data.chunks_exact_mut(16) {
        c.encrypt_block(GenericArray::from_mut_slice(b));
    }
}

pub fn aes128_ecb_decrypt(key: &[u8; 16], data: &mut [u8]) {
    let c = Aes128::new(GenericArray::from_slice(key));
    for b in data.chunks_exact_mut(16) {
        c.decrypt_block(GenericArray::from_mut_slice(b));
    }
}

// ----- enc1103 ------------------------------------------------------------------

const MAC_LEN: usize = 20;
/// Apple sometimes skips a few counters; accept a MAC anywhere in this window.
const COUNTER_WINDOW: u32 = 6;

fn record_mac(counter: u32, body: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(counter.to_be_bytes());
    h.update(body);
    h.finalize().into()
}

/// Keys unwrapped from the server's 36-byte encoding-1103 rectangle.
#[derive(Clone)]
pub struct Enc1103Keys {
    pub cbc_key: [u8; 16],
    iv: [u8; 16],
}

impl Enc1103Keys {
    pub fn unwrap(blob: &[u8; 36], ecb_key: &[u8; 16]) -> Self {
        let mut k = [0u8; 32];
        k.copy_from_slice(&blob[4..36]);
        aes128_ecb_decrypt(ecb_key, &mut k);
        let (mut cbc_key, mut iv) = ([0u8; 16], [0u8; 16]);
        cbc_key.copy_from_slice(&k[..16]);
        iv.copy_from_slice(&k[16..]);
        Self { cbc_key, iv }
    }

    pub fn sender(&self) -> Enc1103Tx {
        Enc1103Tx { aes: Aes128::new(GenericArray::from_slice(&self.cbc_key)), prev: self.iv, counter: 0 }
    }

    pub fn receiver(&self) -> Enc1103Rx {
        Enc1103Rx { aes: Aes128::new(GenericArray::from_slice(&self.cbc_key)), prev: self.iv, counter: 0, pending: Vec::new() }
    }
}

/// Client -> server half. The CBC chain and counter continue across records.
pub struct Enc1103Tx {
    aes: Aes128,
    prev: [u8; 16],
    counter: u32,
}

impl Enc1103Tx {
    pub fn seal(&mut self, msg: &[u8]) -> Vec<u8> {
        let pad = (16 - (2 + msg.len() + MAC_LEN) % 16) % 16;
        let mut block = Vec::with_capacity(2 + msg.len() + pad + MAC_LEN);
        block.extend_from_slice(&(msg.len() as u16).to_be_bytes());
        block.extend_from_slice(msg);
        block.resize(2 + msg.len() + pad, 0);
        let mac = record_mac(self.counter, &block);
        block.extend_from_slice(&mac);
        self.counter = self.counter.wrapping_add(1);
        for b in block.chunks_exact_mut(16) {
            for (x, p) in b.iter_mut().zip(self.prev) {
                *x ^= p;
            }
            self.aes.encrypt_block(GenericArray::from_mut_slice(b));
            self.prev.copy_from_slice(b);
        }
        let mut out = Vec::with_capacity(2 + block.len());
        out.extend_from_slice(&(block.len() as u16).to_be_bytes());
        out.extend_from_slice(&block);
        out
    }
}

/// Server -> client half. Buffers partial records across TCP reads.
pub struct Enc1103Rx {
    aes: Aes128,
    prev: [u8; 16],
    counter: u32,
    pending: Vec<u8>,
}

impl Enc1103Rx {
    fn open_one(&mut self, ct: &[u8]) -> Option<Vec<u8>> {
        let mut pt = ct.to_vec();
        for b in pt.chunks_exact_mut(16) {
            let c: [u8; 16] = b.try_into().unwrap();
            self.aes.decrypt_block(GenericArray::from_mut_slice(b));
            for (x, p) in b.iter_mut().zip(self.prev) {
                *x ^= p;
            }
            self.prev = c;
        }
        let start = self.counter;
        self.counter = self.counter.wrapping_add(1);
        if pt.len() <= MAC_LEN {
            return Some(pt);
        }
        let (body, mac) = pt.split_at(pt.len() - MAC_LEN);
        for c in start.saturating_sub(1)..start + COUNTER_WINDOW {
            if record_mac(c, body)[..] == *mac {
                self.counter = c + 1;
                let n = u16::from_be_bytes([body[0], body[1]]) as usize;
                return Some(body[2..(2 + n).min(body.len())].to_vec());
            }
        }
        None
    }

    /// Feed raw TCP bytes; returns every complete, authenticated message.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(data);
        let mut out = vec![];
        let mut pos = 0;
        while pos + 2 <= self.pending.len() {
            let n = u16::from_be_bytes([self.pending[pos], self.pending[pos + 1]]) as usize;
            if n == 0 || n % 16 != 0 || pos + 2 + n > self.pending.len() {
                break;
            }
            let ct = self.pending[pos + 2..pos + 2 + n].to_vec();
            pos += 2 + n;
            if let Some(m) = self.open_one(&ct) {
                out.push(m);
            }
        }
        self.pending.drain(..pos);
        out
    }
}

// ----- SRTP ---------------------------------------------------------------------

pub const SRTP_BLOB_LEN: usize = 46;
const TAG_LEN: usize = 10;
type HmacSha1 = Hmac<Sha1>;

/// RFC 3711 §4.3.1 key derivation (AES-CM, key derivation rate 0).
fn kdf(master_key: &[u8], master_salt: &[u8], label: u8, len: usize) -> Vec<u8> {
    let c = Aes256::new(GenericArray::from_slice(master_key));
    let mut iv = [0u8; 16];
    iv[..14].copy_from_slice(master_salt);
    iv[7] ^= label;
    let base = u128::from_be_bytes(iv);
    let mut out = Vec::with_capacity(len + 16);
    let mut i = 0u128;
    while out.len() < len {
        let mut b = (base.wrapping_add(i)).to_be_bytes();
        c.encrypt_block(GenericArray::from_mut_slice(&mut b));
        out.extend_from_slice(&b);
        i += 1;
    }
    out.truncate(len);
    out
}

struct Keys {
    aes: Aes256,
    auth: Vec<u8>,
    salt: [u8; 14],
}

impl Keys {
    fn derive(blob: &[u8; SRTP_BLOB_LEN], rtcp: bool) -> Self {
        let (mk, ms) = blob.split_at(32);
        let base = if rtcp { 3 } else { 0 };
        let ck = kdf(mk, ms, base, 32);
        let mut salt = [0u8; 14];
        salt.copy_from_slice(&kdf(mk, ms, base + 2, 14));
        Self { aes: Aes256::new(GenericArray::from_slice(&ck)), auth: kdf(mk, ms, base + 1, 20), salt }
    }

    fn ctr(&self, iv: u128, data: &mut [u8]) {
        for (i, chunk) in data.chunks_mut(16).enumerate() {
            let mut ks = iv.wrapping_add(i as u128).to_be_bytes();
            self.aes.encrypt_block(GenericArray::from_mut_slice(&mut ks));
            for (d, k) in chunk.iter_mut().zip(ks) {
                *d ^= k;
            }
        }
    }

    fn salt128(&self) -> u128 {
        let mut b = [0u8; 16];
        b[..14].copy_from_slice(&self.salt);
        u128::from_be_bytes(b)
    }

    fn tag(&self, parts: &[&[u8]]) -> [u8; TAG_LEN] {
        let mut m = <HmacSha1 as Mac>::new_from_slice(&self.auth).unwrap();
        for p in parts {
            m.update(p);
        }
        let full = m.finalize().into_bytes();
        full[..TAG_LEN].try_into().unwrap()
    }

    fn rtp_iv(&self, ssrc: u32, index: u64) -> u128 {
        self.salt128() ^ ((ssrc as u128) << 64) ^ ((index as u128) << 16)
    }

    fn rtcp_iv(&self, ssrc: u32, index: u32) -> u128 {
        self.salt128() ^ ((ssrc as u128) << 64) ^ ((index as u128) << 16)
    }
}

#[derive(Clone, Copy, Default)]
pub struct SsrcState {
    pub roc: u32,
    pub max_seq: u16,
}

/// Decrypted RTP packet.
pub struct Rtp {
    pub ssrc: u32,
    pub seq: u16,
    pub ts: u32,
    pub marker: bool,
    /// The plaintext RTP header, including CSRCs and extension.
    pub header: Vec<u8>,
    pub payload: Vec<u8>,
}

/// SRTP receiver with per-SSRC rollover tracking (Apple uses several SSRCs).
pub struct SrtpRx {
    keys: Keys,
    states: HashMap<u32, SsrcState>,
    pub counts: HashMap<u32, u64>,
}

impl SrtpRx {
    pub fn new(blob: &[u8; SRTP_BLOB_LEN]) -> Self {
        Self { keys: Keys::derive(blob, false), states: HashMap::new(), counts: HashMap::new() }
    }

    pub fn state(&self, ssrc: u32) -> SsrcState {
        self.states.get(&ssrc).copied().unwrap_or_default()
    }

    pub fn decrypt(&mut self, pkt: &[u8]) -> Option<Rtp> {
        if pkt.len() < 12 + TAG_LEN || pkt[0] >> 6 != 2 {
            return None;
        }
        let body = pkt.len() - TAG_LEN;
        let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
        let ssrc = u32::from_be_bytes(pkt[8..12].try_into().unwrap());
        let st = self.states.get(&ssrc).copied();
        let guess = match st {
            None => 0,
            Some(s) => {
                let diff = seq as i32 - s.max_seq as i32;
                if diff > 0x7fff {
                    s.roc.saturating_sub(1)
                } else if diff < -0x7fff {
                    s.roc + 1
                } else {
                    s.roc
                }
            }
        };
        let mut tried: Vec<u32> = Vec::with_capacity(4);
        for roc in [guess, st.map_or(0, |s| s.roc), guess + 1, guess.saturating_sub(1)] {
            if tried.contains(&roc) {
                continue;
            }
            tried.push(roc);
            if self.keys.tag(&[&pkt[..body], &roc.to_be_bytes()])[..] != pkt[body..] {
                continue;
            }
            let cc = (pkt[0] & 0x0f) as usize;
            let mut hdr = 12 + cc * 4;
            if pkt[0] & 0x10 != 0 {
                if hdr + 4 > body {
                    return None;
                }
                hdr += 4 + 4 * u16::from_be_bytes([pkt[hdr + 2], pkt[hdr + 3]]) as usize;
            }
            if hdr > body {
                return None;
            }
            let mut payload = pkt[hdr..body].to_vec();
            let index = ((roc as u64) << 16) | seq as u64;
            self.keys.ctr(self.keys.rtp_iv(ssrc, index), &mut payload);
            let s = self.states.entry(ssrc).or_insert(SsrcState { roc, max_seq: seq });
            if ((roc as u64) << 16 | seq as u64) > ((s.roc as u64) << 16 | s.max_seq as u64) {
                *s = SsrcState { roc, max_seq: seq };
            }
            *self.counts.entry(ssrc).or_default() += 1;
            return Some(Rtp {
                ssrc,
                seq,
                ts: u32::from_be_bytes(pkt[4..8].try_into().unwrap()),
                marker: pkt[1] & 0x80 != 0,
                header: pkt[..hdr].to_vec(),
                payload,
            });
        }
        None
    }
}

/// SRTP sender, used only for the payload-type 101 keepalive Apple expects.
pub struct SrtpTx {
    keys: Keys,
    ssrc: u32,
    seq: u16,
    roc: u32,
    ts: u32,
}

impl SrtpTx {
    pub fn new(blob: &[u8; SRTP_BLOB_LEN], ssrc: u32) -> Self {
        Self { keys: Keys::derive(blob, false), ssrc, seq: 0, roc: 0, ts: 0 }
    }

    pub fn encrypt(&mut self, payload: &[u8], pt: u8) -> Vec<u8> {
        let (seq, roc, ts) = (self.seq, self.roc, self.ts);
        self.seq = self.seq.wrapping_add(1);
        if self.seq == 0 {
            self.roc += 1;
        }
        self.ts = self.ts.wrapping_add(480);
        let mut pkt = vec![0x80, pt & 0x7f];
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.extend_from_slice(&ts.to_be_bytes());
        pkt.extend_from_slice(&self.ssrc.to_be_bytes());
        let mut body = payload.to_vec();
        self.keys.ctr(self.keys.rtp_iv(self.ssrc, ((roc as u64) << 16) | seq as u64), &mut body);
        pkt.extend_from_slice(&body);
        let tag = self.keys.tag(&[&pkt, &roc.to_be_bytes()]);
        pkt.extend_from_slice(&tag);
        pkt
    }
}

pub struct SrtcpTx {
    keys: Keys,
    index: u32,
}

impl SrtcpTx {
    pub fn new(blob: &[u8; SRTP_BLOB_LEN]) -> Self {
        Self { keys: Keys::derive(blob, true), index: 0 }
    }

    pub fn protect(&mut self, rtcp: &[u8]) -> Vec<u8> {
        let index = self.index;
        self.index = (self.index + 1) & 0x7fff_ffff;
        let ssrc = u32::from_be_bytes(rtcp[4..8].try_into().unwrap());
        let mut out = rtcp.to_vec();
        self.keys.ctr(self.keys.rtcp_iv(ssrc, index), &mut out[8..]);
        out.extend_from_slice(&(0x8000_0000 | index).to_be_bytes());
        let tag = self.keys.tag(&[&out]);
        out.extend_from_slice(&tag);
        out
    }
}

pub struct SrtcpRx {
    keys: Keys,
}

impl SrtcpRx {
    pub fn new(blob: &[u8; SRTP_BLOB_LEN]) -> Self {
        Self { keys: Keys::derive(blob, true) }
    }

    pub fn unprotect(&self, pkt: &[u8]) -> Option<Vec<u8>> {
        if pkt.len() < 8 + 4 + TAG_LEN {
            return None;
        }
        let body = &pkt[..pkt.len() - TAG_LEN];
        if self.keys.tag(&[body])[..] != pkt[pkt.len() - TAG_LEN..] {
            return None;
        }
        let e = u32::from_be_bytes(body[body.len() - 4..].try_into().unwrap());
        let mut out = body[..body.len() - 4].to_vec();
        if e & 0x8000_0000 != 0 {
            let ssrc = u32::from_be_bytes(out[4..8].try_into().unwrap());
            self.keys.ctr(self.keys.rtcp_iv(ssrc, e & 0x7fff_ffff), &mut out[8..]);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3711_key_derivation_vectors() {
        // RFC 3711 appendix B.3 uses AES-128; check the same construction with the
        // 128-bit test vector by deriving through a local AES-128 helper.
        let mk = hex("E1F97A0D3E018BE0D64FA32C06DE4139");
        let ms = hex("0EC675AD498AFEEBB6960B3AABE6");
        let derive = |label: u8, len: usize| {
            let c = Aes128::new(GenericArray::from_slice(&mk));
            let mut iv = [0u8; 16];
            iv[..14].copy_from_slice(&ms);
            iv[7] ^= label;
            let base = u128::from_be_bytes(iv);
            let mut out = vec![];
            let mut i = 0u128;
            while out.len() < len {
                let mut b = base.wrapping_add(i).to_be_bytes();
                c.encrypt_block(GenericArray::from_mut_slice(&mut b));
                out.extend_from_slice(&b);
                i += 1;
            }
            out.truncate(len);
            out
        };
        assert_eq!(derive(0, 16), hex("C61E7A93744F39EE10734AFE3FF7A087"));
        assert_eq!(derive(2, 14), hex("30CBBC08863D8C85D49DB34A9AE1"));
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn srtp_round_trip_and_tamper() {
        let blob = [7u8; SRTP_BLOB_LEN];
        let mut tx = SrtpTx::new(&blob, 0x1234_5678);
        let mut rx = SrtpRx::new(&blob);
        let p = tx.encrypt(b"hello apple", 101);
        let r = rx.decrypt(&p).expect("authentic packet");
        assert_eq!((r.ssrc, r.seq, r.payload.as_slice()), (0x1234_5678, 0, &b"hello apple"[..]));
        let mut bad = tx.encrypt(b"again", 101);
        bad[13] ^= 1;
        assert!(rx.decrypt(&bad).is_none());
    }

    #[test]
    fn srtcp_round_trip() {
        let blob = [9u8; SRTP_BLOB_LEN];
        let mut tx = SrtcpTx::new(&blob);
        let rx = SrtcpRx::new(&blob);
        let rr = [0x81, 201, 0, 7, 1, 2, 3, 4, 9, 9, 9, 9, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let p = tx.protect(&rr);
        assert_eq!(p.len(), rr.len() + 4 + 10);
        assert_ne!(&p[8..rr.len()], &rr[8..], "the report body is encrypted");
        assert_eq!(rx.unprotect(&p).unwrap(), rr);
        let mut bad = p.clone();
        bad[9] ^= 1;
        assert!(rx.unprotect(&bad).is_none());
    }

    #[test]
    fn enc1103_round_trip_across_fragmented_reads() {
        let keys = Enc1103Keys { cbc_key: [3; 16], iv: [5; 16] };
        let mut tx = keys.sender();
        let mut rx = keys.receiver();
        let mut wire = tx.seal(b"first message");
        wire.extend(tx.seal(&[0x55; 100]));
        let (a, b) = wire.split_at(7);
        assert!(rx.feed(a).is_empty());
        let msgs = rx.feed(b);
        assert_eq!(msgs, vec![b"first message".to_vec(), vec![0x55; 100]]);
    }
}
