//! URL normalization and SSRF guards for single-document skill installs.

use crate::model::{MAX_NAME_LEN, SkillFrontmatter};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Maximum accepted raw install URL length.
pub const MAX_INSTALL_URL_LEN: usize = 2048;

/// Normalize a direct Markdown URL, including GitHub blob links.
///
/// # Errors
///
/// Returns an error for malformed URLs, repository/tree URLs, or non-Markdown
/// paths.
pub fn normalize_install_url(raw: &str) -> Result<String, String> {
    let parsed = url::Url::parse(raw)
        .map_err(|error| format!("unsupported url form: parse {raw:?}: {error}"))?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let normalized = if host == "github.com" {
        let segments: Vec<_> = parsed
            .path_segments()
            .map(Iterator::collect)
            .unwrap_or_default();
        if segments.len() >= 5 && segments[2] == "blob" {
            format!(
                "https://raw.githubusercontent.com/{}/{}/{}/{}",
                segments[0],
                segments[1],
                segments[3],
                segments[4..].join("/")
            )
        } else if segments.len() >= 3 && matches!(segments[2], "tree" | "raw") {
            return Err(format!(
                "unsupported url form: only direct SKILL.md links are supported, got {raw:?} (tree/dir URLs are not yet supported)"
            ));
        } else if segments.len() <= 2 {
            return Err(format!(
                "unsupported url form: only direct SKILL.md links are supported, got {raw:?} (whole-repo URLs are not yet supported)"
            ));
        } else {
            raw.to_owned()
        }
    } else {
        raw.to_owned()
    };
    let normalized_url = url::Url::parse(&normalized).map_err(|error| {
        format!("unsupported url form: parse normalized {normalized:?}: {error}")
    })?;
    if !normalized_url.path().to_ascii_lowercase().ends_with(".md") {
        return Err(format!(
            "unsupported url form: path must end in .md, got {normalized:?}"
        ));
    }
    Ok(normalized)
}

/// Derive a safe directory slug from `metadata.id` or the skill name.
///
/// # Errors
///
/// Returns an error when no non-empty bounded slug can be derived.
pub fn derive_install_slug(frontmatter: &SkillFrontmatter) -> Result<String, String> {
    let candidate = frontmatter
        .metadata
        .get("id")
        .and_then(serde_yaml::Value::as_str)
        .unwrap_or(&frontmatter.name);
    let mut slug = String::with_capacity(candidate.len());
    let mut last_dash = false;
    for character in candidate.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        return Err(
            "invalid SKILL.md: cannot derive slug from empty name/id — set a value in frontmatter"
                .to_owned(),
        );
    }
    if slug.len() > MAX_NAME_LEN {
        return Err(format!(
            "invalid SKILL.md: derived slug {slug:?} exceeds {MAX_NAME_LEN} chars"
        ));
    }
    Ok(slug)
}

/// Validate a remote install URL under an explicit local-HTTP policy.
///
/// # Errors
///
/// Returns an error for empty/oversized URLs, unsafe schemes, missing hosts,
/// and literal private or local hosts.
pub fn validate_install_url(raw: &str, allow_local_http: bool) -> Result<(), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("url must not be empty".to_owned());
    }
    if trimmed.len() > MAX_INSTALL_URL_LEN {
        return Err(format!(
            "url exceeds max {MAX_INSTALL_URL_LEN} chars (got {})",
            trimmed.len()
        ));
    }
    let parsed =
        url::Url::parse(trimmed).map_err(|error| format!("invalid url {trimmed:?}: {error}"))?;
    if parsed.scheme() != "https" {
        if allow_local_http && is_loopback_http_url(trimmed) {
            return Ok(());
        }
        return Err(format!(
            "url scheme {:?} not allowed; https only",
            parsed.scheme()
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("url {trimmed:?} has no host"))?;
    if is_private_or_local_host(host) {
        return Err(format!(
            "host {host:?} not allowed (loopback/private/link-local/multicast)"
        ));
    }
    Ok(())
}

/// Resolve a URL host and reject any non-public address.
///
/// # Errors
///
/// Returns an error for malformed URLs, DNS failures/empty answers, or a
/// private, local, link-local, multicast, reserved, or unspecified address.
pub async fn validate_resolved_host(raw_url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(raw_url)
        .map_err(|error| format!("invalid url {raw_url:?} during DNS guard: {error}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("url {raw_url:?} has no host (DNS guard)"))?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let target = if matches!(parsed.host(), Some(url::Host::Ipv6(_))) {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let addresses: Vec<_> = tokio::net::lookup_host(target)
        .await
        .map_err(|error| format!("dns lookup failed for {host:?}: {error}"))?
        .collect();
    if addresses.is_empty() {
        return Err(format!("host {host:?} resolved to no IP addresses"));
    }
    for address in addresses {
        let blocked = match address.ip() {
            IpAddr::V4(ip) => is_non_global_v4(ip),
            IpAddr::V6(ip) => is_non_global_v6(ip),
        };
        if blocked {
            return Err(format!(
                "host {host:?} resolved to non-public IP {} (loopback/private/link-local)",
                address.ip()
            ));
        }
    }
    Ok(())
}

/// Whether the URL is plain HTTP to an exact loopback host.
#[must_use]
pub fn is_loopback_http_url(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|parsed| {
        parsed.scheme() == "http"
            && matches!(
                parsed.host_str(),
                Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
            )
    })
}

/// Whether a hostname or literal address is local/non-public.
#[must_use]
pub fn is_private_or_local_host(host: &str) -> bool {
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let bare = unbracketed.strip_suffix('.').unwrap_or(unbracketed);
    let lower = bare.to_ascii_lowercase();
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.rsplit('.').next() == Some("local")
    {
        return true;
    }
    bare.parse::<IpAddr>().is_ok_and(|ip| match ip {
        IpAddr::V4(ip) => is_non_global_v4(ip),
        IpAddr::V6(ip) => is_non_global_v6(ip),
    })
}

fn is_non_global_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || (a == 100 && (64..=127).contains(&b))
        || a >= 240
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || (a == 198 && (18..=19).contains(&b))
        || a == 0
}

fn is_non_global_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[0] == 0x0100 && segments[1..3] == [0, 0] && segments[3] <= 1)
        || (segments[0] == 0x2001 && segments[1] == 2 && segments[2] == 0)
        || (segments[0] & 0xfff0) == 0x3ff0
        || segments[0] == 0x5f00
        || ip.to_ipv4_mapped().is_some_and(is_non_global_v4)
}
