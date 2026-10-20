//! Proxy configuration.
//!
//! The browser extension hands IDM a compact string built from the browser's own proxy
//! settings. Recovered from `IDMGCExt/background.js`:
//!
//! ```text
//! mode := <n>[:<value>][;<bypass>,<bypass>...]
//!   1 = explicit server(s), value = "scheme://host:port" or a ';'-joined list
//!   2 = direct
//!   3 = auto-detect
//!   4 = system
//!   5 = PAC script (data or URL)
//!   6 = PAC data
//!   7 = fixed servers, value = "http://p;socks5://q"  (+ ";bypass,...")
//! ```
//!
//! Open Download Manager accepts that exact syntax *and* a plain proxy URL, so `--proxy` works the way
//! people expect while remaining compatible with what an IDM-style extension sends.

use crate::error::{OdmError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxySpec {
    /// Go straight out, ignore any ambient proxy configuration.
    Direct,
    /// Use the OS/environment proxy settings.
    System,
    /// Auto-detect (WPAD/PAC discovery).
    AutoDetect,
    /// Use a PAC script (data URL or http(s) URL).
    Pac(String),
    /// One or more explicit proxies, plus an optional bypass list.
    Server {
        proxies: Vec<String>,
        bypass: Vec<String>,
    },
}

impl ProxySpec {
    /// Turn this spec into what a network client needs: either nothing (use ambient
    /// settings), or an ordered list of proxy URLs plus a bypass list.
    pub fn resolve(&self) -> (Option<Vec<String>>, Vec<String>) {
        match self {
            ProxySpec::Direct => (Some(Vec::new()), Vec::new()),
            ProxySpec::System | ProxySpec::AutoDetect => (None, Vec::new()),
            ProxySpec::Pac(url) => (Some(vec![url.clone()]), Vec::new()),
            ProxySpec::Server { proxies, bypass } => {
                if proxies.is_empty() {
                    (Some(Vec::new()), bypass.clone())
                } else {
                    (Some(proxies.clone()), bypass.clone())
                }
            }
        }
    }

    pub fn is_direct(&self) -> bool {
        matches!(self, ProxySpec::Direct)
    }
}

/// Normalise a proxy URL: add `http://` when no scheme was given.
fn normalise_url(s: &str) -> String {
    let s = s.trim();
    if s.contains("://") {
        s.to_string()
    } else {
        format!("http://{s}")
    }
}

fn valid_scheme(s: &str) -> bool {
    matches!(
        s.split("://")
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h" | "socks"
    )
}

/// Split a mixed `;`/`,`-separated list into proxy URLs and bypass entries.
///
/// IDM's compact string is genuinely ambiguous: in `1:http://p:3128;localhost` the `;`
/// introduces bypass entries, while in `7:http://a:1;socks5://b:2` it separates two proxy
/// servers. Open Download Manager resolves it by shape rather than by position — an entry is a proxy only
/// if it carries a recognised scheme *and* a port; anything else is a bypass pattern.
fn split_proxies_and_bypass<'a>(
    items: impl Iterator<Item = &'a str>,
) -> (Vec<String>, Vec<String>) {
    let mut proxies = Vec::new();
    let mut bypass = Vec::new();
    for raw in items {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let norm = normalise_url(raw);
        let has_port = norm
            .rsplit_once(':')
            .map(|(_, p)| p.parse::<u16>().is_ok())
            .unwrap_or(false);
        if has_port && valid_scheme(&norm) {
            proxies.push(norm);
        } else {
            bypass.push(raw.to_string());
        }
    }
    (proxies, bypass)
}

/// Parse either IDM's compact proxy string or a plain proxy URL.
pub fn parse_proxy(input: &str) -> Result<ProxySpec> {
    let s = input.trim();
    if s.is_empty() {
        return Ok(ProxySpec::Direct);
    }

    // Plain URL form, e.g. "socks5://127.0.0.1:9050" (but not a list of several).
    if s.contains("://") && !s.contains([',', ';']) && !s.starts_with(|c: char| c.is_ascii_digit())
    {
        let url = normalise_url(s);
        if !valid_scheme(&url) {
            return Err(OdmError::other(format!("unsupported proxy scheme: {s}")));
        }
        return Ok(ProxySpec::Server {
            proxies: vec![url],
            bypass: Vec::new(),
        });
    }

    // Bare host:port form.
    if !s.contains(':') || s.split(':').next().unwrap_or("").parse::<u64>().is_err() {
        if let Some((host, port)) = s.rsplit_once(':') {
            if port.parse::<u16>().is_ok()
                && !host.is_empty()
                && !host.chars().all(|c| c.is_ascii_digit())
            {
                return Ok(ProxySpec::Server {
                    proxies: vec![format!("http://{s}")],
                    bypass: Vec::new(),
                });
            }
        }
    }

    // Mode-prefixed form: "N" or "N:value" (see the module docs for the mode table).
    let (mode_str, value) = match s.split_once(':') {
        Some((m, v)) => (m.trim(), v.trim()),
        None => (s, ""),
    };
    if mode_str.len() == 1 && mode_str.chars().all(|c| c.is_ascii_digit()) {
        let mode: u8 = mode_str.parse().unwrap();
        return match mode {
            1 | 7 => {
                let items = value
                    .split([';', ','])
                    .map(|p| p.trim())
                    .filter(|p| !p.is_empty());
                let (proxies, bypass) = split_proxies_and_bypass(items);
                Ok(ProxySpec::Server { proxies, bypass })
            }
            2 => Ok(ProxySpec::Direct),
            3 => Ok(ProxySpec::AutoDetect),
            4 => Ok(ProxySpec::System),
            5 | 6 => {
                if value.is_empty() {
                    Ok(ProxySpec::System)
                } else {
                    Ok(ProxySpec::Pac(value.to_string()))
                }
            }
            other => Err(OdmError::other(format!("unknown proxy mode {other}"))),
        };
    }

    // Otherwise the whole string is a server list, with any non-proxy entry as bypass.
    let (proxies, bypass) = split_proxies_and_bypass(
        s.split([';', ','])
            .map(|p| p.trim())
            .filter(|p| !p.is_empty()),
    );
    Ok(ProxySpec::Server { proxies, bypass })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_socks_url() {
        assert_eq!(
            parse_proxy("socks5://127.0.0.1:9050").unwrap(),
            ProxySpec::Server {
                proxies: vec!["socks5://127.0.0.1:9050".into()],
                bypass: vec![]
            }
        );
    }

    #[test]
    fn bare_host_port_gets_http_scheme() {
        assert_eq!(
            parse_proxy("127.0.0.1:8080").unwrap(),
            ProxySpec::Server {
                proxies: vec!["http://127.0.0.1:8080".into()],
                bypass: vec![]
            }
        );
    }

    #[test]
    fn idm_mode_direct_and_system() {
        assert_eq!(parse_proxy("2").unwrap(), ProxySpec::Direct);
        assert_eq!(parse_proxy("4").unwrap(), ProxySpec::System);
        assert_eq!(parse_proxy("3").unwrap(), ProxySpec::AutoDetect);
    }

    #[test]
    fn idm_explicit_mode_with_bypass() {
        let p = parse_proxy("1:http://proxy.corp:3128;localhost,10.0.0.0/8").unwrap();
        assert_eq!(
            p,
            ProxySpec::Server {
                proxies: vec!["http://proxy.corp:3128".into()],
                bypass: vec!["localhost".into(), "10.0.0.0/8".into()]
            }
        );
    }

    #[test]
    fn idm_fixed_servers_multi_scheme() {
        let p = parse_proxy("7:http://a:1;socks5://b:2").unwrap();
        match p {
            ProxySpec::Server { proxies, .. } => {
                assert_eq!(proxies, vec!["http://a:1", "socks5://b:2"]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn pac_modes() {
        assert_eq!(
            parse_proxy("5:http://wpad/wpad.dat").unwrap(),
            ProxySpec::Pac("http://wpad/wpad.dat".into())
        );
        assert_eq!(parse_proxy("5:").unwrap(), ProxySpec::System);
    }

    #[test]
    fn empty_means_direct() {
        assert_eq!(parse_proxy("").unwrap(), ProxySpec::Direct);
        assert!(parse_proxy("  ").unwrap().is_direct());
    }

    #[test]
    fn resolve_shapes() {
        let (proxies, _) = parse_proxy("socks5://127.0.0.1:9150").unwrap().resolve();
        assert_eq!(proxies.unwrap().len(), 1);
        let (proxies, _) = ProxySpec::System.resolve();
        assert!(proxies.is_none());
        let (proxies, _) = ProxySpec::Direct.resolve();
        assert_eq!(proxies.unwrap().len(), 0);
    }

    #[test]
    fn unknown_mode_is_an_error() {
        assert!(parse_proxy("9:whatever").is_err());
    }
}
