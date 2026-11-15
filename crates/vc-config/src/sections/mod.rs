//! One file per crate section (r5 Q16). Each P1 agent edits only its own file.

pub mod api;
pub mod database;
pub mod extract;
pub mod fetch;
pub mod frontier;
pub mod index;
pub mod kinds;
pub mod llm;
pub mod obs;
pub mod parse;
pub mod policy;
pub mod queue;
pub mod render;
pub mod secrets;
pub mod sources;
pub mod store;
pub mod vcrawl;
