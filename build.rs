fn main() {
    println!("cargo:rerun-if-env-changed=PRVIEW_SOURCE_SHA");

    let source_sha = match std::env::var("PRVIEW_SOURCE_SHA") {
        Ok(value) if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            value.to_ascii_lowercase()
        }
        Ok(value) => panic!(
            "PRVIEW_SOURCE_SHA must be an exact 40-character hexadecimal commit id, got {value:?}"
        ),
        Err(std::env::VarError::NotPresent) => "unknown".to_owned(),
        Err(error) => panic!("PRVIEW_SOURCE_SHA is not valid Unicode: {error}"),
    };

    println!("cargo:rustc-env=PRVIEW_BUILD_SOURCE_SHA={source_sha}");
}
