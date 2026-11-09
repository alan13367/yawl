//! Provider-neutral cleanup of search results: duplicate and invalid URLs,
//! domain filtering, and a spread of sources instead of one site's pages.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};

use super::SearchResult;
use crate::tools::web::validate_http_url;

const MAX_PER_HOST: usize = 2;
const TRACKING_PARAMETERS: &[&str] = &["gclid", "fbclid", "msclkid", "ref_src", "mc_cid"];
/// Suffixes used only inside a network, which no public site can have.
const PRIVATE_SUFFIXES: &[&str] = &[
    ".localhost",
    ".local",
    ".localdomain",
    ".internal",
    ".intranet",
    ".lan",
    ".home",
    ".corp",
    ".home.arpa",
];

fn host_of(url: &str) -> Option<String> {
    let uri: ureq::http::Uri = url.parse().ok()?;
    let host = uri.host()?.to_ascii_lowercase();
    Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
}

/// What makes two URLs the same page: not the scheme, `www.`, a fragment, a
/// trailing slash, or tracking parameters.
fn page_key(url: &str) -> String {
    let Ok(uri) = url.parse::<ureq::http::Uri>() else {
        return url.to_string();
    };
    let host = host_of(url).unwrap_or_default();
    // A non-default port is a different server.
    let port = uri
        .port_u16()
        .filter(|port| !matches!(port, 80 | 443))
        .map_or_else(String::new, |port| format!(":{port}"));
    let path = uri.path().trim_end_matches('/');
    let query = uri
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|pair| {
            let name = pair.split('=').next().unwrap_or_default();
            !name.is_empty() && !name.starts_with("utm_") && !TRACKING_PARAMETERS.contains(&name)
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{host}{port}{path}?{query}")
}

fn private_v4(ip: Ipv4Addr) -> bool {
    let [first, second, third, _] = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || first == 0
        // Shared address space, which Tailscale and carriers use.
        || (first == 100 && (64..128).contains(&second))
        || (first == 192 && second == 0 && third == 0)
}

fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => private_v4(ip),
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.to_ipv4_mapped().is_some_and(private_v4)
                || first & 0xfe00 == 0xfc00
                || first & 0xffc0 == 0xfe80
                || first & 0xffc0 == 0xfec0
        }
    }
}

/// Whether a URL itself names this machine or a private network.
fn names_private_address(url: &str) -> bool {
    let Ok(uri) = url.parse::<ureq::http::Uri>() else {
        return false;
    };
    let Some(host) = uri.host() else {
        return false;
    };
    let bracketed = host.starts_with('[');
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    // A scope id such as `::1%1` is not part of the address, but connects.
    let address = host.split('%').next().unwrap_or(&host);
    if let Ok(ip) = address.parse::<IpAddr>() {
        return private_ip(ip);
    }
    // No public host is written in brackets without being an address, has no
    // dot, or ends in a number, which resolvers read as IPv4: `2130706433`.
    bracketed
        || !host.contains('.')
        || PRIVATE_SUFFIXES.iter().any(|suffix| host.ends_with(suffix))
        || host.rsplit('.').next().is_some_and(|label| {
            label.starts_with("0x") || label.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// Whether a public-looking name resolves into a private network.
fn resolves_to_private_address(url: &str) -> bool {
    let Ok(uri) = url.parse::<ureq::http::Uri>() else {
        return false;
    };
    let Some(host) = uri.host() else {
        return false;
    };
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("https") {
            443
        } else {
            80
        });
    (host, port)
        .to_socket_addrs()
        .is_ok_and(|mut addresses| addresses.any(|address| private_ip(address.ip())))
}

/// Whether `web_search` must not open this address by itself. It opens results
/// without the model choosing the address, so a planted result must not be able
/// to reach local services. A name could still change what it resolves to
/// between this check and the request.
pub(in crate::tools::web) fn blocks_automatic_read(url: &str) -> bool {
    names_private_address(url) || resolves_to_private_address(url)
}

pub(super) fn matches_site(url: &str, site: &str) -> bool {
    host_of(url).is_some_and(|host| {
        host == site
            || host
                .strip_suffix(site)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

/// Drops results without a title or a usable URL, and later copies of a page.
pub(super) fn clean(results: impl IntoIterator<Item = SearchResult>) -> Vec<SearchResult> {
    let mut seen = HashSet::new();
    results
        .into_iter()
        .filter(|result| {
            !result.title.trim().is_empty()
                && validate_http_url(&result.url).is_ok()
                && seen.insert(page_key(&result.url))
        })
        .collect()
}

/// Chooses up to `limit` results in rank order. Each site gets two places at
/// first, and its other pages only fill places no other site wanted.
pub(super) fn pick(
    results: &[SearchResult],
    limit: usize,
    site: Option<&str>,
) -> Vec<SearchResult> {
    let eligible: Vec<&SearchResult> = results
        .iter()
        .filter(|result| site.is_none_or(|site| matches_site(&result.url, site)))
        .collect();
    let mut chosen = vec![false; eligible.len()];
    let mut taken = 0;
    let mut per_host: HashMap<Option<String>, usize> = HashMap::new();
    for (index, result) in eligible.iter().enumerate() {
        if taken == limit {
            break;
        }
        let count = per_host.entry(host_of(&result.url)).or_default();
        if site.is_some() || *count < MAX_PER_HOST {
            *count += 1;
            chosen[index] = true;
            taken += 1;
        }
    }
    for slot in chosen.iter_mut() {
        if taken == limit {
            break;
        }
        if !*slot {
            *slot = true;
            taken += 1;
        }
    }
    eligible
        .into_iter()
        .zip(chosen)
        .filter(|(_, chosen)| *chosen)
        .map(|(result, _)| result.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(url: &str) -> SearchResult {
        SearchResult {
            title: "Title".into(),
            url: url.into(),
            snippet: String::new(),
            published: None,
        }
    }

    fn urls(results: &[SearchResult]) -> Vec<&str> {
        results.iter().map(|result| result.url.as_str()).collect()
    }

    #[test]
    fn the_same_page_under_another_address_is_one_result() {
        let cleaned = clean([
            result("https://www.example.com/a/?utm_source=x&id=1#top"),
            result("http://example.com/a?id=1&fbclid=abc"),
            result("https://example.com/a?id=2"),
            result("https://example.com/b"),
        ]);
        assert_eq!(
            urls(&cleaned),
            [
                "https://www.example.com/a/?utm_source=x&id=1#top",
                "https://example.com/a?id=2",
                "https://example.com/b"
            ]
        );
    }

    #[test]
    fn different_ports_and_ref_values_are_different_pages() {
        assert_eq!(
            clean([
                result("https://a.test:8443/x"),
                result("https://a.test/x"),
                result("https://a.test/f?ref=main"),
                result("https://a.test/f?ref=dev"),
            ])
            .len(),
            4
        );
        // The default ports are the same page as no port at all.
        assert_eq!(
            clean([result("https://a.test:443/x"), result("https://a.test/x")]).len(),
            1
        );
    }

    #[test]
    fn results_need_a_title_and_a_web_address() {
        let mut untitled = result("https://example.com/");
        untitled.title = "  ".into();
        let cleaned = clean([
            untitled,
            result("javascript:alert(1)"),
            result("/relative"),
            result("https://example.org/"),
        ]);
        assert_eq!(urls(&cleaned), ["https://example.org/"]);
    }

    #[test]
    fn local_and_private_addresses_are_recognized() {
        for url in [
            "http://localhost:8080/admin",
            "http://LOCALHOST/",
            "http://printer.local/",
            "http://db.internal/",
            "http://app.localhost/",
            "http://127.0.0.1/",
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[fc00::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://2130706433/",
            "http://0x7f.1/",
            "http://127.1/",
            "http://[::1%1]:8080/admin",
            "http://[fe80::1%25en0]/",
            "http://router/admin",
            "http://db:5432/",
            "http://intranet/",
            "http://nas.lan/",
            "http://nas.home.arpa/",
            "http://host.corp/",
            "http://localhost.localdomain/",
            "http://ip6-localhost/",
            "http://0.1.2.3/",
            "http://192.0.0.8/",
            "http://224.0.0.1/",
            "http://[fec0::1]/",
        ] {
            assert!(names_private_address(url), "{url}");
        }
        for url in [
            "https://example.com/",
            "https://8.8.8.8/",
            "http://172.32.0.1/",
            "http://100.128.0.1/",
            "https://[2606:4700::1111]/",
            "https://localhost.example.com/",
            "https://1password.com/",
        ] {
            assert!(!names_private_address(url), "{url}");
        }
    }

    #[test]
    fn site_matching_covers_subdomains_only() {
        assert!(matches_site("https://www.docs.rs/a", "docs.rs"));
        assert!(matches_site("https://api.docs.rs/a", "docs.rs"));
        assert!(!matches_site("https://notdocs.rs/a", "docs.rs"));
        assert!(!matches_site("https://docs.rs.evil.test/a", "docs.rs"));
    }

    #[test]
    fn one_site_cannot_fill_every_place_while_others_are_available() {
        let results = [
            result("https://a.test/1"),
            result("https://a.test/2"),
            result("https://a.test/3"),
            result("https://b.test/1"),
            result("https://c.test/1"),
        ];
        assert_eq!(
            urls(&pick(&results, 4, None)),
            [
                "https://a.test/1",
                "https://a.test/2",
                "https://b.test/1",
                "https://c.test/1"
            ]
        );
        // With nothing else to show, the site's other pages still appear.
        assert_eq!(pick(&results[..3], 3, None).len(), 3);
    }

    #[test]
    fn a_site_filter_is_enforced_and_lifts_the_per_site_limit() {
        let results = [
            result("https://a.test/1"),
            result("https://other.test/1"),
            result("https://a.test/2"),
            result("https://a.test/3"),
        ];
        assert_eq!(
            urls(&pick(&results, 5, Some("a.test"))),
            ["https://a.test/1", "https://a.test/2", "https://a.test/3"]
        );
    }
}
