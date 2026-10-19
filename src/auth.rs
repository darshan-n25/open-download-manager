//! HTTP authentication: Basic, Bearer, Digest (RFC 7616), NTLMv2, and
//! Negotiate (SPNEGO, RFC 4178/4559).
//!
//! IDM's vocabulary, recovered from its string table, is exactly this set:
//! `Authorization: Basic %s`, `Authorization: Digest username="%s", realm="%s",
//! nonce="%s", uri="%s", ...`, `BearerAuth`, `Proxy-Authorization: Basic|Digest`,
//! and SSPI-backed Negotiate (`AcquireCredentialsHandleA`).
//!
//! The one material difference is underneath Negotiate. IDM hands it to Windows
//! SSPI, which can produce a real Kerberos ticket. Riptide implements SPNEGO with
//! NTLMSSP as the inner mechanism, which covers a server that accepts NTLM inside
//! `Negotiate` but not one configured for Kerberos only. See
//! [`spnego`] for why that line is drawn where it is, and how the unsupported case
//! is reported.

pub mod ntlm;
pub mod spnego;
/// The daemon's own bearer token — the server side, not a credential for a remote server.
pub mod token;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthKind {
    Basic,
    Digest,
    Bearer,
    Ntlm,
    /// SPNEGO (`WWW-Authenticate: Negotiate`), completed with NTLM inside.
    Negotiate,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuthSpec {
    pub kind: AuthKind,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Bearer token.
    pub token: Option<String>,
    /// NTLM domain (or `DOMAIN` from `DOMAIN\user`).
    pub domain: Option<String>,
    pub workstation: Option<String>,
}

impl AuthSpec {
    pub fn basic(user: impl Into<String>, pass: impl Into<String>) -> Self {
        AuthSpec {
            kind: AuthKind::Basic,
            username: Some(user.into()),
            password: Some(pass.into()),
            token: None,
            domain: None,
            workstation: None,
        }
    }

    pub fn bearer(token: impl Into<String>) -> Self {
        AuthSpec {
            kind: AuthKind::Bearer,
            username: None,
            password: None,
            token: Some(token.into()),
            domain: None,
            workstation: None,
        }
    }

    pub fn digest(user: impl Into<String>, pass: impl Into<String>) -> Self {
        AuthSpec {
            kind: AuthKind::Digest,
            username: Some(user.into()),
            password: Some(pass.into()),
            token: None,
            domain: None,
            workstation: None,
        }
    }

    /// NTLMv2, sent as bare `Authorization: NTLM <base64>` messages.
    pub fn ntlm(
        user: impl Into<String>,
        pass: impl Into<String>,
        domain: impl Into<String>,
    ) -> Self {
        Self::challenge_response(AuthKind::Ntlm, user, pass, domain)
    }

    /// SPNEGO (`WWW-Authenticate: Negotiate`), completed with NTLMSSP as the inner
    /// mechanism. Same credentials as [`AuthSpec::ntlm`]; only the wrapping and the
    /// header name differ.
    pub fn negotiate(
        user: impl Into<String>,
        pass: impl Into<String>,
        domain: impl Into<String>,
    ) -> Self {
        Self::challenge_response(AuthKind::Negotiate, user, pass, domain)
    }

    fn challenge_response(
        kind: AuthKind,
        user: impl Into<String>,
        pass: impl Into<String>,
        domain: impl Into<String>,
    ) -> Self {
        AuthSpec {
            kind,
            username: Some(user.into()),
            password: Some(pass.into()),
            token: None,
            domain: Some(domain.into()),
            workstation: None,
        }
    }

    pub fn user(&self) -> &str {
        self.username.as_deref().unwrap_or("")
    }

    pub fn pass(&self) -> &str {
        self.password.as_deref().unwrap_or("")
    }

    pub fn domain(&self) -> &str {
        self.domain.as_deref().unwrap_or("")
    }

    pub fn workstation(&self) -> String {
        self.workstation
            .clone()
            .or_else(|| {
                std::env::var("HOSTNAME")
                    .ok()
                    .or_else(|| std::env::var("COMPUTERNAME").ok())
            })
            .unwrap_or_else(|| "RIPTIDE".to_string())
    }

    /// Header this spec can supply *before* being challenged.
    /// Digest, NTLM and Negotiate are challenge-response and therefore return
    /// `None`.
    pub fn preemptive_header(&self) -> Option<(String, String)> {
        match self.kind {
            AuthKind::Basic => Some((
                "Authorization".into(),
                format!(
                    "Basic {}",
                    base64_encode(format!("{}:{}", self.user(), self.pass()).as_bytes())
                ),
            )),
            AuthKind::Bearer => Some((
                "Authorization".into(),
                format!("Bearer {}", self.token.as_deref().unwrap_or("")),
            )),
            AuthKind::Digest | AuthKind::Ntlm | AuthKind::Negotiate => None,
        }
    }
}

pub fn base64_encode(data: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.encode(data)
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.decode(s.trim()).ok()
}

/// Parse a `user:pass` style spec, with an optional `<kind>:` prefix.
///
/// Accepted forms: `user:pass`, `basic:user:pass`, `digest:user:pass`,
/// `bearer:<token>`, `ntlm:DOMAIN\user:pass`, `ntlm:user:pass` (domain defaults to
/// `""`), and `negotiate:` / `negotiate-ntlm:` for the same credentials sent
/// inside SPNEGO instead of as bare NTLM messages.
pub fn parse_auth_spec(s: &str) -> Option<AuthSpec> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (kind, rest) = match s.split_once(':') {
        Some((k, r))
            if matches!(
                k.to_ascii_lowercase().as_str(),
                "basic" | "digest" | "bearer" | "ntlm" | "negotiate" | "negotiate-ntlm"
            ) =>
        {
            (k.to_ascii_lowercase(), r)
        }
        _ => ("basic".to_string(), s),
    };

    match kind.as_str() {
        "bearer" => Some(AuthSpec::bearer(rest)),
        // `negotiate-ntlm:` is the same thing under its accurate name: SPNEGO with
        // NTLMSSP inside. Both spellings are accepted so that neither the user who
        // wants "Negotiate" nor the user who wants "exactly NTLM, wrapped" is
        // surprised.
        "ntlm" | "negotiate" | "negotiate-ntlm" => {
            // Only the first colon separates the credentials from the password, so a
            // password is free to contain colons.
            let (userpart, pass) = rest.split_once(':')?;
            let (domain, user) = split_domain(userpart);
            Some(if kind == "ntlm" {
                AuthSpec::ntlm(user, pass, domain)
            } else {
                AuthSpec::negotiate(user, pass, domain)
            })
        }
        "digest" => {
            let (user, pass) = rest.split_once(':')?;
            Some(AuthSpec::digest(user, pass))
        }
        _ => {
            let (user, pass) = rest.split_once(':')?;
            Some(AuthSpec::basic(user, pass))
        }
    }
}

/// Split `DOMAIN\user` into its two halves, or an empty domain when there is no
/// backslash.
fn split_domain(userpart: &str) -> (String, String) {
    match userpart.split_once('\\') {
        Some((d, u)) => (d.to_string(), u.to_string()),
        None => (String::new(), userpart.to_string()),
    }
}

/// First token of a `WWW-Authenticate` / `Proxy-Authenticate` header.
pub fn scheme_of(header: &str) -> Option<String> {
    let t = header.trim();
    if t.is_empty() {
        return None;
    }
    let name: String = t
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name.to_ascii_lowercase())
    }
}

// ---------------------------------------------------------------------------
// Digest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub opaque: Option<String>,
    pub algorithm: String,
    pub qop: Option<String>,
    pub stale: bool,
    pub charset: Option<String>,
    pub userhash: bool,
}

/// Split a comma-separated header parameter list, honouring quoted strings.
fn split_params(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut key = String::new();
    let mut val = String::new();
    let mut in_key = true;
    let mut in_quotes = false;
    let mut escaped = false;

    for c in s.chars() {
        if escaped {
            val.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            '=' if in_key && !in_quotes => in_key = false,
            ',' if !in_quotes => {
                let k = key.trim().to_ascii_lowercase();
                if !k.is_empty() {
                    out.push((k, val.trim().to_string()));
                }
                key.clear();
                val.clear();
                in_key = true;
            }
            c => {
                if in_key {
                    key.push(c)
                } else {
                    val.push(c)
                }
            }
        }
    }
    let k = key.trim().to_ascii_lowercase();
    if !k.is_empty() {
        out.push((k, val.trim().to_string()));
    }
    out
}

/// Parse a `Digest ...` challenge. Returns `None` if it is not a Digest challenge
/// or the mandatory `realm`/`nonce` are missing.
pub fn parse_digest_challenge(header: &str) -> Option<DigestChallenge> {
    let h = header.trim();
    if scheme_of(h)? != "digest" {
        return None;
    }
    let rest = h[h.find(char::is_whitespace).unwrap_or(h.len())..].trim();
    let params = split_params(rest);
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

    let realm = get("realm")?;
    let nonce = get("nonce")?;
    Some(DigestChallenge {
        realm,
        nonce,
        opaque: get("opaque"),
        algorithm: get("algorithm").unwrap_or_else(|| "MD5".to_string()),
        qop: get("qop"),
        stale: get("stale")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
        charset: get("charset"),
        userhash: get("userhash")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
    })
}

fn hash_hex(algo: &str, data: &[u8]) -> String {
    if algo.to_ascii_uppercase().starts_with("SHA-256") {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(data))
    } else {
        use md5::{Digest, Md5};
        hex::encode(Md5::digest(data))
    }
}

/// Per-connection Digest state. `nc` must increment for every request sent with the
/// same nonce, so this is kept alongside the session rather than recomputed.
#[derive(Debug, Clone)]
pub struct DigestSession {
    pub cnonce: String,
    pub nc: u32,
}

impl DigestSession {
    pub fn new() -> Self {
        DigestSession {
            cnonce: crate::util::gen_id(),
            nc: 0,
        }
    }
}

impl Default for DigestSession {
    fn default() -> Self {
        Self::new()
    }
}

/// Pick the usable qop from the server's list (`auth` preferred over `auth-int`).
fn pick_qop(qop: Option<&str>) -> Option<&str> {
    let q = qop?;
    let items: Vec<&str> = q.split(',').map(|s| s.trim()).collect();
    items
        .iter()
        .find(|i| i.eq_ignore_ascii_case("auth"))
        .or_else(|| items.iter().find(|i| i.eq_ignore_ascii_case("auth-int")))
        .copied()
}

/// Build the `Authorization: Digest ...` header value for one request.
pub fn digest_authorization(
    chal: &DigestChallenge,
    user: &str,
    pass: &str,
    method: &str,
    uri: &str,
    session: &mut DigestSession,
    entity_body: &[u8],
) -> String {
    let algo = chal.algorithm.to_ascii_uppercase();
    let sess = algo.ends_with("-SESS");
    let qop = pick_qop(chal.qop.as_deref());

    let mut ha1 = hash_hex(
        &algo,
        format!("{}:{}:{}", user, chal.realm, pass).as_bytes(),
    );
    if sess {
        ha1 = hash_hex(
            &algo,
            format!("{}:{}:{}", ha1, chal.nonce, session.cnonce).as_bytes(),
        );
    }

    let ha2 = match qop {
        Some(q) if q.eq_ignore_ascii_case("auth-int") => hash_hex(
            &algo,
            format!("{}:{}:{}", method, uri, hash_hex(&algo, entity_body)).as_bytes(),
        ),
        _ => hash_hex(&algo, format!("{}:{}", method, uri).as_bytes()),
    };

    session.nc += 1;
    let nc = format!("{:08x}", session.nc);

    let response = match qop {
        Some(q) => hash_hex(
            &algo,
            format!(
                "{}:{}:{}:{}:{}:{}",
                ha1, chal.nonce, nc, session.cnonce, q, ha2
            )
            .as_bytes(),
        ),
        None => hash_hex(&algo, format!("{}:{}:{}", ha1, chal.nonce, ha2).as_bytes()),
    };

    // The username may itself be a `userhash` (H(user ":" realm)).
    let username_field = if chal.userhash {
        hash_hex(&algo, format!("{}:{}", user, chal.realm).as_bytes())
    } else {
        user.to_string()
    };

    let mut parts = vec![
        format!("username=\"{}\"", username_field),
        format!("realm=\"{}\"", chal.realm),
        format!("nonce=\"{}\"", chal.nonce),
        format!("uri=\"{}\"", uri),
        format!("response=\"{}\"", response),
        format!("algorithm={}", chal.algorithm),
    ];
    if let Some(q) = qop {
        parts.push(format!("qop={}", q));
        parts.push(format!("nc={}", nc));
        parts.push(format!("cnonce=\"{}\"", session.cnonce));
    }
    if let Some(o) = &chal.opaque {
        parts.push(format!("opaque=\"{}\"", o));
    }
    if let Some(c) = &chal.charset {
        parts.push(format!("charset={}", c));
    }
    if chal.userhash {
        parts.push("userhash=true".to_string());
    }
    format!("Digest {}", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_specs() {
        let b = parse_auth_spec("alice:secret").unwrap();
        assert_eq!(b.kind, AuthKind::Basic);
        assert_eq!(b.user(), "alice");

        let br = parse_auth_spec("bearer:abc.def").unwrap();
        assert_eq!(br.kind, AuthKind::Bearer);
        assert_eq!(br.token.as_deref(), Some("abc.def"));

        let n = parse_auth_spec("ntlm:CORP\\bob:hunter2").unwrap();
        assert_eq!(n.kind, AuthKind::Ntlm);
        assert_eq!(n.domain(), "CORP");
        assert_eq!(n.user(), "bob");

        let ng = parse_auth_spec("negotiate:CORP\\bob:hunter2").unwrap();
        assert_eq!(ng.kind, AuthKind::Negotiate);
        assert_eq!(ng.domain(), "CORP");
        assert_eq!(ng.user(), "bob");

        // The accurate name for the same thing, and the prefix is case-insensitive.
        let alias = parse_auth_spec("Negotiate-NTLM:bob:pw").unwrap();
        assert_eq!(alias.kind, AuthKind::Negotiate);
        assert_eq!(alias.domain(), "");
        assert_eq!(alias.user(), "bob");

        // ...but plain NTLM and Negotiate must not be confused with each other.
        assert_ne!(n.kind, ng.kind);

        assert!(parse_auth_spec("nopass").is_none());
    }

    /// Only the *first* colon separates the user (or `DOMAIN\user`) from the
    /// password, so a password may contain colons.
    #[test]
    fn a_password_may_contain_a_colon() {
        let ng = parse_auth_spec("negotiate:CORP\\bob:p:a:ss").unwrap();
        assert_eq!(ng.domain(), "CORP");
        assert_eq!(ng.user(), "bob");
        assert_eq!(ng.pass(), "p:a:ss");
    }

    #[test]
    fn negotiate_is_challenge_response_and_serializes_by_name() {
        let a = AuthSpec::negotiate("bob", "pw", "CORP");
        // Nothing may be sent before the server offers a challenge: the token has to
        // come back from the server's mechTypes choice first.
        assert!(a.preemptive_header().is_none());
        // Jobs are persisted as JSON, so the spelling is part of the on-disk format.
        assert_eq!(serde_json::to_string(&a.kind).unwrap(), "\"negotiate\"");
        assert_eq!(
            serde_json::from_str::<AuthKind>("\"negotiate\"").unwrap(),
            AuthKind::Negotiate
        );
    }

    #[test]
    fn basic_header_is_rfc7617() {
        let a = AuthSpec::basic("Aladdin", "open sesame");
        let (k, v) = a.preemptive_header().unwrap();
        assert_eq!(k, "Authorization");
        assert_eq!(v, "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    }

    #[test]
    fn challenge_parsing_handles_quoted_commas() {
        let h =
            r#"Digest realm="a,b", nonce="xyz", qop="auth,auth-int", algorithm=MD5, opaque="op""#;
        let c = parse_digest_challenge(h).unwrap();
        assert_eq!(c.realm, "a,b");
        assert_eq!(c.nonce, "xyz");
        assert_eq!(c.algorithm, "MD5");
        assert_eq!(c.opaque.as_deref(), Some("op"));
        assert_eq!(pick_qop(c.qop.as_deref()), Some("auth"));
    }

    /// The canonical worked example from RFC 7616 §3.9.1 (MD5 + qop=auth).
    #[test]
    fn digest_matches_rfc7616_example() {
        let chal = DigestChallenge {
            realm: "http-auth@example.org".into(),
            nonce: "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v".into(),
            opaque: Some("FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS".into()),
            algorithm: "MD5".into(),
            qop: Some("auth".into()),
            stale: false,
            charset: None,
            userhash: false,
        };
        let mut s = DigestSession {
            cnonce: "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ".into(),
            nc: 0,
        };
        let hdr = digest_authorization(
            &chal,
            "Mufasa",
            "Circle of Life",
            "GET",
            "/dir/index.html",
            &mut s,
            b"",
        );
        assert!(
            hdr.contains("response=\"8ca523f5e9506fed4657c9700eebdbec\""),
            "{hdr}"
        );
        assert!(hdr.contains("nc=00000001"));
        assert!(hdr.contains("qop=auth"));
    }
}
