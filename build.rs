fn main() {
    println!("cargo:rerun-if-env-changed=PRVIEW_SOURCE_SHA");
    println!("cargo:rerun-if-env-changed=PRVIEW_GENERATOR_RELEASE_TAG");

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

    // These are build-provided release metadata, not independent attestation.
    // The release workflow verifies that the tag resolves to the source SHA;
    // this script only checks that the supplied metadata are well formed.
    // Use a dedicated build input: the workflow's global PRVIEW_RELEASE_TAG
    // also exists during dry runs and cargo package, which must stay unstamped.
    let release_tag = match std::env::var("PRVIEW_GENERATOR_RELEASE_TAG") {
        Ok(value) if value.is_empty() => String::new(),
        Ok(value) => {
            let expected = format!("v{}", std::env::var("CARGO_PKG_VERSION").unwrap());
            assert_eq!(
                value, expected,
                "PRVIEW_GENERATOR_RELEASE_TAG must match the Cargo package version"
            );
            assert_ne!(
                source_sha, "unknown",
                "PRVIEW_GENERATOR_RELEASE_TAG requires an explicit PRVIEW_SOURCE_SHA"
            );
            value
        }
        Err(std::env::VarError::NotPresent) => String::new(),
        Err(error) => panic!("PRVIEW_GENERATOR_RELEASE_TAG is not valid Unicode: {error}"),
    };
    println!("cargo:rustc-env=PRVIEW_BUILD_RELEASE_TAG={release_tag}");
}
