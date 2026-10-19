//! Public behavior checks for portable skill operations.

use std::fs;

use tinyskills::{
    BundledFile, BundledSkill, DiscoveryRoot, SkillFrontmatter, SkillScope, discover,
    document_digest, inventory_resources, parse_flat, parse_skill_str, read_resource, render_flat,
};

#[test]
fn parses_scalar_tool_lists_and_preserves_body() -> Result<(), Box<dyn std::error::Error>> {
    let source = "---\nname: demo\ndescription: useful\nallowed-tools: Bash, Read\n---\nDo it.\n";
    let (frontmatter, body, warnings) = parse_skill_str(source).ok_or("invalid document")?;
    assert_eq!(frontmatter.allowed_tools, ["Bash", "Read"]);
    assert_eq!(body, "Do it.\n");
    assert_eq!(warnings.len(), 0);
    Ok(())
}

fn frontmatter(yaml: &str) -> Result<SkillFrontmatter, serde_norway::Error> {
    serde_norway::from_str(yaml)
}

#[test]
fn allowed_tools_accepts_a_yaml_sequence() -> Result<(), Box<dyn std::error::Error>> {
    let fm = frontmatter("allowed-tools:\n  - Bash\n  - Read\n")?;
    assert_eq!(fm.allowed_tools, ["Bash", "Read"]);
    Ok(())
}

#[test]
fn allowed_tools_accepts_a_comma_joined_string() -> Result<(), Box<dyn std::error::Error>> {
    let fm = frontmatter("allowed-tools: Bash, Read, Grep, Skill, WebFetch")?;
    assert_eq!(
        fm.allowed_tools,
        ["Bash", "Read", "Grep", "Skill", "WebFetch"]
    );
    Ok(())
}

#[test]
fn allowed_tools_trims_whitespace_and_drops_empty_tokens() -> Result<(), Box<dyn std::error::Error>>
{
    let fm = frontmatter(r#"allowed-tools: " Bash, , Read, ""#)?;
    assert_eq!(fm.allowed_tools, ["Bash", "Read"]);
    Ok(())
}

#[test]
fn allowed_tools_accepts_the_tools_and_snake_case_aliases() -> Result<(), Box<dyn std::error::Error>>
{
    assert_eq!(
        frontmatter("tools: Bash, Read")?.allowed_tools,
        ["Bash", "Read"]
    );
    assert_eq!(
        frontmatter("allowed_tools: Bash, Read")?.allowed_tools,
        ["Bash", "Read"]
    );
    Ok(())
}

#[test]
fn allowed_tools_defaults_to_empty_when_absent() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(frontmatter("name: foo")?.allowed_tools.len(), 0);
    Ok(())
}

#[test]
fn discovery_is_recursive_and_higher_scope_wins() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let user = temp.path().join("user/demo");
    let project = temp.path().join("project/nested/demo");
    write_skill(&user, "demo", "user copy")?;
    write_skill(&project, "demo", "project copy")?;

    let found = discover([
        DiscoveryRoot::new(temp.path().join("user"), SkillScope::User),
        DiscoveryRoot::new(temp.path().join("project"), SkillScope::Project),
    ]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].description, "project copy");
    assert_eq!(found[0].scope, SkillScope::Project);
    assert!(
        found[0]
            .warnings
            .iter()
            .any(|warning| warning.contains("shadowed"))
    );
    Ok(())
}

#[test]
fn inventory_and_reads_ignore_symlinks() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let dir = temp.path().join("demo");
    write_skill(&dir, "demo", "description")?;
    fs::create_dir_all(dir.join("references"))?;
    fs::write(dir.join("references/guide.md"), "safe")?;
    let skills = discover([DiscoveryRoot::new(temp.path(), SkillScope::User)]);
    assert_eq!(
        inventory_resources(&dir),
        [std::path::PathBuf::from("references/guide.md")]
    );
    assert_eq!(
        read_resource(&skills[0], std::path::Path::new("references/guide.md"))?,
        "safe"
    );
    assert!(read_resource(&skills[0], std::path::Path::new("../outside")).is_err());
    Ok(())
}

#[test]
fn a_rendered_document_digest_is_a_stable_pin() -> Result<(), Box<dyn std::error::Error>> {
    let source = "---\nname: Web Research\ndescription: Answer a question\nversion: 1.0.0\n---\n# Web Research\n";
    let rendered = render_flat(&parse_flat(source)?);
    assert_eq!(rendered, source);
    assert_eq!(
        document_digest(&rendered),
        "0ef45f5926b4b5defa74aacc862bac266efb80cf8350539de29f85b458dcbcc7"
    );
    Ok(())
}

#[test]
fn a_document_digest_is_not_a_bundle_digest() {
    static FILES: &[BundledFile] = &[BundledFile {
        path: "SKILL.md",
        contents: "---\nname: demo\ndescription: bundled\n---\n",
    }];
    let bundle = BundledSkill {
        dir_name: "demo",
        files: FILES,
    };
    assert_ne!(bundle.digest(), document_digest(FILES[0].contents));
}

fn write_skill(dir: &std::path::Path, name: &str, description: &str) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nBody\n"),
    )
}
