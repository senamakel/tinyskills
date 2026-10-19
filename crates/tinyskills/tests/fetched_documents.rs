//! Fetched-document validation.

use tinyskills::{
    DocumentError, MAX_INSTALL_DOCUMENT_BYTES, check_document_size, redact_url,
    validate_fetched_document,
};

#[test]
fn redact_url_drops_userinfo_query_and_fragment() {
    assert_eq!(
        redact_url("https://user:pw@example.com:8443/a/b.md?token=s#frag"),
        "https://example.com:8443/a/b.md"
    );
    assert_eq!(
        redact_url("https://example.com/x.md"),
        "https://example.com/x.md"
    );
    assert_eq!(redact_url("not a url"), "<unparseable>");
}

#[test]
fn valid_documents_yield_frontmatter_slug_and_warnings() -> Result<(), DocumentError> {
    let document = validate_fetched_document(VALID.as_bytes())?;
    assert_eq!(document.slug, "my-skill");
    assert_eq!(document.frontmatter.description, "does things");
    assert_eq!(document.body, "\nbody\n");
    assert_eq!(document.content, VALID);
    assert_eq!(document.warnings.len(), 0);

    let with_id = "---\nname: x\ndescription: d\nmetadata:\n  id: Custom_ID\n---\n";
    assert_eq!(
        validate_fetched_document(with_id.as_bytes())?.slug,
        "custom-id"
    );

    let bad_yaml = "---\nname: [oops\n---\n";
    assert!(matches!(
        validate_fetched_document(bad_yaml.as_bytes()),
        Err(DocumentError::MissingField("name"))
    ));
    Ok(())
}

#[test]
fn invalid_documents_are_rejected_with_core_compatible_messages() {
    let oversized = vec![b'a'; MAX_INSTALL_DOCUMENT_BYTES + 1];
    let error = validate_fetched_document(&oversized).err();
    assert_eq!(
        error.map(|e| e.to_string()),
        Some(format!(
            "fetch too large: {} bytes exceeds {MAX_INSTALL_DOCUMENT_BYTES} limit",
            MAX_INSTALL_DOCUMENT_BYTES + 1
        ))
    );
    assert!(check_document_size(MAX_INSTALL_DOCUMENT_BYTES as u64).is_ok());
    assert!(check_document_size(MAX_INSTALL_DOCUMENT_BYTES as u64 + 1).is_err());

    let cases: [(&[u8], &str); 5] = [
        (&[0xff, 0xfe], "invalid SKILL.md: body is not valid utf-8"),
        (
            b"---\nname: a\ndescription: b\n",
            "invalid SKILL.md: frontmatter block opened with `---` but never terminated",
        ),
        (
            b"---\ndescription: b\n---\n",
            "invalid SKILL.md: missing required field 'name'",
        ),
        (
            b"---\nname: a\ndescription: '  '\n---\n",
            "invalid SKILL.md: missing required field 'description'",
        ),
        (
            b"---\nname: '!!!'\ndescription: d\n---\n",
            "invalid SKILL.md: cannot derive slug",
        ),
    ];
    for (bytes, expected) in cases {
        let error = validate_fetched_document(bytes)
            .err()
            .map(|e| e.to_string());
        assert!(
            error
                .as_deref()
                .is_some_and(|message| message.starts_with(expected)),
            "{expected} vs {error:?}"
        );
    }
}
