//! Failure-path and compatibility coverage for the portable API.

use std::fs;
use std::path::{Path, PathBuf};

use tinyskills::{
    BundledFile, BundledSkill, DiscoveryRoot, Skill, SkillScope, discover, parse_skill_str,
    read_resource, resolve_skill, scan_root,
};

#[test]
fn parsing_handles_plain_unterminated_and_invalid_yaml() -> Result<(), Box<dyn std::error::Error>> {
    let (frontmatter, body, warnings) = parse_skill_str("plain body").ok_or("plain rejected")?;
    assert!(frontmatter.name.is_empty());
    assert_eq!(body, "plain body");
    assert!(warnings.is_empty());
    assert!(parse_skill_str("---\nname: demo\n").is_none());

    let (frontmatter, _, warnings) =
        parse_skill_str("---\nname: [\n---\nbody\n").ok_or("invalid yaml rejected")?;
    assert!(frontmatter.name.is_empty());
    assert_eq!(warnings.len(), 1);
    assert!(parse_skill_str("").is_none());
    Ok(())
}

#[test]
fn discovery_normalizes_metadata_and_legacy_manifests() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let rich = temp.path().join("rich");
    fs::create_dir_all(rich.join("scripts/nested"))?;
    fs::write(rich.join("scripts/nested/run.sh"), "run")?;
    fs::write(
        rich.join("SKILL.md"),
        "---\nname: renamed\ndescription: rich skill\nplatforms: [linux]\nversion: 1\nauthor: Ada\ntags: [old]\nmetadata:\n  version: '2'\n  author: Grace\n  tags: [new]\n  related_skills: [other]\n  hermes:\n    tags: [hermes]\n    related_skills: [sibling]\n---\nBody\n",
    )?;
    let legacy = temp.path().join("legacy");
    fs::create_dir_all(&legacy)?;
    fs::write(
        legacy.join("skill.json"),
        r#"{"name":"old","description":"legacy","version":"1","prompts":["p.md"]}"#,
    )?;
    let broken = temp.path().join("broken");
    fs::create_dir_all(&broken)?;
    fs::write(broken.join("skill.json"), "{")?;

    let found = scan_root(temp.path(), SkillScope::User);
    let rich = found
        .iter()
        .find(|skill| skill.dir_name == "rich")
        .ok_or("rich missing")?;
    assert_eq!(rich.name, "renamed");
    assert_eq!(rich.version, "2");
    assert_eq!(rich.author.as_deref(), Some("Grace"));
    assert_eq!(rich.tags, ["hermes", "new", "old"]);
    assert_eq!(rich.related_skills, ["other", "sibling"]);
    assert_eq!(rich.source_format, "hermes");
    assert_eq!(rich.resources, [PathBuf::from("scripts/nested/run.sh")]);
    assert!(
        rich.warnings
            .iter()
            .any(|warning| warning.contains("does not match"))
    );
    assert_eq!(
        found
            .iter()
            .find(|skill| skill.dir_name == "legacy")
            .ok_or("legacy missing")?
            .prompts,
        ["p.md"]
    );
    assert!(
        !found
            .iter()
            .find(|skill| skill.dir_name == "broken")
            .ok_or("broken missing")?
            .warnings
            .is_empty()
    );
    Ok(())
}

#[test]
fn discovery_falls_back_for_missing_metadata_and_skips_unsafe_entries()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let fallback = temp.path().join("nested/fallback");
    fs::create_dir_all(&fallback)?;
    fs::write(
        fallback.join("WORKFLOW.md"),
        "# Heading\n\nFirst useful line\n",
    )?;
    fs::create_dir_all(temp.path().join(".hidden"))?;
    fs::write(temp.path().join("file"), "not a directory")?;
    fs::create_dir_all(temp.path().join("node_modules/ignored"))?;
    fs::write(temp.path().join("node_modules/ignored/SKILL.md"), "ignored")?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&fallback, temp.path().join("alias"))?;

    let found = scan_root(temp.path(), SkillScope::Legacy);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "fallback");
    assert_eq!(found[0].description, "First useful line");
    assert_eq!(
        found[0].read_body().as_deref(),
        Some("# Heading\n\nFirst useful line\n")
    );
    assert!(scan_root(&temp.path().join("missing"), SkillScope::User).is_empty());
    Ok(())
}

#[test]
fn collision_resolution_handles_lower_and_equal_precedence()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let project = temp.path().join("project/shared");
    let user = temp.path().join("user/shared");
    write_skill(&project, "shared", "project")?;
    write_skill(&user, "shared", "user")?;
    let found = discover([
        DiscoveryRoot::new(temp.path().join("project"), SkillScope::Project),
        DiscoveryRoot::new(temp.path().join("user"), SkillScope::User),
    ]);
    assert_eq!(found[0].description, "project");
    assert!(
        found[0]
            .warnings
            .iter()
            .any(|warning| warning.contains("ignored"))
    );

    let other = temp.path().join("other/different");
    write_skill(&other, "shared", "equal")?;
    let equal = discover([
        DiscoveryRoot::new(temp.path().join("user"), SkillScope::User),
        DiscoveryRoot::new(temp.path().join("other"), SkillScope::User),
    ]);
    assert_eq!(equal.len(), 1);
    Ok(())
}

#[test]
fn resolve_skill_rejects_missing_and_ambiguous_ids() {
    let skill = |name: &str, dir: &str, location: &str| Skill {
        name: name.to_owned(),
        dir_name: dir.to_owned(),
        location: Some(PathBuf::from(location)),
        ..Skill::default()
    };
    assert!(resolve_skill(Vec::<Skill>::new(), "missing").is_err());
    assert!(
        resolve_skill(
            [skill("one", "same", "a"), skill("two", "same", "b")],
            "same"
        )
        .is_err()
    );
    assert!(
        resolve_skill(
            [skill("same", "one", "a"), skill("same", "two", "b")],
            "same"
        )
        .is_err()
    );
    assert!(resolve_skill([skill("other", "id", "a"), skill("id", "two", "b")], "id").is_err());
    assert!(resolve_skill([skill("other", "id", "a"), skill("id", "two", "a")], "id").is_ok());
    let one = skill("same", "same", "a");
    assert_eq!(
        resolve_skill([one.clone()], "same").map(|found| found.location),
        Ok(one.location)
    );
}

#[test]
fn resource_reads_reject_every_unsafe_shape() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let dir = temp.path().join("demo");
    write_skill(&dir, "demo", "demo")?;
    fs::create_dir_all(dir.join("references/folder"))?;
    fs::write(dir.join("references/binary"), [0xff])?;
    fs::write(dir.join("references/large"), vec![b'x'; 128 * 1024 + 1])?;
    let skill = scan_root(temp.path(), SkillScope::User).remove(0);
    assert!(read_resource(&skill, Path::new("")).is_err());
    assert!(read_resource(&skill, Path::new("/absolute")).is_err());
    assert!(read_resource(&skill, Path::new("./references/binary")).is_err());
    assert!(read_resource(&skill, Path::new("references/missing")).is_err());
    assert!(read_resource(&skill, Path::new("references/folder")).is_err());
    assert!(read_resource(&skill, Path::new("references/binary")).is_err());
    assert!(read_resource(&skill, Path::new("references/large")).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("binary", dir.join("references/link"))?;
        assert!(read_resource(&skill, Path::new("references/link")).is_err());
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside)?;
        fs::write(outside.join("secret"), "secret")?;
        std::os::unix::fs::symlink(&outside, dir.join("references/outside"))?;
        assert!(read_resource(&skill, Path::new("references/outside/secret")).is_err());
    }
    let no_location = Skill {
        name: "none".into(),
        ..Skill::default()
    };
    assert!(read_resource(&no_location, Path::new("file")).is_err());
    let missing_root = Skill {
        name: "missing".into(),
        location: Some(temp.path().join("gone/SKILL.md")),
        ..Skill::default()
    };
    assert!(read_resource(&missing_root, Path::new("file")).is_err());
    Ok(())
}

#[test]
fn bundle_validation_and_exactness_reject_unsafe_state() -> Result<(), Box<dyn std::error::Error>> {
    static MANIFEST: &[BundledFile] = &[BundledFile {
        path: "SKILL.md",
        contents: "body",
    }];
    static NO_MANIFEST: &[BundledFile] = &[BundledFile {
        path: "readme",
        contents: "x",
    }];
    for name in ["", ".hidden", "a/b", "a\\b"] {
        assert!(
            BundledSkill {
                dir_name: name,
                files: MANIFEST
            }
            .validate()
            .is_err()
        );
    }
    assert!(
        BundledSkill {
            dir_name: "empty",
            files: &[]
        }
        .validate()
        .is_err()
    );
    assert!(
        BundledSkill {
            dir_name: "demo",
            files: NO_MANIFEST
        }
        .validate()
        .is_err()
    );
    for path in [
        "",
        "/x",
        "a\\b",
        "a:b",
        "a//b",
        "a/./b",
        "a/../b",
        ".hidden/x",
    ] {
        let files: &'static [BundledFile] = Box::leak(
            vec![
                BundledFile {
                    path: "SKILL.md",
                    contents: "body",
                },
                BundledFile {
                    path: Box::leak(path.to_owned().into_boxed_str()),
                    contents: "x",
                },
            ]
            .into_boxed_slice(),
        );
        assert!(
            BundledSkill {
                dir_name: "demo",
                files
            }
            .validate()
            .is_err()
        );
    }

    let temp = tempfile::tempdir()?;
    let bundle = BundledSkill {
        dir_name: "demo",
        files: MANIFEST,
    };
    let first = tinyskills::bundle::install(temp.path(), &[bundle]);
    assert_eq!(first.written, ["demo"]);
    let unchanged = tinyskills::bundle::install(temp.path(), &[bundle]);
    assert_eq!(unchanged.unchanged, ["demo"]);
    fs::write(temp.path().join("demo/extra"), "x")?;
    assert!(!tinyskills::bundle::is_current_materialization(
        &temp.path().join("demo"),
        bundle
    ));
    assert!(!tinyskills::bundle::is_current_materialization(
        &temp.path().join("missing"),
        bundle
    ));
    let failed = tinyskills::bundle::install(
        temp.path(),
        &[BundledSkill {
            dir_name: "bad/name",
            files: MANIFEST,
        }],
    );
    assert_eq!(failed.failed.len(), 1);
    Ok(())
}

#[test]
fn scope_precedence_and_legacy_body_are_stable() {
    assert_eq!(SkillScope::Builtin.precedence(), 0);
    assert_eq!(SkillScope::Legacy.precedence(), 1);
    assert_eq!(SkillScope::User.precedence(), 2);
    assert_eq!(SkillScope::Project.precedence(), 3);
    assert_eq!(SkillScope::Profile.precedence(), 4);
    assert_eq!(SkillScope::Flow.precedence(), 5);
    assert!(
        Skill {
            legacy: true,
            ..Skill::default()
        }
        .read_body()
        .is_none()
    );
    assert!(
        Skill {
            location: Some(PathBuf::from("missing")),
            ..Skill::default()
        }
        .read_body()
        .is_none()
    );
}

fn write_skill(dir: &Path, name: &str, description: &str) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nBody\n"),
    )
}
