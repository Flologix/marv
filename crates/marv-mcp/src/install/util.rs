//! Small helpers the resolvers share. Not a port of one TS file: these stand in
//! for what JavaScript gave Marv for free (`encodeURIComponent`, `Date.parse`,
//! `JSON.parse` keeping key order).

use crate::McpError;
use crate::install::{ResolveOptions, Source, server_name};

/// Marv's own sentences (`There's no npm package named …`) as the error the user reads.
pub(crate) fn config(text: impl Into<String>) -> McpError {
    McpError::Config(text.into())
}

/// The server's name: `--name`, or the one the source suggests.
pub(crate) fn name_for(opts: &ResolveOptions, source: &Source) -> crate::Result<String> {
    match &opts.name {
        Some(n) => Ok(n.clone()),
        None => server_name(source).map_err(McpError::Config),
    }
}

/// JavaScript's `encodeURIComponent`: everything but `A-Za-z0-9-_.!~*'()` is percent-encoded.
pub(crate) fn encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

const DAY_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;

/// The date part of an ISO timestamp (`2026-01-14`), if it starts with one.
pub(crate) fn day(iso: Option<&str>) -> Option<String> {
    let iso = iso?;
    let b = iso.as_bytes();
    let digits = |r: std::ops::Range<usize>| r.into_iter().all(|i| b.get(i).is_some_and(u8::is_ascii_digit));
    (digits(0..4) && b.get(4) == Some(&b'-') && digits(5..7) && b.get(7) == Some(&b'-') && digits(8..10))
        .then(|| iso[..10].to_string())
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn number(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `Date.parse` for the registries' UTC timestamps (`2026-01-14T09:59:00.000Z`, or a bare date): ms since the
/// epoch. Anything else is `None` (Marv's `NaN`: no age warning).
pub(crate) fn iso_ms(iso: &str) -> Option<f64> {
    day(Some(iso))?;
    let (y, m, d) = (number(&iso[0..4])?, number(&iso[5..7])?, number(&iso[8..10])?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut ms = days_from_civil(y, m, d) as f64 * DAY_MS;
    let rest = &iso[10..];
    if !rest.is_empty() {
        let time = rest.strip_prefix('T')?;
        let time = time.strip_suffix('Z').unwrap_or(time);
        let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
        let parts: Vec<&str> = hms.split(':').collect();
        let [h, min, s] = parts[..] else { return None };
        let (h, min, s) = (number(h)?, number(min)?, number(s)?);
        if h > 24 || min > 59 || s > 59 {
            return None;
        }
        ms += ((h * 60 + min) * 60 + s) as f64 * 1000.0;
        if !frac.is_empty() {
            number(frac)?;
            ms += format!("0.{frac}").parse::<f64>().ok()? * 1000.0;
        }
    }
    Some(ms)
}

/// How old something first published at `first_ms` is, in days (fractional).
pub(crate) fn age_days(now_ms: u64, first_ms: f64) -> f64 {
    (now_ms as f64 - first_ms) / DAY_MS
}

/// "less than a day", "1 day", "3 days".
pub(crate) fn age_text(age: f64) -> String {
    if age < 1.0 {
        return "less than a day".into();
    }
    let n = age.floor() as u64;
    format!("{n} day{}", if n == 1 { "" } else { "s" })
}

pub(crate) use crate::ordered::Ordered;

/// The resolvers' network, served from Marv's fixtures (written from the APIs' documented and real shapes): no
/// test reaches the network.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use crate::install::proposal::{Fetched, ResolveDeps};

    pub const NPM_FILESYSTEM: &str = include_str!("fixtures/npm-server-filesystem.json");
    pub const NPM_WEATHER: &str = include_str!("fixtures/npm-weather-mcp.json");
    pub const PYPI_FETCH: &str = include_str!("fixtures/pypi-mcp-server-fetch.json");
    pub const REGISTRY_SEARCH: &str = include_str!("fixtures/registry-search.json");
    pub const REGISTRY_SERVERS: &str = include_str!("fixtures/registry-servers.json");

    /// 2026-10-12T12:00:00Z.
    pub const NOW: u64 = 1_791_806_400_000;

    #[derive(Default)]
    pub struct Fake {
        /// Every URL asked for, path and query (after the fake origin).
        pub requests: Mutex<Vec<String>>,
        pub search_body: Mutex<Option<String>>,
        pub readme: Option<Option<String>>,
        /// `Some(answer)`: a login prober that records the URLs it's asked about.
        pub probe: Option<bool>,
        pub probed: Mutex<Vec<String>>,
    }

    fn decode(path: &str) -> String {
        let b = path.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%'
                && let Some(n) = path.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(n);
                i += 3;
                continue;
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[async_trait]
    impl ResolveDeps for Fake {
        async fn get(&self, url: &str, _accept: &str, _max: usize, _c: &CancellationToken) -> crate::Result<Fetched> {
            let rest = url.strip_prefix("http://fake").expect("only the fake origin is asked");
            self.requests.lock().unwrap().push(rest.to_string());
            let (path, _query) = rest.split_once('?').unwrap_or((rest, ""));
            let path = decode(path);
            let ok = |body: &str| Ok(Fetched { status: 200, body: body.to_string() });
            match path.as_str() {
                "/npm/@modelcontextprotocol/server-filesystem" => return ok(NPM_FILESYSTEM),
                "/npm/weather-mcp" => return ok(NPM_WEATHER),
                "/pypi/pypi/mcp-server-fetch/json" => return ok(PYPI_FETCH),
                "/registry/v0/servers" => {
                    let body = self.search_body.lock().unwrap().clone();
                    return ok(body.as_deref().unwrap_or(REGISTRY_SEARCH));
                }
                _ => {}
            }
            if let Some(name) = path.strip_prefix("/registry/v0/servers/").and_then(|p| p.strip_suffix("/versions/latest")) {
                let servers: HashMap<String, serde_json::Value> = serde_json::from_str(REGISTRY_SERVERS).unwrap();
                if let Some(entry) = servers.get(name) {
                    return ok(&entry.to_string());
                }
            }
            Ok(Fetched { status: 404, body: "{\"error\":\"Not found\"}".into() })
        }
        async fn readme(&self, _o: &str, _r: &str, _c: &CancellationToken) -> crate::Result<Option<String>> {
            Ok(self.readme.clone().expect("a README was given"))
        }
        async fn probe_login(&self, url: &str, _c: &CancellationToken) -> Option<bool> {
            let answer = self.probe?;
            self.probed.lock().unwrap().push(url.to_string());
            Some(answer)
        }
        fn npm_registry(&self) -> &str {
            "http://fake/npm"
        }
        fn pypi(&self) -> &str {
            "http://fake/pypi"
        }
        fn registry(&self) -> &str {
            "http://fake/registry"
        }
        fn now_ms(&self) -> u64 {
            NOW
        }
        fn cancel(&self) -> CancellationToken {
            CancellationToken::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_as_javascript_parses_them() {
        assert_eq!(iso_ms("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(iso_ms("2026-10-12T12:00:00.000Z"), Some(fake::NOW as f64));
        assert_eq!(iso_ms("2025-04-07T18:00:00.000000Z"), Some(1_744_048_800_000.0));
        assert_eq!(iso_ms("2024-02-29"), Some(1_709_164_800_000.0));
        assert_eq!(iso_ms("yesterday"), None);
        assert_eq!(day(Some("2026-01-14T09:59:00.000Z")).as_deref(), Some("2026-01-14"));
        assert_eq!(age_text(0.5), "less than a day");
        assert_eq!(age_text(1.9), "1 day");
        assert_eq!(age_text(2.1), "2 days");
    }

    #[test]
    fn components_encode_as_encode_uri_component() {
        assert_eq!(encode_component("io.github.a/x y"), "io.github.a%2Fx%20y");
        assert_eq!(encode_component("é"), "%C3%A9");
    }

    #[test]
    fn objects_keep_their_order() {
        let v: Ordered = serde_json::from_str(r#"{"b": 1, "a": [true, null, "x"], "b": 2}"#).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["b", "a"]);
        assert_eq!(v.get("b"), Some(&Ordered::Number(2.into())));
    }
}
