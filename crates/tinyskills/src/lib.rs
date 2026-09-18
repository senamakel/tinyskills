//! Portable primitives for agentskills.io-style skill bundles.
//!
//! `tinyskills` owns the host-independent parts of skill handling: document
//! parsing, metadata, deterministic discovery, collision precedence, safe
//! resource reads, and materialization of compile-time bundles. Product policy
//! such as installation roots, workspace trust, RPC, approvals, and execution
//! remains with the embedding host.

pub mod bundle;
pub mod discovery;
pub mod document;
pub mod model;
pub mod resource;

pub use bundle::{BundledFile, BundledSkill, InstallReport};
pub use discovery::{DiscoveryRoot, discover, load_skill_dir, resolve_collisions, scan_root};
pub use document::{inventory_resources, parse_skill, parse_skill_str};
pub use model::{Skill, SkillFrontmatter, SkillScope};
pub use resource::{read_resource, resolve_skill};
