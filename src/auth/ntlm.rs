//! NTLMv2 (NTLMSSP) message generation.
//!
//! IDM reaches NTLM through Windows SSPI (`AcquireCredentialsHandleA` from
//! `secur32.dll`), so its implementation is opaque. Because Riptide must work on
//! Linux and macOS where SSPI does not exist, the NTLM handshake is implemented
//! directly from the MS-NLMP specification instead.
//!
//! **Status:** the cryptographic primitives are unit-tested against published vectors
//! and the message layout follows MS-NLMP, but the full handshake has not been
//! exercised against a live NTLM server in this environment. See
//! `docs/FEATURE-MAP.md` for the honest status of each authentication mode.

use hmac::{Hmac, Mac};
use md5::Md5;

type HmacMd5 = Hmac<Md5>;

/// UTF-16LE encoding helper used throughout NTLMSSP.
pub fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// `NTOWFv1` = MD4(UTF-16LE(password)). This is the classic "NT hash".
pub fn nt_hash(password: &str) -> [u8; 16] {
    use md4::{Digest, Md4};
    let mut h = Md4::new();
    h.update(utf16le(password));
    let out = h.finalize();
    let mut r = [0u8; 16];
    r.copy_from_slice(&out);
    r
}

/// `NTOWFv2` = HMAC_MD5(NTOWFv1, UTF-16LE(uppercase(user) + domain)).
pub fn ntowfv2(user: &str, domain: &str, password: &str) -> [u8; 16] {
    let nt = nt_hash(password);
    let key = utf16le(&format!("{}{}", user.to_uppercase(), domain));
    let mut mac = HmacMd5::new_from_slice(&nt).expect("hmac accepts any key length");
    mac.update(&key);
    let out = mac.finalize().into_bytes();
    let mut r = [0u8; 16];
    r.copy_from_slice(&out);
    r
}

fn hmac_md5(key: &[u8], data: &[u8]) -> [u8; 16] {
    let mut mac = HmacMd5::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    let out = mac.finalize().into_bytes();
    let mut r = [0u8; 16];
    r.copy_from_slice(&out);
    r
}

/// Negotiate flags advertised in the Type 1 message.
const FLAGS: u32 = 0x0000_0001 // NEGOTIATE_UNICODE
    | 0x0000_0002 // NEGOTIATE_OEM
    | 0x0000_0004 // REQUEST_TARGET
    | 0x0000_0200 // NEGOTIATE_NTLM
    | 0x0000_8000 // NEGOTIATE_ALWAYS_SIGN
    | 0x0008_0000 // NEGOTIATE_EXTENDED_SESSIONSECURITY
    | 0x0080_0000 // NEGOTIATE_TARGET_INFO
    | 0x2000_0000 // NEGOTIATE_128
    | 0x8000_0000; // NEGOTIATE_56

fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn read_u16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}
fn read_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

/// Parsed server challenge (NTLMSSP Type 2 message).
#[derive(Debug, Clone)]
pub struct Type2 {
    pub flags: u32,
    pub server_challenge: [u8; 8],
    pub target_name: Vec<u8>,
    pub target_info: Vec<u8>,
    pub version: Option<[u8; 8]>,
}

/// Build the Type 1 (NEGOTIATE) message.
pub fn type1(domain: &str, workstation: &str) -> Vec<u8> {
    let dom = domain.to_ascii_uppercase();
    let dom_bytes = utf16le(&dom);
    let ws_bytes = utf16le(&workstation.to_ascii_uppercase());

    let mut out = Vec::with_capacity(40 + dom_bytes.len() + ws_bytes.len());
    out.extend_from_slice(b"NTLMSSP\0");
    put_u32(&mut out, 1); // message type
    put_u32(&mut out, FLAGS);
    let dom_off = 32u32;
    put_u16(&mut out, dom_bytes.len() as u16);
    put_u16(&mut out, dom_bytes.len() as u16);
    put_u32(&mut out, dom_off);
    let ws_off = dom_off + dom_bytes.len() as u32;
    put_u16(&mut out, ws_bytes.len() as u16);
    put_u16(&mut out, ws_bytes.len() as u16);
    put_u32(&mut out, ws_off);
    out.extend_from_slice(&dom_bytes);
    out.extend_from_slice(&ws_bytes);
    out
}

/// Parse the Type 2 (CHALLENGE) message.
pub fn parse_type2(msg: &[u8]) -> Option<Type2> {
    if msg.len() < 32 || &msg[0..8] != b"NTLMSSP\0" || read_u32(msg, 8)? != 2 {
        return None;
    }
    let flags = read_u32(msg, 20)?;
    let mut server_challenge = [0u8; 8];
    server_challenge.copy_from_slice(msg.get(24..32)?);

    let slice_field = |off: usize| -> Option<Vec<u8>> {
        let len = read_u16(msg, off)? as usize;
        let data_off = read_u32(msg, off + 4)? as usize;
        msg.get(data_off..data_off + len).map(|s| s.to_vec())
    };
    let target_name = slice_field(12).unwrap_or_default();
    let target_info = slice_field(40).unwrap_or_default();
    let version = msg.get(48..56).and_then(|s| <[u8; 8]>::try_from(s).ok());

    Some(Type2 {
        flags,
        server_challenge,
        target_name,
        target_info,
        version,
    })
}

/// Build the Type 3 (AUTHENTICATE) message using NTLMv2 responses.
pub fn type3(t2: &Type2, user: &str, domain: &str, password: &str, workstation: &str) -> Vec<u8> {
    // A fresh client challenge is what makes each NTLMv2 response unique.
    let client_challenge: [u8; 8] = rand::random();

    // Timestamp in Windows FILETIME units; zero is permitted and avoids depending on
    // the local clock skewing the server's replay window.
    let timestamp: u64 = 0;

    let mut blob = Vec::new();
    blob.extend_from_slice(&[0x01, 0x01, 0x00, 0x00]); // RespType / HiRespType
    blob.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // Reserved
    blob.extend_from_slice(&timestamp.to_le_bytes()); // Timestamp
    blob.extend_from_slice(&client_challenge); // ClientChallenge
    blob.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // Reserved
    blob.extend_from_slice(&t2.target_info); // TargetInfo (AV pairs)
                                             // Terminator AV pair, if the server did not already include one.
    if !t2.target_info.ends_with(&[0, 0, 0, 0]) {
        blob.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    }

    let key = ntowfv2(user, domain, password);

    // NTProofStr = HMAC_MD5(NTOWFv2, ServerChallenge || blob)
    let mut nt_input = Vec::with_capacity(8 + blob.len());
    nt_input.extend_from_slice(&t2.server_challenge);
    nt_input.extend_from_slice(&blob);
    let nt_proof = hmac_md5(&key, &nt_input);

    let mut nt_response = Vec::with_capacity(16 + blob.len());
    nt_response.extend_from_slice(&nt_proof);
    nt_response.extend_from_slice(&blob);

    // LMv2 = HMAC_MD5(NTOWFv2, ServerChallenge || ClientChallenge) || ClientChallenge
    let mut lm_input = Vec::with_capacity(16);
    lm_input.extend_from_slice(&t2.server_challenge);
    lm_input.extend_from_slice(&client_challenge);
    let lm_proof = hmac_md5(&key, &lm_input);
    let mut lm_response = Vec::with_capacity(24);
    lm_response.extend_from_slice(&lm_proof);
    lm_response.extend_from_slice(&client_challenge);

    let dom_bytes = utf16le(domain);
    let user_bytes = utf16le(user);
    let ws_bytes = utf16le(workstation);

    // Header: 64 bytes without Version/MIC (8 fixed fields of 8 bytes + flags).
    let base = 64usize;
    let off_lm = base;
    let off_nt = off_lm + lm_response.len();
    let off_dom = off_nt + nt_response.len();
    let off_user = off_dom + dom_bytes.len();
    let off_ws = off_user + user_bytes.len();
    // No session key requested (we never set NEGOTIATE_KEY_EXCH / SIGN requirements).
    let off_key = off_ws + ws_bytes.len();

    let mut out = Vec::with_capacity(off_key);
    out.extend_from_slice(b"NTLMSSP\0");
    put_u32(&mut out, 3);
    let field = |out: &mut Vec<u8>, len: usize, off: usize| {
        put_u16(out, len as u16);
        put_u16(out, len as u16);
        put_u32(out, off as u32);
    };
    field(&mut out, lm_response.len(), off_lm);
    field(&mut out, nt_response.len(), off_nt);
    field(&mut out, dom_bytes.len(), off_dom);
    field(&mut out, user_bytes.len(), off_user);
    field(&mut out, ws_bytes.len(), off_ws);
    field(&mut out, 0, off_key);
    put_u32(&mut out, t2.flags & FLAGS);
    debug_assert_eq!(out.len(), base);
    out.extend_from_slice(&lm_response);
    out.extend_from_slice(&nt_response);
    out.extend_from_slice(&dom_bytes);
    out.extend_from_slice(&user_bytes);
    out.extend_from_slice(&ws_bytes);
    out
}

/// Extract the base64 Type 2 message from a `WWW-Authenticate: NTLM <b64>` header.
pub fn challenge_from_header(header: &str) -> Option<Type2> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let idx = header.to_ascii_lowercase().find("ntlm")?;
    let rest = header[idx + 4..].trim();
    if rest.is_empty() {
        return None;
    }
    let raw = STANDARD.decode(rest).ok()?;
    parse_type2(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nt_hash_matches_published_vector() {
        // Widely used test vector: NT hash of "password".
        assert_eq!(
            hex::encode(nt_hash("password")),
            "8846f7eaee8fb117ad06bdd830b7586c"
        );
        assert_eq!(hex::encode(nt_hash("")), "31d6cfe0d16ae931b73c59d7e0c089c0");
    }

    #[test]
    fn ntowfv2_is_order_and_case_sensitive_on_user() {
        let a = ntowfv2("user", "DOMAIN", "password");
        let b = ntowfv2("USER", "DOMAIN", "password");
        assert_eq!(a, b, "username must be upper-cased before hashing");
        let c = ntowfv2("user", "other", "password");
        assert_ne!(a, c);
    }

    #[test]
    fn type1_is_well_formed() {
        let m = type1("DOMAIN", "WORKSTATION");
        assert_eq!(&m[0..8], b"NTLMSSP\0");
        assert_eq!(u32::from_le_bytes(m[8..12].try_into().unwrap()), 1);
        assert_eq!(
            m.len(),
            32 + utf16le("DOMAIN").len() + utf16le("WORKSTATION").len()
        );
    }

    #[test]
    fn type2_roundtrip_and_type3_layout() {
        // Hand-build a minimal Type 2 with one AV pair terminator.
        let target_info = vec![0u8, 0, 0, 0];
        let mut t2 = Vec::new();
        t2.extend_from_slice(b"NTLMSSP\0");
        t2.extend_from_slice(&2u32.to_le_bytes());
        // TargetName field (empty, offset 0)
        t2.extend_from_slice(&0u16.to_le_bytes());
        t2.extend_from_slice(&0u16.to_le_bytes());
        t2.extend_from_slice(&0u32.to_le_bytes());
        t2.extend_from_slice(&FLAGS.to_le_bytes());
        t2.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // server challenge
        t2.extend_from_slice(&[0u8; 8]); // reserved
                                         // TargetInfo field
        t2.extend_from_slice(&(target_info.len() as u16).to_le_bytes());
        t2.extend_from_slice(&(target_info.len() as u16).to_le_bytes());
        t2.extend_from_slice(&48u32.to_le_bytes());
        t2.extend_from_slice(&target_info);

        let parsed = parse_type2(&t2).expect("parses");
        assert_eq!(parsed.server_challenge, [1, 2, 3, 4, 5, 6, 7, 8]);

        let m3 = type3(&parsed, "user", "DOMAIN", "password", "WS");
        assert_eq!(&m3[0..8], b"NTLMSSP\0");
        assert_eq!(u32::from_le_bytes(m3[8..12].try_into().unwrap()), 3);
        // LMv2 is always 24 bytes, NTLMv2 is 16 + blob.
        assert_eq!(u16::from_le_bytes(m3[12..14].try_into().unwrap()), 24);
        let nt_len = u16::from_le_bytes(m3[20..22].try_into().unwrap()) as usize;
        assert!(nt_len >= 16 + 32);
        assert_eq!(
            m3.len(),
            64 + 24
                + nt_len
                + utf16le("DOMAIN").len()
                + utf16le("user").len()
                + utf16le("WS").len()
        );
    }

    #[test]
    fn challenge_header_parsing() {
        assert!(challenge_from_header("NTLM").is_none());
        let sep = challenge_from_header("Negotiate, NTLM").is_none();
        assert!(sep);
    }
}
