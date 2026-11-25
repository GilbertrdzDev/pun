#![cfg(unix)]
mod common;

use common::{PUN, scratch};

use std::path::Path;
use std::process::{Command, Output};

fn run(cmd: &mut Command) -> Output {
    (cmd.env("PUN_AGENT", "0"))
        .env("PUN_CACHE_DIR", common::test_cache_dir())
        .output()
        .unwrap()
}

#[test]
fn shim_runs_the_workspace_version_with_its_arguments() {
    if Command::new("php").arg("-v").output().is_err() {
        return;
    }
    let dir = scratch("composer", "shim");
    let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../composer/bin/pun");
    std::fs::copy(&shim, dir.join("pun")).unwrap();
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        _ => "unknown-linux-musl",
    };
    let binary = dir.join(format!(
        "pun-{}-{}-{os}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH
    ));
    std::fs::copy(PUN, &binary).unwrap();
    std::fs::write(dir.join("Bad.php"), "<?php\n$a = array( 1 );\n").unwrap();

    for args in [
        &["--version"][..],
        &["format", "--test", "Bad.php"],
        &["no-such-command"],
    ] {
        let via_shim = run(Command::new("php")
            .arg(dir.join("pun"))
            .args(args)
            .current_dir(&dir));
        let direct = run(Command::new(&binary).args(args).current_dir(&dir));
        assert_eq!(
            via_shim.status.code(),
            direct.status.code(),
            "exit code of {args:?}"
        );
        assert_eq!(via_shim.stdout, direct.stdout, "stdout of {args:?}");
    }

    // A new file at the same path, not a write into the old one: Linux refuses to write to a binary any process still
    // executes (ETXTBSY), and a just-exited run can hold it for a moment.
    std::fs::remove_file(&binary).unwrap();
    std::fs::write(&binary, b"\x7fELF not a binary").unwrap();
    let broken = run(Command::new("php").arg(dir.join("pun")).current_dir(&dir));
    assert_eq!(broken.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&broken.stderr),
        format!(
            "pun: could not start {}; delete it to download it again\n",
            binary.display()
        )
    );
}

/// A local GitHub serving one release asset per request path; every other path gets 404.
fn serve(routes: Vec<(String, Vec<u8>)>) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut head = Vec::new();
            let mut byte = [0];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            let path = head.split(' ').nth(1).unwrap_or_default();
            let (status, body) = (routes.iter().find(|(p, _)| p == path))
                .map_or(("404 Not Found", &b"Not Found"[..]), |(_, b)| ("200 OK", b));
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(&[reply.as_bytes(), body].concat());
        }
    });
    url
}

/// A first run downloads an archive and a checksum from the same release, so a replaced pair still matches: a
/// signed-in gh must verify the build attestation before the binary is unpacked, as `docs/public/install` does.
#[test]
fn shim_checks_the_build_attestation_before_it_unpacks_a_download() {
    let Ok(php) = which("php") else { return };
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        _ => "unknown-linux-musl",
    };
    let target = format!("{}-{os}", std::env::consts::ARCH);
    let version = env!("CARGO_PKG_VERSION");

    let (server, path) = release(&target, version);
    let name = format!("punphp-{target}.tar.gz");

    let dir = scratch("composer", "attestation");
    // Only the tools the shim runs, so the machine's own gh never answers.
    let tools = dir.join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    for tool in ["tar", "gzip", "rm"] {
        std::os::unix::fs::symlink(which(tool).unwrap(), tools.join(tool)).unwrap();
    }
    let log = dir.join("gh.log");
    let gh = |verify: u8| fake_gh(&dir, &log, &tools, verify);
    let shim = |case: &str, path: &str, env: &[(&str, &str)]| {
        let bin = dir.join(case);
        std::fs::create_dir_all(&bin).unwrap();
        let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../composer/bin/pun");
        std::fs::copy(&shim, bin.join("pun")).unwrap();
        let out = run(Command::new(&php)
            .arg(bin.join("pun"))
            .arg("--version")
            .env("PATH", path)
            .env("PUN_GITHUB_URL", &server)
            .env_remove("PUN_REQUIRE_ATTESTATION")
            .envs(env.iter().copied()));
        let installed = bin.join(format!("pun-{version}-{target}")).exists();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr,
            installed,
        )
    };
    let downloading = format!("pun: downloading {server}{path}\n");

    // (exit code, stdout, installed) and stderr after the downloading line.
    let ran = (Some(0), "downloaded pun --version\n".to_owned(), true);
    let outcome = |out: (Option<i32>, String, String, bool)| ((out.0, out.1, out.3), out.2);
    let tampered = format!(
        "{downloading}pun: the build attestation of {name} does not verify; the download may have been tampered with\n"
    );
    assert_eq!(
        outcome(shim("tampered", &gh(1), &[])),
        ((Some(2), String::new(), false), tampered)
    );
    // Every argument but the third, the archive's path in the shim's temporary directory.
    let log = std::fs::read_to_string(&log).unwrap();
    let args: Vec<&str> = log.lines().collect();
    assert_eq!(
        [&args[..2], &args[3..]].concat().join(" "),
        format!(
            "attestation verify --repo shipfastlabs/pun --signer-workflow \
             github.com/shipfastlabs/pun/.github/workflows/release.yml --source-ref refs/tags/v{version} \
             --deny-self-hosted-runners"
        )
    );

    let tools = tools.display().to_string();
    let required = format!(
        "{downloading}pun: PUN_REQUIRE_ATTESTATION=1 needs the GitHub CLI (gh), signed in\n"
    );
    let env = [("PUN_REQUIRE_ATTESTATION", "1")];
    assert_eq!(
        outcome(shim("required", &tools, &env)),
        ((Some(2), String::new(), false), required.clone())
    );
    let verified = format!("{downloading}pun: verified the GitHub build attestation\n");
    assert_eq!(
        outcome(shim("verified", &gh(0), &[])),
        (ran.clone(), verified)
    );
    // Without gh the download installs as it always has.
    assert_eq!(
        outcome(shim("no-gh", &tools, &[])),
        (ran.clone(), downloading.clone())
    );

    // A dependency's bin proxy at the binary's name in the shared vendor/bin is a script, not Pun: it is downloaded over.
    let claimed = dir.join("claimed");
    std::fs::create_dir_all(&claimed).unwrap();
    std::fs::write(
        claimed.join(format!("pun-{version}-{target}")),
        "#!/usr/bin/env php\n<?php\necho \"impostor\\n\";\n",
    )
    .unwrap();
    assert_eq!(outcome(shim("claimed", &tools, &[])), (ran, downloading));
    // Composer scripts put vendor/bin first in PATH; a dependency's `gh` there does not vouch for the download.
    let with_gh = dir.join("bin-gh");
    std::fs::create_dir_all(&with_gh).unwrap();
    std::fs::copy(dir.join("gh-0/gh"), with_gh.join("gh")).unwrap();
    let path = format!("{}:{tools}", with_gh.display());
    assert_eq!(
        outcome(shim("bin-gh", &path, &env)),
        ((Some(2), String::new(), false), required)
    );
}

/// A local release `v<version>` holding a `pun` script for `target` and its checksum, as cargo-dist writes it:
/// (server URL, archive path).
fn release(target: &str, version: &str) -> (String, String) {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        vec![],
        flate2::Compression::fast(),
    ));
    let script = b"#!/bin/sh\necho downloaded pun \"$@\"\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(script.len() as u64);
    header.set_mode(0o755);
    (tar.append_data(&mut header, format!("punphp-{target}/pun"), &script[..])).unwrap();
    let archive = tar.into_inner().unwrap().finish().unwrap();
    let digest = Sha256::digest(&archive)
        .iter()
        .fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        });
    let name = format!("punphp-{target}.tar.gz");
    let path = format!("/shipfastlabs/pun/releases/download/v{version}/{name}");
    let sum = format!("{digest} *{name}\n").into_bytes();
    let server = serve(vec![
        (path.clone(), archive),
        (format!("{path}.sha256"), sum),
    ]);
    (server, path)
}

/// A PATH with a signed-in `gh` that logs its `attestation verify` arguments to `log` and exits `verify`, then `tools`.
fn fake_gh(dir: &Path, log: &Path, tools: &Path, verify: u8) -> String {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join(format!("gh-{verify}"));
    std::fs::create_dir_all(&bin).unwrap();
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = auth ] && exit 0\nprintf '%s\\n' \"$@\" > '{}'\nexit {verify}\n",
        log.display()
    );
    std::fs::write(bin.join("gh"), script).unwrap();
    std::fs::set_permissions(bin.join("gh"), PermissionsExt::from_mode(0o755)).unwrap();
    format!("{}:{}", bin.display(), tools.display())
}

fn which(tool: &str) -> Result<std::path::PathBuf, ()> {
    let out = Command::new("sh")
        .args(["-c", &format!("command -v {tool}")])
        .output()
        .map_err(drop)?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() && !path.is_empty() {
        Ok(path.into())
    } else {
        Err(())
    }
}
