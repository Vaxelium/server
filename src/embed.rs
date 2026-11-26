//! Link previews. The server fetches the first web link in a channel message once, and keeps what the page says:
//! its title, description, site name and picture. Members never fetch the page themselves, so their addresses are
//! not given to the site, and the server refuses to fetch anything on its own network.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine;
use common::{LinkEmbed, MAX_EMBED_DESCRIPTION_LEN, MAX_EMBED_IMAGE_BYTES, MAX_EMBED_TITLE_LEN};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tokio::sync::Semaphore;

/// Most pages fetched at the same time, so a burst of links cannot tie up the server.
const FETCHES_AT_ONCE: usize = 4;
/// Most of a page that is read. The title and the meta tags are in its head.
const MAX_PAGE_BYTES: usize = 512 * 1024;
/// Most redirects followed to reach the page.
const MAX_REDIRECTS: usize = 3;
const FETCH_TIMEOUT: Duration = Duration::from_secs(6);

/// Whether an address is not a public one: this machine, the local network, or another range that is not on the
/// open internet.
pub fn blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || a == 0
                // Shared address space for carrier-grade NAT, 100.64.0.0/10.
                || (a == 100 && (b & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return blocked(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique local addresses, fc00::/7, and link-local, fe80::/10.
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
        }
    }
}

/// Whether a link may be fetched: http or https, with a host that is not a private address written as a number.
pub fn allowed_url(url: &reqwest::Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    // Addresses written as numbers skip the resolver, so they are checked here.
    match host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
        Ok(ip) => !blocked(ip),
        Err(_) => true,
    }
}

/// Looks up a host and keeps only the public addresses it has. A host with none is not fetched.
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|addr| !blocked(addr.ip()))
                .collect();
            if found.is_empty() {
                return Err("that address is not on the public internet".into());
            }
            Ok(Box::new(found.into_iter()) as Addrs)
        })
    }
}

fn client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(PublicOnly))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if !allowed_url(attempt.url()) {
                attempt.error("the link leads somewhere that is not a public web page")
            } else {
                attempt.follow()
            }
        }))
        .timeout(FETCH_TIMEOUT)
        .user_agent("Vaxelium-link-preview")
        .build()
        .ok()
}

/// Limits how many pages are fetched at once.
fn slots() -> &'static Semaphore {
    static SLOTS: OnceLock<Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| Semaphore::new(FETCHES_AT_ONCE))
}

/// Fetches the preview for a link. None when the page cannot be had, is not a web page, or has nothing to show.
pub async fn fetch(link: &str) -> Option<LinkEmbed> {
    let url = reqwest::Url::parse(link).ok()?;
    if !allowed_url(&url) {
        return None;
    }
    let _slot = slots().acquire().await.ok()?;
    let client = client()?;
    let res = client.get(url).send().await.ok()?;
    if !res.status().is_success() || !content_type(&res).starts_with("text/html") {
        return None;
    }
    let page_url = res.url().clone();
    let body = read_up_to(res, MAX_PAGE_BYTES).await;
    let page = parse_page(&String::from_utf8_lossy(&body));
    let image = match page.image.as_deref().and_then(|src| page_url.join(src).ok()) {
        Some(src) if allowed_url(&src) => fetch_image(&client, src).await,
        _ => String::new(),
    };
    let site = if page.site.is_empty() {
        page_url.host_str().unwrap_or_default().to_string()
    } else {
        page.site
    };
    if page.title.is_empty() && page.description.is_empty() && image.is_empty() {
        return None;
    }
    Some(LinkEmbed {
        url: link.to_string(),
        site: clip(&site, MAX_EMBED_TITLE_LEN),
        title: clip(&page.title, MAX_EMBED_TITLE_LEN),
        description: clip(&page.description, MAX_EMBED_DESCRIPTION_LEN),
        image,
    })
}

/// The picture a page names, as base64, when it is a picture of a type the app shows and not too large.
async fn fetch_image(client: &reqwest::Client, src: reqwest::Url) -> String {
    let Ok(res) = client.get(src).send().await else {
        return String::new();
    };
    if !res.status().is_success() {
        return String::new();
    }
    let bytes = read_up_to(res, MAX_EMBED_IMAGE_BYTES + 1).await;
    if bytes.is_empty() || bytes.len() > MAX_EMBED_IMAGE_BYTES || !common::can_preview(common::sniff_mime(&bytes)) {
        return String::new();
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn content_type(res: &reqwest::Response) -> String {
    res.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Reads the body until it ends or `limit` bytes have been read, whichever comes first.
async fn read_up_to(mut res: reqwest::Response, limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok(Some(chunk)) = res.chunk().await {
        let room = limit.saturating_sub(out.len());
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// What a page says about itself.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub title: String,
    pub description: String,
    pub site: String,
    /// The picture's address as the page gives it, which may be relative.
    pub image: Option<String>,
}

/// Reads the title and the Open Graph and Twitter meta tags of a page. The Open Graph ones win when both are there.
pub fn parse_page(html: &str) -> Page {
    // Lowercasing ASCII keeps every byte in place, so positions found in the copy are right in the original.
    let lower = html.to_ascii_lowercase();
    let mut meta: Vec<Vec<(String, String)>> = Vec::new();
    let mut at = 0;
    while let Some(found) = lower[at..].find("<meta") {
        let start = at + found + "<meta".len();
        let Some(end) = lower[start..].find('>') else { break };
        meta.push(attributes(&html[start..start + end]));
        at = start + end;
    }
    let value = |keys: &[&str]| -> String {
        keys.iter()
            .find_map(|key| {
                meta.iter().find_map(|tag| {
                    let name = tag.iter().find(|(n, _)| n == "property" || n == "name")?.1.to_ascii_lowercase();
                    let content = tag.iter().find(|(n, _)| n == "content")?.1.clone();
                    (name == *key && !content.trim().is_empty()).then_some(content)
                })
            })
            .map(|content| tidy(&decode_entities(&content)))
            .unwrap_or_default()
    };
    let title_tag = lower
        .find("<title")
        .and_then(|open| lower[open..].find('>').map(|close| open + close + 1))
        .and_then(|from| lower[from..].find("</title").map(|to| (from, from + to)))
        .map(|(from, to)| tidy(&decode_entities(&html[from..to])))
        .unwrap_or_default();
    let image = value(&["og:image", "og:image:url", "twitter:image"]);
    Page {
        title: first_text(&[value(&["og:title"]), title_tag, value(&["twitter:title"])]),
        description: first_text(&[value(&["og:description", "description", "twitter:description"])]),
        site: value(&["og:site_name"]),
        image: (!image.is_empty()).then_some(image),
    }
}

fn first_text(options: &[String]) -> String {
    options.iter().find(|s| !s.is_empty()).cloned().unwrap_or_default()
}

/// The attributes of a tag's inside, as (lowercase name, value). Quoted and unquoted values are both read.
fn attributes(tag: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = tag.char_indices().peekable();
    while let Some(&(_, c)) = chars.peek() {
        if c.is_whitespace() || c == '/' {
            chars.next();
            continue;
        }
        let mut name = String::new();
        while let Some(&(_, c)) = chars.peek() {
            if c.is_whitespace() || c == '=' || c == '/' {
                break;
            }
            name.push(c.to_ascii_lowercase());
            chars.next();
        }
        while chars.peek().is_some_and(|&(_, c)| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        if chars.peek().is_some_and(|&(_, c)| c == '=') {
            chars.next();
            while chars.peek().is_some_and(|&(_, c)| c.is_whitespace()) {
                chars.next();
            }
            match chars.peek().copied() {
                Some((_, quote)) if quote == '"' || quote == '\'' => {
                    chars.next();
                    for (_, c) in chars.by_ref() {
                        if c == quote {
                            break;
                        }
                        value.push(c);
                    }
                }
                _ => {
                    while let Some(&(_, c)) = chars.peek() {
                        if c.is_whitespace() {
                            break;
                        }
                        value.push(c);
                        chars.next();
                    }
                }
            }
        }
        if !name.is_empty() {
            out.push((name, value));
        }
    }
    out
}

/// Turns the named and numbered character references a page may use back into their characters.
pub fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';').filter(|&i| i <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..semi];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => name.strip_prefix('#').and_then(|num| {
                let code = match num.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => num.parse::<u32>().ok(),
                }?;
                char::from_u32(code)
            }),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Collapses runs of whitespace (including line breaks) into single spaces.
fn tidy(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_local_addresses_are_blocked() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.0.9", "172.16.5.5", "169.254.1.1", "100.64.0.1", "0.0.0.0"] {
            assert!(blocked(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in ["::1", "fc00::5", "fe80::1", "::ffff:10.0.0.1", "::"] {
            assert!(blocked(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in ["8.8.8.8", "93.184.216.34", "2606:4700::1111"] {
            assert!(!blocked(ip.parse().unwrap()), "{ip} should be allowed");
        }
    }

    #[test]
    fn only_web_addresses_are_fetched() {
        let ok = |s: &str| allowed_url(&reqwest::Url::parse(s).unwrap());
        assert!(ok("https://example.com/page"));
        assert!(ok("http://93.184.216.34/"));
        assert!(!ok("http://127.0.0.1:8080/"));
        assert!(!ok("http://[::1]/"));
        assert!(!ok("http://192.168.1.1/admin"));
        assert!(!ok("file:///etc/passwd"));
    }

    #[test]
    fn the_page_title_and_meta_tags_are_read() {
        let html = r#"<html><head>
            <TITLE>Plain title</TITLE>
            <meta property="og:title" content="Fancy &amp; bright">
            <meta name='description' content="A short
              description">
            <meta property=og:site_name content=Example>
            <meta property="og:image" content="/pics/cover.png">
            </head></html>"#;
        let page = parse_page(html);
        assert_eq!(page.title, "Fancy & bright");
        assert_eq!(page.description, "A short description");
        assert_eq!(page.site, "Example");
        assert_eq!(page.image.as_deref(), Some("/pics/cover.png"));
    }

    #[test]
    fn the_title_tag_is_used_when_there_is_no_open_graph_title() {
        let page = parse_page("<title> Just &quot;this&quot; </title>");
        assert_eq!(page.title, "Just \"this\"");
        assert_eq!(page.description, "");
        assert_eq!(page.image, None);
    }

    #[test]
    fn entities_decode_and_stray_ampersands_stay() {
        assert_eq!(decode_entities("a &amp; b &#39;c&#x27; &#xZZ; &unknown; & d"), "a & b 'c' &#xZZ; &unknown; & d");
    }
}
