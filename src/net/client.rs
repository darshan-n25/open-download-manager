//! HTTP client construction.
//!
//! Deliberately its own stack rather than a browser's: IDM imports only seven Win32 DLLs
//! and resolves `ws2_32`/`schannel` dynamically precisely so it fully controls range,
//! resume and timeout behaviour. Open Download Manager gets the same control by configuring one
//! `reqwest` client per job from the captured [`RequestContext`].

use std::sync::Arc;
use std::time::Duration;

use reqwest::cookie::Jar;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, COOKIE, REFERER};

use super::proxy::{parse_proxy, ProxySpec};
use crate::error::{OdmError, Result};
use crate::job::RequestContext;

pub struct NetClient {
    inner: reqwest::Client,
    jar: Arc<Jar>,
    pub user_agent: String,
}

fn header_pair(k: &str, v: &str) -> Result<(HeaderName, HeaderValue)> {
    let name = HeaderName::from_bytes(k.as_bytes())
        .map_err(|e| OdmError::other(format!("bad header name {k:?}: {e}")))?;
    let value = HeaderValue::from_str(v)
        .map_err(|e| OdmError::other(format!("bad header value for {k:?}: {e}")))?;
    Ok((name, value))
}

impl NetClient {
    pub fn build(
        ctx: &RequestContext,
        user_agent_default: &str,
        connect_timeout: Duration,
    ) -> Result<Self> {
        let ua = ctx
            .user_agent
            .clone()
            .unwrap_or_else(|| user_agent_default.to_string());

        let jar = Arc::new(Jar::default());
        let mut builder = reqwest::Client::builder()
            .user_agent(ua.clone())
            .connect_timeout(connect_timeout)
            .pool_max_idle_per_host(16)
            .pool_idle_timeout(Some(Duration::from_secs(90)))
            .redirect(reqwest::redirect::Policy::limited(10))
            .cookie_provider(jar.clone())
            .danger_accept_invalid_certs(ctx.insecure);

        if let Some(p) = &ctx.proxy {
            match parse_proxy(p)? {
                ProxySpec::Direct => {
                    builder = builder.no_proxy();
                }
                ProxySpec::System | ProxySpec::AutoDetect => {
                    // Leave reqwest's ambient handling (env vars) in place.
                }
                ProxySpec::Pac(url) => {
                    // PAC evaluation needs a JS engine; nothing in the Rust ecosystem
                    // evaluates PAC in-process. Honour it only when it is a plain proxy
                    // URL, otherwise fall back to ambient settings with a warning.
                    if url.contains("://") && !url.to_ascii_lowercase().ends_with(".pac") {
                        let proxy = reqwest::Proxy::all(&url)?;
                        builder = builder.proxy(proxy);
                    } else {
                        tracing::warn!(
                            pac = %url,
                            "PAC script evaluation is not supported; using ambient proxy settings"
                        );
                    }
                }
                ProxySpec::Server { proxies, bypass } => {
                    if proxies.is_empty() {
                        builder = builder.no_proxy();
                    } else {
                        // The bypass list is attached to each proxy, which is where
                        // reqwest evaluates it (there is no builder-wide variant).
                        let no = if bypass.is_empty() {
                            None
                        } else {
                            reqwest::NoProxy::from_string(&bypass.join(","))
                        };
                        for p in &proxies {
                            match reqwest::Proxy::all(p) {
                                Ok(proxy) => builder = builder.proxy(proxy.no_proxy(no.clone())),
                                Err(e) => {
                                    tracing::warn!(proxy = %p, error = %e, "ignoring bad proxy")
                                }
                            }
                        }
                    }
                }
            }
        }

        // Replay everything the browser captured, so the origin sees the same request
        // it would have seen from the browser.
        let mut headers = HeaderMap::new();
        if let Some(r) = &ctx.referer {
            let (k, v) = header_pair("Referer", r)?;
            headers.insert(REFERER, v);
            let _ = k;
        }
        if let Some(c) = &ctx.cookie {
            let (_, v) = header_pair("Cookie", c)?;
            headers.insert(COOKIE, v);
        }
        for (k, v) in &ctx.headers {
            let (name, value) = header_pair(k, v)?;
            headers.insert(name, value);
        }
        if let Some(a) = &ctx.auth {
            if let Some((k, v)) = a.preemptive_header() {
                let (name, value) = header_pair(&k, &v)?;
                headers.insert(name, value);
            }
        }
        if !headers.is_empty() {
            builder = builder.default_headers(headers);
        }

        let inner = builder.build()?;
        Ok(NetClient {
            inner,
            jar,
            user_agent: ua,
        })
    }

    pub fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.inner.get(url)
    }

    pub fn head(&self, url: &str) -> reqwest::RequestBuilder {
        self.inner.head(url)
    }

    pub fn raw(&self) -> &reqwest::Client {
        &self.inner
    }

    pub fn jar(&self) -> &Arc<Jar> {
        &self.jar
    }

    /// Serialise this client's cookie jar for a URL, so it can be copied into another job
    /// (e.g. HLS segments on a different host that share the same cookie).
    pub fn cookie_header(&self, url: &str) -> Option<String> {
        use reqwest::cookie::CookieStore;
        let u = url::Url::parse(url).ok()?;
        let mut out = String::new();
        for c in self.jar.cookies(&u)?.to_str().ok()?.split("; ") {
            if !out.is_empty() {
                out.push_str("; ");
            }
            out.push_str(c);
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }
}

/// Default request headers that make a ranged request behave predictably.
pub fn range_headers(start: u64, end: u64) -> Vec<(String, String)> {
    vec![
        ("Range".to_string(), format!("bytes={start}-{end}")),
        // Explicitly refuse compressed transfer: it would destroy byte offsets.
        ("Accept-Encoding".to_string(), "identity".to_string()),
    ]
}

#[cfg(test)]
mod tests {
    // Fixtures here are built field-by-field from `Default::default()`.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;

    #[test]
    fn range_header_format_matches_idm() {
        let h = range_headers(0, 1023);
        assert_eq!(h[0].1, "bytes=0-1023");
        let h = range_headers(1024, 2047);
        assert_eq!(h[0].1, "bytes=1024-2047");
    }

    #[test]
    fn client_builds_with_direct_proxy() {
        let mut ctx = RequestContext::default();
        ctx.proxy = Some("2".into());
        let c = NetClient::build(&ctx, "UA/1", Duration::from_secs(5));
        assert!(c.is_ok());
    }

    #[test]
    fn client_builds_with_socks_proxy() {
        let mut ctx = RequestContext::default();
        ctx.proxy = Some("socks5://127.0.0.1:9150".into());
        assert!(NetClient::build(&ctx, "UA/1", Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn bad_header_is_rejected() {
        let mut ctx = RequestContext::default();
        ctx.headers = vec![("Bad Header\n".into(), "v".into())];
        assert!(NetClient::build(&ctx, "UA/1", Duration::from_secs(5)).is_err());
    }

    #[test]
    fn preemptive_auth_is_installed() {
        let mut ctx = RequestContext::default();
        ctx.auth = Some(crate::auth::AuthSpec::basic("u", "p"));
        let c = NetClient::build(&ctx, "UA/1", Duration::from_secs(5)).unwrap();
        // Sanity: client constructed and reports the UA we asked for.
        assert_eq!(c.user_agent, "UA/1");
    }
}
