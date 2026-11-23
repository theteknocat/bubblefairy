//! Preview cards for links Messages made none for: the page's title, site name and picture, read from its
//! Open Graph tags. This module only reads HTML and judges URLs; fetching is in `ui.rs`.

/// What a page says about itself.
#[derive(Debug, PartialEq)]
pub struct Page {
    pub title: String,
    pub site: String,
    /// absolute address of the picture
    pub image: Option<String>,
    /// absolute address of the page's own icon (a PNG, JPEG, GIF or WebP we can draw)
    pub icon: Option<String>,
}

/// Only addresses on the public internet are fetched: a link to a router or a local service stays a plain card.
// ponytail: judged by the address as written; a name that resolves to a private address gets through
pub fn public_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or("");
    let name = if host.starts_with('[') {
        host.trim_matches(['[', ']'])
            .split("]:")
            .next()
            .unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    let name = name.to_ascii_lowercase();
    if name.is_empty()
        || name == "localhost"
        || name.ends_with(".local")
        || name.ends_with(".internal")
        || name.ends_with(".lan")
    {
        return false;
    }
    // an IPv4 address inside an IPv6 one (`::ffff:192.168.1.1`) is judged as the IPv4 address
    let private_v4 = |a: std::net::Ipv4Addr| {
        a.is_private()
            || a.is_loopback()
            || a.is_link_local()
            || a.is_unspecified()
            || a.is_broadcast()
            || a.octets()[0] == 0
            || (a.octets()[0] == 100 && a.octets()[1] & 0xc0 == 64) // carrier-grade NAT, also Tailscale
    };
    match name.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(a)) => !private_v4(a),
        Ok(std::net::IpAddr::V6(a)) => match a.to_ipv4_mapped() {
            Some(v4) => !private_v4(v4),
            None => {
                !(a.is_loopback()
                    || a.is_unspecified()
                    || (a.segments()[0] & 0xfe00) == 0xfc00
                    || (a.segments()[0] & 0xffc0) == 0xfe80)
            }
        },
        // Not an address in the usual form: only a real name passes. Its last part (the top-level domain) is letters
        // only, or punycode. That rejects a bare name ("printer") and the short or hex forms the system's resolver
        // still turns into an address ("127.1", "0x7f.0.0.1").
        Err(_) => {
            let tld = name.trim_end_matches('.').rsplit('.').next().unwrap_or("");
            name.contains('.')
                && (tld.starts_with("xn--")
                    || (!tld.is_empty() && tld.chars().all(|c| c.is_ascii_lowercase())))
        }
    }
}

/// Width and height of a PNG, JPEG, GIF or WebP from its header (the GNOME runtime has no gdk-pixbuf loaders to ask).
pub fn image_size(b: &[u8]) -> Option<(i32, i32)> {
    let be16 = |i: usize| {
        b.get(i..i + 2)
            .map(|x| i32::from(u16::from_be_bytes([x[0], x[1]])))
    };
    let le16 = |i: usize| {
        b.get(i..i + 2)
            .map(|x| i32::from(u16::from_le_bytes([x[0], x[1]])))
    };
    let le24 = |i: usize| {
        b.get(i..i + 3)
            .map(|x| i32::from(x[0]) | i32::from(x[1]) << 8 | i32::from(x[2]) << 16)
    };
    if b.starts_with(b"\x89PNG") {
        let w = b.get(16..20)?;
        let h = b.get(20..24)?;
        return Some((
            i32::from_be_bytes(w.try_into().ok()?),
            i32::from_be_bytes(h.try_into().ok()?),
        ));
    }
    if b.starts_with(b"GIF8") {
        return Some((le16(6)?, le16(8)?));
    }
    if b.starts_with(b"RIFF") && b.get(8..12)? == b"WEBP" {
        return match b.get(12..16)? {
            b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
            b"VP8L" => {
                let v = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
                Some(((v & 0x3fff) as i32 + 1, ((v >> 14) & 0x3fff) as i32 + 1))
            }
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            _ => None,
        };
    }
    if b.starts_with(&[0xff, 0xd8]) {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xff {
                i += 1;
                continue;
            }
            let m = b[i + 1];
            if m == 0xff || m == 0x00 || m == 0x01 || (0xd0..=0xd8).contains(&m) {
                i += if m == 0xff { 1 } else { 2 };
                continue;
            }
            if (0xc0..=0xcf).contains(&m) && !matches!(m, 0xc4 | 0xc8 | 0xcc) {
                return Some((be16(i + 7)?, be16(i + 5)?));
            }
            i += 2 + be16(i + 2)? as usize;
        }
    }
    None
}

/// The host of a URL without "www.".
pub fn host(url: &str) -> String {
    url.split('/')
        .nth(2)
        .unwrap_or(url)
        .trim_start_matches("www.")
        .to_string()
}

/// Read the title, site name and picture out of a page's HTML (only its head matters).
pub fn parse(html: &str, page_url: &str) -> Page {
    // YouTube's tags come after ~800 KB of script; the cap only has to stop a runaway page
    let head = &html[..html.floor_char_boundary(2_500_000)];
    let lower = head.to_ascii_lowercase();
    let head = lower.find("</head>").map_or(head, |i| &head[..i]);
    let lower = &lower[..head.len()];
    let (mut og_title, mut tw_title, mut site, mut image) = (None, None, None, None);
    let mut at = 0;
    while let Some(i) = lower[at..].find("<meta") {
        let start = at + i;
        let end = lower[start..].find('>').map_or(lower.len(), |e| start + e);
        let tag = &head[start..end];
        at = end;
        let key = attr(tag, "property")
            .or_else(|| attr(tag, "name"))
            .map(|k| k.to_ascii_lowercase());
        let content = attr(tag, "content")
            .map(|c| decode(&c))
            .filter(|c| !c.trim().is_empty());
        match (key.as_deref(), content) {
            (Some("og:title"), Some(c)) => og_title = og_title.or(Some(c)),
            (Some("twitter:title"), Some(c)) => tw_title = tw_title.or(Some(c)),
            (Some("og:site_name"), Some(c)) => site = site.or(Some(c)),
            (
                Some("og:image" | "og:image:url" | "og:image:secure_url" | "twitter:image"),
                Some(c),
            ) => image = image.or(Some(c)),
            _ => {}
        }
    }
    let page_title = lower.find("<title").and_then(|i| {
        let from = head[i..].find('>')? + i + 1;
        let to = lower[from..].find("</title>")? + from;
        Some(decode(head[from..to].trim()))
    });
    Page {
        title: og_title
            .or(tw_title)
            .or(page_title)
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        site: site.unwrap_or_else(|| host(page_url)),
        image: image.map(|i| resolve(page_url, i.trim())),
        icon: pick_icon(head, lower, page_url),
    }
}

/// The best `<link rel="...icon">` in the head that we can draw (no SVG or ICO: the runtime can't size or decode
/// them): an Apple touch icon first (usually 180 px), else the largest declared size, else the first one listed.
fn pick_icon(head: &str, lower: &str, page_url: &str) -> Option<String> {
    let mut best: Option<(i32, String)> = None;
    let mut at = 0;
    while let Some(i) = lower[at..].find("<link") {
        let start = at + i;
        let end = lower[start..].find('>').map_or(lower.len(), |e| start + e);
        let tag = &head[start..end];
        at = end;
        let rel = attr(tag, "rel").map(|r| r.to_ascii_lowercase());
        let is_icon = rel.as_deref().is_some_and(|r| {
            r.split_whitespace()
                .any(|t| t == "icon" || t.starts_with("apple-touch-icon"))
        });
        let href = attr(tag, "href")
            .map(|h| decode(&h))
            .filter(|h| !h.trim().is_empty());
        let (true, Some(href)) = (is_icon, href) else {
            continue;
        };
        let file = href
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if file.ends_with(".svg") || file.ends_with(".ico") {
            continue;
        }
        let declared = attr(tag, "sizes")
            .and_then(|s| s.split(['x', 'X']).next()?.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let touch = if rel.is_some_and(|r| r.contains("apple-touch-icon")) {
            1000
        } else {
            0
        };
        if best.as_ref().is_none_or(|(b, _)| declared + touch > *b) {
            best = Some((declared + touch, href));
        }
    }
    best.map(|(_, h)| resolve(page_url, h.trim()))
}

/// The value of an attribute (`name="..."`, `name='...'` or `name=bare`) in one tag.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        let before_ok = at == 0
            || lower.as_bytes()[at - 1].is_ascii_whitespace()
            || lower.as_bytes()[at - 1] == b'"'
            || lower.as_bytes()[at - 1] == b'\'';
        let after = tag[at + name.len()..].trim_start();
        if before_ok && let Some(v) = after.strip_prefix('=') {
            let v = v.trim_start();
            return Some(match v.chars().next()? {
                q @ ('"' | '\'') => v[1..].split(q).next()?.to_string(),
                _ => v
                    .split(|c: char| c.is_whitespace() || c == '>')
                    .next()?
                    .to_string(),
            });
        }
        from = at + name.len();
    }
    None
}

/// Make `rel` (as written in the page) an absolute address.
pub fn resolve(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let scheme = if base.starts_with("http://") {
        "http:"
    } else {
        "https:"
    };
    if let Some(r) = rel.strip_prefix("//") {
        return format!("{scheme}//{r}");
    }
    let origin: String = base.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
    if rel.starts_with('/') {
        return format!("{origin}{rel}");
    }
    let dir = base.split(['?', '#']).next().unwrap_or(base);
    let dir = dir
        .rfind('/')
        .filter(|i| *i >= origin.len())
        .map_or(origin.as_str(), |i| &dir[..i]);
    format!("{dir}/{rel}")
}

/// The handful of entities that show up in titles and addresses.
fn decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_open_graph_and_falls_back() {
        let html = r#"<html><head><title>Plain &amp; simple</title>
            <meta property="og:title" content="Theme Park Ticket Deals &#8211; Universal">
            <META NAME='og:site_name' CONTENT='Universal Studios Hollywood'>
            <meta property=og:image content=/img/a.jpg></head><body><meta property="og:title" content="late"></body></html>"#;
        let p = parse(html, "https://www.example.com/deals/page?x=1");
        assert_eq!(p.title, "Theme Park Ticket Deals – Universal");
        assert_eq!(p.site, "Universal Studios Hollywood");
        assert_eq!(
            p.image.as_deref(),
            Some("https://www.example.com/img/a.jpg")
        );
        let q = parse(
            "<head><title> Just a\n title </title></head>",
            "http://www.site.org/x",
        );
        assert_eq!(
            (q.title.as_str(), q.site.as_str(), q.image),
            ("Just a title", "site.org", None)
        );
    }

    #[test]
    fn picks_a_drawable_icon() {
        let page = "https://www.example.com/a/b";
        let icon = |head: &str| parse(head, page).icon;
        // an Apple touch icon beats the plain ones; SVG and ICO are skipped
        let html = r#"<head><link rel="icon" href="/favicon.ico">
            <link rel="shortcut icon" type="image/svg+xml" href="/i.svg?v=2">
            <link rel="icon" sizes="32x32" href="/f32.png">
            <link rel='apple-touch-icon' sizes='180x180' href='/touch.png'></head>"#;
        assert_eq!(
            icon(html).as_deref(),
            Some("https://www.example.com/touch.png")
        );
        // else the largest declared size
        let html = r#"<head><link rel="icon" sizes="16x16" href="s.png"><link rel="icon" sizes="96x96" href="big.png"></head>"#;
        assert_eq!(
            icon(html).as_deref(),
            Some("https://www.example.com/a/big.png")
        );
        // nothing drawable, or no icon at all
        assert_eq!(
            icon(r#"<head><link rel="icon" href="/favicon.ico"></head>"#),
            None
        );
        assert_eq!(
            icon(r#"<head><link rel="stylesheet" href="/a.css"></head>"#),
            None
        );
    }

    #[test]
    fn reads_image_sizes() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&[0, 0, 4, 176, 0, 0, 2, 88]); // 1200 x 600
        assert_eq!(image_size(&png), Some((1200, 600)));
        // SOI, an APP0 segment (length 4), then SOF0 with height 630 and width 1200
        let jpg = [
            0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xc0, 0, 11, 8, 2, 118, 4, 176, 3, 0, 0,
        ];
        assert_eq!(image_size(&jpg), Some((1200, 630)));
        assert_eq!(image_size(b"not an image at all"), None);
    }

    #[test]
    fn resolves_addresses() {
        assert_eq!(
            resolve("https://a.com/x/y.html", "z.png"),
            "https://a.com/x/z.png"
        );
        assert_eq!(resolve("https://a.com", "z.png"), "https://a.com/z.png");
        assert_eq!(
            resolve("https://a.com/x/", "//cdn.b.com/i.jpg"),
            "https://cdn.b.com/i.jpg"
        );
    }

    #[test]
    fn only_public_addresses() {
        for ok in [
            "https://example.com/a",
            "http://93.184.216.34/x",
            "https://user@example.org:8443/p",
            "https://example.com./a",
            "http://[::ffff:93.184.216.34]/",
        ] {
            assert!(public_url(ok), "{ok}");
        }
        for no in [
            "http://localhost:3000",
            "http://192.168.1.1/",
            "http://10.0.0.5",
            "http://127.0.0.1",
            "http://[::1]/",
            "http://printer/",
            "http://nas.local/",
            "ftp://example.com",
            "http://169.254.1.1",
            "http://127.1/",
            "http://192.168.1/",
            "http://0x7f.0x0.0x0.0x1/",
            "http://[::ffff:192.168.1.1]/",
            "http://[::ffff:127.0.0.1]:8080/",
            "http://100.64.0.1/",
            "http://0.1.2.3/",
        ] {
            assert!(!public_url(no), "{no}");
        }
    }
}
