//! Ranking installed skills against a plain-language query.
//!
//! Generic library behaviour over [`Skill`] metadata: which roots are scanned
//! and whose skills a caller may see stay with the host. Ranking is
//! [`tinytools::rank::Bm25Index`] over the skill id, name, description and
//! tags, and results are returned as a capped, sanitised projection
//! ([`SkillHit`]) rather than the whole [`Skill`], so a search pays only for
//! the skills that matched.

use serde::Serialize;
use tinytools::rank::Bm25Index;

use crate::{Skill, SkillScope};

/// Cap on a returned description, in bytes.
///
/// Skill descriptions are third-party metadata; the same 240 a prompt
/// catalogue uses, so a skill cannot present one size there and another here.
pub const MAX_DESCRIPTION: usize = 240;

/// The compact per-result projection of a [`Skill`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkillHit {
    /// The id the caller must use to refer to the skill.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Sanitised description, capped at [`MAX_DESCRIPTION`].
    pub description: String,
    /// Search tags.
    pub tags: Vec<String>,
    /// Discovery scope.
    pub scope: SkillScope,
}

/// The text a skill is matched on.
///
/// `dir_name` is included as well as `name` because they diverge: the id a
/// caller must eventually use is `dir_name`.
fn searchable_text(skill: &Skill) -> String {
    format!(
        "{} {} {} {}",
        skill.dir_name,
        skill.name,
        skill.description,
        skill.tags.join(" ")
    )
}

/// Rank `skills` against `query`, best first, at most `limit` results.
///
/// A query with no relevant match returns nothing rather than `limit`
/// arbitrary entries.
#[must_use]
pub fn rank<'a>(skills: &'a [Skill], query: &str, limit: usize) -> Vec<&'a Skill> {
    let texts: Vec<(String, String)> = skills
        .iter()
        .map(|s| (s.id().to_string(), searchable_text(s)))
        .collect();
    let index = Bm25Index::build(texts.iter().map(|(id, text)| (id.as_str(), text.as_str())));
    index
        .search(query, limit)
        .into_iter()
        .map(|i| &skills[i])
        .collect()
}

/// Project a skill to its capped, sanitised search result.
///
/// The description is author-controlled text a model will read as if the host
/// had said it, so control characters and instruction fences are stripped and
/// the length capped. The crate stays free of network dependencies, so the
/// host supplies the sanitiser (`tinytools::sanitize::sanitize_for_llm`
/// takes `(text, max_bytes)`); it is called with [`MAX_DESCRIPTION`].
#[must_use]
pub fn project(skill: &Skill, sanitize: impl Fn(&str, usize) -> String) -> SkillHit {
    SkillHit {
        id: skill.id().to_string(),
        name: skill.name.clone(),
        description: sanitize(&skill.description, MAX_DESCRIPTION),
        tags: skill.tags.clone(),
        scope: skill.scope,
    }
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
