// sqlx::migrate! reads the migrations when it compiles, and cargo doesn't
// know it: without this a new migration isn't in the next build.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
