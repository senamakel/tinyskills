//! URL and slug hardening coverage.

use std::collections::HashMap;

use tinyskills::SkillFrontmatter;
use tinyskills::install::{
    MAX_INSTALL_URL_LEN, derive_install_slug, is_loopback_http_url, is_private_or_local_host,
    normalize_install_url, validate_install_url, validate_resolved_host,
};

#[test]
fn normalizes_supported_urls_and_rejects_non_documents() {
    assert_eq!(
        normalize_install_url("https://github.com/o/r/blob/main/path/SKILL.md"),
        Ok("https://raw.githubusercontent.com/o/r/main/path/SKILL.md".to_owned())
    );
    assert_eq!(
        normalize_install_url("https://example.com/SKILL.MD?raw=1"),
        Ok("https://example.com/SKILL.MD?raw=1".to_owned())
    );
    for url in [
        "not a url",
        "https://github.com/o/r",
        "https://github.com/o/r/tree/main/path",
        "https://github.com/o/r/raw/main/path",
        "https://example.com/archive.zip",
    ] {
        assert!(normalize_install_url(url).is_err(), "{url}");
    }
}

#[test]
fn derives_bounded_slugs_from_id_or_name() {
    let mut metadata = HashMap::new();
    metadata.insert(
        "id".to_owned(),
        serde_yaml::Value::String("Stable ID".to_owned()),
    );
    let frontmatter = SkillFrontmatter {
        name: "Display Name".to_owned(),
        metadata,
        ..SkillFrontmatter::default()
    };
    assert_eq!(
        derive_install_slug(&frontmatter),
        Ok("stable-id".to_owned())
    );
    assert!(derive_install_slug(&SkillFrontmatter::default()).is_err());
    assert!(
        derive_install_slug(&SkillFrontmatter {
            name: "x".repeat(65),
            ..SkillFrontmatter::default()
        })
        .is_err()
    );
    assert_eq!(
        derive_install_slug(&SkillFrontmatter {
            name: " --A  B-- ".into(),
            ..SkillFrontmatter::default()
        }),
        Ok("a-b".into())
    );
}

#[test]
fn validates_scheme_length_and_literal_host_safety() {
    assert!(validate_install_url("", false).is_err());
    assert!(
        validate_install_url(
            &format!("https://example.com/{}", "x".repeat(MAX_INSTALL_URL_LEN)),
            false
        )
        .is_err()
    );
    assert!(validate_install_url("not a url", false).is_err());
    assert!(validate_install_url("ftp://example.com/SKILL.md", false).is_err());
    assert!(validate_install_url("http://localhost/SKILL.md", false).is_err());
    assert!(validate_install_url("http://localhost/SKILL.md", true).is_ok());
    assert!(validate_install_url("https://127.0.0.1/SKILL.md", false).is_err());
    assert!(validate_install_url("https://example.com/SKILL.md", false).is_ok());
}

#[test]
fn classifies_local_hosts_and_loopback_http_exactly() {
    for host in [
        "localhost",
        "LOCALHOST.",
        "sub.localhost",
        "device.local",
        "127.0.0.1",
        "10.0.0.1",
        "169.254.1.1",
        "100.64.0.1",
        "198.51.100.1",
        "[::1]",
        "fd00::1",
        "fe80::1",
        "ff02::1",
        "::ffff:127.0.0.1",
    ] {
        assert!(is_private_or_local_host(host), "{host}");
    }
    for host in ["example.com", "8.8.8.8", "2606:4700:4700::1111"] {
        assert!(!is_private_or_local_host(host), "{host}");
    }
    assert!(is_loopback_http_url("http://127.0.0.1/a"));
    assert!(is_loopback_http_url("http://[::1]/a"));
    assert!(!is_loopback_http_url("https://localhost/a"));
    assert!(!is_loopback_http_url("http://127.0.0.2/a"));
    assert!(!is_loopback_http_url("bad"));
}

#[tokio::test]
async fn dns_guard_rejects_invalid_and_loopback_targets() {
    assert!(validate_resolved_host("not a url").await.is_err());
    assert!(
        validate_resolved_host("https://127.0.0.1/SKILL.md")
            .await
            .is_err()
    );
    assert!(
        validate_resolved_host("https://[::1]/SKILL.md")
            .await
            .is_err()
    );
}
