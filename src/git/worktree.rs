//! Ephemeral git worktree support for remote check verification
//!
//! Creates a detached git worktree at a specific commit, with
//! local dependencies (node_modules, .venv) symlinked to preserve local caches.

use super::cmd::git_cmd;
#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

#[cfg(unix)]
const BORROWED_LINKS_MANIFEST: &str = ".prview-borrowed-links";

const MAX_SYMLINK_RESOLUTIONS: usize = 40;

#[cfg(unix)]
const MAX_JS_SHIM_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitPathResolution {
    Missing,
    Runnable,
    /// The target owns the requested entry or a symlink prefix, but Git alone
    /// cannot prove a runnable final file. The finished snapshot must resolve
    /// it and fail before spawn when it remains a directory/broken/non-file.
    Unresolved,
}

/// Classify `relative_path` in `commit`, following repository-relative
/// symlinks as far as the Git tree can prove.
///
/// `git2::Tree::get_path` deliberately does not traverse a blob stored with
/// mode `120000`, so a target-owned `.bin -> bin-owned` needs this small
/// resolver before eligibility can truthfully say whether the target contains
/// `node_modules/.bin/<tool>`. An absolute target-owned symlink is an existing
/// tool candidate too, but its final host path cannot be resolved from the Git
/// tree; admit it here and let the finished-snapshot resolver either reject a
/// missing/non-file target or classify the external bytes as borrowed.
pub(crate) fn commit_path_resolution(
    repo_root: &Path,
    commit: &str,
    relative_path: &Path,
) -> CommitPathResolution {
    let Ok(repo) = git2::Repository::discover(repo_root) else {
        return CommitPathResolution::Unresolved;
    };
    let Ok(commit) = repo
        .revparse_single(commit)
        .and_then(|object| object.peel_to_commit())
    else {
        return CommitPathResolution::Unresolved;
    };
    let Ok(tree) = commit.tree() else {
        return CommitPathResolution::Unresolved;
    };
    let Some(mut pending) = relative_components(relative_path) else {
        return CommitPathResolution::Unresolved;
    };
    let mut resolved = PathBuf::new();
    let mut followed_symlink = false;

    for _ in 0..MAX_SYMLINK_RESOLUTIONS {
        let Some(component) = pending.pop_front() else {
            return CommitPathResolution::Unresolved;
        };
        resolved.push(component);
        let Ok(entry) = tree.get_path(&resolved) else {
            return if followed_symlink {
                CommitPathResolution::Unresolved
            } else {
                CommitPathResolution::Missing
            };
        };

        if entry.filemode() == 0o120000 {
            followed_symlink = true;
            let Ok(object) = entry.to_object(&repo) else {
                return CommitPathResolution::Unresolved;
            };
            let Some(blob) = object.as_blob() else {
                return CommitPathResolution::Unresolved;
            };
            let Ok(target) = std::str::from_utf8(blob.content()) else {
                return CommitPathResolution::Unresolved;
            };
            if Path::new(target).is_absolute() {
                return CommitPathResolution::Runnable;
            }
            let Some(next) = resolved_symlink_path(&resolved, Path::new(target), &pending) else {
                return CommitPathResolution::Unresolved;
            };
            let Some(next_components) = relative_components(&next) else {
                return CommitPathResolution::Unresolved;
            };
            resolved.clear();
            pending = next_components;
            continue;
        }

        if pending.is_empty() {
            return if entry.kind() == Some(git2::ObjectType::Blob) {
                CommitPathResolution::Runnable
            } else {
                CommitPathResolution::Unresolved
            };
        }
        if entry.kind() != Some(git2::ObjectType::Tree) {
            return CommitPathResolution::Unresolved;
        }
    }

    CommitPathResolution::Unresolved
}

fn relative_components(path: &Path) -> Option<VecDeque<std::ffi::OsString>> {
    let mut normalized = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(component) => normalized.push(component.to_os_string()),
            std::path::Component::ParentDir => {
                normalized.pop()?;
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return None,
        }
    }
    Some(normalized.into())
}

fn resolved_symlink_path(
    symlink_path: &Path,
    target: &Path,
    tail: &VecDeque<std::ffi::OsString>,
) -> Option<PathBuf> {
    if target.is_absolute() {
        return None;
    }
    let mut combined = symlink_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(target);
    combined.extend(tail.iter());
    let normalized = relative_components(&combined)?;
    Some(normalized.into_iter().collect())
}

/// What a static proof could establish about the executable closure a check
/// resolves through this snapshot.
///
/// Three states, not two, because "proved to read borrowed bytes" and "could
/// not be proved either way" are different facts. Collapsing them makes
/// `Borrowed` a bag for everything the resolver cannot read — and every real
/// `npm`/`pnpm`/`yarn` shim is something it cannot read, so the bag swallows
/// the ordinary case. An unrecognised grammar is the ABSENCE of evidence, in
/// both directions; it is not evidence that ambient bytes ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClosureProof {
    /// Every statically visible byte of the closure is target-owned: the
    /// scanned bytes are exactly the reviewed commit's.
    TargetOnly,
    /// Positive evidence that the closure consumes bytes from outside the
    /// snapshot — a link prview itself created, or a canonical identity that
    /// resolves outside the snapshot root.
    Borrowed,
    /// The snapshot is genuine (its source is exactly the reviewed commit), but
    /// the dependency chain the tool would execute is opaque to a static proof.
    /// Neither `TargetOnly` nor `Borrowed` is earned, so neither is claimed.
    Unproven,
}

/// What resolving `relative_path` in this snapshot proves about the bytes it
/// consumes.
///
/// The manifest lives beside the worktree, inside the same temporary directory,
/// so target-owned files cannot forge or collide with it and it disappears with
/// the snapshot. Its paths identify links created by prview, but comparison is
/// made through canonical filesystem identity rather than byte-exact spelling:
/// on a case-insensitive filesystem `ESLint` and `eslint` may name the same
/// created link. Canonical resolution also exposes target-owned absolute
/// symlinks that escape the snapshot.
///
/// A package-manager wrapper is not the final payload. Only strict, anchored
/// wrapper grammars contribute payload paths; comments, strings, and arbitrary
/// `require(` substrings are not evidence. A script whose closure cannot be
/// proved from one of those grammars is [`ClosureProof::Unproven`] — not
/// borrowed, because nothing here observed a borrow.
///
/// Positive borrow evidence is settled BEFORE the unproved case. The two are
/// not competing guesses: one is a proof and the other is its absence, so the
/// proof publishes even when the closure analysis also came up short. A
/// target-owned absolute symlink into the operator's tree is exactly that
/// shape — its content may be unreadable while its canonical identity is
/// plainly outside the snapshot.
#[cfg(unix)]
pub(crate) fn path_uses_prview_borrow(snapshot_root: &Path, relative_path: &Path) -> ClosureProof {
    let Some(parent) = snapshot_root.parent() else {
        return ClosureProof::TargetOnly;
    };
    let encoded = std::fs::read(parent.join(BORROWED_LINKS_MANIFEST)).unwrap_or_default();
    use std::os::unix::ffi::OsStringExt as _;
    let borrowed: Vec<PathBuf> = encoded
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(std::ffi::OsString::from_vec(path.to_vec())))
        .collect();
    if borrowed
        .iter()
        .any(|created| relative_path.starts_with(created) || created.starts_with(relative_path))
    {
        return ClosureProof::Borrowed;
    }
    let consumed = consumed_paths(snapshot_root, relative_path);
    if consumed.package_wrapper
        && borrowed.iter().any(|created| {
            created.starts_with("node_modules") || Path::new("node_modules").starts_with(created)
        })
    {
        return ClosureProof::Borrowed;
    }
    if consumed
        .paths
        .iter()
        .any(|path| path_is_external_or_borrowed(snapshot_root, path, borrowed.as_slice()))
    {
        return ClosureProof::Borrowed;
    }
    if !consumed.closure_proven {
        return ClosureProof::Unproven;
    }
    ClosureProof::TargetOnly
}

#[cfg(unix)]
fn path_is_external_or_borrowed(snapshot_root: &Path, path: &Path, borrowed: &[PathBuf]) -> bool {
    let Ok(snapshot_identity) = std::fs::canonicalize(snapshot_root) else {
        return false;
    };
    let Ok(path_identity) = std::fs::canonicalize(path) else {
        return false;
    };

    if !path_identity.starts_with(&snapshot_identity) {
        return true;
    }

    borrowed.iter().any(|relative| {
        std::fs::canonicalize(snapshot_root.join(relative))
            .is_ok_and(|identity| path_identity.starts_with(identity))
    })
}

/// Whether the kernel's own image loader CLAIMS `path` as an executable for
/// this platform and this architecture.
///
/// This is the one positive proof that no interpreter indirection exists, and
/// the bar has to be "the loader claims the file", never "the file opens with a
/// magic we recognise". prview spawns through `Command`, hence `execvp`, and
/// POSIX requires `execvp` to retry a file through `/bin/sh` on exactly one
/// condition: `ENOEXEC` — no loader recognised the image as its own.
///
/// What that buys is narrower than "a header that parses", and the difference
/// is the proof. Validating a header is not the same as predicting the
/// loader's verdict: on macOS the kernel picks the fat slice with the highest
/// `cpusubtype` GRADE, so an image in which merely SOME host slice validates
/// can still be handed to `/bin/sh` through the slice the grader actually
/// picks — measured on macOS/arm64, a real `arm64` binary beside a bogus
/// `arm64e` entry fell through to the shell (exit 126), as did every
/// `FAT_MAGIC_64` image, real slice included. So the proof holds only over an
/// acceptance set narrowed to shapes a kernel probe measured with ZERO
/// fallback: a fully validated thin header, or a 32-bit fat image in which
/// EVERY host-`cputype` entry is itself claimable and at least one exists —
/// whichever entry the grader picks is then one this proof read.
///
/// Inside that set a file has two futures, and both keep the closure
/// target-only: the kernel executes the committed bytes, or the loader rejects
/// the image outright (`EBADMACHO`, `EBADARCH`, `EBADEXEC`) with no shell in
/// the path. Outside it there is a third one, and it is what this function
/// exists to prevent. Nothing here leans on the shell declining to interpret
/// accepted bytes: bash refuses a file carrying a NUL before the first
/// newline, which every accepted macOS header happens to carry, but that is an
/// accident of the format rather than a defence this code chose — `dash`, the
/// `/bin/sh` of the Linux hosts this runs on, makes no such promise.
///
/// A recognised PREFIX buys neither future. A
/// host-format magic on a truncated or non-executable header is claimed by
/// nothing, falls through to `ENOEXEC`, and `/bin/sh` then runs the remaining
/// bytes as a script with the full unbounded indirection a shell allows —
/// measured on macOS/arm64: a `CF FA ED FE` prefix followed by a shell line
/// executes that line, while the same magic carrying a complete `MH_EXECUTE`
/// header for the host `cputype` fails `EBADMACHO` without a fallback.
///
/// Everything the loader does not claim is [`ClosureProof::Unproven`]. That
/// includes formats this platform has no loader for at all: ELF on macOS and
/// Mach-O on Linux are recognisable, not executable, so they are the fallback
/// case rather than a proof.
///
/// The read is bounded — a header and, for a universal binary, one slice
/// header — so the proof costs the same on a 4 KiB launcher and a 400 MiB
/// toolchain. That is why it may run before the script size bound: nothing here
/// reads content, and file size neither strengthens nor weakens the claim.
#[cfg(unix)]
fn kernel_claims_native_executable(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    platform_header_claims_executable(&file, metadata.len())
}

/// Reads exactly `buffer.len()` bytes at `offset`, or reports failure. A short
/// file is a failed proof, never a partial one.
#[cfg(unix)]
fn read_header_at(file: &std::fs::File, offset: u64, buffer: &mut [u8]) -> bool {
    use std::os::unix::fs::FileExt as _;

    file.read_exact_at(buffer, offset).is_ok()
}

/// `CPU_TYPE_ARM64` / `CPU_TYPE_X86_64` — `CPU_ARCH_ABI64 | CPU_TYPE_{ARM,X86}`.
/// An architecture with no entry here has no proof path, because a `cputype`
/// this build cannot name cannot be compared against the running kernel.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const HOST_MACH_CPU_TYPE: Option<u32> = Some(0x0100_0000 | 12);
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const HOST_MACH_CPU_TYPE: Option<u32> = Some(0x0100_0000 | 7);
#[cfg(all(
    target_os = "macos",
    not(any(target_arch = "aarch64", target_arch = "x86_64"))
))]
const HOST_MACH_CPU_TYPE: Option<u32> = None;

/// macOS: only Mach-O, only this host's `cputype`, only a complete header.
///
/// Thin magics are read in host byte order on purpose. The byte-swapped forms
/// (`MH_CIGAM`, `MH_CIGAM_64`) describe an image for a machine of the opposite
/// endianness, which this kernel never executes; the same goes for the
/// little-endian fat forms (`FAT_CIGAM`, `FAT_CIGAM_64`), since the fat header
/// is big-endian by definition. Recognising them would only widen the set of
/// files that are named and not claimed.
#[cfg(target_os = "macos")]
fn platform_header_claims_executable(file: &std::fs::File, length: u64) -> bool {
    const MH_MAGIC: u32 = 0xFEED_FACE;
    const MH_MAGIC_64: u32 = 0xFEED_FACF;
    const FAT_MAGIC: u32 = 0xCAFE_BABE;
    const FAT_MAGIC_64: u32 = 0xCAFE_BABF;

    let Some(host_cpu_type) = HOST_MACH_CPU_TYPE else {
        return false;
    };
    let mut magic = [0u8; 4];
    if !read_header_at(file, 0, &mut magic) {
        return false;
    }
    match u32::from_le_bytes(magic) {
        MH_MAGIC_64 => mach_header_claims_executable(file, 0, length, host_cpu_type, true),
        MH_MAGIC => mach_header_claims_executable(file, 0, length, host_cpu_type, false),
        _ => match u32::from_be_bytes(magic) {
            FAT_MAGIC => fat_header_claims_executable(file, length, host_cpu_type),
            // Recognised and refused on purpose. `exec` does not claim a
            // 64-bit fat image at all on this platform: measured on
            // macOS/arm64, a `FAT_MAGIC_64` file carrying a real, working host
            // slice still returned `ENOEXEC` and ran under `/bin/sh`. Naming
            // the magic here is documentation of that measurement; treating it
            // as evidence would certify exactly the files the loader drops.
            FAT_MAGIC_64 => false,
            _ => false,
        },
    }
}

/// A complete `mach_header`/`mach_header_64` at `slice_offset`, for an image of
/// `slice_length` bytes.
///
/// `filetype` is load-bearing rather than decorative: a header that is valid in
/// every other respect but says `MH_DYLIB` is NOT claimed, returns `ENOEXEC`,
/// and hands the file to `/bin/sh` — measured on macOS/arm64. The `sizeofcmds`
/// bound is the weaker, conservative half: an overflowing load-command table
/// fails `EBADMACHO` without a fallback, so checking it only narrows an already
/// safe acceptance set.
#[cfg(target_os = "macos")]
fn mach_header_claims_executable(
    file: &std::fs::File,
    slice_offset: u64,
    slice_length: u64,
    host_cpu_type: u32,
    wide: bool,
) -> bool {
    /// Smallest `load_command`: `cmd` plus `cmdsize`.
    const LOAD_COMMAND_MIN_BYTES: u64 = 8;
    const MH_EXECUTE: u32 = 2;

    let header_bytes: u64 = if wide { 32 } else { 28 };
    if slice_length < header_bytes {
        return false;
    }
    // Every field this proof reads lives in the 24 bytes both layouts share.
    let mut header = [0u8; 24];
    if !read_header_at(file, slice_offset, &mut header) {
        return false;
    }
    let field = |offset: usize| -> u32 {
        u32::from_le_bytes([
            header[offset],
            header[offset + 1],
            header[offset + 2],
            header[offset + 3],
        ])
    };
    if field(4) != host_cpu_type || field(12) != MH_EXECUTE {
        return false;
    }
    let commands = u64::from(field(16));
    let commands_bytes = u64::from(field(20));
    if commands == 0 || commands_bytes == 0 {
        return false;
    }
    if commands.saturating_mul(LOAD_COMMAND_MIN_BYTES) > commands_bytes {
        return false;
    }
    header_bytes.saturating_add(commands_bytes) <= slice_length
}

/// A universal binary is claimed only when EVERY `fat_arch` entry carrying the
/// host `cputype` is itself claimable, and at least one such entry exists.
///
/// "Some entry validates" is the wrong rule, and the difference is measurable.
/// XNU does not take the first matching entry: it grades the candidates and
/// picks the best one, with `arm64e` outranking `arm64` (and `x86_64h`
/// outranking `x86_64`) under one and the same `cputype`. A real `arm64`
/// binary in slice #1 beside an `arm64e` entry pointing at shell text is
/// therefore accepted on the strongest possible evidence and still executed by
/// `/bin/sh` — measured on macOS/arm64, exit 126. Since the grading order is
/// the kernel's and not this code's to reproduce, the sound rule is to require
/// ALL of them: then the entry the grader picks is one this proof validated,
/// whichever it is. Rejecting the whole file on one unclaimable host entry
/// costs nothing real — Apple ships one host slice per image.
///
/// Validating the inner header is load-bearing on top of that. A fat header
/// advertising a host slice whose bytes are not a Mach-O image is NOT claimed,
/// returns `ENOEXEC`, and reaches `/bin/sh` — measured on macOS/arm64.
/// Matching the `cpusubtype` is deliberately NOT required: Apple ships
/// `/bin/ls` as an `arm64e` slice that a plain `arm64` host executes, so a
/// subtype match would reject the platform's own binaries while the all-host
/// rule above already covers the grade it encodes.
#[cfg(target_os = "macos")]
fn fat_header_claims_executable(file: &std::fs::File, length: u64, host_cpu_type: u32) -> bool {
    const MH_MAGIC: u32 = 0xFEED_FACE;
    const MH_MAGIC_64: u32 = 0xFEED_FACF;
    const FAT_HEADER_BYTES: u64 = 8;
    /// One `fat_arch`: `cputype`, `cpusubtype`, `offset`, `size`, `align`.
    /// Only the 32-bit table is read, because `FAT_MAGIC_64` is never a proof.
    const FAT_ARCHITECTURE_BYTES: u64 = 20;
    /// Sanity ceiling on `nfat_arch`; Apple ships a handful, never thousands.
    const MAX_FAT_ARCHITECTURES: u32 = 64;

    let mut count = [0u8; 4];
    if !read_header_at(file, 4, &mut count) {
        return false;
    }
    let architectures = u32::from_be_bytes(count);
    if architectures == 0 || architectures > MAX_FAT_ARCHITECTURES {
        return false;
    }
    let table_bytes = u64::from(architectures).saturating_mul(FAT_ARCHITECTURE_BYTES);
    if FAT_HEADER_BYTES.saturating_add(table_bytes) > length {
        return false;
    }
    let mut host_entry_seen = false;
    for index in 0..u64::from(architectures) {
        let entry = FAT_HEADER_BYTES + index * FAT_ARCHITECTURE_BYTES;
        let mut cpu_type = [0u8; 4];
        if !read_header_at(file, entry, &mut cpu_type) {
            return false;
        }
        if u32::from_be_bytes(cpu_type) != host_cpu_type {
            continue;
        }
        // From here every failure is the whole file's failure: this entry is a
        // candidate the grader may prefer, so an unclaimable one is a shell
        // fallback waiting to happen, not an entry to skip past.
        host_entry_seen = true;
        let mut extent = [0u8; 8];
        if !read_header_at(file, entry + 8, &mut extent) {
            return false;
        }
        let offset = u64::from(u32::from_be_bytes(
            extent[..4].try_into().expect("four bytes"),
        ));
        let size = u64::from(u32::from_be_bytes(
            extent[4..].try_into().expect("four bytes"),
        ));
        if offset.saturating_add(size) > length {
            return false;
        }
        let mut slice_magic = [0u8; 4];
        if !read_header_at(file, offset, &mut slice_magic) {
            return false;
        }
        let claimed = match u32::from_le_bytes(slice_magic) {
            MH_MAGIC_64 => mach_header_claims_executable(file, offset, size, host_cpu_type, true),
            MH_MAGIC => mach_header_claims_executable(file, offset, size, host_cpu_type, false),
            _ => false,
        };
        if !claimed {
            return false;
        }
    }
    host_entry_seen
}

/// `EM_X86_64` / `EM_AARCH64`. As on macOS, an architecture with no entry has
/// no proof path.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const HOST_ELF_MACHINE: Option<u16> = Some(62);
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const HOST_ELF_MACHINE: Option<u16> = Some(183);
#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
const HOST_ELF_MACHINE: Option<u16> = None;

/// Linux: only ELF, only this host's `e_machine`, only a complete header.
///
/// `e_machine` is the load-bearing field here — `binfmt_elf` rejects a foreign
/// machine with `ENOEXEC`, which is precisely the code that reaches `/bin/sh`,
/// so a Mach-O (or a cross-compiled ELF) is a fallback vector rather than a
/// proof. `ET_DYN` is accepted beside `ET_EXEC` because every PIE executable —
/// which is what a modern toolchain emits by default — is `ET_DYN`, and the
/// kernel claims both.
///
/// This branch is narrower than `binfmt_elf`'s full triage and deliberately
/// stays on the conservative side of it, because the fields it does NOT model
/// are all further `-ENOEXEC` exits: an image with no `PT_LOAD` segment, or a
/// `PT_INTERP` whose length fails the loader's bounds, passes this header
/// check and is still dropped to `/bin/sh`. The set accepted here is not
/// proved shell-free by measurement the way the macOS set is — there was no
/// Linux host in the round that wrote it — so every field it does model is
/// matched exactly rather than loosely: `e_phentsize` for equality, and the
/// program-header table against the same `56 * e_phnum` product the kernel
/// computes, BOTH halves of that bound. An earlier round asserted that
/// exactness while `e_phnum` was still bounded from below only, which left
/// `e_phnum = 1171` claimed here and `-ENOEXEC` in the kernel.
#[cfg(target_os = "linux")]
fn platform_header_claims_executable(file: &std::fs::File, length: u64) -> bool {
    const ELF_HEADER_BYTES: u64 = 64;
    const ELFCLASS64: u8 = 2;
    const ELFDATA2LSB: u8 = 1;
    const EV_CURRENT: u8 = 1;
    const ET_EXEC: u16 = 2;
    const ET_DYN: u16 = 3;
    /// `sizeof(Elf64_Phdr)`, compared for EQUALITY rather than as a minimum.
    /// `load_elf_phdrs()` demands the exact size and returns NULL otherwise,
    /// which `load_elf_binary()` turns into `-ENOEXEC` — the one code that
    /// reaches `/bin/sh`. A larger entry size is a header the validator would
    /// pass and the kernel would drop.
    const PROGRAM_HEADER_BYTES: u16 = 56;
    /// The kernel's bound on the WHOLE program-header table, reproduced as the
    /// same arithmetic rather than as a count. `load_elf_phdrs()` computes
    /// `sizeof(struct elf_phdr) * e_phnum` and leaves through the same
    /// `goto out` as the entry size above — hence the same `-ENOEXEC`, hence
    /// `/bin/sh` — when that product is `0` or greater than 65536. With the
    /// 56-byte entry demanded above, the largest claimable count is 1170, so
    /// `e_phnum = 1171` is the first header a looser check hands to the shell.
    ///
    /// The lower half of that condition (`size == 0`) is why a count of zero is
    /// refused here: a table of no segments is not a cautious reading of a
    /// claimable image, it is an image the loader itself drops.
    ///
    /// `PN_XNUM` (`0xffff`, "the real count lives in section 0's `sh_info`")
    /// needs no arm of its own. `binfmt_elf` implements extended numbering only
    /// where it WRITES a core dump; the load path just multiplies, and
    /// `56 * 0xffff` is fifty-odd times past the bound — so this arithmetic
    /// already refuses that header exactly as the kernel does.
    const PROGRAM_HEADER_TABLE_BYTES_MAX: u64 = 65536;

    let Some(host_machine) = HOST_ELF_MACHINE else {
        return false;
    };
    if length < ELF_HEADER_BYTES {
        return false;
    }
    let mut header = [0u8; 64];
    if !read_header_at(file, 0, &mut header) {
        return false;
    }
    if header[..4] != [0x7F, b'E', b'L', b'F'] {
        return false;
    }
    if header[4] != ELFCLASS64 || header[5] != ELFDATA2LSB || header[6] != EV_CURRENT {
        return false;
    }
    let file_type = u16::from_le_bytes([header[16], header[17]]);
    if file_type != ET_EXEC && file_type != ET_DYN {
        return false;
    }
    if u16::from_le_bytes([header[18], header[19]]) != host_machine {
        return false;
    }
    if u32::from_le_bytes([header[20], header[21], header[22], header[23]]) != u32::from(EV_CURRENT)
    {
        return false;
    }
    let program_header_offset = u64::from_le_bytes(header[32..40].try_into().expect("eight bytes"));
    let header_size = u16::from_le_bytes([header[52], header[53]]);
    let program_header_size = u16::from_le_bytes([header[54], header[55]]);
    let program_headers = u16::from_le_bytes([header[56], header[57]]);
    // Widened to `u64` BEFORE the multiply, because the product of two `u16`
    // fields leaves `u16` long before it reaches the bound, and a wrapped
    // product reads as a small, legal table — the precise false positive this
    // comparison exists to refuse.
    let table_bytes = u64::from(program_headers).saturating_mul(u64::from(program_header_size));
    if u64::from(header_size) != ELF_HEADER_BYTES
        || program_header_size != PROGRAM_HEADER_BYTES
        || table_bytes == 0
        || table_bytes > PROGRAM_HEADER_TABLE_BYTES_MAX
        || program_header_offset == 0
    {
        return false;
    }
    program_header_offset.saturating_add(table_bytes) <= length
}

/// Every other Unix: no proof path, so no file is ever proved target-only by
/// its header. The conservative answer is the only honest one — claiming a
/// closure this build cannot reason about is the failure mode this whole
/// function exists to prevent.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn platform_header_claims_executable(_file: &std::fs::File, _length: u64) -> bool {
    false
}

#[cfg(unix)]
struct ConsumedPaths {
    paths: Vec<PathBuf>,
    package_wrapper: bool,
    closure_proven: bool,
}

#[cfg(unix)]
fn consumed_paths(snapshot_root: &Path, relative_path: &Path) -> ConsumedPaths {
    let invocation = snapshot_root.join(relative_path);
    let mut consumed = vec![invocation.clone()];
    let Ok(metadata) = std::fs::metadata(&invocation) else {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            // Nothing can execute when final resolution fails. The check layer
            // reports that failure before spawn; provenance still describes the
            // target snapshot rather than inventing a borrowed execution.
            closure_proven: true,
        };
    };
    if !metadata.is_file() {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: true,
        };
    }
    use std::os::unix::fs::PermissionsExt as _;
    if metadata.permissions().mode() & 0o111 == 0 {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: true,
        };
    }
    // A header the platform's loader claims is the ONE positive proof that no
    // script indirection exists: the kernel either executes these bytes or
    // refuses the image, and neither path reaches an interpreter.
    //
    // Checked BEFORE the size bound, because the proof reads a bounded header
    // rather than content — it neither gains nor loses strength with file size,
    // and real compiled tools are routinely larger than a shim bound (macOS
    // ships `/bin/ls` at ~150 KiB). The bound guards CONTENT analysis, which
    // this does not perform. An oversized file that fails the header proof
    // still falls through to that bound and stays unproved.
    if kernel_claims_native_executable(&invocation) {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: true,
        };
    }
    if metadata.len() > MAX_JS_SHIM_BYTES {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: false,
        };
    }
    let Ok(bytes) = std::fs::read(&invocation) else {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: false,
        };
    };
    // No `#!` and no claimed platform header proves NOTHING about the closure,
    // and it must never be read as "native binary". prview spawns through
    // `Command`, hence `execvp`, and POSIX requires `execvp` to retry an
    // `ENOEXEC` file through `/bin/sh` — so this file is a shell script whose
    // interpreter was chosen for it, with the full, unbounded indirection a
    // shell allows. The header proof above is the only thing that rules it out.
    if !bytes.starts_with(b"#!") {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: false,
        };
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: false,
        };
    };
    let Some(bin_dir) = invocation.parent() else {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: false,
        };
    };

    let active_lines: Vec<&str> = text
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    let package_wrapper = is_known_pnpm_shell_wrapper(text, &active_lines);
    if !package_wrapper && !is_proved_direct_shell_script(text, &active_lines) {
        return ConsumedPaths {
            paths: consumed,
            package_wrapper: false,
            closure_proven: false,
        };
    }

    if package_wrapper {
        let basedir =
            regex::Regex::new(r#"\$basedir/([^\"'\s;|&)]+)"#).expect("static basedir regex");
        for line in &active_lines {
            for capture in basedir.captures_iter(line) {
                let Some(relative) = capture.get(1) else {
                    continue;
                };
                let candidate = bin_dir.join(relative.as_str());
                if candidate.exists() {
                    consumed.push(candidate);
                }
            }
        }
    }

    consumed.sort();
    consumed.dedup();
    ConsumedPaths {
        paths: consumed,
        package_wrapper,
        closure_proven: true,
    }
}

#[cfg(unix)]
fn is_known_pnpm_shell_wrapper(text: &str, active_lines: &[&str]) -> bool {
    let Some(shebang) = text.lines().next() else {
        return false;
    };
    if !matches!(shebang.trim(), "#!/bin/sh" | "#!/usr/bin/env sh") {
        return false;
    }
    if active_lines.len() != 2 || active_lines[0] != "basedir=$(dirname \"$0\")" {
        return false;
    }
    regex::Regex::new(r#"^exec node \"\$basedir/[^\"]+\" \"\$@\"$"#)
        .expect("static pnpm wrapper regex")
        .is_match(active_lines[1])
}

#[cfg(unix)]
fn is_proved_direct_shell_script(text: &str, active_lines: &[&str]) -> bool {
    let Some(shebang) = text.lines().next() else {
        return false;
    };
    if !matches!(
        shebang.trim(),
        "#!/bin/sh" | "#!/bin/bash" | "#!/usr/bin/env sh" | "#!/usr/bin/env bash"
    ) {
        return false;
    }
    let literal_output = regex::Regex::new(r#"^(?:printf|echo) '[^']*'$"#)
        .expect("static direct shell output regex");
    active_lines.iter().all(|line| {
        line == &":"
            || line == &"true"
            || line == &"false"
            || literal_output.is_match(line)
            || line == &"exit"
            || line
                .strip_prefix("exit ")
                .is_some_and(|code| !code.is_empty() && code.chars().all(|ch| ch.is_ascii_digit()))
    })
}

#[cfg(not(unix))]
pub(crate) fn path_uses_prview_borrow(
    _snapshot_root: &Path,
    _relative_path: &Path,
) -> ClosureProof {
    ClosureProof::TargetOnly
}

#[cfg(unix)]
fn create_borrowed_link(
    source: &Path,
    exposed: &Path,
    snapshot_root: &Path,
    borrowed_links: &mut Vec<PathBuf>,
) -> Result<()> {
    std::os::unix::fs::symlink(source, exposed).with_context(|| {
        format!(
            "failed to expose {} as borrowed dependency {}",
            source.display(),
            exposed.display()
        )
    })?;
    borrowed_links.push(
        exposed
            .strip_prefix(snapshot_root)
            .context("borrowed dependency escaped snapshot root")?
            .to_path_buf(),
    );
    Ok(())
}

#[cfg(unix)]
fn link_missing_entries(
    ambient: &Path,
    snapshot: &Path,
    snapshot_root: &Path,
    borrowed_links: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in std::fs::read_dir(ambient)
        .with_context(|| format!("failed to enumerate ambient {}", ambient.display()))?
    {
        let entry = entry.with_context(|| {
            format!("failed to read an entry from ambient {}", ambient.display())
        })?;
        let borrowed = entry.path();
        let exposed = snapshot.join(entry.file_name());
        match std::fs::symlink_metadata(&exposed) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect target-owned dependency path {}",
                        exposed.display()
                    )
                });
            }
        }
        create_borrowed_link(&borrowed, &exposed, snapshot_root, borrowed_links)?;
    }
    Ok(())
}

#[cfg(unix)]
fn write_borrowed_links_manifest(temp_root: &Path, borrowed_links: &mut [PathBuf]) -> Result<()> {
    if borrowed_links.is_empty() {
        return Ok(());
    }
    borrowed_links.sort();
    use std::os::unix::ffi::OsStrExt as _;
    let mut encoded = Vec::new();
    for path in borrowed_links {
        encoded.extend_from_slice(path.as_os_str().as_bytes());
        encoded.push(0);
    }
    std::fs::write(temp_root.join(BORROWED_LINKS_MANIFEST), encoded)
        .context("failed to record snapshot borrowed-link provenance")
}

/// Roll back one exact worktree registration without spawning another child.
///
/// This is the cancellation backstop for the interval after `git worktree add`
/// has written its common-dir metadata but before it returns to prview. It is
/// deliberately path-scoped: a review must never prune another worktree merely
/// because both registrations live in the same repository.
fn prune_registered_worktree(repo_root: &Path, worktree_path: &Path) -> Result<bool> {
    let repo = git2::Repository::open(repo_root)?;
    let expected = comparable_worktree_path(worktree_path);
    for name in repo.worktrees()?.iter().flatten() {
        let Ok(worktree) = repo.find_worktree(name) else {
            continue;
        };
        if comparable_worktree_path(worktree.path()) != expected {
            continue;
        }
        let mut options = git2::WorktreePruneOptions::new();
        options.valid(true).locked(true).working_tree(false);
        worktree.prune(Some(&mut options))?;
        return Ok(true);
    }
    Ok(false)
}

fn comparable_worktree_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
            .unwrap_or_else(|| path.to_path_buf())
    })
}

struct WorktreeRegistrationRollback {
    repo_root: PathBuf,
    worktree_path: PathBuf,
    armed: bool,
}

impl WorktreeRegistrationRollback {
    fn new(repo_root: &Path, worktree_path: &Path) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            worktree_path: worktree_path.to_path_buf(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WorktreeRegistrationRollback {
    fn drop(&mut self) {
        if self.armed {
            let _ = prune_registered_worktree(&self.repo_root, &self.worktree_path);
        }
    }
}

/// An ephemeral detached `git worktree` checked out at a specific commit. Kept
/// alive for the duration of a scan; the worktree is deregistered and its files
/// removed on drop, on every path (scan success or error).
pub struct WorktreeSnapshot {
    pub repo_root: PathBuf,
    pub worktree_path: PathBuf,
    pub(crate) original_target_sha: String,
    registered: bool,
    // Owns the enclosing temp dir; dropped after the worktree is deregistered so
    // the directory removal is the backstop for the `git worktree remove` call.
    _tmp: tempfile::TempDir,
}

impl Drop for WorktreeSnapshot {
    fn drop(&mut self) {
        // Drop can run while unwinding an async stage. Never start or wait for a
        // child here: the explicit success path owns governed `git worktree
        // remove`, while this backstop only prunes this exact registration in
        // process. TempDir removes the checkout files after this method returns.
        if self.registered
            && matches!(
                prune_registered_worktree(&self.repo_root, &self.worktree_path),
                Ok(true)
            )
        {
            self.registered = false;
        }
    }
}

impl WorktreeSnapshot {
    /// Deregister this snapshot with an owned Git child or its path-exact,
    /// in-process cancellation fallback.
    pub fn cleanup(&mut self) -> Result<()> {
        if !self.registered {
            return Ok(());
        }
        let mut remove = git_cmd();
        remove
            .args(["worktree", "remove", "--force"])
            .arg(&self.worktree_path)
            .current_dir(&self.repo_root);
        let output = match crate::proc::output_governed_with_timeout(
            remove,
            "git worktree remove",
            std::time::Duration::from_secs(60),
        ) {
            Ok(output) => output,
            Err(error) => {
                let rollback = prune_registered_worktree(&self.repo_root, &self.worktree_path);
                if matches!(&rollback, Ok(true)) {
                    self.registered = false;
                }
                if crate::governor::is_cancellation(&error) {
                    return Err(error);
                }
                return match rollback {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(error.context(
                        "git worktree remove failed and the exact registration was not found for rollback",
                    )),
                    Err(rollback) => Err(error.context(format!(
                        "git worktree remove failed and exact registration rollback failed: {rollback}"
                    ))),
                };
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return match prune_registered_worktree(&self.repo_root, &self.worktree_path) {
                Ok(true) => {
                    self.registered = false;
                    Ok(())
                }
                Ok(false) => anyhow::bail!(
                    "git worktree remove failed: {}; exact registration was not found for rollback",
                    stderr.trim()
                ),
                Err(rollback) => anyhow::bail!(
                    "git worktree remove failed: {}; exact registration rollback also failed: {rollback}",
                    stderr.trim()
                ),
            };
        }
        self.registered = false;
        Ok(())
    }
}

/// Create an ephemeral detached worktree of `commit` under a fresh temp dir.
pub fn create_worktree_snapshot(repo_root: &Path, commit: &str) -> Result<WorktreeSnapshot> {
    // Resolve symbolic inputs once, before creating the checkout. All later
    // integrity comparisons use this immutable source identity.
    let original_target_sha = git2::Repository::discover(repo_root)?
        .revparse_single(commit)?
        .peel_to_commit()?
        .id()
        .to_string();
    let tmp = tempfile::tempdir()?;
    // `git worktree add` wants a path it can create, so point it at a fresh
    // subdirectory of the temp dir rather than the (already-created) temp root.
    let worktree_path = tmp.path().join("snapshot");
    // A reviewed commit is input data, not an operator checkout. In particular,
    // `worktree add` must not execute an inherited/global post-checkout hook:
    // that hook can require ambient tools, mutate the snapshot, or inspect an
    // unrelated checkout. Point Git at an empty, snapshot-owned hook directory
    // without changing the repository's persistent configuration.
    let hooks_path = tmp.path().join("hooks");
    std::fs::create_dir(&hooks_path)?;
    // Armed before the child starts: if cancellation/timeout wins after Git has
    // registered the path but before the command returns, Drop can still undo
    // that exact administrative entry in-process.
    let mut registration_rollback = WorktreeRegistrationRollback::new(repo_root, &worktree_path);

    let mut hooks_config = std::ffi::OsString::from("core.hooksPath=");
    hooks_config.push(&hooks_path);
    let mut command = git_cmd();
    command
        .arg("-c")
        .arg(hooks_config)
        .args(["worktree", "add", "--detach", "--force"])
        .arg(&worktree_path)
        .arg(&original_target_sha)
        .current_dir(repo_root);
    let output = crate::proc::output_governed_with_timeout(
        command,
        "git worktree add",
        std::time::Duration::from_secs(60),
    )?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {}", stderr.trim());
    }

    // Symlink untracked dependencies (node_modules and .venv) to bypass reinstall overhead.
    // A failed borrow is terminal instead of silently leaving a snapshot whose
    // JS eligibility was decided from the operator checkout but whose toolchain
    // is absent at execution time. A target is allowed to commit files under
    // node_modules; preserve that directory and borrow only missing top-level
    // dependency entries in that case. Linking `.bin` alone is insufficient for
    // npm/pnpm shims because they resolve sibling package paths such as
    // `../eslint` from the snapshot.
    #[cfg(unix)]
    {
        let mut borrowed_links = Vec::new();
        let nm = repo_root.join("node_modules");
        let snapshot_nm = worktree_path.join("node_modules");
        if nm.exists() {
            if !snapshot_nm.exists() {
                create_borrowed_link(&nm, &snapshot_nm, &worktree_path, &mut borrowed_links)?;
            } else {
                let ambient_bin = nm.join(".bin");
                let snapshot_bin = snapshot_nm.join(".bin");
                if ambient_bin.exists() {
                    link_missing_entries(&nm, &snapshot_nm, &worktree_path, &mut borrowed_links)?;
                    if std::fs::symlink_metadata(&snapshot_bin)
                        .is_ok_and(|metadata| metadata.file_type().is_dir())
                    {
                        link_missing_entries(
                            &ambient_bin,
                            &snapshot_bin,
                            &worktree_path,
                            &mut borrowed_links,
                        )?;
                    }
                }
            }
        }
        let venv = repo_root.join(".venv");
        let snapshot_venv = worktree_path.join(".venv");
        if venv.exists() && !snapshot_venv.exists() {
            create_borrowed_link(&venv, &snapshot_venv, &worktree_path, &mut borrowed_links)?;
        }
        write_borrowed_links_manifest(tmp.path(), &mut borrowed_links)?;
    }

    registration_rollback.disarm();
    Ok(WorktreeSnapshot {
        repo_root: repo_root.to_path_buf(),
        worktree_path,
        original_target_sha,
        registered: true,
        _tmp: tmp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with_commit() -> (tempfile::TempDir, git2::Repository) {
        let tmp = tempfile::tempdir().expect("repo tempdir");
        let repo = git2::Repository::init(tmp.path()).expect("init repo");
        let mut config = repo.config().expect("repo config");
        config.set_str("user.name", "prview test").expect("name");
        config
            .set_str("user.email", "prview@example.test")
            .expect("email");
        drop(config);
        let tree_id = repo.index().expect("index").write_tree().expect("tree id");
        {
            let tree = repo.find_tree(tree_id).expect("tree");
            let signature = repo.signature().expect("signature");
            repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
                .expect("initial commit");
        }
        (tmp, repo)
    }

    /// A snapshot-shaped directory holding one executable tool, with no
    /// borrowed-links manifest beside it — so nothing but the tool's own bytes
    /// can decide the classification.
    #[cfg(unix)]
    fn snapshot_with_tool(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().expect("snapshot tempdir");
        let root = tmp.path().join("snapshot");
        let bin_dir = root.join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).expect("snapshot bin dir");
        let tool = bin_dir.join("eslint");
        std::fs::write(&tool, bytes).expect("snapshot tool");
        let mut permissions = std::fs::metadata(&tool)
            .expect("tool metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&tool, permissions).expect("executable tool");
        (tmp, root)
    }

    /// Bytes of a real, kernel-executable image for THIS host.
    ///
    /// Synthesising one from a magic prefix is the circularity these fixtures
    /// exist to break: it would prove only that the validator agrees with
    /// itself, and the prefix-shaped file it produces is exactly the one the
    /// kernel hands to `/bin/sh`. A freshly compiled binary is the strongest
    /// available source; a system executable the OS itself ships and runs is
    /// the fallback on a machine with no C compiler.
    #[cfg(unix)]
    fn host_native_executable_bytes(scratch: &Path) -> Option<Vec<u8>> {
        let source = scratch.join("probe.c");
        let binary = scratch.join("probe");
        if std::fs::write(&source, "int main(void) { return 0; }\n").is_ok()
            && std::process::Command::new("cc")
                .arg("-o")
                .arg(&binary)
                .arg(&source)
                .status()
                .is_ok_and(|status| status.success())
            && let Ok(bytes) = std::fs::read(&binary)
        {
            return Some(bytes);
        }
        ["/bin/ls", "/bin/cat", "/bin/sh", "/usr/bin/env"]
            .into_iter()
            .find_map(|candidate| std::fs::read(candidate).ok())
    }

    /// The magic of a format this platform has NO loader for. It is
    /// recognisable and never executable, which is precisely the shape that
    /// returns `ENOEXEC` and reaches `/bin/sh`.
    #[cfg(target_os = "macos")]
    const FOREIGN_FORMAT_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
    #[cfg(target_os = "linux")]
    const FOREIGN_FORMAT_MAGIC: [u8; 4] = [0xCF, 0xFA, 0xED, 0xFE];

    /// The magic of THIS platform's own executable format. On its own, without
    /// the rest of the header, it is still not a proof — the loader claims
    /// nothing from four bytes.
    #[cfg(target_os = "macos")]
    const HOST_FORMAT_MAGIC: [u8; 4] = [0xCF, 0xFA, 0xED, 0xFE];
    #[cfg(target_os = "linux")]
    const HOST_FORMAT_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];

    #[cfg(unix)]
    #[test]
    fn a_real_host_binary_proves_a_target_only_closure() {
        let relative = Path::new("node_modules/.bin/eslint");
        let scratch = tempfile::tempdir().expect("scratch tempdir");
        let Some(native) = host_native_executable_bytes(scratch.path()) else {
            // Nothing on this machine is independently established as
            // kernel-executable, so there is no honest positive fixture. A
            // synthesised one would re-introduce the circularity above, so the
            // case is skipped by name rather than faked.
            eprintln!(
                "skipped: no C compiler and no readable system executable on this host, \
                 so no independently kernel-executable fixture exists"
            );
            return;
        };

        // Real compiled tools run past the shim bound, and that is the point:
        // the header proof reads a bounded window, so size neither grants nor
        // withholds it.
        let (_tmp, root) = snapshot_with_tool(&native);
        assert_eq!(
            path_uses_prview_borrow(&root, relative),
            ClosureProof::TargetOnly,
            "a complete platform header for this host's architecture is claimed by the \
             kernel's loader, so the closure is the invocation itself",
        );
    }

    /// Apple ships `/bin/ls` as a universal binary, so this is the fat path on
    /// real bytes: a `cputype` match inside the `fat_arch` table, and a slice
    /// whose own `mach_header` is claimable.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_universal_binary_with_a_host_slice_proves_a_target_only_closure() {
        let relative = Path::new("node_modules/.bin/eslint");
        let Ok(universal) = std::fs::read("/bin/ls") else {
            eprintln!("skipped: /bin/ls is not readable, so no real universal binary is available");
            return;
        };
        assert_eq!(
            universal.get(..4),
            Some([0xCA, 0xFE, 0xBA, 0xBE].as_slice()),
            "this fixture is only meaningful while /bin/ls is a fat binary",
        );
        let (_tmp, root) = snapshot_with_tool(&universal);
        assert_eq!(
            path_uses_prview_borrow(&root, relative),
            ClosureProof::TargetOnly,
            "a universal binary carrying a claimable slice for the host cputype is \
             executed by the kernel, not by an interpreter",
        );
    }

    /// The `cpusubtype` XNU grades ABOVE the host's baseline under the same
    /// `cputype`: `arm64e` on Apple Silicon, `x86_64h` on Intel. The validator
    /// never compares it — this is the fixture side, where it is what makes a
    /// hostile entry the one the kernel would actually pick.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    const HOST_GRADED_CPU_SUBTYPE: u32 = 2;
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    const HOST_GRADED_CPU_SUBTYPE: u32 = 8;

    /// A 32-bit universal binary over `(cputype, cpusubtype, bytes)` slices,
    /// built from the format rather than from a recognised prefix: an 8-byte
    /// fat header, one 20-byte `fat_arch` per slice, then the slice bodies.
    #[cfg(all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ))]
    fn universal_binary(slices: &[(u32, u32, &[u8])]) -> Vec<u8> {
        const SLICE_ALIGNMENT: usize = 16;

        let table_end = 8 + 20 * slices.len();
        let mut image = 0xCAFE_BABEu32.to_be_bytes().to_vec();
        image.extend_from_slice(
            &u32::try_from(slices.len())
                .expect("slice count")
                .to_be_bytes(),
        );
        let mut body: Vec<u8> = Vec::new();
        for (cpu_type, cpu_subtype, bytes) in slices {
            let offset = (table_end + body.len()).next_multiple_of(SLICE_ALIGNMENT);
            body.resize(offset - table_end, 0);
            body.extend_from_slice(bytes);
            image.extend_from_slice(&cpu_type.to_be_bytes());
            image.extend_from_slice(&cpu_subtype.to_be_bytes());
            image.extend_from_slice(&u32::try_from(offset).expect("slice offset").to_be_bytes());
            image.extend_from_slice(
                &u32::try_from(bytes.len())
                    .expect("slice size")
                    .to_be_bytes(),
            );
            image.extend_from_slice(&4u32.to_be_bytes());
        }
        assert_eq!(
            image.len(),
            table_end,
            "the fat table ends where the bodies begin"
        );
        image.extend_from_slice(&body);
        image
    }

    /// The same slices under `FAT_MAGIC_64`: 8-byte header, 32-byte entries
    /// with 64-bit `offset`/`size` and a trailing `reserved` word.
    #[cfg(all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ))]
    fn fat64_universal_binary(slices: &[(u32, u32, &[u8])]) -> Vec<u8> {
        const SLICE_ALIGNMENT: usize = 16;

        let table_end = 8 + 32 * slices.len();
        let mut image = 0xCAFE_BABFu32.to_be_bytes().to_vec();
        image.extend_from_slice(
            &u32::try_from(slices.len())
                .expect("slice count")
                .to_be_bytes(),
        );
        let mut body: Vec<u8> = Vec::new();
        for (cpu_type, cpu_subtype, bytes) in slices {
            let offset = (table_end + body.len()).next_multiple_of(SLICE_ALIGNMENT);
            body.resize(offset - table_end, 0);
            body.extend_from_slice(bytes);
            image.extend_from_slice(&cpu_type.to_be_bytes());
            image.extend_from_slice(&cpu_subtype.to_be_bytes());
            image.extend_from_slice(&u64::try_from(offset).expect("slice offset").to_be_bytes());
            image.extend_from_slice(
                &u64::try_from(bytes.len())
                    .expect("slice size")
                    .to_be_bytes(),
            );
            image.extend_from_slice(&4u32.to_be_bytes());
            image.extend_from_slice(&0u32.to_be_bytes());
        }
        assert_eq!(
            image.len(),
            table_end,
            "the fat table ends where the bodies begin"
        );
        image.extend_from_slice(&body);
        image
    }

    /// A thin Mach-O for this host, or a named skip. Fat bytes cannot be
    /// nested inside a `fat_arch` slice, so a host whose only available
    /// executable is universal has no honest fixture here.
    #[cfg(all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ))]
    fn thin_host_slice_bytes(scratch: &Path) -> Option<Vec<u8>> {
        let native = host_native_executable_bytes(scratch)?;
        if native.get(..4) == Some([0xCF, 0xFA, 0xED, 0xFE].as_slice()) {
            return Some(native);
        }
        eprintln!(
            "skipped: the only kernel-executable image available on this host is not a thin \
             Mach-O, so it cannot stand in for a universal slice"
        );
        None
    }

    /// The whole file is refused when ONE host-`cputype` entry is unclaimable,
    /// because the loader grades entries and this code cannot say which one it
    /// will pick. The control in the same test is the point: the identical
    /// builder with the real slice alone still proves a target-only closure,
    /// so the refusal below is the hostile entry, never a broken fixture.
    #[cfg(all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ))]
    #[test]
    fn a_universal_binary_with_one_unclaimable_host_slice_is_unproven() {
        let relative = Path::new("node_modules/.bin/eslint");
        let scratch = tempfile::tempdir().expect("scratch tempdir");
        let Some(host_cpu_type) = HOST_MACH_CPU_TYPE else {
            eprintln!("skipped: this build has no host cputype, so no fat entry can match it");
            return;
        };
        let Some(native) = thin_host_slice_bytes(scratch.path()) else {
            return;
        };

        let (_tmp, root) = snapshot_with_tool(&universal_binary(&[(host_cpu_type, 0, &native)]));
        assert_eq!(
            path_uses_prview_borrow(&root, relative),
            ClosureProof::TargetOnly,
            "control: a universal image whose only host entry is a real binary is claimed, \
             so this fixture shape is sound and the case below is not passing by accident",
        );

        // The verifier's attack: the real binary is slice #1 on the host's
        // baseline subtype, and the graded-higher entry points at shell text.
        // Accepting on slice #1 hands the file to `/bin/sh` through slice #2.
        let launcher = magic_prefixed_launcher(FOREIGN_FORMAT_MAGIC);
        let proof = path_uses_prview_borrow(
            &snapshot_with_tool(&universal_binary(&[
                (host_cpu_type, 0, &native),
                (host_cpu_type, HOST_GRADED_CPU_SUBTYPE, &launcher),
            ]))
            .1,
            relative,
        );
        assert_eq!(
            proof,
            ClosureProof::Unproven,
            "one unclaimable host slice is a shell fallback the grader may choose, so the \
             image proves nothing however strong the other entries are",
        );
        assert_ne!(
            proof,
            ClosureProof::TargetOnly,
            "a real binary beside a hostile entry must never certify an exact snapshot scan",
        );
    }

    /// `FAT_MAGIC_64` is recognised and never a proof: measured on
    /// macOS/arm64, `exec` does not claim a 64-bit fat image even when the
    /// slice it advertises is a real, working host binary — it returns
    /// `ENOEXEC` and the file runs under `/bin/sh`.
    #[cfg(all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ))]
    #[test]
    fn a_fat64_universal_binary_is_unproven_even_with_a_real_host_slice() {
        let relative = Path::new("node_modules/.bin/eslint");
        let scratch = tempfile::tempdir().expect("scratch tempdir");
        let Some(host_cpu_type) = HOST_MACH_CPU_TYPE else {
            eprintln!("skipped: this build has no host cputype, so no fat entry can match it");
            return;
        };
        let Some(native) = thin_host_slice_bytes(scratch.path()) else {
            return;
        };

        let (_tmp, root) =
            snapshot_with_tool(&fat64_universal_binary(&[(host_cpu_type, 0, &native)]));
        let proof = path_uses_prview_borrow(&root, relative);
        assert_eq!(
            proof,
            ClosureProof::Unproven,
            "the strongest possible fat64 image is still not claimed by this platform's \
             loader, so recognising its magic would certify a shell fallback",
        );
        assert_ne!(
            proof,
            ClosureProof::TargetOnly,
            "a magic the kernel refuses is not evidence, whatever it wraps",
        );
    }

    /// `e_phentsize` is matched for equality, not as a minimum: `load_elf_phdrs()`
    /// demands `sizeof(Elf64_Phdr)` exactly and turns any other size into
    /// `-ENOEXEC`, the code that reaches `/bin/sh`. The fixture is a real host
    /// binary with that one field rewritten, so the control is the unmodified
    /// image and the only variable is the field under test.
    ///
    /// STATIC correction: this cell has no runtime measurement behind it, the
    /// round that wrote it had no Linux host. It runs on CI.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_elf_program_header_size_the_kernel_rejects_is_unproven() {
        let relative = Path::new("node_modules/.bin/eslint");
        let scratch = tempfile::tempdir().expect("scratch tempdir");
        let Some(native) = host_native_executable_bytes(scratch.path()) else {
            eprintln!(
                "skipped: no C compiler and no readable system executable on this host, \
                 so no independently kernel-executable fixture exists"
            );
            return;
        };
        if native.get(..5) != Some([0x7F, b'E', b'L', b'F', 2].as_slice()) {
            eprintln!("skipped: the available host executable is not a 64-bit ELF image");
            return;
        }

        let (_tmp, root) = snapshot_with_tool(&native);
        assert_eq!(
            path_uses_prview_borrow(&root, relative),
            ClosureProof::TargetOnly,
            "control: the unmodified host binary is claimed, so each rejection below is \
             the rewritten field and nothing else",
        );

        let program_header_offset =
            u64::from_le_bytes(native[32..40].try_into().expect("eight bytes"));
        let program_headers = u64::from(u16::from_le_bytes([native[56], native[57]]));
        for size in [55u16, 57u16] {
            // Without this the case could pass on the bounds check instead of
            // the size check, and prove nothing about either.
            let table_bytes = program_headers.saturating_mul(u64::from(size));
            assert!(
                program_header_offset.saturating_add(table_bytes)
                    <= u64::try_from(native.len()).expect("image length fits u64"),
                "the near-miss table must still fit inside the file, or the rejection \
                 would come from the bounds check rather than from `e_phentsize`",
            );

            let mut mutated = native.clone();
            mutated[54..56].copy_from_slice(&size.to_le_bytes());
            let (_tmp, root) = snapshot_with_tool(&mutated);
            let proof = path_uses_prview_borrow(&root, relative);
            assert_eq!(
                proof,
                ClosureProof::Unproven,
                "`e_phentsize` = {size} is not `sizeof(Elf64_Phdr)`, so `load_elf_phdrs()` \
                 fails and the image falls through to `/bin/sh`",
            );
            assert_ne!(
                proof,
                ClosureProof::TargetOnly,
                "a program-header size the kernel refuses must never certify a snapshot scan",
            );
        }
    }

    /// A 64-bit ELF header built from the specification instead of from a host
    /// toolchain. The cells below then measure format discrimination on every
    /// runner, including the ones with no `cc` and no readable system binary,
    /// where a toolchain-derived fixture can only skip itself — and a cell that
    /// skips proves nothing about the field it is named after.
    #[cfg(target_os = "linux")]
    fn synthetic_host_elf(machine: u16, file_type: u16, entry_size: u16, entries: u16) -> Vec<u8> {
        const PROGRAM_HEADER_OFFSET: u64 = 64;
        /// Bytes past the advertised table, so no fixture built here is ever
        /// refused by the header-length gate or by the in-file bounds check —
        /// a rejection has to come from the field under test.
        const TAIL_BYTES: usize = 16;

        let mut bytes = vec![0u8; usize::try_from(PROGRAM_HEADER_OFFSET).expect("header fits")];
        bytes[..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
        bytes[4] = 2; // ELFCLASS64
        bytes[5] = 1; // ELFDATA2LSB
        bytes[6] = 1; // EV_CURRENT
        bytes[16..18].copy_from_slice(&file_type.to_le_bytes());
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        bytes[32..40].copy_from_slice(&PROGRAM_HEADER_OFFSET.to_le_bytes()); // e_phoff
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
        bytes[54..56].copy_from_slice(&entry_size.to_le_bytes());
        bytes[56..58].copy_from_slice(&entries.to_le_bytes());
        let table_bytes = usize::from(entries) * usize::from(entry_size);
        bytes.resize(bytes.len() + table_bytes + TAIL_BYTES, 0);
        bytes
    }

    /// The cell the verifier's F3 asked for. On Linux the magic-prefix fixtures
    /// are refused by the 64-byte length gate before a single field is read, so
    /// they would still pass with `e_machine` deleted — and deleting it accepts
    /// a cross-compiled ELF as target-only, which is the false positive this
    /// whole proof exists to refuse. This cell runs the other way round: a
    /// complete, claimable header is the control, and each rejection moves
    /// exactly one field away from it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_complete_elf_header_is_claimed_only_for_this_host_and_this_kind() {
        /// The header length gate, which no fixture in this cell may trip.
        const ELF_HEADER_BYTES: usize = 64;
        const ET_REL: u16 = 1;
        const ET_EXEC: u16 = 2;
        const ET_DYN: u16 = 3;
        const PROGRAM_HEADER_BYTES: u16 = 56;
        /// `EM_386`: a real machine and never this branch's host, so it is the
        /// shape a cross-compiler emits — claimed by no loader running here.
        const FOREIGN_MACHINE: u16 = 3;

        let relative = Path::new("node_modules/.bin/eslint");
        let Some(host_machine) = HOST_ELF_MACHINE else {
            eprintln!("skipped: this build has no host `e_machine`, so no header can match it");
            return;
        };

        for file_type in [ET_EXEC, ET_DYN] {
            let image = synthetic_host_elf(host_machine, file_type, PROGRAM_HEADER_BYTES, 9);
            let (_tmp, root) = snapshot_with_tool(&image);
            assert_eq!(
                path_uses_prview_borrow(&root, relative),
                ClosureProof::TargetOnly,
                "control: a complete host header with `e_type` = {file_type} is claimed by \
                 `binfmt_elf`, so every rejection below is the rewritten field and nothing else",
            );
        }

        for (label, image) in [
            (
                "a cross-compiled `e_machine`",
                synthetic_host_elf(FOREIGN_MACHINE, ET_DYN, PROGRAM_HEADER_BYTES, 9),
            ),
            (
                "a relocatable object rather than an executable",
                synthetic_host_elf(host_machine, ET_REL, PROGRAM_HEADER_BYTES, 9),
            ),
            (
                "an `e_phentsize` the loader refuses",
                synthetic_host_elf(host_machine, ET_DYN, PROGRAM_HEADER_BYTES + 1, 9),
            ),
        ] {
            // Without this the case could be passing on the length gate, which
            // reads no field at all — exactly the vacuity F3 found.
            assert!(
                image.len() > ELF_HEADER_BYTES,
                "{label}: the fixture must outlive the header-length gate, or the case would \
                 pass without the validator reading a single field",
            );

            let (_tmp, root) = snapshot_with_tool(&image);
            let proof = path_uses_prview_borrow(&root, relative);
            assert_eq!(
                proof,
                ClosureProof::Unproven,
                "{label}: `binfmt_elf` drops this header with `-ENOEXEC`, the one code that \
                 sends the file to `/bin/sh`",
            );
            assert_ne!(
                proof,
                ClosureProof::TargetOnly,
                "{label} must never certify an exact snapshot scan",
            );
        }
    }

    /// `e_phnum` is bounded from ABOVE as well as from below: `load_elf_phdrs()`
    /// computes `sizeof(Elf64_Phdr) * e_phnum` and refuses the image when that
    /// product is `0` or greater than 65536 — the same `goto out`, the same
    /// `-ENOEXEC`, the same `/bin/sh` as the entry size beside it. `1170` is the
    /// largest count that fits; `1171` is the first that does not.
    ///
    /// STATIC correction, like its sibling: no Linux host measured this cell,
    /// the source is `fs/binfmt_elf.c`, `load_elf_phdrs()`. It runs on CI.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_elf_program_header_count_the_kernel_rejects_is_unproven() {
        const ET_DYN: u16 = 3;
        const PROGRAM_HEADER_BYTES: u16 = 56;
        /// `56 * 1170 = 65_520`, the largest table `load_elf_phdrs()` reads.
        const LARGEST_CLAIMABLE_COUNT: u16 = 1170;
        /// `PN_XNUM`. Extended numbering lives only in the kernel's core-dump
        /// writer, never on the load path, so here it is just a huge product.
        const PN_XNUM: u16 = 0xffff;

        let relative = Path::new("node_modules/.bin/eslint");
        let Some(host_machine) = HOST_ELF_MACHINE else {
            eprintln!("skipped: this build has no host `e_machine`, so no header can match it");
            return;
        };

        let accepted = synthetic_host_elf(
            host_machine,
            ET_DYN,
            PROGRAM_HEADER_BYTES,
            LARGEST_CLAIMABLE_COUNT,
        );
        // The largest claimable table also carries the image past the script
        // size bound, which pins the ordering this proof depends on: the header
        // is read first, so a claim here is a claim about the header.
        assert!(
            u64::try_from(accepted.len()).expect("image length fits u64") > MAX_JS_SHIM_BYTES,
            "the control must sit past the shim bound, or it would not show that the header \
             proof runs before the size bound",
        );
        let (_tmp, root) = snapshot_with_tool(&accepted);
        assert_eq!(
            path_uses_prview_borrow(&root, relative),
            ClosureProof::TargetOnly,
            "control: `56 * 1170` is exactly the kernel's bound, so this header is claimed and \
             each rejection below is the count and nothing else",
        );

        for count in [0, LARGEST_CLAIMABLE_COUNT + 1, 2000, PN_XNUM] {
            let image = synthetic_host_elf(host_machine, ET_DYN, PROGRAM_HEADER_BYTES, count);
            // Without this the case could be passing on the in-file bounds
            // check — the last thing the branch evaluates — and would prove
            // nothing about the kernel's arithmetic.
            let table_bytes = u64::from(count) * u64::from(PROGRAM_HEADER_BYTES);
            assert!(
                64 + table_bytes <= u64::try_from(image.len()).expect("image length fits u64"),
                "the advertised table must fit inside the fixture, or the rejection would come \
                 from the bounds check rather than from `e_phnum` = {count}",
            );

            let (_tmp, root) = snapshot_with_tool(&image);
            let proof = path_uses_prview_borrow(&root, relative);
            assert_eq!(
                proof,
                ClosureProof::Unproven,
                "`56 * {count}` is outside `load_elf_phdrs()`'s bound, so the kernel returns \
                 `-ENOEXEC` and `execvp` retries the file through `/bin/sh`",
            );
            assert_ne!(
                proof,
                ClosureProof::TargetOnly,
                "a program-header count the kernel refuses must never certify a snapshot scan",
            );
        }
    }

    /// A shell launcher wearing an object-file prefix. Everything after the
    /// magic is what `/bin/sh` would run.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn magic_prefixed_launcher(magic: [u8; 4]) -> Vec<u8> {
        let mut bytes = magic.to_vec();
        bytes.extend_from_slice(
            b"\nexec node \"$(dirname \"$0\")/../eslint/bin/eslint.js\" \"$@\"\n",
        );
        bytes
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_foreign_object_format_prefix_is_unproven() {
        // A format with no loader on this platform, in front of a shell line.
        // `execvp` returns `ENOEXEC`, `/bin/sh` runs the line with full
        // indirection, and the operator's ambient bytes execute — so
        // recognising the magic must certify nothing.
        let relative = Path::new("node_modules/.bin/eslint");
        let (_tmp, root) = snapshot_with_tool(&magic_prefixed_launcher(FOREIGN_FORMAT_MAGIC));
        let proof = path_uses_prview_borrow(&root, relative);
        assert_eq!(
            proof,
            ClosureProof::Unproven,
            "a foreign object format is recognisable, not executable, so it proves nothing",
        );
        assert_ne!(
            proof,
            ClosureProof::TargetOnly,
            "prefixing a shell script with a foreign magic must never flip a run to an \
             exact snapshot claim",
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_host_format_magic_without_a_complete_header_is_unproven() {
        // The sharpest cell: the magic is the one every real binary on this
        // host carries, and the file still falls through to `/bin/sh`, because
        // a loader claims a header, not a prefix.
        let relative = Path::new("node_modules/.bin/eslint");
        let (_tmp, root) = snapshot_with_tool(&magic_prefixed_launcher(HOST_FORMAT_MAGIC));
        let proof = path_uses_prview_borrow(&root, relative);
        assert_eq!(
            proof,
            ClosureProof::Unproven,
            "a host-format magic on an incomplete header is claimed by no loader",
        );
        assert_ne!(
            proof,
            ClosureProof::TargetOnly,
            "four bytes of the host's own magic are not a platform header",
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn an_oversized_magic_prefix_is_unproven() {
        // Size is not what withholds the proof here — the absent header is —
        // but this is the cell the size bound used to keep cautious, so it gets
        // its own pin.
        let relative = Path::new("node_modules/.bin/eslint");
        let bound = usize::try_from(MAX_JS_SHIM_BYTES).expect("shim bound fits usize");
        let mut oversized = HOST_FORMAT_MAGIC.to_vec();
        oversized.resize(bound * 2, 0);
        let (_tmp, root) = snapshot_with_tool(&oversized);
        let proof = path_uses_prview_borrow(&root, relative);
        assert_eq!(
            proof,
            ClosureProof::Unproven,
            "padding a magic prefix past the shim bound does not build a platform header",
        );
        assert_ne!(
            proof,
            ClosureProof::TargetOnly,
            "an oversized file with no claimable header is unread, not proved",
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_executable_without_a_shebang_is_unproven_never_native() {
        let relative = Path::new("node_modules/.bin/eslint");

        // The exact regressed vector: no `#!` and no platform header. `Command`
        // spawns through `execvp`, which POSIX requires to retry an `ENOEXEC`
        // file through `/bin/sh`, so these bytes run with full shell
        // indirection. Certifying them as an exact snapshot scan is the claim
        // this pins shut.
        let (_tmp, root) =
            snapshot_with_tool(b"exec node \"$(dirname \"$0\")/../eslint/bin/eslint.js\" \"$@\"\n");
        let proof = path_uses_prview_borrow(&root, relative);
        assert_eq!(
            proof,
            ClosureProof::Unproven,
            "a missing interpreter directive proves nothing in either direction",
        );
        assert_ne!(
            proof,
            ClosureProof::TargetOnly,
            "the absence of `#!` must never stand in for a claimed platform header",
        );

        // Shorter than any header window: nothing to validate.
        let (_tmp, root) = snapshot_with_tool(b"\x7FEL");
        assert_eq!(
            path_uses_prview_borrow(&root, relative),
            ClosureProof::Unproven,
            "a file too short to carry a header cannot have proved one",
        );

        // An oversized SCRIPT still has no proved kind, so the content bound
        // keeps withholding the exact claim.
        let bound = usize::try_from(MAX_JS_SHIM_BYTES).expect("shim bound fits usize");
        let mut oversized = b"#!/bin/sh\n".to_vec();
        oversized.resize(bound * 2, b'\n');
        let (_tmp, root) = snapshot_with_tool(&oversized);
        assert_eq!(
            path_uses_prview_borrow(&root, relative),
            ClosureProof::Unproven,
            "an oversized script's closure is unread, not borrowed and not proved",
        );
    }

    fn registered_paths(repo: &git2::Repository) -> Vec<PathBuf> {
        repo.worktrees()
            .expect("worktree names")
            .iter()
            .flatten()
            .map(|name| {
                repo.find_worktree(name)
                    .expect("registered worktree")
                    .path()
                    .to_path_buf()
            })
            .collect()
    }

    #[test]
    fn registration_rollback_is_path_exact_and_handles_locked_worktrees() {
        let (repo_tmp, repo) = repo_with_commit();
        let worktrees_tmp = tempfile::tempdir().expect("worktree tempdir");
        let candidate_path = worktrees_tmp.path().join("candidate");
        let control_path = worktrees_tmp.path().join("control");
        let candidate = repo
            .worktree("candidate", &candidate_path, None)
            .expect("candidate worktree");
        candidate
            .lock(Some("partial registration"))
            .expect("lock candidate");
        let _control = repo
            .worktree("control", &control_path, None)
            .expect("control worktree");

        drop(WorktreeRegistrationRollback::new(
            repo_tmp.path(),
            &candidate_path,
        ));

        let paths = registered_paths(&repo);
        assert!(
            !paths
                .iter()
                .any(|path| comparable_worktree_path(path)
                    == comparable_worktree_path(&candidate_path)),
            "the exact partial registration must be removed"
        );
        assert!(
            paths
                .iter()
                .any(|path| comparable_worktree_path(path)
                    == comparable_worktree_path(&control_path)),
            "rollback must not prune a sibling worktree"
        );
    }

    #[test]
    fn registration_rollback_skips_an_unreadable_sibling_before_the_exact_target() {
        let (repo_tmp, repo) = repo_with_commit();
        let worktrees_tmp = tempfile::tempdir().expect("worktree tempdir");
        let stale_path = worktrees_tmp.path().join("stale");
        let healthy_path = worktrees_tmp.path().join("healthy");
        let target_path = worktrees_tmp.path().join("target");
        let _stale = repo
            .worktree("a-stale", &stale_path, None)
            .expect("stale sibling registration");
        let _healthy = repo
            .worktree("m-healthy", &healthy_path, None)
            .expect("healthy sibling registration");
        let target = repo
            .worktree("z-target", &target_path, None)
            .expect("target registration");
        target
            .lock(Some("exact rollback target"))
            .expect("lock target");

        std::fs::remove_file(repo.path().join("worktrees/a-stale/gitdir"))
            .expect("make the sibling registration unreadable");
        assert!(repo.find_worktree("a-stale").is_err());
        assert!(
            prune_registered_worktree(repo_tmp.path(), &target_path)
                .expect("a stale sibling must not abort the exact lookup")
        );

        let names = repo.worktrees().expect("remaining worktree names");
        assert!(names.iter().flatten().any(|name| name == "m-healthy"));
        assert!(!names.iter().flatten().any(|name| name == "z-target"));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_creation_does_not_execute_checkout_hooks() {
        use std::os::unix::fs::PermissionsExt;

        let (repo_tmp, repo) = repo_with_commit();
        let hooks = repo_tmp.path().join("operator-hooks");
        std::fs::create_dir(&hooks).expect("hooks dir");
        let marker = repo_tmp.path().join("post-checkout-ran");
        let hook = hooks.join("post-checkout");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf called > '{}'\nexit 1\n",
                marker.display()
            ),
        )
        .expect("write hook");
        let mut permissions = std::fs::metadata(&hook)
            .expect("hook metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&hook, permissions).expect("make hook executable");
        repo.config()
            .expect("repo config")
            .set_str("core.hooksPath", hooks.to_str().expect("utf8 temp path"))
            .expect("configure hooks");

        let head = repo.head().unwrap().target().unwrap().to_string();
        let snapshot = create_worktree_snapshot(repo_tmp.path(), &head)
            .expect("operator hooks must not participate in snapshot creation");
        assert!(snapshot.worktree_path.is_dir());
        assert!(!marker.exists(), "post-checkout hook must stay isolated");
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_borrows_untracked_node_modules_with_a_real_symlink() {
        let (repo_tmp, repo) = repo_with_commit();
        let node_modules = repo_tmp.path().join("node_modules");
        std::fs::create_dir(&node_modules).expect("node_modules");
        std::fs::write(node_modules.join("marker"), "operator dependency\n")
            .expect("dependency marker");
        let head = repo.head().unwrap().target().unwrap().to_string();

        let snapshot = create_worktree_snapshot(repo_tmp.path(), &head).expect("snapshot");
        let borrowed = snapshot.worktree_path.join("node_modules");
        assert!(
            borrowed.is_symlink(),
            "borrow must be visible to provenance"
        );
        assert_eq!(
            std::fs::canonicalize(&borrowed).expect("borrow target"),
            std::fs::canonicalize(&node_modules).expect("operator dependencies"),
        );
        assert_eq!(
            std::fs::read_to_string(borrowed.join("marker")).expect("borrowed marker"),
            "operator dependency\n",
        );
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_drop_deregisters_in_process_without_spawning_git() {
        use std::os::unix::fs::PermissionsExt;

        let (repo_tmp, _repo) = repo_with_commit();
        let head = git2::Repository::open(repo_tmp.path())
            .expect("open repo")
            .head()
            .expect("head")
            .target()
            .expect("head oid")
            .to_string();
        let snapshot = create_worktree_snapshot(repo_tmp.path(), &head).expect("snapshot");
        let snapshot_path = snapshot.worktree_path.clone();
        let marker = repo_tmp.path().join("drop-spawned-git");
        let shim = repo_tmp.path().join("git-shim");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\nprintf called > '{}'\nexit 1\n",
                marker.display()
            ),
        )
        .expect("write git shim");
        let mut permissions = std::fs::metadata(&shim)
            .expect("shim metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&shim, permissions).expect("make shim executable");

        let _override = crate::git::override_test_git_program(shim);
        drop(snapshot);

        assert!(!marker.exists(), "Drop must not start a git child");
        let repo = git2::Repository::open(repo_tmp.path()).expect("reopen repo");
        assert!(
            registered_paths(&repo)
                .iter()
                .all(|path| comparable_worktree_path(path)
                    != comparable_worktree_path(&snapshot_path)),
            "Drop must prune the exact registration"
        );
        assert!(
            !snapshot_path.exists(),
            "TempDir still owns checkout cleanup"
        );
    }

    #[tokio::test]
    async fn cancelled_drop_deregisters_an_existing_snapshot_in_process() {
        let (repo_tmp, _repo) = repo_with_commit();
        let head = git2::Repository::open(repo_tmp.path())
            .expect("open repo")
            .head()
            .expect("head")
            .target()
            .expect("head oid")
            .to_string();
        let snapshot = create_worktree_snapshot(repo_tmp.path(), &head).expect("snapshot");
        let snapshot_path = snapshot.worktree_path.clone();
        let governor = std::sync::Arc::new(crate::governor::ResourceGovernor::new());
        governor.cancel();

        crate::governor::with_run_scope(governor, async move {
            drop(snapshot);
        })
        .await;

        let repo = git2::Repository::open(repo_tmp.path()).expect("reopen repo");
        assert!(
            registered_paths(&repo)
                .iter()
                .all(|path| comparable_worktree_path(path)
                    != comparable_worktree_path(&snapshot_path)),
            "cancelled Drop must not leave a worktree registration"
        );
        assert!(
            !snapshot_path.exists(),
            "the snapshot tempdir still owns filesystem cleanup"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn cancelled_worktree_add_rolls_back_a_completed_registration() {
        use std::os::unix::fs::PermissionsExt;

        let (repo_tmp, repo) = repo_with_commit();
        let repo_root = repo_tmp.path().to_path_buf();
        let baseline = registered_paths(&repo).len();
        let ready = repo_tmp.path().join("worktree-add-ready");
        let shim = repo_tmp.path().join("worktree-add-shim");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\ngit \"$@\"\nstatus=$?\nif [ \"$3\" = worktree ] && [ \"$4\" = add ] && [ \"$status\" -eq 0 ]; then\n  printf '%s\\n' \"$7\" > '{}'\n  sleep 30\nfi\nexit \"$status\"\n",
                ready.display()
            ),
        )
        .expect("write git shim");
        let mut permissions = std::fs::metadata(&shim)
            .expect("shim metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&shim, permissions).expect("make shim executable");

        let governor = std::sync::Arc::new(crate::governor::ResourceGovernor::new());
        let canceller = {
            let governor = std::sync::Arc::clone(&governor);
            let ready = ready.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !ready.exists() {
                    if std::time::Instant::now() >= deadline {
                        governor.cancel();
                        panic!("git shim never completed worktree registration");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                governor.cancel();
            })
        };
        let result =
            crate::governor::with_run_scope(std::sync::Arc::clone(&governor), async move {
                crate::governor::blocking_stage(|| {
                    let _override = crate::git::override_test_git_program(shim);
                    create_worktree_snapshot(&repo_root, "HEAD")
                })
            })
            .await;
        canceller.join().expect("canceller");

        let error = match result {
            Ok(_) => panic!("cancellation must interrupt the worktree-add shim"),
            Err(error) => error,
        };
        assert!(crate::governor::is_cancellation(&error), "{error:#}");
        assert_eq!(governor.inflight_count(), 0);
        let registered_path = PathBuf::from(
            std::fs::read_to_string(&ready)
                .expect("registered path receipt")
                .trim(),
        );
        let repo = git2::Repository::open(repo_tmp.path()).expect("reopen repo");
        assert_eq!(
            registered_paths(&repo).len(),
            baseline,
            "cancelled add must restore the registration count"
        );
        assert!(
            !registered_path.exists(),
            "cancelled add must also release its TempDir-owned checkout"
        );
    }
}
