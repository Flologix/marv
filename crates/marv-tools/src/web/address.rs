//! Where `web_fetch` may connect: the public internet only. The model picks the URL, and a page it reads can carry
//! planted instructions; without this they could make it fetch something on the user's own machine or network: a
//! router's admin page, a debugging port (localhost:9222), a cloud metadata service (169.254.169.254) and the
//! credentials it hands out.
//!
//! Ports Marv's `src/tools/web/address.ts`; [`is_private_address`] fully, with its tests.
//!
//! - A host that is, or resolves to, any private address is refused (fail closed: no answer is no proof the name is
//!   public); `localhost` and `*.localhost` count as 127.0.0.1 without a lookup.
//! - Only "no such name" blames the address; a failing resolver is the network's fault, and the message says so.
//! - Lookups time out after [`DNS_TIMEOUT_MS`] = 10 s and stop with the run (Esc).
//! - marv-rs: [`check_address`] returns the addresses it approved, and the download connects to those only (Marv
//!   resolved the name a second time, so a DNS answer could change in between: rebinding).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use url::{Host, Url};

use crate::types::ToolError;
use crate::web::STOPPED;

pub const DNS_TIMEOUT_MS: u64 = 10_000;

fn private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0 // "this network"
        || a == 10
        || a == 127 // loopback
        || (a == 100 && (64..=127).contains(&b)) // carrier-grade NAT
        || (a == 169 && b == 254) // link-local, incl. cloud metadata
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || a >= 224 // multicast, reserved, broadcast
}

fn v4_of(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::new((hi >> 8) as u8, (hi & 255) as u8, (lo >> 8) as u8, (lo & 255) as u8)
}

fn private_v6(ip: Ipv6Addr) -> bool {
    let g = ip.segments();
    let zero = |r: std::ops::Range<usize>| g[r].iter().all(|x| *x == 0);
    if zero(0..6) {
        return true; // ::, ::1, and the old IPv4-compatible form
    }
    if zero(0..5) && g[5] == 0xffff {
        return private_v4(v4_of(g[6], g[7])); // ::ffff:127.0.0.1
    }
    if zero(0..4) && g[4] == 0xffff && g[5] == 0 {
        return true; // ::ffff:0:0/96 (SIIT)
    }
    if g[0] == 0x64 && g[1] == 0xff9b && zero(2..6) {
        return private_v4(v4_of(g[6], g[7])); // NAT64
    }
    if g[0] == 0x64 && g[1] == 0xff9b && g[2] == 1 {
        return private_v4(v4_of(g[6], g[7])); // local-use NAT64 (64:ff9b:1::/48)
    }
    if g[0] == 0x2002 {
        return private_v4(v4_of(g[1], g[2])); // 6to4 embeds the v4
    }
    // unique local (fc00::/7), link-local (fe80::/10), old site-local (fec0::/10), multicast (ff00::/8)
    (g[0] & 0xfe00) == 0xfc00 || (g[0] & 0xffc0) == 0xfe80 || (g[0] & 0xffc0) == 0xfec0 || (g[0] & 0xff00) == 0xff00
}

/// Whether an address is on the user's own machine or network. Not an address at all: refused rather than guessed.
pub fn is_private_address(address: &str) -> bool {
    let ip = address.trim_start_matches('[').trim_end_matches(']');
    let ip = ip.split('%').next().unwrap_or(ip); // a zone ("fe80::1%eth0") isn't part of the address
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => private_v4(v4),
        Ok(IpAddr::V6(v6)) => private_v6(v6),
        Err(_) => true,
    }
}

/// Why a lookup failed: only "no such name" blames the address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupError {
    NotFound,
    /// The resolver itself failed (no network, DNS not answering).
    Failed,
}

/// Name resolution, a trait so tests (which have no network) can replace it.
#[async_trait::async_trait]
pub trait Resolve: Send + Sync {
    async fn lookup(&self, host: &str) -> Result<Vec<IpAddr>, LookupError>;
}

/// A resolver held in settings that print themselves (`Debug`).
#[derive(Clone)]
pub struct Resolver(pub Arc<dyn Resolve>);

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Resolver")
    }
}

/// The system's resolver (getaddrinfo, through tokio).
pub struct SystemResolver;

#[async_trait::async_trait]
impl Resolve for SystemResolver {
    async fn lookup(&self, host: &str) -> Result<Vec<IpAddr>, LookupError> {
        match tokio::net::lookup_host((host, 0)).await {
            Ok(addrs) => Ok(addrs.map(|a| a.ip()).collect()),
            Err(err) => {
                // getaddrinfo's EAI_NONAME / EAI_NODATA; anything else (EAI_AGAIN…) is the network's fault.
                let message = err.to_string().to_lowercase();
                let not_found = ["not known", "no address associated", "nodename nor servname", "no such host"]
                    .iter()
                    .any(|m| message.contains(m));
                Err(if not_found { LookupError::NotFound } else { LookupError::Failed })
            }
        }
    }
}

#[derive(Clone)]
pub struct CheckOptions {
    /// Tests only: let a local server through the address check (the scheme check stays).
    pub allow_private: bool,
    pub dns_timeout: Duration,
    /// Default: [`SystemResolver`].
    pub resolver: Option<Arc<dyn Resolve>>,
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self { allow_private: false, dns_timeout: Duration::from_millis(DNS_TIMEOUT_MS), resolver: None }
    }
}

/// What can be refused without asking DNS: anything but http(s), a private address literal, `localhost`. A name is
/// left for [`check_address`] when the fetch runs: looking it up sooner (in the approval's preview) would send the
/// name to DNS before anyone said yes, and a name can carry data (`<secret>.example.com`).
pub fn check_address_unresolved(url: &Url, options: &CheckOptions) -> Result<(), ToolError> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(ToolError(format!("Only http and https links can be fetched, not {}.", url.scheme())));
    }
    if options.allow_private {
        return Ok(());
    }
    let host = match url.host() {
        Some(Host::Ipv4(ip)) => ip.to_string(),
        Some(Host::Ipv6(ip)) => ip.to_string(),
        Some(Host::Domain(name)) => {
            let host = name.trim_end_matches('.').to_ascii_lowercase();
            if host != "localhost" && !host.ends_with(".localhost") {
                return Ok(());
            }
            host
        }
        None => return Err(ToolError(format!("Not a valid URL: {url}. Give the full address, e.g. https://example.com/page."))),
    };
    let address = if url.host().is_some_and(|h| matches!(h, Host::Domain(_))) { "127.0.0.1".to_string() } else { host.clone() };
    if is_private_address(&address) {
        return Err(ToolError(format!(
            "Refused: {host} is a private address (this machine or its network). To reach a local server, use bash with network: true (it asks first)."
        )));
    }
    Ok(())
}

/// Refuses a URL web_fetch mustn't fetch: anything but http(s), or a host that is, or resolves to, a private address.
/// The URL parser has already normalized odd spellings of an address (0x7f.1, 2130706433 → 127.0.0.1). `cancel` is
/// the run's: stopping it stops a slow lookup too.
///
/// Returns the addresses the name resolved to (none for an address, or with `allow_private`): marv-rs connects to
/// exactly those, so a second lookup can't answer differently (Marv's DNS-rebinding gap).
pub async fn check_address(url: &Url, options: &CheckOptions, cancel: &CancellationToken) -> Result<Vec<IpAddr>, ToolError> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(ToolError(format!("Only http and https links can be fetched, not {}.", url.scheme())));
    }
    if options.allow_private {
        return Ok(Vec::new());
    }
    let (host, addresses, resolved) = match url.host() {
        Some(Host::Ipv4(ip)) => (ip.to_string(), vec![ip.to_string()], Vec::new()),
        Some(Host::Ipv6(ip)) => (ip.to_string(), vec![ip.to_string()], Vec::new()),
        Some(Host::Domain(name)) => {
            // Look up exactly the name the connection will use (a trailing dot is a different, absolute name); the
            // dot-less lowercase form is only for comparing and for messages.
            let host = name.trim_end_matches('.').to_ascii_lowercase();
            if host == "localhost" || host.ends_with(".localhost") {
                (host, vec!["127.0.0.1".to_string()], Vec::new())
            } else {
                let resolver: Arc<dyn Resolve> = options.resolver.clone().unwrap_or_else(|| Arc::new(SystemResolver));
                let ips = tokio::select! {
                    () = cancel.cancelled() => return Err(ToolError(STOPPED.into())),
                    () = tokio::time::sleep(options.dns_timeout) => {
                        return Err(ToolError(format!("Couldn't resolve {host}: timed out.")));
                    }
                    answer = resolver.lookup(name) => match answer {
                        Ok(ips) => ips,
                        Err(LookupError::NotFound) => return Err(ToolError(format!("Couldn't resolve {host}: check the address."))),
                        Err(LookupError::Failed) => {
                            return Err(ToolError(format!("Couldn't resolve {host}: no network, or DNS isn't answering?")));
                        }
                    },
                };
                // No answer is no proof the name is public: fail closed.
                if ips.is_empty() {
                    return Err(ToolError(format!("Couldn't resolve {host}: check the address.")));
                }
                (host, ips.iter().map(IpAddr::to_string).collect(), ips)
            }
        }
        None => return Err(ToolError(format!("Not a valid URL: {url}. Give the full address, e.g. https://example.com/page."))),
    };
    if let Some(blocked) = addresses.iter().find(|a| is_private_address(a)) {
        let subject = if *blocked == host { format!("{host} is") } else { format!("{host} is {blocked},") };
        return Err(ToolError(format!(
            "Refused: {subject} a private address (this machine or its network). To reach a local server, use bash with network: true (it asks first)."
        )));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_lookup_only_literals_and_localhost_are_refused() {
        let check = |u: &str| check_address_unresolved(&Url::parse(u).unwrap(), &CheckOptions::default());
        assert!(check("http://127.0.0.1/").is_err());
        assert!(check("http://[::1]/").is_err());
        assert!(check("http://10.0.0.5/x").is_err());
        assert!(check("http://localhost:3000/").is_err());
        assert!(check("http://a.localhost/").is_err());
        assert!(check("file:///etc/passwd").is_err());
        assert!(check("https://secret-data.example.com/").is_ok(), "a name waits for the fetch's own lookup");
        assert!(check("https://93.184.215.14/").is_ok());
    }

    #[test]
    fn private_addresses() {
        for ip in [
            "127.0.0.1",
            "127.0.0.2",
            "10.0.0.1",
            "172.16.5.4",
            "172.31.255.255",
            "192.168.1.5",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "[::1]",
            "fe80::1",
            "fe80::1%eth0",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:7f00:1",
            "::ffff:192.168.0.1",
            "64:ff9b::a00:1",
            "not an address",
            "192.0.0.1",
            "198.18.0.1",
            "198.19.255.255",
            "64:ff9b:1::a00:1",
            "::ffff:0:8.8.8.8",
            "2002:7f00:1::",
            "2002:c0a8:101::",
            "fec0::1",
            "feff::1",
        ] {
            assert!(is_private_address(ip), "{ip} is private");
        }
    }

    #[test]
    fn public_addresses() {
        for ip in [
            "93.184.215.14",
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700::6810:84e5",
            "::ffff:8.8.8.8",
            "172.15.255.255",
            "100.63.255.255",
            "169.253.0.1",
            "223.255.255.255",
            "198.17.255.255",
            "198.20.0.1",
            "64:ff9b::808:808",
            "2002:808:808::",
        ] {
            assert!(!is_private_address(ip), "{ip} is public");
        }
    }

    /// A resolver with fixed answers, counting its lookups.
    struct Fake {
        answer: Result<Vec<&'static str>, LookupError>,
        hang: bool,
        asked: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Resolve for Fake {
        async fn lookup(&self, host: &str) -> Result<Vec<IpAddr>, LookupError> {
            self.asked.lock().unwrap().push(host.to_string());
            if self.hang {
                std::future::pending::<()>().await;
            }
            self.answer.clone().map(|ips| ips.iter().map(|ip| ip.parse().unwrap()).collect())
        }
    }

    fn fake(answer: Result<Vec<&'static str>, LookupError>) -> Arc<Fake> {
        Arc::new(Fake { answer, hang: false, asked: Default::default() })
    }

    async fn check_with(url: &str, resolver: &Arc<Fake>) -> Result<Vec<IpAddr>, ToolError> {
        let options = CheckOptions { resolver: Some(resolver.clone()), ..CheckOptions::default() };
        check_address(&Url::parse(url).unwrap(), &options, &CancellationToken::new()).await
    }

    async fn check(url: &str) -> Result<Vec<IpAddr>, ToolError> {
        check_with(url, &fake(Err(LookupError::Failed))).await
    }

    #[tokio::test]
    async fn only_http_and_https() {
        assert_eq!(check("file:///etc/passwd").await.unwrap_err().0, "Only http and https links can be fetched, not file.");
        assert!(check("ftp://example.com/").await.unwrap_err().0.ends_with("not ftp."));
    }

    #[tokio::test]
    async fn refuses_private_addresses_however_theyre_written() {
        for url in [
            "http://127.0.0.1:9222/json",
            "http://0x7f.1/",
            "http://2130706433/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://10.0.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1./",
            "http://[::FFFF:127.0.0.1]/",
        ] {
            assert!(check(url).await.unwrap_err().0.contains("a private address"), "{url}");
        }
        assert_eq!(
            check("http://127.0.0.1:9222/json").await.unwrap_err().0,
            "Refused: 127.0.0.1 is a private address (this machine or its network). To reach a local server, use bash with network: true (it asks first)."
        );
    }

    #[tokio::test]
    async fn refuses_localhost_without_asking_dns() {
        let dns = fake(Ok(vec!["93.184.215.14"]));
        assert!(
            check_with("http://localhost:3000/", &dns)
                .await
                .unwrap_err()
                .0
                .starts_with("Refused: localhost is 127.0.0.1, a private address")
        );
        assert!(check_with("http://app.localhost/", &dns).await.unwrap_err().0.contains("a private address"));
        assert!(dns.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refuses_a_name_that_resolves_to_a_private_address_even_among_public_ones() {
        let dns = fake(Ok(vec!["93.184.215.14", "192.168.1.5"]));
        assert!(
            check_with("https://sneaky.example/", &dns)
                .await
                .unwrap_err()
                .0
                .starts_with("Refused: sneaky.example is 192.168.1.5, a private address")
        );
        let v6 = fake(Ok(vec!["2606:4700::1", "fe80::1"]));
        assert!(check_with("https://sneaky.example/", &v6).await.unwrap_err().0.contains("a private address"));
    }

    #[tokio::test]
    async fn lets_a_public_site_through_and_returns_what_it_resolved_to() {
        let dns = fake(Ok(vec!["93.184.215.14"]));
        assert_eq!(check_with("https://example.com/page", &dns).await.unwrap(), ["93.184.215.14".parse::<IpAddr>().unwrap()]);
        assert_eq!(*dns.asked.lock().unwrap(), ["example.com"]);
    }

    #[tokio::test]
    async fn a_name_that_doesnt_resolve_and_a_lookup_that_failed_for_another_reason() {
        assert_eq!(
            check_with("https://nope.invalid/", &fake(Err(LookupError::NotFound))).await.unwrap_err().0,
            "Couldn't resolve nope.invalid: check the address."
        );
        assert_eq!(
            check("https://example.com/").await.unwrap_err().0,
            "Couldn't resolve example.com: no network, or DNS isn't answering?"
        );
        assert_eq!(
            check_with("https://empty.example/", &fake(Ok(vec![]))).await.unwrap_err().0,
            "Couldn't resolve empty.example: check the address."
        ); // fail closed
    }

    #[tokio::test]
    async fn looks_up_the_name_the_connection_will_use_trailing_dot_kept() {
        let dns = fake(Ok(vec!["93.184.215.14"]));
        check_with("https://Example.com./", &dns).await.unwrap();
        assert_eq!(*dns.asked.lock().unwrap(), ["example.com."]); // the URL lowercases the host; the dot stays
    }

    #[tokio::test]
    async fn a_lookup_that_never_answers_is_cut_off_and_an_aborted_run_stops_it() {
        assert_eq!(DNS_TIMEOUT_MS, 10_000);
        let slow = Arc::new(Fake { answer: Ok(vec![]), hang: true, asked: Default::default() });
        let options =
            CheckOptions { resolver: Some(slow.clone()), dns_timeout: Duration::from_millis(20), ..CheckOptions::default() };
        let url = Url::parse("https://slow.example/").unwrap();
        assert_eq!(
            check_address(&url, &options, &CancellationToken::new()).await.unwrap_err().0,
            "Couldn't resolve slow.example: timed out."
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        let options = CheckOptions { resolver: Some(slow), ..CheckOptions::default() };
        let started = std::time::Instant::now();
        assert_eq!(check_address(&url, &options, &cancel).await.unwrap_err().0, STOPPED);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn allow_private_skips_the_address_check_not_the_scheme_check() {
        let options = CheckOptions { allow_private: true, ..CheckOptions::default() };
        let token = CancellationToken::new();
        assert!(check_address(&Url::parse("http://127.0.0.1/").unwrap(), &options, &token).await.is_ok());
        assert!(
            check_address(&Url::parse("file:///x").unwrap(), &options, &token)
                .await
                .unwrap_err()
                .0
                .starts_with("Only http and https")
        );
    }
}
