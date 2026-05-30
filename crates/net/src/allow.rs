//! Connection allowlist for the host-side networking bridge.
//!
//! Shared by `wwwvm-proxy` (the WebSocket↔TCP gateway) and the in-process
//! guest↔internet bridge (`crates/net`'s DNS forwarder + TCP NAT), so the
//! security policy has exactly one implementation.
//!
//! Entries come from the `WWWVM_PROXY_ALLOWLIST` env var: comma-separated
//! `host:port`, `host:*` (any port on that host), bare `host` (also any
//! port), or `*` (anything). The default is **empty → deny everything**, so
//! a misconfigured deployment fails closed rather than open. `*` must never
//! be the shipped default — an open proxy is dangerous.

/// A parsed allowlist. Empty means deny-all.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    entries: Vec<AllowEntry>,
}

#[derive(Debug, Clone)]
enum AllowEntry {
    /// `*` — allow any host and port.
    Anything,
    /// A specific host, optionally pinned to one port (`None` = any port).
    Host { host: String, port: Option<u16> },
}

impl Allowlist {
    /// Parse the allowlist from `WWWVM_PROXY_ALLOWLIST` (empty/unset →
    /// deny-all).
    pub fn from_env() -> Self {
        Self::parse(&std::env::var("WWWVM_PROXY_ALLOWLIST").unwrap_or_default())
    }

    /// Parse a comma-separated allowlist string. Whitespace around entries
    /// is trimmed; empty entries are dropped (so a trailing comma or an
    /// empty string yields an empty, deny-all list).
    pub fn parse(raw: &str) -> Self {
        let entries = raw
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                let s = s.trim();
                if s == "*" {
                    return AllowEntry::Anything;
                }
                if let Some((h, p)) = s.rsplit_once(':') {
                    let port = if p == "*" { None } else { p.parse().ok() };
                    AllowEntry::Host {
                        host: h.to_string(),
                        port,
                    }
                } else {
                    AllowEntry::Host {
                        host: s.to_string(),
                        port: None,
                    }
                }
            })
            .collect();
        Self { entries }
    }

    /// True if the allowlist has no entries — i.e. it denies everything.
    /// Callers use this to warn about a fail-closed misconfiguration.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether a `host:port` connection is permitted. OR semantics across
    /// entries; host match is case-insensitive.
    pub fn permits(&self, host: &str, port: u16) -> bool {
        self.entries.iter().any(|e| match e {
            AllowEntry::Anything => true,
            // `map_or(true, …)` keeps this MSRV-clean (vs `is_none_or`).
            AllowEntry::Host { host: h, port: p } => {
                h.eq_ignore_ascii_case(host) && p.map_or(true, |pp| pp == port)
            }
        })
    }

    /// Whether *any* port on `host` is permitted — the port-agnostic check
    /// the DNS forwarder uses to decide whether to resolve a name at all
    /// (the per-port check happens later, at TCP connect time).
    pub fn permits_host(&self, host: &str) -> bool {
        self.entries.iter().any(|e| match e {
            AllowEntry::Anything => true,
            AllowEntry::Host { host: h, .. } => h.eq_ignore_ascii_case(host),
        })
    }

    /// The distinct named hosts in the allowlist — used to pre-resolve the
    /// DNS cache at startup. A `*` entry contributes no name (nothing to
    /// pre-resolve), so a wildcard allowlist yields no cache entries and the
    /// forwarder simply answers NXDOMAIN until on-demand resolution exists.
    pub fn hosts(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for e in &self.entries {
            if let AllowEntry::Host { host, .. } = e {
                let lc = host.to_ascii_lowercase();
                if !names.contains(&lc) {
                    names.push(lc);
                }
            }
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allowlist_denies_everything() {
        let a = Allowlist::parse("");
        assert!(!a.permits("example.com", 80));
        assert!(!a.permits_host("example.com"));
        assert!(a.is_empty());
    }

    #[test]
    fn star_allows_anything() {
        let a = Allowlist::parse("*");
        assert!(a.permits("evil.example.com", 9000));
        assert!(a.permits_host("evil.example.com"));
        assert!(!a.is_empty());
    }

    #[test]
    fn host_port_exact_match() {
        let a = Allowlist::parse("example.com:443");
        assert!(a.permits("example.com", 443));
        assert!(!a.permits("example.com", 80));
        assert!(!a.permits("other.com", 443));
    }

    #[test]
    fn host_wildcard_port() {
        let a = Allowlist::parse("example.com:*");
        assert!(a.permits("example.com", 80));
        assert!(a.permits("example.com", 443));
        assert!(!a.permits("other.com", 443));
    }

    #[test]
    fn host_match_is_case_insensitive() {
        let a = Allowlist::parse("Example.COM:443");
        assert!(a.permits("example.com", 443));
        assert!(a.permits_host("EXAMPLE.com"));
    }

    /// Multiple comma-separated entries compose with OR semantics: a host
    /// matching *any* entry is allowed. A regression collapsing the split
    /// (one entry of the whole joined string) would deny everything except
    /// the exact "a:80,b:443" hostname — silently breaking multi-host lists.
    #[test]
    fn multiple_entries_compose_or() {
        let a = Allowlist::parse("example.com:443,localhost:8080");
        assert!(a.permits("example.com", 443));
        assert!(a.permits("localhost", 8080));
        assert!(!a.permits("example.com", 8080), "wrong port");
        assert!(!a.permits("localhost", 443), "wrong port");
        assert!(!a.permits("other.com", 443), "host not in list");
    }

    /// Whitespace around entries is trimmed — users naturally write
    /// `"a:80, b:443"`; if the trim drops, `" b:443"` never matches `"b"`.
    #[test]
    fn whitespace_around_entries_is_trimmed() {
        let a = Allowlist::parse("  example.com:443  ,\tlocalhost:8080");
        assert!(a.permits("example.com", 443));
        assert!(a.permits("localhost", 8080));
    }

    /// A bare host with no `:port` means "any port on this host" — distinct
    /// from `host:*` but with the same outcome.
    #[test]
    fn host_without_colon_allows_any_port() {
        let a = Allowlist::parse("example.com");
        assert!(a.permits("example.com", 80));
        assert!(a.permits("example.com", 443));
        assert!(a.permits("example.com", 31337));
        assert!(!a.permits("other.com", 80));
    }

    /// permits_host is port-agnostic but still host-scoped: it must not
    /// authorize names that aren't in the list (no open resolver).
    #[test]
    fn permits_host_is_still_deny_by_default() {
        let a = Allowlist::parse("dl-cdn.alpinelinux.org:443");
        assert!(a.permits_host("dl-cdn.alpinelinux.org"));
        assert!(!a.permits_host("evil.example.com"));
    }
}
