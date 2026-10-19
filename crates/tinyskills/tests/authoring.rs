//! Slugs, rendering, and bundle scaffolding.

use std::fs;

use tinyskills::{
    AuthoringError, BundleDocument, BundleSpec, DescriptionTooLong, FrontmatterTooLarge,
    MAX_DESCRIPTION_LEN, MAX_NAME_LEN, PunctuationRule, ScaffoldOptions, SlugError, SlugRules,
    check_frontmatter_size, parse_skill_str, render_workflow_frontmatter, render_workflow_md,
    scaffold_bundle_in_scratch, slugify, slugify_with, validate_description,
    validate_description_chars, validate_display_name, validate_slug, yaml_scalar,
};

fn spec(slug: &str) -> BundleSpec {
    BundleSpec {
        slug: slug.to_owned(),
        description: "Does the thing.".to_owned(),
        ..BundleSpec::default()
    }
}

fn edit() -> ScaffoldOptions {
    ScaffoldOptions {
        overwrite: true,
        ..ScaffoldOptions::default()
    }
}

#[test]
fn slugify_collapses_separators_and_trims() -> Result<(), AuthoringError> {
    assert_eq!(slugify("Hello  World")?, "hello-world");
    assert_eq!(slugify("--foo__bar--")?, "foo-bar");
    assert_eq!(slugify("ALL CAPS skill!")?, "all-caps-skill");
    assert!(slugify("   ").is_err());
    assert!(slugify("!!!").is_err());
    assert!(matches!(
        slugify(&"a".repeat(65)),
        Err(AuthoringError::SlugTooLong { .. })
    ));
    Ok(())
}

#[test]
fn validation_trims_and_bounds() -> Result<(), AuthoringError> {
    assert_eq!(validate_display_name("  Name ")?, "Name");
    assert!(matches!(
        validate_display_name(" "),
        Err(AuthoringError::EmptyName)
    ));
    assert!(validate_display_name(&"a".repeat(65)).is_err());
    assert_eq!(validate_description(" d ")?, "d");
    assert!(validate_description("").is_err());
    assert!(validate_description(&"a".repeat(1025)).is_err());
    Ok(())
}

#[test]
fn yaml_scalar_quotes_only_when_needed() {
    assert_eq!(yaml_scalar("plain text"), "plain text");
    assert_eq!(yaml_scalar(""), "\"\"");
    assert_eq!(yaml_scalar("a: b"), "\"a: b\"");
    assert_eq!(yaml_scalar("- item"), "\"- item\"");
    assert_eq!(yaml_scalar("trailing "), "\"trailing \"");
    assert_eq!(
        yaml_scalar("line\nbreak \"q\" \\"),
        "\"line\\nbreak \\\"q\\\" \\\\\""
    );
}

#[test]
fn rendered_frontmatter_round_trips_through_the_parser() -> Result<(), Box<dyn std::error::Error>> {
    let spec = BundleSpec {
        slug: "demo".to_owned(),
        description: "Fix: the # thing".to_owned(),
        license: Some("MIT".to_owned()),
        author: Some("Ada".to_owned()),
        tags: vec!["a".to_owned(), "b c".to_owned()],
        allowed_tools: vec!["Bash".to_owned(), "Read".to_owned()],
    };
    let rendered = render_workflow_frontmatter(&spec);
    let (frontmatter, _, warnings) = parse_skill_str(&rendered).ok_or("not parseable")?;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(frontmatter.name, "demo");
    assert_eq!(frontmatter.description, "Fix: the # thing");
    assert_eq!(frontmatter.license.as_deref(), Some("MIT"));
    assert_eq!(frontmatter.allowed_tools, ["Bash", "Read"]);
    assert_eq!(
        frontmatter
            .metadata
            .get("author")
            .and_then(serde_norway::Value::as_str),
        Some("Ada")
    );

    let document = render_workflow_md(&spec);
    assert!(document.starts_with(&rendered));
    assert!(document.contains("# demo\n"));
    assert!(document.contains("## Instructions"));
    Ok(())
}

#[test]
fn minimal_frontmatter_omits_optional_blocks() {
    let rendered = render_workflow_frontmatter(&spec("demo"));
    assert_eq!(
        rendered,
        "---\nname: demo\ndescription: Does the thing.\n---\n"
    );
}

#[test]
fn scaffold_creates_document_and_resource_dirs() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("skills");
    let outcome = scaffold_bundle_in_scratch(&root, &spec("demo"), &ScaffoldOptions::default())?;
    assert_eq!(outcome.slug, "demo");
    assert!(outcome.document.ends_with("demo/WORKFLOW.md"));
    assert!(outcome.dir.starts_with(fs::canonicalize(&root)?));
    for sub in tinyskills::RESOURCE_DIRS {
        assert!(outcome.dir.join(sub).is_dir(), "{sub}");
    }
    let found = tinyskills::scan_root(&root, tinyskills::SkillScope::User);
    assert_eq!(found[0].dir_name, "demo");

    let as_skill = scaffold_bundle_in_scratch(
        &root,
        &spec("other"),
        &ScaffoldOptions {
            document: BundleDocument::Skill,
            ..ScaffoldOptions::default()
        },
    )?;
    assert!(as_skill.document.ends_with("other/SKILL.md"));
    Ok(())
}

#[test]
fn scaffold_rejects_collisions_and_missing_edit_targets() -> Result<(), Box<dyn std::error::Error>>
{
    let temp = tempfile::tempdir()?;
    scaffold_bundle_in_scratch(temp.path(), &spec("demo"), &ScaffoldOptions::default())?;
    let error = scaffold_bundle_in_scratch(temp.path(), &spec("demo"), &ScaffoldOptions::default())
        .err()
        .ok_or("collision accepted")?;
    assert!(matches!(error, AuthoringError::AlreadyExists { .. }));
    assert!(
        error
            .to_string()
            .starts_with("skill 'demo' already exists at ")
    );

    let error = scaffold_bundle_in_scratch(temp.path(), &spec("ghost"), &edit())
        .err()
        .ok_or("edit of missing accepted")?;
    assert!(matches!(error, AuthoringError::NotFound { .. }));
    assert!(error.to_string().contains("does not exist at"));
    Ok(())
}

#[test]
fn scaffold_rejects_unsafe_slugs_and_bad_descriptions() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    for slug in ["", ".", "..", "../x", "a/b", "a\\b", "/abs"] {
        let error =
            scaffold_bundle_in_scratch(temp.path(), &spec(slug), &ScaffoldOptions::default())
                .err()
                .ok_or("unsafe slug accepted")?;
        assert!(
            matches!(error, AuthoringError::InvalidSlug { .. }),
            "{slug}"
        );
    }
    let blank = BundleSpec {
        description: "  ".to_owned(),
        ..spec("demo")
    };
    assert!(matches!(
        scaffold_bundle_in_scratch(temp.path(), &blank, &ScaffoldOptions::default()),
        Err(AuthoringError::EmptyDescription)
    ));
    assert!(!temp.path().join("demo").exists());
    Ok(())
}

#[test]
fn edit_preserves_body_and_retires_the_other_document() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let dir = temp.path().join("demo");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("SKILL.md"),
        "---\nname: demo\ndescription: old\n---\n\nHand written instructions.\n",
    )?;
    let updated = BundleSpec {
        description: "new description".to_owned(),
        ..spec("demo")
    };
    let outcome = scaffold_bundle_in_scratch(temp.path(), &updated, &edit())?;
    let written = fs::read_to_string(&outcome.document)?;
    assert!(written.contains("description: new description"));
    assert!(written.ends_with("\nHand written instructions.\n"));
    assert!(!written.contains("## Instructions"));
    assert!(!dir.join("SKILL.md").exists(), "legacy document retired");
    assert!(dir.join("WORKFLOW.md").exists());
    Ok(())
}

#[test]
fn edit_refuses_to_overwrite_an_unparseable_body() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let dir = temp.path().join("broken");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("SKILL.md"), "---\nname: broken\ndescription: x\n")?;
    let error = scaffold_bundle_in_scratch(temp.path(), &spec("broken"), &edit())
        .err()
        .ok_or("edit accepted")?;
    assert!(
        error
            .to_string()
            .to_lowercase()
            .contains("could not be parsed"),
        "{error}"
    );
    assert!(fs::read_to_string(dir.join("SKILL.md"))?.contains("name: broken"));
    assert!(!dir.join("WORKFLOW.md").exists());
    Ok(())
}

#[test]
fn edit_refuses_unparseable_target_even_when_fallback_is_valid()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let dir = temp.path().join("broken");
    fs::create_dir_all(&dir)?;
    // WORKFLOW.md exists but is unparseable (no closing ---)
    fs::write(
        dir.join("WORKFLOW.md"),
        "---\nname: broken\ndescription: x\n",
    )?;
    // SKILL.md is valid
    fs::write(
        dir.join("SKILL.md"),
        "---\nname: broken\ndescription: old\n---\n\nValid body.\n",
    )?;
    let error = scaffold_bundle_in_scratch(temp.path(), &spec("broken"), &edit())
        .err()
        .ok_or("edit accepted")?;
    assert!(
        error
            .to_string()
            .to_lowercase()
            .contains("could not be parsed"),
        "{error}"
    );
    // Both documents must remain unmodified
    assert!(fs::read_to_string(dir.join("WORKFLOW.md"))?.contains("name: broken"));
    assert!(fs::read_to_string(dir.join("SKILL.md"))?.contains("Valid body"));
    Ok(())
}

#[test]
fn edit_finds_bundles_under_legacy_roots() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let legacy = temp.path().join("legacy");
    let dir = legacy.join("old-one");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("SKILL.md"),
        "---\nname: old-one\ndescription: d\n---\n\nbody\n",
    )?;
    let options = ScaffoldOptions {
        overwrite: true,
        legacy_roots: vec![legacy.clone()],
        ..ScaffoldOptions::default()
    };
    let outcome =
        scaffold_bundle_in_scratch(&temp.path().join("primary"), &spec("old-one"), &options)?;
    assert_eq!(
        outcome.dir,
        fs::canonicalize(temp.path().join("primary/old-one"))?
    );
    assert!(outcome.document.ends_with("old-one/WORKFLOW.md"));
    assert!(
        dir.join("SKILL.md").is_file(),
        "legacy source remains untouched"
    );
    assert!(outcome.dir.join("WORKFLOW.md").is_file());
    Ok(())
}

#[cfg(unix)]
#[test]
fn edit_refuses_symlinks_in_read_only_legacy_sources() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let legacy = temp.path().join("legacy");
    let dir = legacy.join("old-one");
    let outside = tempfile::tempdir()?;
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("SKILL.md"),
        "---\nname: old-one\ndescription: d\n---\nbody\n",
    )?;
    fs::write(outside.path().join("secret.md"), "outside")?;
    std::os::unix::fs::symlink(outside.path().join("secret.md"), dir.join("references.md"))?;
    let options = ScaffoldOptions {
        overwrite: true,
        legacy_roots: vec![legacy],
        ..ScaffoldOptions::default()
    };
    let result =
        scaffold_bundle_in_scratch(&temp.path().join("primary"), &spec("old-one"), &options);
    assert!(matches!(result, Err(AuthoringError::SymlinkedDir { .. })));
    assert!(
        !temp
            .path()
            .join("primary/old-one")
            .join("references.md")
            .exists()
    );
    assert_eq!(
        fs::read_to_string(outside.path().join("secret.md"))?,
        "outside"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn edit_refuses_non_file_entries_in_read_only_legacy_sources()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let legacy = temp.path().join("legacy");
    let dir = legacy.join("old-one");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("SKILL.md"),
        "---\nname: old-one\ndescription: d\n---\nbody\n",
    )?;
    let _socket = std::os::unix::net::UnixListener::bind(dir.join("socket"))?;
    let options = ScaffoldOptions {
        overwrite: true,
        legacy_roots: vec![legacy],
        ..ScaffoldOptions::default()
    };
    let result =
        scaffold_bundle_in_scratch(&temp.path().join("primary"), &spec("old-one"), &options);
    assert!(matches!(result, Err(AuthoringError::Io { .. })));
    let scratch_bundle = temp.path().join("primary/old-one");
    assert!(scratch_bundle.join("SKILL.md").is_file());
    assert!(!scratch_bundle.join("socket").exists());
    assert!(dir.join("socket").exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn scaffold_refuses_a_symlinked_bundle_dir() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    std::os::unix::fs::symlink(outside.path(), temp.path().join("demo"))?;
    let error = scaffold_bundle_in_scratch(temp.path(), &spec("demo"), &edit())
        .err()
        .ok_or("symlink accepted")?;
    assert!(matches!(error, AuthoringError::SymlinkedDir { .. }));
    assert!(fs::read_dir(outside.path())?.next().is_none());
    Ok(())
}

// --- product bounds on slugs ---

/// A host whose skill routes sit beside static ones, with a tighter cap.
const PRODUCT: SlugRules<'static> = SlugRules::new()
    .max_chars(12)
    .reserved(&["draft", "upload"])
    .truncate(true);

#[test]
fn default_slug_rules_match_slugify() -> Result<(), AuthoringError> {
    let rules = SlugRules::default();
    assert_eq!(rules.max_chars, MAX_NAME_LEN);
    assert_eq!(
        slugify_with("Hello  World", &rules)?,
        slugify("Hello  World")?
    );
    assert!(matches!(
        slugify_with(&"a".repeat(65), &rules),
        Err(AuthoringError::SlugTooLong { .. })
    ));
    Ok(())
}

/// Truncating keeps a long name authorable instead of making its author
/// rename it to satisfy a limit they cannot see.
#[test]
fn a_truncating_rule_cuts_at_the_cap_without_a_trailing_dash() -> Result<(), AuthoringError> {
    assert_eq!(
        slugify_with("Quarterly Board Pack", &PRODUCT)?,
        "quarterly-bo"
    );
    assert_eq!(slugify_with("Sales Repor Q3", &PRODUCT)?, "sales-repor");
    Ok(())
}

/// A derived slug never lands on a reserved one: it would be created and then
/// unreachable behind the static route of the same name.
#[test]
fn slugify_steps_around_a_reserved_slug() -> Result<(), AuthoringError> {
    assert_eq!(slugify_with("Draft", &PRODUCT)?, "draft-2");
    assert_eq!(slugify_with("Drafting", &PRODUCT)?, "drafting");
    Ok(())
}

/// A host that folds every punctuation run to a separator and never refuses a
/// name: what it derives always passes its own [`validate_slug`].
#[test]
fn separator_rules_with_a_fallback_always_yield_a_valid_slug() -> Result<(), AuthoringError> {
    const SEPARATED: SlugRules<'static> = SlugRules::new()
        .reserved(&["draft", "upload", "registry"])
        .truncate(true)
        .punctuation(PunctuationRule::Separator)
        .fallback("skill");

    assert_eq!(
        slugify_with("Q3 board-pack (v2)", &SEPARATED)?,
        "q3-board-pack-v2"
    );
    assert_eq!(slugify_with("Registry", &SEPARATED)?, "registry-2");
    assert_eq!(slugify_with("???", &SEPARATED)?, "skill");
    for name in ["x".repeat(200), " a ".repeat(40), "Upload".to_owned()] {
        let slug = slugify_with(&name, &SEPARATED)?;
        assert_eq!(
            validate_slug(&slug, &SEPARATED),
            Ok(()),
            "{name:?} -> {slug:?}"
        );
    }
    Ok(())
}

#[test]
fn validate_slug_accepts_a_safe_bounded_unreserved_slug() {
    assert_eq!(
        validate_slug("press-outreach", &SlugRules::default()),
        Ok(())
    );
    assert_eq!(validate_slug("a1", &PRODUCT), Ok(()));
}

#[test]
fn validate_slug_names_each_refusal() {
    assert_eq!(validate_slug("", &PRODUCT), Err(SlugError::Empty));
    for bad in ["-lead", "Upper", "under_score", "a/b", "..", "sp ace"] {
        assert_eq!(
            validate_slug(bad, &PRODUCT),
            Err(SlugError::InvalidShape { slug: bad.into() }),
            "{bad}"
        );
    }
    assert_eq!(
        validate_slug("thirteen-char", &PRODUCT),
        Err(SlugError::TooLong {
            length: 13,
            max: 12
        })
    );
    assert_eq!(
        validate_slug("upload", &PRODUCT),
        Err(SlugError::Reserved {
            slug: "upload".into()
        })
    );
    let message = SlugError::Reserved {
        slug: "upload".into(),
    }
    .to_string();
    assert!(message.contains("reserved"), "{message}");
}

// --- character and byte budgets ---

fn skill_md(description: &str) -> String {
    format!("---\nname: demo\ndescription: {description}\n---\n# Body\n")
}

#[test]
fn a_description_at_the_character_cap_passes_and_one_over_is_refused() {
    let at_cap = "d".repeat(MAX_DESCRIPTION_LEN);
    assert_eq!(
        validate_description_chars(&at_cap, MAX_DESCRIPTION_LEN),
        Ok(())
    );

    let over = "d".repeat(MAX_DESCRIPTION_LEN + 1);
    let refused = validate_description_chars(&over, MAX_DESCRIPTION_LEN);
    assert_eq!(
        refused,
        Err(DescriptionTooLong {
            chars: MAX_DESCRIPTION_LEN + 1,
            max: MAX_DESCRIPTION_LEN,
        })
    );
    let message = refused
        .map_err(|error| error.to_string())
        .err()
        .unwrap_or_default();
    assert!(message.contains("1025 characters"), "{message}");
    assert!(message.contains("1024"), "{message}");
}

#[test]
fn the_description_budget_counts_characters_not_bytes() {
    let multibyte = "é".repeat(MAX_DESCRIPTION_LEN);
    assert!(multibyte.len() > MAX_DESCRIPTION_LEN);
    assert_eq!(
        validate_description_chars(&multibyte, MAX_DESCRIPTION_LEN),
        Ok(())
    );
    assert_eq!(validate_description_chars("", 0), Ok(()));
}

#[test]
fn a_frontmatter_block_over_the_byte_budget_is_refused() {
    let padding = "x".repeat(4096);
    let src = format!("---\nname: demo\ndescription: short\npadding: {padding}\n---\n# Body\n");
    let refused = check_frontmatter_size(&src, 4096);
    assert!(
        matches!(refused, Err(FrontmatterTooLarge { bytes, max: 4096 }) if bytes > 4096),
        "{refused:?}"
    );
    let message = refused
        .map_err(|error| error.to_string())
        .err()
        .unwrap_or_default();
    assert!(message.contains("frontmatter block"), "{message}");
}

#[test]
fn a_frontmatter_block_within_the_budget_passes_with_unknown_keys() {
    let padding = "x".repeat(2048);
    let src = format!("---\nname: demo\ndescription: short\npadding: {padding}\n---\n# Body\n");
    assert_eq!(check_frontmatter_size(&src, 4096), Ok(()));
}

#[test]
fn the_frontmatter_budget_is_inclusive_and_excludes_the_fences_and_body() {
    let src = skill_md("d");
    let block = "name: demo\ndescription: d\n".len();
    assert_eq!(check_frontmatter_size(&src, block), Ok(()));
    assert_eq!(
        check_frontmatter_size(&src, block - 1),
        Err(FrontmatterTooLarge {
            bytes: block,
            max: block - 1,
        })
    );
    let big_body = format!("{}{}", skill_md("d"), "b".repeat(10_000));
    assert_eq!(check_frontmatter_size(&big_body, block), Ok(()));
}

#[test]
fn a_document_without_frontmatter_passes_the_size_check() {
    assert_eq!(check_frontmatter_size("# No frontmatter\n", 0), Ok(()));
    assert_eq!(check_frontmatter_size("---\nname: x\n", 0), Ok(()));
}
