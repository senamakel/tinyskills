//! Minimal smoke test for the public tinyskills API.

fn main() {
    let source = "---\nname: demo\ndescription: example\n---\nBody\n";
    let Some((frontmatter, body, warnings)) = tinyskills::parse_skill_str(source) else {
        eprintln!("example document did not parse");
        std::process::exit(1);
    };
    assert_eq!(frontmatter.name, "demo");
    assert_eq!(body, "Body\n");
    assert!(warnings.is_empty());
    println!("parsed {}", frontmatter.name);
}
