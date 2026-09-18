//! URL normalization and SSRF guards for single-document skill installs.

use crate::model::{MAX_NAME_LEN, SkillFrontmatter};
use std::net::SocketAddr;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use thiserror::Error;

/// Maximum accepted raw install URL length.
pub const MAX_INSTALL_URL_LEN: usize = 2048;

/// Errors returned by install URL normalization and SSRF validation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum InstallError {
    /// The input URL is empty.
    #[error("url must not be empty")]
    EmptyUrl,
    /// The input URL exceeds the bounded parser input size.
    #[error("url exceeds max {max} chars (got {len})")]
    UrlTooLong {
        /// Number of input bytes.
        len: usize,
        /// Maximum accepted byte length.
        max: usize,
    },
    /// The input is not a valid URL.
    #[error("invalid url {input:?}: {message}")]
    InvalidUrl {
        /// The rejected input.
        input: String,
        /// Parser diagnostic.
        message: String,
    },
    /// The URL form is not a direct Markdown document link.
    #[error("unsupported url form: {0}")]
    UnsupportedUrl(String),
    /// The URL uses a scheme that is not allowed by policy.
    #[error("url scheme {0:?} not allowed; https only")]
    UnsupportedScheme(String),
    /// The URL does not include a host.
    #[error("url {0:?} has no host")]
    MissingHost(String),
    /// The URL host is local or non-public.
    #[error("host {host:?} not allowed (loopback/private/link-local/multicast)")]
    UnsafeHost {
        /// Hostname or literal address.
        host: String,
    },
    /// No safe slug could be derived from the frontmatter.
    #[error("invalid SKILL.md: cannot derive slug from empty name/id — set a value in frontmatter")]
    EmptySlug,
    /// The derived slug exceeds the bounded name length.
    #[error("invalid SKILL.md: derived slug exceeds {max} chars")]
    SlugTooLong {
        /// Maximum accepted slug length.
        max: usize,
    },
    /// DNS resolution failed.
    #[error("dns lookup failed for {host:?}: {message}")]
    DnsLookup {
        /// Hostname that failed to resolve.
        host: String,
        /// Resolver diagnostic.
        message: String,
    },
    /// DNS returned no addresses.
    #[error("host {host:?} resolved to no IP addresses")]
    NoAddresses {
        /// Hostname that had no answers.
        host: String,
    },
    /// DNS returned a non-public address.
    #[error("host {host:?} resolved to non-public IP {address}")]
    NonPublicAddress {
        /// Hostname that resolved unsafely.
        host: String,
        /// Rejected address.
        address: IpAddr,
    },
}

/// Normalize a direct Markdown URL, including GitHub blob links.
///
/// # Errors
///
/// Returns an error for malformed URLs, repository/tree URLs, or non-Markdown
/// paths.
pub fn normalize_install_url(raw: &str) -> Result<String, InstallError> {
    if raw.len() > MAX_INSTALL_URL_LEN {
        return Err(InstallError::UrlTooLong {
            len: raw.len(),
            max: MAX_INSTALL_URL_LEN,
        });
    }
    let parsed = url::Url::parse(raw).map_err(|error| InstallError::InvalidUrl {
        input: raw.to_owned(),
        message: error.to_string(),
    })?;
    if has_path_traversal(raw) {
        return Err(InstallError::UnsupportedUrl(
            "URL paths must not contain traversal segments".to_owned(),
        ));
    }
    let parsed_host = parsed
        .host_str()
        .ok_or_else(|| InstallError::MissingHost(raw.to_owned()))?;
    let host = parsed_host.to_ascii_lowercase();
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
            return Err(InstallError::UnsupportedUrl(
                "only direct SKILL.md links are supported (tree/dir URLs are not yet supported)"
                    .to_owned(),
            ));
        } else if segments.len() <= 2 {
            return Err(InstallError::UnsupportedUrl(
                "only direct SKILL.md links are supported (whole-repo URLs are not yet supported)"
                    .to_owned(),
            ));
        } else {
            raw.to_owned()
        }
    } else {
        raw.to_owned()
    };
    let normalized_url =
        url::Url::parse(&normalized).map_err(|error| InstallError::InvalidUrl {
            input: normalized.clone(),
            message: error.to_string(),
        })?;
    if !normalized_url.path().to_ascii_lowercase().ends_with(".md") {
        return Err(InstallError::UnsupportedUrl(
            "path must end in .md".to_owned(),
        ));
    }
    Ok(normalized)
}

/// Derive a safe directory slug from `metadata.id` or the skill name.
///
/// # Errors
///
/// Returns an error when no non-empty bounded slug can be derived.
pub fn derive_install_slug(frontmatter: &SkillFrontmatter) -> Result<String, InstallError> {
    let candidate = frontmatter
        .metadata
        .get("id")
        .and_then(serde_yaml::Value::as_str)
        .unwrap_or(&frontmatter.name);
    if candidate.len() > MAX_NAME_LEN.saturating_mul(4) {
        return Err(InstallError::SlugTooLong { max: MAX_NAME_LEN });
    }
    let mut slug = String::with_capacity(candidate.len().min(MAX_NAME_LEN));
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
        return Err(InstallError::EmptySlug);
    }
    if slug.len() > MAX_NAME_LEN {
        return Err(InstallError::SlugTooLong { max: MAX_NAME_LEN });
    }
    Ok(slug)
}

/// Validate a remote install URL under an explicit local-HTTP policy.
///
/// # Errors
///
/// Returns an error for empty/oversized URLs, unsafe schemes, missing hosts,
/// and literal private or local hosts.
pub fn validate_install_url(raw: &str, allow_local_http: bool) -> Result<(), InstallError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(InstallError::EmptyUrl);
    }
    if trimmed.len() > MAX_INSTALL_URL_LEN {
        return Err(InstallError::UrlTooLong {
            len: trimmed.len(),
            max: MAX_INSTALL_URL_LEN,
        });
    }
    let parsed = url::Url::parse(trimmed).map_err(|error| InstallError::InvalidUrl {
        input: trimmed.to_owned(),
        message: error.to_string(),
    })?;
    if parsed.scheme() != "https" {
        if allow_local_http && is_loopback_http_url(trimmed) {
            return Ok(());
        }
        return Err(InstallError::UnsupportedScheme(parsed.scheme().to_owned()));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| InstallError::MissingHost(trimmed.to_owned()))?;
    if host.is_empty() {
        return Err(InstallError::MissingHost(trimmed.to_owned()));
    }
    if is_private_or_local_host(host) {
        return Err(InstallError::UnsafeHost {
            host: host.to_owned(),
        });
    }
    Ok(())
}

/// Resolve a URL host and reject any non-public address.
///
/// # Errors
///
/// Returns an error for malformed URLs, DNS failures/empty answers, or a
/// private, local, link-local, multicast, reserved, or unspecified address.
pub async fn validate_resolved_host(raw_url: &str) -> Result<Vec<SocketAddr>, InstallError> {
    let parsed = url::Url::parse(raw_url).map_err(|error| InstallError::InvalidUrl {
        input: raw_url.to_owned(),
        message: error.to_string(),
    })?;
    let host = parsed
        .host_str()
        .ok_or_else(|| InstallError::MissingHost(raw_url.to_owned()))?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let target = if matches!(parsed.host(), Some(url::Host::Ipv6(_))) {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let addresses: Vec<_> = tokio::net::lookup_host(target)
        .await
        .map_err(|error| InstallError::DnsLookup {
            host: host.to_owned(),
            message: error.to_string(),
        })?
        .collect();
    if addresses.is_empty() {
        return Err(InstallError::NoAddresses {
            host: host.to_owned(),
        });
    }
    for address in &addresses {
        let blocked = match address.ip() {
            IpAddr::V4(ip) => is_non_global_v4(ip),
            IpAddr::V6(ip) => is_non_global_v6(ip),
        };
        if blocked {
            return Err(InstallError::NonPublicAddress {
                host: host.to_owned(),
                address: address.ip(),
            });
        }
    }
    Ok(addresses)
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

fn has_path_traversal(raw: &str) -> bool {
    let path = raw
        .split_once("//")
        .and_then(|(_, remainder)| remainder.split_once('/').map(|(_, path)| path))
        .unwrap_or_default()
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    path.split('/').any(|segment| {
        segment == "."
            || segment == ".."
            || segment.eq_ignore_ascii_case("%2e")
            || segment.eq_ignore_ascii_case("%2e%2e")
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
        || (a == 192 && b == 0 && (c == 0 || c == 2))
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
