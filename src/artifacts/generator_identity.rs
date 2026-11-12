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
    #[cfg(target_os = "macos")]
    verify_macos_running_image(&executable)?;
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

// <sys/proc_info.h>: proc_regioninfo and proc_regionwithpathinfo. libc supplies
// the vnode_info_path component and the libproc function declarations.
#[cfg(target_os = "macos")]
#[repr(C)]
struct MacosRegionInfo {
    _protection: u32,
    _max_protection: u32,
    _inheritance: u32,
    _flags: u32,
    _offset: u64,
    _behavior: u32,
    _user_wired_count: u32,
    _user_tag: u32,
    _pages_resident: u32,
    _pages_shared_now_private: u32,
    _pages_swapped_out: u32,
    _pages_dirtied: u32,
    _ref_count: u32,
    _shadow_depth: u32,
    _share_mode: u32,
    _private_pages_resident: u32,
    _shared_pages_resident: u32,
    _obj_id: u32,
    _depth: u32,
    address: u64,
    size: u64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MacosRegionWithPathInfo {
    region: MacosRegionInfo,
    vnode: libc::vnode_info_path,
}

// The SDK API is stable; libc deprecates its Mach-O type bindings in favor
// of mach2. This query only needs an opaque address, not those header types.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    #[link_name = "_dyld_get_image_header"]
    fn dyld_image_header(index: u32) -> *const libc::c_void;
}

#[cfg(target_os = "macos")]
fn verify_macos_running_image(executable: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    const PROC_PIDREGIONPATHINFO: libc::c_int = 8;
    // SAFETY: dyld owns the returned pointer for the process lifetime. Index
    // zero is the main executable; use only its address, without dereferencing.
    let header = unsafe { dyld_image_header(0) };
    if header.is_null() {
        return Err(io::Error::other(
            "running_image_identity_unavailable: no main image",
        ));
    }
    let address = header as usize as u64;
    let mut info = std::mem::MaybeUninit::<MacosRegionWithPathInfo>::zeroed();
    let size = std::mem::size_of::<MacosRegionWithPathInfo>() as libc::c_int;
    // SAFETY: the fixed ABI buffer is writable for its exact reported length.
    // Only this process and the mapping containing its main image are queried.
    let written = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            PROC_PIDREGIONPATHINFO,
            address,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written <= 0 {
        return Err(io::Error::other(format!(
            "running_image_identity_unavailable: {}",
            io::Error::last_os_error()
        )));
    }
    if written != size {
        return Err(io::Error::other(format!(
            "running_image_identity_unavailable: short native result ({written}/{size})"
        )));
    }
    // SAFETY: the exact-size query initialized the ABI struct. Its scalar
    // fields and libc's vnode_info_path also permit their initial zero values.
    let info = unsafe { info.assume_init() };
    let metadata = executable.metadata()?;
    let vnode = info.vnode.vip_vi.vi_stat;
    // Kernel queries may return the next region or a region without a vnode.
    // Neither proves the identity of the main executable. Compare the opened
    // handle, then hash that same handle so pathname replacements cannot race.
    if address < info.region.address
        || address - info.region.address >= info.region.size
        || vnode.vst_ino == 0
        || !metadata.is_file()
        || metadata.dev() as u32 != vnode.vst_dev
        || metadata.ino() != vnode.vst_ino
    {
        return Err(io::Error::other(
            "running_image_identity_unverified: opened executable does not match loaded image vnode",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn review80_r2_macos_region_binding_matches_native_sdk_abi() {
        assert_eq!(std::mem::size_of::<MacosRegionInfo>(), 96);
        assert_eq!(std::mem::offset_of!(MacosRegionInfo, address), 80);
        assert_eq!(std::mem::offset_of!(MacosRegionInfo, size), 88);
        assert_eq!(std::mem::size_of::<libc::vnode_info_path>(), 1176);
        assert_eq!(std::mem::size_of::<MacosRegionWithPathInfo>(), 1272);
        assert_eq!(std::mem::offset_of!(MacosRegionWithPathInfo, vnode), 96);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn review80_r2_macos_replaced_executable_is_unavailable() {
        const CHILD: &str = "PRVIEW_TEST_MACOS_REPLACED_EXECUTABLE_HASH";
        if std::env::var_os(CHILD).is_some() {
            let installed = std::env::current_exe().unwrap();
            assert_eq!(
                installed.file_name().and_then(|name| name.to_str()),
                Some("owned-macos-identity-test")
            );
            let retired = installed.with_extension("retired");
            std::fs::rename(&installed, &retired).unwrap();
            std::fs::write(&installed, b"replacement executable bytes").unwrap();
            // First identity collection occurs after replacement of the
            // installed path; the old executing vnode remains at retired.
            let identity = current();
            assert_eq!(identity.binary_build_id_status, "unavailable");
            assert!(identity.binary_build_id.is_none());
            assert!(
                identity
                    .binary_build_id_reason
                    .as_deref()
                    .unwrap()
                    .contains("running_image_identity_unverified:")
            );
            assert!(std::ptr::eq(identity, current()));
            return;
        }
        let owned = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let installed = owned.path().join("owned-macos-identity-test");
        std::fs::copy(&executable, &installed).unwrap();
        let output = std::process::Command::new(&installed)
            .args([
                "--exact",
                "artifacts::generator_identity::tests::review80_r2_macos_replaced_executable_is_unavailable",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

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
