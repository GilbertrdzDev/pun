use crate::Exit;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::hash::{BuildHasher, Hasher};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(not(windows))]
use std::path::Component;
use std::path::Path;
use std::process::Command;

/// Linux and macOS binaries are the upstream release. Windows binaries are published by this fork.
fn repo() -> &'static str {
    #[cfg(windows)]
    {
        "GilbertrdzDev/pun"
    }
    #[cfg(not(windows))]
    {
        "shipfastlabs/pun"
    }
}

fn archive_name(target: &str) -> String {
    #[cfg(windows)]
    {
        let _ = target;
        "pun-windows-x86_64.zip".to_string()
    }
    #[cfg(not(windows))]
    {
        format!("punphp-{target}.tar.gz")
    }
}

/// A standalone install is `pun` (or `pun.exe`). Composer names its copy `pun-<version>-<target>`.
fn standalone_pun(name: &std::ffi::OsStr) -> bool {
    let n = name.to_string_lossy();
    let stem = n.strip_suffix(".exe").unwrap_or(&n);
    stem.eq_ignore_ascii_case("pun")
}

#[derive(clap::Args)]
pub(crate) struct SelfUpdateArgs {
    /// The release to install, such as `v0.2.0` (default: the latest).
    version: Option<String>,
    /// Only report whether a newer release is out.
    #[arg(long)]
    check: bool,
}

pub(crate) fn run(a: &SelfUpdateArgs) -> Result<Exit> {
    let exe = pun_tokens::php_compat::canonicalize(std::env::current_exe()?)
        .context("can't find the running pun")?;
    if !standalone_pun(exe.file_name().unwrap_or_default()) {
        bail!(
            "this pun was installed with Composer; update it with `composer update shipfastlabs/pun`"
        );
    }
    let current = env!("CARGO_PKG_VERSION");
    // Pun's tests serve the releases from a local server.
    let server = std::env::var("PUN_GITHUB_URL").ok();
    let web = server.as_deref().unwrap_or("https://github.com");
    let api = server.as_deref().unwrap_or("https://api.github.com");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(server.is_none())
        .user_agent(format!("pun/{current}"))
        .build()
        .into();
    let tag = match &a.version {
        Some(v) => format!("v{}", v.trim_start_matches('v')),
        None => latest(&agent, web, api)?,
    };
    if !valid_tag(&tag) {
        bail!("not a release version: {tag}");
    }
    let version = &tag[1..];
    if a.version.is_none() && pun_packages::version::compare(version, current) <= 0 {
        eprintln!("pun {current} is up to date (the latest release is {version})");
        return Ok(Exit::Success);
    }
    if a.check {
        eprintln!("pun {version} is out (this is {current}); run `pun self-update` to install it");
        return Ok(Exit::Success);
    }

    let target = target()?;
    let archive = archive_name(&target);
    let url = format!("{web}/{}/releases/download/{tag}/{archive}", repo());
    eprintln!("Downloading pun {version} for {target}");
    let bytes = get(&agent, &url)?;
    let sums = String::from_utf8(get(&agent, &format!("{url}.sha256"))?).unwrap_or_default();
    let expected = sums.split_whitespace().next().unwrap_or_default();
    let actual = hex(&Sha256::digest(&bytes));
    if expected.is_empty() || actual != expected {
        bail!(
            "checksum mismatch for {archive} (expected {expected}, got {actual}); the download may be corrupt"
        );
    }
    eprintln!(" ✔ verified the SHA-256 checksum");
    attestation(&bytes, &archive, &tag)?;
    let binary = extract(&bytes, &target).with_context(|| format!("unpacking {archive}"))?;

    let dir = exe.parent().context("the running pun has no directory")?;
    let staged = dir.join(format!(".pun.{}", std::process::id()));
    std::fs::write(&staged, &binary)
        .with_context(|| format!("can't write to {}", dir.display()))?;
    let result = (|| {
        #[cfg(unix)]
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        let out = Command::new(&staged)
            .arg("--version")
            .output()
            .context("the downloaded pun does not run on this machine")?;
        let reported = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if reported != format!("pun {version}") {
            bail!("the downloaded pun reports '{reported}', not 'pun {version}'");
        }
        install_over(&staged, &exe)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result?;
    eprintln!("Updated pun {current} → {version} at {}", exe.display());
    Ok(Exit::Success)
}

/// A plain release version: `v` and three dot-separated numbers, then only letters, digits, dots and dashes.
fn valid_tag(tag: &str) -> bool {
    let Some(rest) = tag.strip_prefix('v') else {
        return false;
    };
    let mut parts = rest.splitn(3, '.');
    let (Some(major), Some(minor), Some(patch)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let patch_start = patch
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or_default();
    digits(major)
        && digits(minor)
        && digits(patch_start)
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// The latest stable release, else the newest pre-release; github.com's latest release when the API's rate limit is
/// used up.
fn latest(agent: &ureq::Agent, web: &str, api: &str) -> Result<String> {
    let api = format!("{api}/repos/{}/releases", repo());
    for url in [format!("{api}/latest"), format!("{api}?per_page=1")] {
        let Ok(body) = get(agent, &url) else { continue };
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let release = if json.is_array() { &json[0] } else { &json };
        if let Some(tag) = release["tag_name"].as_str() {
            return Ok(tag.to_string());
        }
    }
    // When the API's rate limit is used up: github.com's releases/latest, which has none, redirects to the newest
    // published release (to /releases without one). The releases feed would also name a tag whose release is
    // still building, with no archives to download yet.
    let url = format!("{web}/{}/releases/latest", repo());
    let res = (agent.get(&url).config().max_redirects(0).build().call())
        .with_context(|| format!("download failed: {url}"))?;
    (res.headers().get("location"))
        .and_then(|l| l.to_str().ok()?.split("/releases/tag/").nth(1))
        .map(String::from)
        .context("could not find the latest release; name one: `pun self-update v0.1.0`")
}

fn get(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    let mut res = agent
        .get(url)
        .call()
        .with_context(|| format!("download failed: {url}"))?;
    let mut body = vec![];
    res.body_mut()
        .as_reader()
        .read_to_end(&mut body)
        .with_context(|| format!("download failed: {url}"))?;
    Ok(body)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// The release archive this machine runs. Each platform compiles only its own `target()`.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the other platforms' target() fails"
)]
fn target() -> Result<String> {
    Ok(format!("{}-unknown-linux-musl", std::env::consts::ARCH))
}

/// A shell under Rosetta still gets the Apple silicon build.
#[cfg(all(
    target_os = "macos",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the other platforms' target() fails"
)]
fn target() -> Result<String> {
    let translated = Command::new("sysctl")
        .args(["-n", "sysctl.proc_translated"])
        .output();
    let arch = match std::env::consts::ARCH {
        "x86_64" if translated.is_ok_and(|o| o.stdout.trim_ascii() == b"1") => "aarch64",
        arch => arch,
    };
    Ok(format!("{arch}-apple-darwin"))
}

#[cfg(all(
    not(any(target_arch = "x86_64", target_arch = "aarch64")),
    not(windows)
))]
fn target() -> Result<String> {
    bail!(
        "unsupported CPU architecture {} (Pun ships x86_64 and aarch64 binaries)",
        std::env::consts::ARCH
    )
}

#[cfg(all(windows, target_arch = "x86_64"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the other platforms' target() fails"
)]
fn target() -> Result<String> {
    Ok("x86_64-pc-windows-msvc".to_string())
}

#[cfg(all(windows, not(target_arch = "x86_64")))]
fn target() -> Result<String> {
    bail!(
        "unsupported CPU architecture {} (the Windows build is x86_64)",
        std::env::consts::ARCH
    )
}

#[cfg(all(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    not(any(target_os = "linux", target_os = "macos", target_os = "windows"))
))]
fn target() -> Result<String> {
    bail!(
        "unsupported OS {} (Pun ships Linux, macOS and Windows binaries)",
        std::env::consts::OS
    )
}

/// Puts `staged` in place of the running binary. Windows will not overwrite a running executable, so the
/// running file is renamed aside first (the process keeps that file) and the new one takes its path.
fn install_over(staged: &Path, exe: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::rename(staged, exe).with_context(|| format!("can't replace {}", exe.display()))
    }
    #[cfg(windows)]
    {
        let backup = exe.with_file_name(format!(
            ".{}.old",
            exe.file_name().unwrap_or_default().to_string_lossy()
        ));
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(exe, &backup)
            .with_context(|| format!("can't replace {}", exe.display()))?;
        if let Err(error) = std::fs::rename(staged, exe) {
            let _ = std::fs::rename(&backup, exe);
            return Err(error).with_context(|| format!("can't replace {}", exe.display()));
        }
        if std::fs::remove_file(&backup).is_err() {
            eprintln!(
                " · left the previous binary at {} (Windows keeps a running executable until it exits)",
                backup.display()
            );
        }
        Ok(())
    }
}

/// GitHub's build attestation: proof the archive came from the release workflow for `tag` on a GitHub-hosted runner,
/// which the checksum (published beside the archive) cannot give.
fn attestation(bytes: &[u8], archive: &str, tag: &str) -> Result<()> {
    let required = std::env::var("PUN_REQUIRE_ATTESTATION").is_ok_and(|v| v == "1");
    let gh = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !gh {
        if required {
            bail!("PUN_REQUIRE_ATTESTATION=1 needs the GitHub CLI (gh), signed in");
        }
        eprintln!(" · skipped the build attestation (needs a signed-in gh)");
        return Ok(());
    }
    // In a new directory only this user can enter: an entry another account plants in the shared temp directory can't
    // take the write, or swap the archive before gh reads it.
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let dir = std::env::temp_dir().join(format!("pun-{}-{random:016x}", std::process::id()));
    #[cfg(unix)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    #[cfg(windows)]
    let builder = std::fs::DirBuilder::new();
    builder.create(&dir)?;
    let file = dir.join(archive);
    if let Err(e) = std::fs::write(&file, bytes) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e.into());
    }
    let status = Command::new("gh")
        .args(["attestation", "verify"])
        .arg(&file)
        .args(["--repo", repo(), "--signer-workflow", &signer_workflow()])
        .args([
            "--source-ref",
            &format!("refs/tags/{tag}"),
            "--deny-self-hosted-runners",
        ])
        .output();
    let _ = std::fs::remove_dir_all(&dir);
    if !status.is_ok_and(|o| o.status.success()) {
        bail!(
            "the build attestation of {archive} does not verify; the download may have been tampered with"
        );
    }
    eprintln!(" ✔ verified the GitHub build attestation");
    Ok(())
}

fn signer_workflow() -> String {
    #[cfg(windows)]
    {
        format!(
            "github.com/{}/.github/workflows/release-windows.yml",
            repo()
        )
    }
    #[cfg(not(windows))]
    {
        format!("github.com/{}/.github/workflows/release.yml", repo())
    }
}

/// The `pun` binary from the release archive.
fn extract(bytes: &[u8], target: &str) -> Result<Vec<u8>> {
    #[cfg(windows)]
    {
        let _ = target;
        extract_zip(bytes)
    }
    #[cfg(not(windows))]
    {
        extract_tar(bytes, target)
    }
}

/// `pun.exe` from `pun-windows-x86_64.zip`. Entries may sit at the archive root or one directory down.
/// A path that escapes the archive is refused.
#[cfg(windows)]
fn extract_zip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .context("the download is not a zip archive")?;
    let mut binary = None;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name()?.replace('\\', "/");
        if name.contains("..") || name.starts_with('/') || name.contains(':') {
            bail!("it holds an unexpected path: {name}");
        }
        let file = name.rsplit('/').next().unwrap_or(name.as_str());
        if entry.is_file() && file.eq_ignore_ascii_case("pun.exe") {
            let mut data = vec![];
            entry.read_to_end(&mut data)?;
            binary = Some(data);
        }
    }
    binary.context("it holds no pun.exe")
}

/// The `pun` binary from the archive, which may hold only regular files and directories under `punphp-<target>/`.
#[cfg(not(windows))]
fn extract_tar(bytes: &[u8], target: &str) -> Result<Vec<u8>> {
    let top = format!("punphp-{target}");
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut binary = None;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            bail!(
                "it holds an entry that is not a file or directory: {}",
                path.display()
            );
        }
        let mut parts = path.components();
        let inside = parts.next() == Some(Component::Normal(top.as_ref()))
            && parts.all(|c| matches!(c, Component::Normal(_)));
        if !inside {
            bail!("it holds an unexpected path: {}", path.display());
        }
        if kind.is_file() && path == Path::new(&top).join("pun") {
            let mut data = vec![];
            entry.read_to_end(&mut data)?;
            binary = Some(data);
        }
    }
    binary.context("it holds no pun binary")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    const T: &str = "x86_64-unknown-linux-musl";

    /// A gzip tar of (path, entry type, contents); the path bytes go into the header as they are, `..` included.
    #[cfg(not(windows))]
    fn archive(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
        let mut out = tar::Builder::new(flate2::write::GzEncoder::new(
            vec![],
            flate2::Compression::fast(),
        ));
        for (path, kind, data) in entries {
            let mut h = tar::Header::new_old();
            h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
            h.set_entry_type(*kind);
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            out.append(&h, *data).unwrap();
        }
        out.into_inner().unwrap().finish().unwrap()
    }

    #[cfg(not(windows))]
    #[test]
    fn extract_takes_only_the_release_layout() {
        use tar::EntryType::{Directory, Regular, Symlink};
        let top = format!("punphp-{T}/");
        let pun = format!("punphp-{T}/pun");
        let good = archive(&[
            (&top, Directory, b""),
            (&format!("{top}LICENSE"), Regular, b"MIT"),
            (&pun, Regular, b"ELF"),
        ]);
        assert_eq!(extract(&good, T).unwrap(), b"ELF");
        let error = |entries: &[(&str, tar::EntryType, &[u8])]| {
            format!("{:#}", extract(&archive(entries), T).unwrap_err())
        };
        assert!(error(&[(&pun, Symlink, b"")]).contains("not a file or directory"));
        assert!(error(&[(&format!("{top}../pun"), Regular, b"x")]).contains("unexpected path"));
        assert!(
            error(&[("punphp-aarch64-apple-darwin/pun", Regular, b"x")])
                .contains("unexpected path")
        );
        assert!(error(&[("/etc/pun", Regular, b"x")]).contains("unexpected path"));
        assert!(error(&[(&format!("{top}LICENSE"), Regular, b"MIT")]).contains("no pun binary"));
    }

    /// The Windows release is a zip of `pun.exe`, at the root or one directory down. A path that escapes is refused.
    #[cfg(windows)]
    #[test]
    fn extract_takes_only_the_release_zip() {
        fn packed(files: &[(&str, &[u8])]) -> Vec<u8> {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
            let options = zip::write::SimpleFileOptions::default();
            for (name, data) in files {
                writer.start_file(*name, options).unwrap();
                std::io::Write::write_all(&mut writer, data).unwrap();
            }
            writer.finish().unwrap().into_inner()
        }
        assert_eq!(
            extract(&packed(&[("pun.exe", b"MZ"), ("LICENSE", b"MIT")]), "win").unwrap(),
            b"MZ"
        );
        assert_eq!(
            extract(&packed(&[("pun/pun.exe", b"MZ")]), "win").unwrap(),
            b"MZ"
        );
        let error =
            |files: &[(&str, &[u8])]| format!("{:#}", extract(&packed(files), "win").unwrap_err());
        assert!(error(&[("../pun.exe", b"MZ")]).contains("unexpected path"));
        assert!(error(&[("LICENSE", b"MIT")]).contains("no pun.exe"));
    }

    #[test]
    fn tags_must_be_release_versions() {
        for tag in ["v0.1.0", "v12.30.4", "v0.2.0-beta.1"] {
            assert!(valid_tag(tag), "{tag}");
        }
        for tag in [
            "vlatest",
            "0.1.0",
            "v1.2",
            "v1.2.x",
            "v1.2.3/../x",
            "v1.2.3?a=b",
            "v.1.2",
        ] {
            assert!(!valid_tag(tag), "{tag}");
        }
    }
}
