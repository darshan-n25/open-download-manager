//! SPNEGO (RFC 4178), the wrapper a Windows server means when it answers
//! `WWW-Authenticate: Negotiate`.
//!
//! The honest scope statement, up front: **Riptide implements SPNEGO with
//! NTLMSSP as the inner mechanism and nothing else.** It offers one mechanism in
//! its `mechTypes` list, the NTLMSSP OID, and carries an `NTLMSSP NEGOTIATE`
//! message as the optimistic `mechToken`. A server that accepts NTLM inside
//! SPNEGO — IIS's default `Negotiate, NTLM` provider order, and anything else
//! that honours the offered list — is authenticated exactly as if the job had
//! asked for `ntlm:`. A server configured for Kerberos *only* cannot be
//! authenticated against, because producing a Kerberos token needs an actual
//! Kerberos implementation and a realm; that case is detected and reported in
//! those words rather than failing as a parse error.
//!
//! Listing only NTLM is deliberate rather than lazy. RFC 4178 §3.1 says the
//! mechanism list "MUST NOT contain ... any mechanism for which the client does
//! not have appropriate credentials", so advertising Kerberos and then failing to
//! produce a token would be a lie that wastes a round trip.
//!
//! ## What is on the wire
//!
//! The first token is an `initialContextToken` (RFC 2743 §3.1): an
//! `[APPLICATION 0]` wrapper naming SPNEGO, containing a `NegTokenInit`. Every
//! later token is a bare `NegTokenResp` — RFC 4178 §4.2 is explicit that
//! "subsequent tokens MUST NOT be encapsulated in this GSS-API generic token
//! framing".
//!
//! The DER uses **explicit** context tags (the ASN.1 module is `DEFINITIONS
//! EXPLICIT TAGS`), so `mechTypes [0] MechTypeList` encodes as `A0 <len> 30
//! <len> ...`, with the inner SEQUENCE and its own tag retained. RFC 4178
//! Appendix D spells the initial bytes out, and the golden tests below assert
//! them octet for octet.
//!
//! Only the handful of DER productions SPNEGO actually needs are implemented
//! here — no ASN.1 library, matching how [`crate::auth::ntlm`] hand-rolls
//! MS-NLMP. The encoder is verified against `openssl asn1parse`, an independent
//! DER parser, rather than only against itself.

use super::{base64_decode, base64_encode, scheme_of};

/// Object identifiers, as the raw contents octets of a DER `OBJECT IDENTIFIER`.
pub mod oid {
    /// `1.3.6.1.5.5.2` — the SPNEGO pseudo-mechanism itself.
    pub const SPNEGO: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x02];
    /// `1.3.6.1.4.1.311.2.2.10` — NTLMSSP.
    pub const NTLMSSP: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x02, 0x0a];
    /// `1.2.840.113554.1.2.2` — Kerberos 5.
    pub const KERBEROS_V5: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02];
    /// `1.2.840.48018.1.2.2` — the wrong-but-deployed Kerberos OID that RFC 4178
    /// Appendix C warns about. Recognised so that a legacy server still produces
    /// an accurate "this is Kerberos" diagnosis.
    pub const MS_KERBEROS_V5: &[u8] = &[0x2a, 0x86, 0x48, 0x82, 0xf7, 0x12, 0x01, 0x02, 0x02];
}

/// Which mechanism a nested token belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mech {
    /// An `NTLMSSP` message — the one mechanism Riptide can complete.
    Ntlm,
    /// A Kerberos mechanism token, which Riptide cannot complete.
    Kerberos,
}

/// A parsed `NegTokenInit` or `NegTokenResp`, or as much of one as arrived.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerToken {
    /// The mechanism named by the GSS-API `initialContextToken` wrapper, when the
    /// server used one.
    pub outer_mech: Option<Vec<u8>>,
    /// `negState` from a `NegTokenResp`: 0 accept-completed, 1 accept-incomplete,
    /// 2 reject, 3 request-mic.
    pub state: Option<u8>,
    /// `supportedMech` from a `NegTokenResp`.
    pub supported_mech: Option<Vec<u8>>,
    /// The mechanism list from a `NegTokenInit`.
    pub mech_types: Vec<Vec<u8>>,
    /// The nested mechanism message: `responseToken` or `mechToken`.
    pub token: Option<Vec<u8>>,
}

impl ServerToken {
    /// The mechanism this token is for.
    ///
    /// A nested token is authoritative when it is recognisable; otherwise the
    /// declared `supportedMech`, the wrapper OID, and finally the offered list are
    /// consulted, in that order of trustworthiness.
    pub fn mech(&self) -> Option<Mech> {
        if let Some(t) = &self.token {
            if let Some(m) = mech_of_token(t) {
                return Some(m);
            }
        }
        for oid in [self.supported_mech.as_ref(), self.outer_mech.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Some(m) = mech_of_oid(oid) {
                return Some(m);
            }
        }
        self.mech_types.iter().find_map(|o| mech_of_oid(o))
    }

    /// `true` when the server said `reject`.
    pub fn is_reject(&self) -> bool {
        self.state == Some(2)
    }
}

fn mech_of_oid(oid: &[u8]) -> Option<Mech> {
    if oid == oid::NTLMSSP {
        return Some(Mech::Ntlm);
    }
    if oid == oid::KERBEROS_V5 || oid == oid::MS_KERBEROS_V5 {
        return Some(Mech::Kerberos);
    }
    None
}

/// Classify a nested mechanism token.
fn mech_of_token(token: &[u8]) -> Option<Mech> {
    if token.starts_with(b"NTLMSSP\0") {
        return Some(Mech::Ntlm);
    }
    // A mechanism token is itself a GSS-API token: an `[APPLICATION 0]` wrapper
    // whose first component names the mechanism.
    if let Some((0x60, contents, _)) = tlv(token) {
        if let Some((0x06, oid, _)) = tlv(contents) {
            return mech_of_oid(oid);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// The DER definite-length form of `n`.
fn der_length(n: usize) -> Vec<u8> {
    if n < 0x80 {
        return vec![n as u8];
    }
    let mut be = Vec::new();
    let mut v = n;
    while v > 0 {
        be.push((v & 0xff) as u8);
        v >>= 8;
    }
    be.reverse();
    let mut out = vec![0x80 | be.len() as u8];
    out.extend_from_slice(&be);
    out
}

/// One DER TLV: identifier octet, length, contents.
fn der_tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(contents.len() + 4);
    out.push(tag);
    out.extend_from_slice(&der_length(contents.len()));
    out.extend_from_slice(contents);
    out
}

fn der_join(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

/// The client's first token: an `initialContextToken` naming SPNEGO, whose
/// `NegTokenInit` offers NTLMSSP alone and carries `ntlmssp` as the optimistic
/// mechanism token.
pub fn initial_token(ntlmssp: &[u8]) -> Vec<u8> {
    // MechTypeList ::= SEQUENCE OF MechType
    let mech_list = der_tlv(0x30, &der_tlv(0x06, oid::NTLMSSP));
    // mechTypes [0] MechTypeList  (explicit, so the SEQUENCE tag stays)
    let mech_types = der_tlv(0xa0, &mech_list);
    // mechToken [2] OCTET STRING
    let mech_token = der_tlv(0xa2, &der_tlv(0x04, ntlmssp));
    // NegTokenInit ::= SEQUENCE { mechTypes, mechToken }
    let init = der_tlv(0x30, &der_join(&[&mech_types, &mech_token]));
    // NegotiationToken ::= CHOICE { negTokenInit [0] NegTokenInit }
    let negotiation = der_tlv(0xa0, &init);
    // InitialContextToken ::= [APPLICATION 0] SEQUENCE { thisMech, innerContextToken }
    der_tlv(
        0x60,
        &der_join(&[&der_tlv(0x06, oid::SPNEGO), &negotiation]),
    )
}

/// The client's last token: a bare `NegTokenResp` carrying `ntlmssp` as the
/// `responseToken`, with `negState` set to `accept-completed` — NTLM is the
/// odd-token case, so the initiator sends the final mechanism token and is done.
pub fn response_token(ntlmssp: &[u8]) -> Vec<u8> {
    // negState [0] ENUMERATED accept-completed(0)
    let state = der_tlv(0xa0, &[0x0a, 0x01, 0x00]);
    // responseToken [2] OCTET STRING
    let token = der_tlv(0xa2, &der_tlv(0x04, ntlmssp));
    let resp = der_tlv(0x30, &der_join(&[&state, &token]));
    der_tlv(0xa1, &resp)
}

/// Base64 of [`initial_token`], ready for `Authorization: Negotiate <...>`.
pub fn initial_authorization(ntlmssp: &[u8]) -> String {
    base64_encode(&initial_token(ntlmssp))
}

/// Base64 of [`response_token`], ready for `Authorization: Negotiate <...>`.
pub fn response_authorization(ntlmssp: &[u8]) -> String {
    base64_encode(&response_token(ntlmssp))
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Read one TLV, returning `(tag, contents, bytes consumed)`.
fn tlv(buf: &[u8]) -> Option<(u8, &[u8], usize)> {
    let tag = *buf.first()?;
    let first = *buf.get(1)?;
    let (len, header) = if first < 0x80 {
        (first as usize, 2usize)
    } else {
        let n = (first & 0x7f) as usize;
        // Indefinite length and absurd lengths are not DER; SPNEGO is DER only.
        if n == 0 || n > 4 || buf.len() < 2 + n {
            return None;
        }
        let mut v = 0usize;
        for b in &buf[2..2 + n] {
            v = (v << 8) | *b as usize;
        }
        (v, 2 + n)
    };
    let end = header.checked_add(len)?;
    if end > buf.len() {
        return None;
    }
    Some((tag, &buf[header..end], end))
}

fn walk_fields(st: &mut ServerToken, mut rest: &[u8]) {
    while let Some((t, c, used)) = tlv(rest) {
        rest = &rest[used..];
        match t {
            // negState ENUMERATED in a NegTokenResp, or mechTypes in a NegTokenInit.
            0xa0 => {
                if let Some((0x0a, e, _)) = tlv(c) {
                    st.state = e.first().copied();
                } else if let Some((0x30, list, _)) = tlv(c) {
                    st.mech_types = oids_of(list);
                }
            }
            // supportedMech OID in a NegTokenResp, or reqFlags in a NegTokenInit.
            0xa1 => {
                if let Some((0x06, oid, _)) = tlv(c) {
                    st.supported_mech = Some(oid.to_vec());
                }
            }
            // responseToken / mechToken.
            0xa2 => {
                if let Some((0x04, tok, _)) = tlv(c) {
                    st.token = Some(tok.to_vec());
                }
            }
            _ => {}
        }
    }
}

fn oids_of(mut list: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some((0x06, oid, used)) = tlv(list) {
        out.push(oid.to_vec());
        list = &list[used..];
    }
    out
}

/// Parse whichever shape a server chose to send.
///
/// Servers are inconsistent here: some send a bare `NegTokenResp`, some wrap it in
/// an `initialContextToken`, some answer with a Kerberos mechanism token inside the
/// wrapper. All are handled, and a shape that is not recognised yields `None`
/// rather than a guess. A wrapper that names a mechanism is reported even when
/// what follows is not a negotiation token — that is exactly the Kerberos case,
/// and losing it would turn a precise diagnosis into a parse error.
pub fn parse_server_token(der: &[u8]) -> Option<ServerToken> {
    let mut st = ServerToken::default();

    let inner = match tlv(der) {
        Some((0x60, contents, _)) => {
            let (t, oid, used) = tlv(contents)?;
            if t != 0x06 {
                return None;
            }
            st.outer_mech = Some(oid.to_vec());
            &contents[used..]
        }
        _ => der,
    };

    if let Some((tag, contents, _)) = tlv(inner) {
        let seq = match tag {
            // NegotiationToken ::= CHOICE { negTokenInit [0], negTokenResp [1] }
            0xa0 | 0xa1 => match tlv(contents) {
                Some((0x30, c, _)) => Some(c),
                _ => None,
            },
            // A bare SEQUENCE: not conformant, but unambiguous.
            0x30 => Some(contents),
            _ => None,
        };
        if let Some(seq) = seq {
            walk_fields(&mut st, seq);
        }
    }

    let recognised = st.outer_mech.is_some()
        || st.state.is_some()
        || st.supported_mech.is_some()
        || st.token.is_some()
        || !st.mech_types.is_empty();
    if recognised {
        Some(st)
    } else {
        None
    }
}

/// The decoded token from a `WWW-Authenticate: Negotiate <base64>` header.
///
/// `None` means either that the header is not a Negotiate challenge, or that it is
/// one carrying no data — which RFC 4559 §4.1 says is how a server invites the
/// client to start the exchange.
pub fn negotiate_payload(header: &str) -> Option<Vec<u8>> {
    let h = header.trim();
    if scheme_of(h)? != "negotiate" {
        return None;
    }
    // Index into the trimmed header, or a leading space would be taken for the
    // separator and the scheme itself would be fed to the base64 decoder.
    let rest = h[h.find(char::is_whitespace)?..].trim();
    if rest.is_empty() {
        return None;
    }
    base64_decode(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Independent of the encoder, this is the octet sequence RFC 4178 Appendix D
    /// describes: `30 nn A0 nn 30 nn 06 09 <kerberos oid>`, with SPNEGO's own
    /// wrapper and the NTLMSSP OID substituted.
    #[test]
    fn the_initial_token_matches_the_layout_rfc_4178_specifies() {
        let got = initial_token(&[0xaa, 0xbb, 0xcc]);
        let want: &[u8] = &[
            0x60, 0x23, // [APPLICATION 0], 35 content octets
            0x06, 0x06, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x02, // thisMech = SPNEGO
            0xa0, 0x19, // [0] negTokenInit, 25 content octets
            0x30, 0x17, // NegTokenInit SEQUENCE, 23 content octets
            0xa0, 0x0e, // mechTypes [0], 14 content octets
            0x30, 0x0c, // MechTypeList SEQUENCE, 12 content octets
            0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x02, 0x0a, // NTLMSSP
            0xa2, 0x05, // mechToken [2], 5 content octets
            0x04, 0x03, 0xaa, 0xbb, 0xcc, // the optimistic token
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn the_response_token_is_a_bare_neg_token_resp() {
        let got = response_token(&[0x11, 0x22]);
        let want: &[u8] = &[
            0xa1, 0x0d, // [1] negTokenResp, 13 content octets
            0x30, 0x0b, // SEQUENCE, 11 content octets
            0xa0, 0x03, 0x0a, 0x01, 0x00, // negState [0] ENUMERATED accept-completed
            0xa2, 0x04, 0x04, 0x02, 0x11, 0x22, // responseToken [2] OCTET STRING
        ];
        assert_eq!(got, want);
        // RFC 4178 4.2: subsequent tokens MUST NOT carry the GSS-API wrapper.
        assert_eq!(got[0], 0xa1, "no initialContextToken framing here");
    }

    #[test]
    fn our_own_initial_token_parses_back_to_ntlm() {
        let type1 = crate::auth::ntlm::type1("DOMAIN", "WKS");
        let parsed = parse_server_token(&initial_token(&type1)).expect("well-formed");
        assert_eq!(parsed.outer_mech.as_deref(), Some(oid::SPNEGO));
        assert_eq!(parsed.mech_types, vec![oid::NTLMSSP.to_vec()]);
        assert_eq!(parsed.token.as_deref(), Some(&type1[..]));
        assert_eq!(parsed.mech(), Some(Mech::Ntlm));
    }

    #[test]
    fn a_challenge_in_a_neg_token_resp_is_extracted() {
        // What a server sends once it picks NTLM: state accept-incomplete plus a
        // mechanism token. Assembled from the raw octets rather than by a helper,
        // so the test does not lean on the encoder it is checking.
        let challenge = b"NTLMSSP\0\x02\x00\x00\x00".to_vec();
        let resp = der_tlv(
            0xa1,
            &der_tlv(
                0x30,
                &der_join(&[
                    &der_tlv(0xa0, &[0x0a, 0x01, 0x01]),
                    &der_tlv(0xa2, &der_tlv(0x04, &challenge)),
                ]),
            ),
        );

        let parsed = parse_server_token(&resp).expect("well-formed");
        assert_eq!(parsed.state, Some(1), "accept-incomplete");
        assert_eq!(parsed.mech(), Some(Mech::Ntlm));
        assert_eq!(parsed.token.as_deref(), Some(&challenge[..]));
    }

    #[test]
    fn a_kerberos_token_in_the_wrapper_is_recognised_as_kerberos() {
        // Exactly the shape a Kerberos-only server answers with: an
        // initialContextToken whose mechanism is Kerberos.
        let krb5 = [0x6b, 0x82, 0x00, 0x10];
        let tok = der_tlv(0x60, &der_join(&[&der_tlv(0x06, oid::KERBEROS_V5), &krb5]));
        let parsed = parse_server_token(&tok).expect("the wrapper alone is informative");
        assert_eq!(parsed.mech(), Some(Mech::Kerberos));
        assert_eq!(parsed.token, None, "there is no NTLMSSP message to be had");
    }

    #[test]
    fn the_legacy_microsoft_kerberos_oid_is_also_recognised() {
        let tok = der_tlv(
            0x60,
            &der_join(&[&der_tlv(0x06, oid::MS_KERBEROS_V5), &[0x00]]),
        );
        assert_eq!(
            parse_server_token(&tok).and_then(|t| t.mech()),
            Some(Mech::Kerberos),
            "RFC 4178 Appendix C: this wrong OID is deployed, so it must still diagnose"
        );
    }

    #[test]
    fn a_kerberos_supported_mech_alone_is_enough() {
        // No mechanism token, but the server named its choice.
        let resp = der_tlv(
            0xa1,
            &der_tlv(
                0x30,
                &der_join(&[
                    &der_tlv(0xa0, &[0x0a, 0x01, 0x00]),
                    &der_tlv(0xa1, &der_tlv(0x06, oid::KERBEROS_V5)),
                ]),
            ),
        );
        let parsed = parse_server_token(&resp).expect("well-formed");
        assert_eq!(parsed.supported_mech.as_deref(), Some(oid::KERBEROS_V5));
        assert_eq!(parsed.mech(), Some(Mech::Kerberos));
    }

    #[test]
    fn a_reject_is_reported_as_one() {
        let resp = der_tlv(0xa1, &der_tlv(0x30, &der_tlv(0xa0, &[0x0a, 0x01, 0x02])));
        let parsed = parse_server_token(&resp).expect("well-formed");
        assert!(parsed.is_reject());
        assert_eq!(parsed.mech(), None);
    }

    #[test]
    fn multi_octet_lengths_round_trip() {
        // A real Type 1 is 32 octets, but nothing may assume the lengths fit in the
        // short form; 300 forces the 0x82 encoding.
        let big: Vec<u8> = (0..300u32).map(|i| (i % 251) as u8).collect();
        let parsed = parse_server_token(&initial_token(&big)).expect("well-formed");
        assert_eq!(parsed.token.as_deref(), Some(&big[..]));
        // And the length octets really did take the long form.
        assert_eq!(initial_token(&big)[1], 0x82);
    }

    #[test]
    fn a_truncated_or_bogus_token_is_rejected_rather_than_guessed() {
        assert_eq!(parse_server_token(&[]), None);
        assert_eq!(parse_server_token(&[0xa1]), None);
        assert_eq!(
            parse_server_token(&[0xa1, 0xff, 0x00]),
            None,
            "runs past the end"
        );
        assert_eq!(parse_server_token(b"not der at all"), None);
        // A length encoded with the indefinite form (BER, not DER).
        assert_eq!(parse_server_token(&[0xa1, 0x80, 0x00, 0x00]), None);
    }

    #[test]
    fn the_negotiate_header_payload_is_decoded() {
        let tok = initial_token(b"NTLMSSP\0");
        let header = format!("Negotiate {}", base64_encode(&tok));
        assert_eq!(negotiate_payload(&header).as_deref(), Some(&tok[..]));
        // Case-insensitive scheme, leading spaces, and a token that needs padding.
        assert_eq!(
            negotiate_payload(&format!("  negotiate   {}", base64_encode(&[1, 2, 3]))).as_deref(),
            Some(&[1u8, 2, 3][..])
        );

        // A token-less invitation (RFC 4559 4.1) and a different scheme.
        assert_eq!(negotiate_payload("Negotiate"), None);
        assert_eq!(negotiate_payload("Negotiate   "), None);
        assert_eq!(negotiate_payload("NTLM TlRMTVNTUAAB"), None);
        assert_eq!(negotiate_payload("Basic realm=x"), None);
    }

    #[test]
    fn a_bare_ntlmssp_message_is_not_mistaken_for_spnego() {
        // Non-conformant, but a server could send the raw message; the caller must
        // not treat it as a negotiation token.
        assert_eq!(parse_server_token(b"NTLMSSP\0\x02\x00\x00\x00"), None);
        assert_eq!(mech_of_token(b"NTLMSSP\0"), Some(Mech::Ntlm));
    }

    #[test]
    fn the_authorization_helpers_are_the_base64_of_the_tokens() {
        assert_eq!(
            initial_authorization(&[0xaa, 0xbb, 0xcc]),
            base64_encode(&initial_token(&[0xaa, 0xbb, 0xcc]))
        );
        assert_eq!(
            response_authorization(&[0x11, 0x22]),
            base64_encode(&response_token(&[0x11, 0x22]))
        );
        // ...and both decode as valid base64 back into DER.
        assert!(base64_decode(&initial_authorization(b"x")).is_some());
        assert!(base64_decode(&response_authorization(b"x")).is_some());
    }
}
