//! Marks an official build of gleon with the `gleon_official` cfg.
//!
//! The release workflow passes `GLEON_OFFICIAL_SECRET`; it is checked here, when the crate is
//! compiled, so the binary holds only the outcome. `GLEON_REQUIRE_OFFICIAL` makes a build without
//! the right secret fail instead of producing an unofficial binary.

#[path = "build/official.rs"]
mod official;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(gleon_official)");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=build/official.rs");
    println!("cargo::rerun-if-env-changed=GLEON_OFFICIAL_SECRET");
    println!("cargo::rerun-if-env-changed=GLEON_REQUIRE_OFFICIAL");

    let secret = std::env::var("GLEON_OFFICIAL_SECRET").ok();
    let required = std::env::var_os("GLEON_REQUIRE_OFFICIAL").is_some();
    match official::decide(secret.as_deref(), required) {
        Ok(true) => println!("cargo::rustc-cfg=gleon_official"),
        Ok(false) => {}
        Err(reason) => {
            eprintln!("{reason}");
            std::process::exit(1);
        }
    }
}
