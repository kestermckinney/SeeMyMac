//! Apple screensharingd authentication (RFB security type 0x21).
//!
//! Both variants start with an "RSA1" exchange that hands us the server's RSA key.
//! The SRP variant (SRP-6a, RFC 5054 4096-bit group, SHA-512, PBKDF2) is what
//! current macOS uses; the older variant sends AES-wrapped credentials. Either
//! returns the 16-byte key that unwraps the enc1103 control-channel keys.

use super::crypto::aes128_ecb_encrypt;
use super::net::Tcp;
use anyhow::{bail, Context, Result};
use num_bigint::BigUint;
use rand::RngCore;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use sha2::{Digest, Sha256, Sha512};

/// The server rejected the credentials (or spoke an unexpected dialect).
#[derive(Debug)]
pub struct AuthError(pub String);

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AuthError {}

fn auth_err(m: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(AuthError(m.into()))
}

const MOD_LEN: usize = 512;

fn rsa1_init(t: &mut Tcp) -> Result<RsaPublicKey> {
    t.send(b"\x21\x00\x00\x00\x0a\x01\x00RSA1\x00\x00\x00\x00")?;
    let n = t.read_u32()? as usize;
    if !(6..=16384).contains(&n) {
        bail!("unexpected RSA1 reply length {n}");
    }
    let pkt = t.read_exact(n)?;
    let klen = u32::from_be_bytes(pkt[2..6].try_into().unwrap()) as usize;
    let der = pkt.get(6..6 + klen).context("truncated RSA1 key")?;
    RsaPublicKey::from_public_key_der(der).or_else(|_| RsaPublicKey::from_pkcs1_der(der)).context("server RSA key")
}

fn rsa_encrypt(key: &RsaPublicKey, data: &[u8]) -> Result<Vec<u8>> {
    key.encrypt(&mut rand::thread_rng(), Pkcs1v15Encrypt, data).context("RSA encrypt")
}

/// ARD credential slot: NUL-terminated, random-padded to 64 bytes.
fn ard_slot(s: &str) -> [u8; 64] {
    let mut out = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut out);
    let b = s.as_bytes();
    let n = b.len().min(63);
    out[..n].copy_from_slice(&b[..n]);
    out[n] = 0;
    out
}

pub fn nonsrp(t: &mut Tcp, user: &str, pass: &str) -> Result<[u8; 16]> {
    let key = rsa1_init(t)?;
    let mut aes_key = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut aes_key);
    let mut creds = [0u8; 128];
    creds[..64].copy_from_slice(&ard_slot(user));
    creds[64..].copy_from_slice(&ard_slot(pass));
    aes128_ecb_encrypt(&aes_key, &mut creds);
    let mut blob = b"\x01\x00RSA1\x00\x01".to_vec();
    blob.extend_from_slice(&creds);
    blob.extend_from_slice(b"\x00\x01");
    blob.extend_from_slice(&rsa_encrypt(&key, &aes_key)?);
    let mut msg = (blob.len() as u32).to_be_bytes().to_vec();
    msg.extend_from_slice(&blob);
    t.send(&msg)?;
    t.read_exact(4)?;
    let result = t.read_u32()?;
    if result != 0 {
        return Err(auth_err(format!("authentication failed (code {result})")));
    }
    Ok(aes_key)
}

struct Challenge {
    n: BigUint,
    nb: Vec<u8>,
    g: BigUint,
    salt: Vec<u8>,
    b: BigUint,
    bb: Vec<u8>,
    iterations: u32,
    cap: Vec<u8>,
}

fn parse_challenge(s: &[u8]) -> Result<Challenge> {
    let mut p = 12;
    let get = |p: &mut usize, n: usize| -> Result<Vec<u8>> {
        let v = s.get(*p..*p + n).context("truncated SRP challenge")?.to_vec();
        *p += n;
        Ok(v)
    };
    if s.get(p) != Some(&0) {
        return Err(auth_err("unexpected SRP challenge layout"));
    }
    p += 1;
    let nb = get(&mut p, MOD_LEN)?;
    let gl = u16::from_be_bytes(get(&mut p, 2)?.try_into().unwrap()) as usize;
    let g = BigUint::from_bytes_be(&get(&mut p, gl)?);
    let sl = get(&mut p, 1)?[0] as usize;
    let salt = get(&mut p, sl)?;
    let bl = u16::from_be_bytes(get(&mut p, 2)?.try_into().unwrap()) as usize;
    let bb = get(&mut p, bl)?;
    let iterations = u64::from_be_bytes(get(&mut p, 8)?.try_into().unwrap());
    let cl = u16::from_be_bytes(get(&mut p, 2)?.try_into().unwrap()) as usize;
    let cap = get(&mut p, cl)?;
    if iterations > 1_000_000 {
        bail!("SRP iteration count {iterations} is unreasonably high");
    }
    Ok(Challenge { n: BigUint::from_bytes_be(&nb), nb, g, salt, b: BigUint::from_bytes_be(&bb), bb, iterations: iterations as u32, cap })
}

fn pad(x: &BigUint) -> Vec<u8> {
    let b = x.to_bytes_be();
    let mut out = vec![0u8; MOD_LEN.saturating_sub(b.len())];
    out.extend_from_slice(&b);
    out
}

fn sha512(parts: &[&[u8]]) -> [u8; 64] {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// SRP-6a client computation. Returns (A, M1, K).
fn solve(c: &Challenge, password: &str, a: &BigUint) -> (Vec<u8>, [u8; 64], [u8; 64]) {
    let gp = pad(&c.g);
    let k = BigUint::from_bytes_be(&sha512(&[&c.nb, &gp]));
    let big_a = c.g.modpow(a, &c.n);
    let ab = pad(&big_a);
    let u = BigUint::from_bytes_be(&sha512(&[&ab, &c.bb]));
    // Apple's SALTED-SHA512-PBKDF2: x = H(salt || H(":" || PBKDF2(pw, salt, iters, 128)))
    let mut dk = [0u8; 128];
    pbkdf2::pbkdf2_hmac::<Sha512>(password.as_bytes(), &c.salt, c.iterations, &mut dk);
    let inner = sha512(&[b":", &dk]);
    let x = BigUint::from_bytes_be(&sha512(&[&c.salt, &inner])) % &c.n;
    let kgx = (k * c.g.modpow(&x, &c.n)) % &c.n;
    let base = ((&c.b % &c.n) + &c.n - kgx) % &c.n;
    let s = base.modpow(&(a + u * x), &c.n);
    let key = sha512(&[&pad(&s)]);
    let hn = sha512(&[&c.nb]);
    let hg = sha512(&[&gp]);
    let x: Vec<u8> = hn.iter().zip(hg).map(|(a, b)| a ^ b).collect();
    let m1 = sha512(&[&x, &sha512(&[b""]), &c.salt, &ab, &c.bb, &key]);
    (ab, m1, key)
}

pub fn srp(t: &mut Tcp, user: &str, pass: &str) -> Result<[u8; 16]> {
    let key = rsa1_init(t)?;
    let ub = user.as_bytes();
    let mut inner = (ub.len() as u32).to_be_bytes().to_vec();
    inner.extend_from_slice(ub);
    inner.extend_from_slice(&[0, 0, 0]);
    let mut payload = (inner.len() as u32).to_be_bytes().to_vec();
    payload.extend_from_slice(&inner);
    let enc = rsa_encrypt(&key, &payload)?;
    if enc.len() != 256 {
        return Err(auth_err(format!("expected a 2048-bit server key, got {} bytes", enc.len())));
    }
    let mut c2s1 = vec![1, 0];
    c2s1.extend_from_slice(b"RSA1\x00\x02");
    c2s1.extend_from_slice(&256u16.to_be_bytes());
    c2s1.extend_from_slice(&enc);
    c2s1.resize(c2s1.len() + 384, 0);
    let mut msg = 650u32.to_be_bytes().to_vec();
    msg.extend_from_slice(&c2s1);
    t.send(&msg)?;

    let n = t.read_u32()? as usize;
    if n > 65536 {
        bail!("unexpected SRP challenge length {n}");
    }
    let s2c1 = t.read_exact(n)?;
    if n < 1000 {
        return Err(auth_err("the Mac did not offer SRP authentication"));
    }
    let ch = parse_challenge(&s2c1)?;
    let mut ar = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut ar);
    let a = BigUint::from_bytes_be(&ar) % (&ch.n - 1u32) + 1u32;
    let (ab, m1, k) = solve(&ch, pass, &a);

    let mut civ = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut civ);
    let mut sd = (MOD_LEN as u16).to_be_bytes().to_vec();
    sd.extend_from_slice(&ab);
    sd.push(64);
    sd.extend_from_slice(&m1);
    sd.extend_from_slice(&(ch.cap.len() as u16).to_be_bytes());
    sd.extend_from_slice(&ch.cap);
    sd.push(16);
    sd.extend_from_slice(&civ);
    let mut pay = vec![1, 0];
    pay.extend_from_slice(b"RSA1\x00\x02");
    pay.extend_from_slice(&((sd.len() + 4) as u16).to_be_bytes());
    pay.extend_from_slice(&(sd.len() as u32).to_be_bytes());
    pay.extend_from_slice(&sd);
    if pay.len() > 1076 {
        bail!("SRP proof too large");
    }
    pay.resize(1076, 0);
    let mut msg = 1076u32.to_be_bytes().to_vec();
    msg.extend_from_slice(&pay);
    t.send(&msg)?;

    let m2 = t.read_u32()? as usize;
    if m2 > 65536 {
        bail!("unexpected SRP result length {m2}");
    }
    t.read_exact(m2)?;
    let result = t.read_u32()?;
    if result != 0 {
        return Err(auth_err(format!("authentication failed (code {result})")));
    }
    let d = Sha256::digest(k);
    Ok(d[..16].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny SRP server built from the same equations must derive the same key.
    #[test]
    fn srp_client_matches_a_reference_server() {
        // RFC 5054 4096-bit group isn't needed for the algebra; a 512-byte-padded
        // small safe prime keeps the test fast while exercising padding.
        let n = BigUint::parse_bytes(b"EEAF0AB9ADB38DD69C33F80AFA8FC5E86072618775FF3C0B9EA2314C9C256576D674DF7496EA81D3383B4813D692C6E0E0D5D8E250B98BE48E495C1D6089DAD15DC7D7B46154D6B6CE8EF4AD69B15D4982559B297BCF1885C529F566660E57EC68EDBC3C05726CC02FD4CBF4976EAA9AFD5138FE8376435B9FC61D2FC0EB06E3", 16).unwrap();
        let g = BigUint::from(2u32);
        let salt = vec![0x42; 32];
        let iterations = 1000;
        let password = "correct horse";
        // Server side: verifier v = g^x.
        let mut dk = [0u8; 128];
        pbkdf2::pbkdf2_hmac::<Sha512>(password.as_bytes(), &salt, iterations, &mut dk);
        let x = BigUint::from_bytes_be(&sha512(&[&salt, &sha512(&[b":", &dk])])) % &n;
        let v = g.modpow(&x, &n);
        let k = BigUint::from_bytes_be(&sha512(&[&pad(&n), &pad(&g)]));
        let bsec = BigUint::from(123456789u64);
        let big_b = (&k * &v + g.modpow(&bsec, &n)) % &n;
        let ch = Challenge { n: n.clone(), nb: pad(&n), g: g.clone(), salt: salt.clone(), bb: pad(&big_b), b: big_b.clone(), iterations, cap: vec![] };
        let a = BigUint::from(987654321u64);
        let (ab, _m1, key) = solve(&ch, password, &a);
        let big_a = BigUint::from_bytes_be(&ab);
        let u = BigUint::from_bytes_be(&sha512(&[&ab, &pad(&big_b)]));
        let s_server = (big_a * v.modpow(&u, &n)).modpow(&bsec, &n);
        assert_eq!(key, sha512(&[&pad(&s_server)]));
    }
}
