const RERUN_PATHS: &[&str] = &[
    "migrations",
    "src",
    "build.rs",
    "Cargo.toml",
    "Cargo.lock",
    ".git/HEAD",
    ".git/index",
    ".git/refs",
];

fn main() {
    for path in RERUN_PATHS {
        println!("cargo:rerun-if-changed={path}");
    }
    built::write_built_file().expect("failed to collect build information");
}
