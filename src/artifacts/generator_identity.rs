//! Identity of the generator executable, independent of the reviewed repository.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Serialize)]
pub(super) struct GeneratorIdentity {
    pub git_sha: &'static str,
    pub binary_build_id: Option<String>,
    pub binary_build_id_status: &'static str,
    pub binary_build_id_reason: Option<String>,
    /// Build-provided metadata; the release workflow owns tag/SHA verification.
    pub release_tag: Option<&'static str>,
}

/// Hash the actual executable once per process, shared by every generated pack
/// and export surface. Runtime environment variables and CWD are never sources
/// of generator identity. The digest identifies bytes, not a signed attestation.
pub(super) fn current() -> &'static GeneratorIdentity {
    static IDENTITY: OnceLock<GeneratorIdentity> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        GeneratorIdentity::collect(
            env!("PRVIEW_BUILD_SOURCE_SHA"),
            option_env!("PRVIEW_BUILD_RELEASE_TAG").filter(|tag| !tag.is_empty()),
            running_executable(),
        )
    })
}

fn running_executable() -> io::Result<PathBuf> {
    // Open the executing image, not a pathname that an upgrade can replace.
    // Linux keeps this handle usable after the installed inode is unlinked.
    #[cfg(target_os = "linux")]
    {
        Ok(PathBuf::from("/proc/self/exe"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe()
    }
}

impl GeneratorIdentity {
    fn collect(
        git_sha: &'static str,
        release_tag: Option<&'static str>,
        executable: io::Result<PathBuf>,
    ) -> Self {
        let digest = executable
            .map_err(|error| format!("executable_path_unavailable: {error}"))
            .and_then(|path| {
                hash_executable(&path).map_err(|error| format!("executable_unreadable: {error}"))
            });
        let (binary_build_id, binary_build_id_status, binary_build_id_reason) = match digest {
            Ok(digest) => (Some(digest), "available", None),
            Err(reason) => (None, "unavailable", Some(reason)),
        };
        Self {
            git_sha,
            binary_build_id,
            binary_build_id_status,
            binary_build_id_reason,
            release_tag,
        }
    }
}

fn hash_executable(path: &Path) -> io::Result<String> {
    let mut executable = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let bytes_read = executable.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generator_identity_hashes_actual_executable_once() {
        let identity = current();
        assert!(std::ptr::eq(identity, current()));
        let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(
            identity.binary_build_id.as_deref(),
            Some(format!("sha256:{:x}", Sha256::digest(bytes)).as_str())
        );
        assert_eq!(identity.binary_build_id_status, "available");
        assert!(identity.binary_build_id_reason.is_none());
        assert_eq!(identity.git_sha, env!("PRVIEW_BUILD_SOURCE_SHA"));
    }

    #[test]
    fn generator_identity_unknown_stamps_stay_unknown_when_executable_is_unreadable() {
        let temp = tempfile::tempdir().unwrap();
        let identity =
            GeneratorIdentity::collect("unknown", None, Ok(temp.path().join("missing-executable")));
        let json = serde_json::to_value(identity).unwrap();
        assert_eq!(json["git_sha"], "unknown");
        assert!(json["release_tag"].is_null());
        assert!(json["binary_build_id"].is_null());
        assert_eq!(json["binary_build_id_status"], "unavailable");
        assert!(
            json["binary_build_id_reason"]
                .as_str()
                .unwrap()
                .starts_with("executable_unreadable:")
        );
    }

    #[test]
    fn generator_identity_unavailable_executable_path_has_explicit_reason() {
        let identity = GeneratorIdentity::collect(
            "unknown",
            None,
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no executable path",
            )),
        );
        assert!(identity.binary_build_id.is_none());
        assert_eq!(identity.binary_build_id_status, "unavailable");
        assert_eq!(
            identity.binary_build_id_reason.as_deref(),
            Some("executable_path_unavailable: no executable path")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn review80_running_executable_survives_replacement() {
        const CHILD: &str = "PRVIEW_TEST_REPLACED_EXECUTABLE_HASH";
        if let Some(expected) = std::env::var_os(CHILD) {
            let installed = std::env::current_exe().unwrap();
            assert_eq!(
                installed.file_name().and_then(|name| name.to_str()),
                Some("owned-identity-test")
            );
            let retired = installed.with_extension("retired");
            std::fs::rename(&installed, &retired).unwrap();
            std::fs::write(&installed, b"replacement executable bytes").unwrap();
            std::fs::remove_file(&retired).unwrap();
            let identity = current();
            assert_eq!(identity.binary_build_id_status, "available");
            assert_eq!(identity.binary_build_id.as_deref(), expected.to_str());
            return;
        }
        let owned = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let expected = hash_executable(&executable).unwrap();
        let installed = owned.path().join("owned-identity-test");
        std::fs::copy(&executable, &installed).unwrap();
        let output = std::process::Command::new(&installed)
            .args([
                "--exact",
                "artifacts::generator_identity::tests::review80_running_executable_survives_replacement",
                "--nocapture",
            ])
            .env(CHILD, &expected)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
