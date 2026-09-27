mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

const UNKNOWN_COMMIT: &str = "unknown";
const DIRTY_SUFFIX: &str = "-dirty";

pub fn commit() -> &'static str {
    built_info::GIT_COMMIT_HASH.unwrap_or(UNKNOWN_COMMIT)
}

pub fn dirty() -> Option<bool> {
    built_info::GIT_DIRTY
}

pub fn user_agent() -> String {
    let suffix = match dirty() {
        Some(true) => DIRTY_SUFFIX,
        _ => "",
    };
    format!(
        "{}/{} ({}{suffix})",
        built_info::PKG_NAME,
        built_info::PKG_VERSION,
        commit()
    )
}
