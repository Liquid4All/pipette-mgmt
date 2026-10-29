fn main() {
    println!("cargo:rerun-if-env-changed=RELEASE_VERSION");

    let version = std::env::var("RELEASE_VERSION").unwrap_or_else(|_| "dev".to_string());

    println!("cargo:rustc-env=PIPETTE_MGMT_VERSION={version}");
}
