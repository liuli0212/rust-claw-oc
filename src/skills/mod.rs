//! Unified skill system.
//!
//! This module provides the new General Skill Runtime data model
//! (`SkillDef`) alongside backward-compatible access to legacy
//! script-template skills.

pub mod definition;
pub mod migrate;
pub mod parser;
pub mod policy;
pub mod preamble;
pub mod registry;
pub mod runtime;
pub mod state;

// Re-export the legacy module so existing `crate::skills::load_skills()` calls
// continue to work during the migration period.
mod legacy;
#[allow(unused_imports)]
pub use legacy::{load_skills, SkillTool};
