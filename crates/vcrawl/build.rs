//! `sqlx::migrate!` (in `src/migrate.rs`) embeds the expand (`migrations/`) and contract
//! (`migrations/contract/`) sets at compile time but does not tell Cargo to rebuild when a
//! migration is added or edited; without this an image could ship without its newest migration.

fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
    println!("cargo:rerun-if-changed=../../migrations/contract");
}
