//! Replacing the running executable with a newer one.
//!
//! The order of operations is the whole design, and every step exists because
//! the step after it must not be reachable without it:
//!
//! 1. **Refuse unless this copy is ours to replace.** A managed install is
//!    told what to run instead. See [`super::origin`].
//! 2. **Verify the manifest's signature** — done before this module is
//!    reached, since a `Manifest` cannot be obtained any other way.
//! 3. **Verify the archive against the digest** the signed manifest names.
//! 4. **Stage beside the target, on the same filesystem.** `rename` is only
//!    atomic within a filesystem, and a half-written executable in `$PATH` is
//!    the worst outcome available here.
//! 5. **Run the staged binary and make it state its own version.** A download
//!    that cannot execute — wrong architecture, a signature the platform
//!    rejects, a truncated file that still hashed because it was hashed
//!    whole — is caught while the working executable is still in place.
//! 6. **One `rename`.** Atomic: either the new binary is there or the old one
//!    is, never neither. The running process keeps its own inode and finishes
//!    normally.
//!
//! Nothing is deleted at any point. The old executable's inode survives until
//! the last process using it exits, and the staged file is removed only after
//! it has been renamed into place or has failed.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

use super::net::{self, Fetcher};
use super::origin::Origin;
use super::{verify, Manifest};

/// What the executable is called inside the release archive.
pub const ARCHIVE_MEMBER: &str = "prune-juice";

#[derive(Clone, Debug)]
pub struct Installed {
    pub version: String,
    pub path: PathBuf,
    pub bytes: u64,
}

/// Removes the staged file unless it was renamed into place. Without this a
/// failed install leaves a stray executable next to the real one.
struct Staged(Option<PathBuf>);

impl Staged {
    fn keep(&mut self) {
        self.0 = None;
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Download, verify and install the CLI artifact for this machine.
///
/// `exe` is the executable to replace — normally `std::env::current_exe()`.
pub fn install(
    fetcher: &dyn Fetcher,
    manifest: &Manifest,
    origin: &Origin,
    exe: &Path,
) -> Result<Installed> {
    if !origin.self_replace_allowed() {
        // The check already ran and already reported the new version. This is
        // the only thing being declined, and it is declined with the command
        // that will actually work.
        let why = origin.why_not().unwrap_or("this copy is managed elsewhere");
        return Err(Error::Config(match origin.upgrade_command() {
            Some(cmd) => format!("not replacing this binary: {why}. Run `{cmd}` instead."),
            None => format!("not replacing this binary: {why}."),
        }));
    }

    let artifact = manifest.cli_for_host().ok_or_else(|| {
        Error::Config(format!(
            "release {} has no build for {} — install it by hand, or build from source",
            manifest.version,
            super::HOST_TARGET
        ))
    })?;

    // Resolved, so a symlinked `~/bin/prune-juice` is replaced where the bytes
    // actually live rather than being turned into a regular file.
    let target = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let dir = target
        .parent()
        .ok_or_else(|| Error::Config(format!("{} has no parent directory", target.display())))?;
    writable(dir, &target)?;

    let archive = fetcher.get(
        &artifact.url,
        net::MAX_ARTIFACT_BYTES,
        super::DOWNLOAD_TIMEOUT,
    )?;
    if artifact.bytes > 0 && archive.len() as u64 != artifact.bytes {
        return Err(Error::Config(format!(
            "{} is {} bytes and the signed manifest says {}",
            artifact.url,
            archive.len(),
            artifact.bytes
        )));
    }
    verify::digest(&archive, &artifact.sha256)?;

    let binary = extract(&archive)?;
    let staged_path = dir.join(format!(".prune-juice.update.{}", std::process::id()));
    let mut staged = Staged(Some(staged_path.clone()));
    write_executable(&staged_path, &binary, &target)?;

    let reported = version_of(&staged_path)?;
    if reported != manifest.version {
        return Err(Error::Config(format!(
            "the downloaded binary reports version {reported} and the signed manifest \
             promised {} — refusing to install it",
            manifest.version
        )));
    }

    // The one atomic step. Everything above could be abandoned safely; from
    // here there is nothing left to fail.
    std::fs::rename(&staged_path, &target).map_err(|e| {
        Error::Config(format!(
            "could not move the new binary into place at {}: {e}",
            target.display()
        ))
    })?;
    staged.keep();

    Ok(Installed {
        version: manifest.version.clone(),
        path: target,
        bytes: binary.len() as u64,
    })
}

/// Fail early and clearly when the install directory is not ours to write.
///
/// Doing this before the download means a `/usr/local/bin` install says so in
/// a second rather than after pulling several megabytes.
fn writable(dir: &Path, target: &Path) -> Result<()> {
    let probe = dir.join(format!(".prune-juice.write-test.{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(Error::Config(format!(
            "cannot write to {} ({e}), so {} cannot be replaced. \
             Reinstall by hand, or move the binary somewhere you own.",
            dir.display(),
            target.display()
        ))),
    }
}

/// Pull the executable out of a `.tar.gz`.
///
/// Entries are read into memory rather than unpacked. `tar`'s `unpack` honours
/// the paths inside the archive, and an archive is exactly the kind of input
/// that should not get to choose where a file lands — even a signed one, since
/// the signature proves who built it and not that they built it correctly.
fn extract(archive: &[u8]) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder.take(net::MAX_ARTIFACT_BYTES));
    for entry in tar.entries().map_err(Error::Io)? {
        let mut entry = entry.map_err(Error::Io)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let is_target = entry
            .path()
            .map(|p| p.file_name().is_some_and(|n| n == ARCHIVE_MEMBER))
            .unwrap_or(false);
        if !is_target {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(Error::Io)?;
        if bytes.is_empty() {
            return Err(Error::Config(
                "the release archive contains an empty prune-juice".into(),
            ));
        }
        return Ok(bytes);
    }
    Err(Error::Config(format!(
        "the release archive contains no `{ARCHIVE_MEMBER}` executable"
    )))
}

/// Write the staged binary, fsync it, and give it the mode the current
/// executable has.
///
/// The fsync matters for the same reason it does in the vault: a rename that
/// outruns the data leaves a correctly-named file full of nothing, and here
/// that file is the tool itself.
fn write_executable(path: &Path, bytes: &[u8], model: &Path) -> Result<()> {
    use std::io::Write;

    let mut file = std::fs::File::create(path).map_err(Error::Io)?;
    file.write_all(bytes).map_err(Error::Io)?;
    file.sync_all().map_err(Error::Io)?;
    drop(file);

    // Copied from the binary being replaced rather than hardcoded to 0755:
    // an install that was deliberately group-writable or mode 700 stays that
    // way. A missing executable bit is added, since the file has to run.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(model)
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(0o755);
        let mode = mode | 0o100;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(Error::Io)?;
    }
    #[cfg(not(unix))]
    let _ = model;
    Ok(())
}

/// Ask a binary what version it is.
///
/// This is the step that turns "the bytes are authentic" into "the bytes run
/// here". A signature says who made a file; only executing it says the
/// platform will accept it.
fn version_of(path: &Path) -> Result<String> {
    let out = std::process::Command::new(path)
        .arg("--version")
        .output()
        .map_err(|e| {
            Error::Config(format!(
                "the downloaded binary would not run ({e}) — the working copy has been left alone"
            ))
        })?;
    if !out.status.success() {
        return Err(Error::Config(format!(
            "the downloaded binary exited {} when asked for its version — \
             the working copy has been left alone",
            out.status.code().unwrap_or(-1)
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // `prune-juice 0.2.0`
    text.split_whitespace()
        .last()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Config("the downloaded binary reported no version".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::net::StaticFetcher;
    use crate::update::ArtifactKind;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "pj-install-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A `.tar.gz` holding one file at `name`.
    fn targz(name: &str, body: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_path(name).unwrap();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, body).unwrap();
        let tar = builder.into_inner().unwrap();

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        gz.finish().unwrap()
    }

    /// A shell script that behaves like the real binary for `--version`.
    fn fake_cli(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho \"prune-juice {version}\"\n").into_bytes()
    }

    fn manifest(version: &str, url: &str, archive: &[u8]) -> Manifest {
        Manifest {
            schema: 1,
            version: version.into(),
            published: None,
            notes_url: None,
            notes: None,
            artifacts: vec![crate::update::Artifact {
                kind: ArtifactKind::Cli,
                target: crate::update::HOST_TARGET.into(),
                url: url.into(),
                sha256: verify::sha256_hex(archive),
                bytes: archive.len() as u64,
            }],
        }
    }

    fn existing_exe(dir: &Path) -> PathBuf {
        let p = dir.join("prune-juice");
        std::fs::write(&p, fake_cli("0.1.0")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[test]
    fn the_happy_path_replaces_the_binary_atomically() {
        let dir = tmpdir("happy");
        let exe = existing_exe(&dir);
        let archive = targz("prune-juice", &fake_cli("0.2.0"));
        let url = "https://example.test/prune-juice-0.2.0.tar.gz";
        let f = StaticFetcher::new().with(url, archive.clone());

        let done = install(
            &f,
            &manifest("0.2.0", url, &archive),
            &Origin::Standalone,
            &exe,
        )
        .unwrap();

        assert_eq!(done.version, "0.2.0");
        assert_eq!(version_of(&exe).unwrap(), "0.2.0");
        // Nothing left behind.
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "prune-juice")
            .collect();
        assert!(strays.is_empty(), "{strays:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_tampered_archive_is_refused_and_the_working_copy_survives() {
        let dir = tmpdir("digest");
        let exe = existing_exe(&dir);
        let real = targz("prune-juice", &fake_cli("0.2.0"));
        let served = targz("prune-juice", &fake_cli("6.6.6"));
        let url = "https://example.test/x.tar.gz";
        // The manifest is signed, so its digest is authentic; the *server*
        // returns something else. Its declared length is set to what the
        // server actually serves, so the cheap length check cannot be what
        // catches this — the digest has to.
        let mut m = manifest("0.2.0", url, &real);
        m.artifacts[0].bytes = served.len() as u64;
        let f = StaticFetcher::new().with(url, served);

        let e = install(&f, &m, &Origin::Standalone, &exe).unwrap_err();
        assert!(e.to_string().contains("digest"), "{e}");
        assert_eq!(version_of(&exe).unwrap(), "0.1.0", "must be untouched");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_binary_whose_version_disagrees_with_the_manifest_is_refused() {
        // The archive is authentic and hashes correctly, and still is not the
        // release it claims to be. Caught before the rename.
        let dir = tmpdir("mismatch");
        let exe = existing_exe(&dir);
        let archive = targz("prune-juice", &fake_cli("0.1.9"));
        let url = "https://example.test/x.tar.gz";
        let f = StaticFetcher::new().with(url, archive.clone());

        let e = install(
            &f,
            &manifest("0.2.0", url, &archive),
            &Origin::Standalone,
            &exe,
        )
        .unwrap_err();
        assert!(e.to_string().contains("0.1.9"), "{e}");
        assert_eq!(version_of(&exe).unwrap(), "0.1.0");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_binary_that_will_not_run_never_reaches_the_target_path() {
        let dir = tmpdir("norun");
        let exe = existing_exe(&dir);
        // Not an executable of any kind on this platform.
        let archive = targz("prune-juice", b"\x00\x01\x02not a program");
        let url = "https://example.test/x.tar.gz";
        let f = StaticFetcher::new().with(url, archive.clone());

        let e = install(
            &f,
            &manifest("0.2.0", url, &archive),
            &Origin::Standalone,
            &exe,
        )
        .unwrap_err();
        assert!(e.to_string().contains("left alone"), "{e}");
        assert_eq!(version_of(&exe).unwrap(), "0.1.0");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_install_leaves_no_staged_file_behind() {
        let dir = tmpdir("staged");
        let exe = existing_exe(&dir);
        let archive = targz("prune-juice", &fake_cli("0.1.9"));
        let url = "https://example.test/x.tar.gz";
        let f = StaticFetcher::new().with(url, archive.clone());
        let _ = install(
            &f,
            &manifest("0.2.0", url, &archive),
            &Origin::Standalone,
            &exe,
        );
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".prune-juice"))
            .collect();
        assert!(strays.is_empty(), "{strays:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_managed_install_is_refused_with_the_command_that_works() {
        let dir = tmpdir("brew");
        let exe = existing_exe(&dir);
        let archive = targz("prune-juice", &fake_cli("0.2.0"));
        let url = "https://example.test/x.tar.gz";
        let f = StaticFetcher::new().with(url, archive.clone());
        let e = install(
            &f,
            &manifest("0.2.0", url, &archive),
            &Origin::Homebrew,
            &exe,
        )
        .unwrap_err();
        assert!(e.to_string().contains("brew upgrade prune-juice"), "{e}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_release_with_no_build_for_this_machine_says_so() {
        let dir = tmpdir("noartifact");
        let exe = existing_exe(&dir);
        let mut m = manifest("0.2.0", "https://example.test/x", b"");
        m.artifacts[0].target = "sparc64-unknown-linux-gnu".into();
        let f = StaticFetcher::new();
        let e = install(&f, &m, &Origin::Standalone, &exe).unwrap_err();
        assert!(e.to_string().contains("no build for"), "{e}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_archive_without_the_executable_is_an_error_not_a_silent_no_op() {
        let e = extract(&targz("README.md", b"hello")).unwrap_err();
        assert!(e.to_string().contains("no `prune-juice`"), "{e}");
    }

    #[test]
    fn an_archive_entry_is_never_allowed_to_choose_where_it_lands() {
        // The member is found by file name and its bytes are read into
        // memory, so the directory part of the entry's path is information
        // and never an instruction. `tar::Archive::unpack` would treat it as
        // an instruction, which is why it is not used.
        let body = fake_cli("0.2.0");
        for path in ["prune-juice", "dist/prune-juice", "a/b/c/prune-juice"] {
            assert_eq!(extract(&targz(path, &body)).unwrap(), body, "{path}");
        }
        // Nothing was written anywhere by the extraction itself.
        assert!(!Path::new("dist").exists() && !Path::new("a/b/c").exists());
    }

    #[test]
    fn the_mode_of_the_replaced_binary_is_preserved() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tmpdir("mode");
            let exe = existing_exe(&dir);
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
            let archive = targz("prune-juice", &fake_cli("0.2.0"));
            let url = "https://example.test/x.tar.gz";
            let f = StaticFetcher::new().with(url, archive.clone());
            install(
                &f,
                &manifest("0.2.0", url, &archive),
                &Origin::Standalone,
                &exe,
            )
            .unwrap();
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "a deliberately private install stays private");
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
