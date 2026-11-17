//! `sqlx::migrate!` (in `src/pg.rs`) embeds `migrations/` at compile time but does not tell
//! Cargo to rebuild when a migration is added or edited; without this a changed migration is
//! silently missing from the test containers until something else touches the crate.

fn main() {
    println!("cargo:rerun-if-changed=../migrations");
}
