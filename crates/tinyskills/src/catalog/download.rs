//! Where a catalog entry's `SKILL.md` lives, and the URL to fetch it from.
//!
//! [`derive_download_url`] picks the download URL for an entry at index time;
//! the rest of this module is the per-source knowledge it draws on. Nothing
//! here performs I/O: hosts probe the candidate URLs themselves.
//!
//! - **`ClawHub`** entries carry only a slug; `ClawHub`'s file API serves the raw
//!   `SKILL.md` for it.
//! - **`skills.sh`** entries point at `skills.sh/<owner>/<repo>/<skill>`, a
//!   listing of a GitHub repo. Repos keep skills in different directories, so
//!   a host locates the file at install time: the conventional directories
//!   first ([`SkillsShRef::candidate_urls`]), then one recursive tree listing
//!   of the repo ([`SkillsShRef::tree_api_url`], [`find_skill_md_in_tree`]).
//!
//! `LobeHub` entries are system-prompt agents with no `SKILL.md` at all and stay
//! uninstallable.

use serde_json::Value;

const CLAWHUB_SKILLS_API: &str = "https://clawhub.ai/api/v1/skills";
const GITHUB_RAW: &str = "https://raw.githubusercontent.com";
const GITHUB_REPOS_API: &str = "https://api.github.com/repos";
/// Directories a `skills.sh` repo conventionally keeps a skill under, probed in
/// this order before listing the whole repo.
const SKILLS_SH_BASE_DIRS: [&str; 4] = ["", "skills/", ".agents/skills/", ".claude/skills/"];

/// Resolve a fetchable `SKILL.md` URL for a catalog entry.
///
/// Precedence:
/// 1. `download_base_override`, when non-blank: `<base>/<name>/SKILL.md`
///    (intended for tests and mirrors; the host decides where it comes from).
/// 2. `docs_path`: Hermes' own bundled / optional skills, which live in the
///    `NousResearch/hermes-agent` repo under `skills/` / `optional-skills/`.
/// 3. `source_url` on GitHub (browse.sh, NVIDIA, GitHub, ...): the blob/tree
///    view is rewritten to the `raw.githubusercontent.com` `SKILL.md`.
/// 4. `ClawHub` `identifier`: `ClawHub`'s file API by slug.
/// 5. `skills.sh` `source_url`: the most common location in the listed GitHub
///    repo. A host should locate the real one before fetching.
///
/// Returns an empty string when no download exists (`LobeHub` agents have no
/// `SKILL.md`).
#[must_use]
pub fn derive_download_url(
    source: &str,
    identifier: Option<&str>,
    name: &str,
    docs_path: Option<&str>,
    source_url: Option<&str>,
    download_base_override: Option<&str>,
) -> String {
    if let Some(base) = download_base_override {
        let base = base.trim().trim_end_matches('/');
        if !base.is_empty() {
            // Validate name as a single path segment to prevent traversal
            if !name.contains(['/', '\\']) && name != ".." && name != "." {
                return format!("{base}/{name}/SKILL.md");
            }
        }
    }
    if let Some(url) = docs_path.and_then(download_url_from_docs_path) {
        return url;
    }
    if let Some(url) = source_url.and_then(download_url_from_source_url) {
        return url;
    }
    if source.eq_ignore_ascii_case("clawhub")
        && let Some(url) = identifier.and_then(clawhub_download_url)
    {
        return url;
    }
    if let Some(skill) = source_url.and_then(SkillsShRef::parse)
        && let Some(url) = skill.candidate_urls().into_iter().next()
    {
        return url;
    }
    String::new()
}

/// Rewrite a GitHub `source_url` (blob or tree view) into the raw `SKILL.md`
/// download URL. Returns `None` for non-GitHub hosts (portal pages that serve
/// HTML, not raw markdown).
///
/// - blob: `.../github.com/{owner}/{repo}/blob/{branch}/{path}` becomes
///   `.../raw.githubusercontent.com/{owner}/{repo}/{branch}/{path}`
/// - tree (directory): same rewrite, then append `/SKILL.md`.
#[must_use]
pub fn download_url_from_source_url(source_url: &str) -> Option<String> {
    let rest = source_url
        .strip_prefix("https://github.com/")
        .or_else(|| source_url.strip_prefix("http://github.com/"))?;

    if rest.contains(['?', '#']) {
        return None;
    }

    // {owner}/{repo}/{blob|tree}/{branch}/{path...}
    let parts: Vec<&str> = rest.splitn(5, '/').collect();
    if parts.len() < 5 {
        return None;
    }
    let (owner, repo, kind, branch, path) = (parts[0], parts[1], parts[2], parts[3], parts[4]);
    if owner.is_empty() || repo.is_empty() || branch.is_empty() || path.is_empty() {
        return None;
    }

    let path = path.trim_end_matches('/');
    if ![owner, repo, branch]
        .into_iter()
        .chain(path.split('/'))
        .all(is_safe_segment)
    {
        return None;
    }
    let raw = format!("https://raw.githubusercontent.com/{owner}/{repo}/{branch}/{path}");
    match kind {
        // blob points directly at a file; only append SKILL.md if it isn't one.
        "blob" => {
            if std::path::Path::new(&raw)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            {
                Some(raw)
            } else {
                Some(format!("{raw}/SKILL.md"))
            }
        }
        // tree points at a directory: the skill's SKILL.md lives inside it.
        "tree" => Some(format!("{raw}/SKILL.md")),
        _ => None,
    }
}

/// Raw `SKILL.md` URL for a Hermes `docsPath` such as
/// `bundled/apple/apple-apple-notes` or `optional/devops/devops-docker`.
///
/// Returns `None` for other shapes and for any segment that is not a plain
/// path segment (see [`is_safe_segment`]).
#[must_use]
pub fn download_url_from_docs_path(docs_path: &str) -> Option<String> {
    let parts: Vec<&str> = docs_path.split('/').collect();
    if parts.len() != 3 {
        return None;
    }
    let root = match parts[0] {
        "bundled" => "skills",
        "optional" => "optional-skills",
        _ => return None,
    };
    let category = parts[1];
    let prefixed_slug = parts[2];
    let skill = prefixed_slug
        .strip_prefix(&format!("{category}-"))
        .unwrap_or(prefixed_slug);
    // `category` and `skill` come from the catalog and are spliced into a URL
    // path. A reserved character (space, `#`, `?`, `%`) would change what the
    // URL names, so an entry that is not a plain path segment gets no download
    // URL and is reported as not installable rather than fetched from a
    // different path.
    if !is_safe_segment(category) || !is_safe_segment(skill) {
        return None;
    }
    Some(format!(
        "https://raw.githubusercontent.com/NousResearch/hermes-agent/main/{root}/{category}/{skill}/SKILL.md"
    ))
}

/// Whether a catalog value is one plain URL path segment: non-empty, not `.`
/// or `..`, and only ASCII alphanumerics, `-`, `_` and `.`.
#[must_use]
pub fn is_safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Raw `SKILL.md` URL for a `ClawHub` skill slug, or `None` when the slug is not
/// a plain path segment.
#[must_use]
pub fn clawhub_download_url(slug: &str) -> Option<String> {
    is_safe_segment(slug).then(|| format!("{CLAWHUB_SKILLS_API}/{slug}/file?path=SKILL.md"))
}

/// A `skills.sh` listing, `https://`skills.sh`/<owner>/<repo>/<skill>`.
///
/// A host resolves the listing to a real `SKILL.md` by probing
/// [`candidate_urls`](Self::candidate_urls) and, if none exists, fetching
/// [`tree_api_url`](Self::tree_api_url) and calling
/// [`locate_in_tree`](Self::locate_in_tree).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillsShRef<'a> {
    /// GitHub owner of the listed repo.
    pub owner: &'a str,
    /// GitHub repo name.
    pub repo: &'a str,
    /// Skill directory name within the repo.
    pub skill: &'a str,
}

impl<'a> SkillsShRef<'a> {
    /// Parse a `skills.sh` listing URL. Returns `None` for other hosts, for
    /// URLs without exactly three segments, and for unsafe segments.
    #[must_use]
    pub fn parse(source_url: &'a str) -> Option<Self> {
        let rest = source_url.strip_prefix("https://skills.sh/")?;
        let mut parts = rest.trim_end_matches('/').split('/');
        let (owner, repo, skill) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || ![owner, repo, skill].into_iter().all(is_safe_segment) {
            return None;
        }
        Some(Self { owner, repo, skill })
    }

    /// Raw URLs of the conventional skill locations, most common first.
    #[must_use]
    pub fn candidate_urls(&self) -> Vec<String> {
        SKILLS_SH_BASE_DIRS
            .iter()
            .filter_map(|base| self.raw_url(&format!("{base}{}/SKILL.md", self.skill)))
            .collect()
    }

    /// Raw URL of `path` in this repo. Each segment is percent-encoded: a path
    /// from the repo's tree listing can hold `#`, `?` or spaces, which would
    /// otherwise cut the URL short or change what it names.
    ///
    /// Returns `None` only if the raw-content base URL cannot carry path
    /// segments, which does not happen for the built-in base.
    #[must_use]
    pub fn raw_url(&self, path: &str) -> Option<String> {
        let mut url = url::Url::parse(GITHUB_RAW).ok()?;
        url.path_segments_mut()
            .ok()?
            .extend([self.owner, self.repo, "HEAD"])
            .extend(path.split('/'));
        Some(url.into())
    }

    /// GitHub API URL of the repo's recursive tree listing, for hosts that
    /// fall back to [`locate_in_tree`](Self::locate_in_tree).
    #[must_use]
    pub fn tree_api_url(&self) -> String {
        format!(
            "{GITHUB_REPOS_API}/{}/{}/git/trees/HEAD?recursive=1",
            self.owner, self.repo
        )
    }

    /// The `owner/repo` label used in user-facing messages, as
    /// `github.com/<owner>/<repo>`.
    #[must_use]
    pub fn repo_label(&self) -> String {
        format!("github.com/{}/{}", self.owner, self.repo)
    }

    /// Locate this skill in a GitHub recursive tree listing and return its raw
    /// URL. The error is a [`TreeMiss`]; [`miss_message`](Self::miss_message)
    /// renders it for a user.
    ///
    /// # Errors
    ///
    /// Returns [`TreeMiss`] when the listing does not identify exactly one
    /// `<skill>/SKILL.md`, or when the resulting URL cannot be built
    /// ([`TreeMiss::Absent`]).
    pub fn locate_in_tree(&self, tree: &Value) -> Result<String, TreeMiss> {
        let path = find_skill_md_in_tree(tree, self.skill)?;
        self.raw_url(&path).ok_or(TreeMiss::Absent)
    }

    /// A user-facing explanation of why the tree listing did not yield a
    /// location for this skill.
    #[must_use]
    pub fn miss_message(&self, miss: &TreeMiss) -> String {
        let repo = self.repo_label();
        let skill = self.skill;
        match miss {
            TreeMiss::Absent => {
                format!("'{skill}' is listed on skills.sh, but {repo} has no {skill}/SKILL.md")
            }
            TreeMiss::Truncated => format!(
                "{repo} is too large for GitHub to list in one response, so '{skill}' could not be located"
            ),
            TreeMiss::Ambiguous(paths) => format!(
                "{repo} has more than one {skill}/SKILL.md ({}), and skills.sh does not say which one it lists",
                paths.join(", ")
            ),
        }
    }
}

/// Why a repo tree listing did not yield exactly one skill location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeMiss {
    /// The listing is complete and has no `<skill>/SKILL.md`.
    Absent,
    /// GitHub truncated the listing and it shows at most one match, so neither
    /// absence nor uniqueness is proven.
    Truncated,
    /// Several directories are named for the skill; picking one would be a guess.
    Ambiguous(Vec<String>),
}

/// The single path of `<skill>/SKILL.md` at any depth in a GitHub recursive
/// tree listing (`GET /repos/{owner}/{repo}/git/trees/HEAD?recursive=1`).
///
/// # Errors
///
/// Returns [`TreeMiss`] when the listing has no match, has several matches, or
/// is truncated without proving uniqueness.
pub fn find_skill_md_in_tree(tree: &Value, skill: &str) -> Result<String, TreeMiss> {
    let suffix = format!("/{skill}/SKILL.md");
    let mut matches: Vec<String> = tree
        .get("tree")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("blob"))
        .filter_map(|item| item.get("path").and_then(Value::as_str))
        .filter(|path| path.ends_with(&suffix) || *path == &suffix[1..])
        .map(str::to_string)
        .collect();
    // A truncated listing can omit a match: it proves neither that the skill is
    // absent nor that a lone visible match is the only one. Two visible matches
    // are ambiguous either way.
    let truncated = tree.get("truncated").and_then(Value::as_bool) == Some(true);
    if truncated && matches.len() < 2 {
        return Err(TreeMiss::Truncated);
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(TreeMiss::Absent),
        _ => Err(TreeMiss::Ambiguous(matches)),
    }
}
