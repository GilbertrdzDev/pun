#![cfg(unix)]
mod common;

use common::{clean_env, scratch};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// cargo-dist's target triple for this machine (`dist-workspace.toml` lists the four that ship).
fn target() -> String {
    let os = if cfg!(target_os = "macos") {
        "apple-darwin"
    } else {
        "unknown-linux-musl"
    };
    format!("{}-{os}", std::env::consts::ARCH)
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// A whole HTTP/1.1 response with `body`.
fn response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// A local GitHub that answers each request path with its response, and every other path with 404.
fn serve(routes: Vec<(String, Vec<u8>)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
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
            let not_found = response("404 Not Found", b"Not Found");
            let reply = (routes.iter().find(|(p, _)| p == path)).map_or(&not_found, |(_, r)| r);
            let _ = stream.write_all(reply);
        }
    });
    url
}

/// A release archive laid out as cargo-dist's: `punphp-<target>/` with the licenses and the `pun` binary.
fn archive(pun: Option<&str>) -> Vec<u8> {
    let top = format!("punphp-{}", target());
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        vec![],
        flate2::Compression::fast(),
    ));
    let mut add = |path: String, kind: tar::EntryType, mode: u32, data: &[u8]| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_size(data.len() as u64);
        h.set_mode(mode);
        tar.append_data(&mut h, path, data).unwrap();
    };
    add(format!("{top}/"), tar::EntryType::Directory, 0o755, b"");
    add(
        format!("{top}/LICENSE"),
        tar::EntryType::Regular,
        0o644,
        b"MIT",
    );
    if let Some(script) = pun {
        add(
            format!("{top}/pun"),
            tar::EntryType::Regular,
            0o755,
            script.as_bytes(),
        );
    }
    tar.into_inner().unwrap().finish().unwrap()
}

/// The routes of release `tag` with this archive and, as cargo-dist writes it, `<sha256> *<archive>` beside it.
fn release(tag: &str, archive: &[u8], sum: Option<&str>) -> Vec<(String, Vec<u8>)> {
    let name = format!("punphp-{}.tar.gz", target());
    let digest = sha256(archive);
    let sum = format!("{} *{name}\n\n", sum.unwrap_or(&digest));
    let url = format!("/shipfastlabs/pun/releases/download/{tag}/{name}");
    vec![
        (url.clone(), response("200 OK", archive)),
        (format!("{url}.sha256"), response("200 OK", sum.as_bytes())),
    ]
}

/// A copy of the test's pun, named `name`, alone in a new directory: self-update replaces the copy, never the
/// original. A hard link is enough, since the update renames a new file over the name.
fn install(home: &Path, dir: &str, name: &str) -> PathBuf {
    let exe = home.join(dir).join(name);
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    let pun = Path::new(env!("CARGO_BIN_EXE_pun"));
    if std::fs::hard_link(pun, &exe).is_err() {
        std::fs::copy(pun, &exe).unwrap();
    }
    exe
}

/// A `gh` that is signed in and records the `gh attestation verify` call (its arguments, then the archive), and
/// exits with `verify_exit`.
fn fake_gh(home: &Path, verify_exit: u8) -> PathBuf {
    let bin = home.join(format!("gh-{verify_exit}"));
    std::fs::create_dir_all(&bin).unwrap();
    let log = home.join(format!("gh-{verify_exit}.log"));
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = auth ] && exit 0\nprintf '%s\\n' \"$@\" > '{log}'\n/bin/cp \"$3\" '{log}.archive'\n/bin/ls -ld \"${{3%/*}}\" > '{log}.dir'\nexit {verify_exit}\n",
        log = log.display()
    );
    let gh = bin.join("gh");
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    bin
}

/// Runs `exe self-update ARGS` against `server` with only `path` on PATH: (stderr, exit code).
fn run(
    exe: &Path,
    server: Option<&str>,
    path: &Path,
    env: &[(&str, &str)],
    args: &[&str],
) -> (String, Option<i32>) {
    let mut cmd = Command::new(exe);
    clean_env(&mut cmd, &[])
        .arg("self-update")
        .args(args)
        .env("PUN_AGENT", "0")
        .env("PATH", path)
        .env("TMPDIR", path.parent().unwrap().join("tmp"))
        .envs(env.iter().copied());
    if let Some(url) = server {
        cmd.env("PUN_GITHUB_URL", url);
    }
    let out = cmd.output().unwrap();
    (String::from_utf8(out.stderr).unwrap(), out.status.code())
}

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = (std::fs::read_dir(dir).unwrap().flatten())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A scratch home with an empty PATH directory, a TMPDIR and a copy of pun at bin/pun.
fn setup(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let home = scratch("self-update", name);
    let empty = home.join("empty");
    std::fs::create_dir_all(home.join("tmp")).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    let pun = install(&home, "bin", "pun");
    (home, empty, pun)
}

/// The releases the install tests ask for: v9.9.9 is good, the others are each broken in one way.
fn releases(home: &Path) -> (String, Vec<u8>) {
    let new = archive(Some("#!/bin/sh\necho 'pun 9.9.9'\n"));
    let mut routes = release("v9.9.9", &new, None);
    routes.extend(release("v9.9.8", &new, Some(&"0".repeat(64))));
    routes.extend(release("v9.9.7", &archive(None), None));
    routes.extend(release(
        "v9.9.6",
        &archive(Some("#!/bin/sh\necho 'pun 1.2.3'\n")),
        None,
    ));
    // A pun that, while it is checked, puts a directory where the running pun was.
    let replaced = format!(
        "#!/bin/sh\n/bin/rm '{exe}'\n/bin/mkdir -p '{exe}/x'\necho 'pun 9.9.5'\n",
        exe = home.join("moved/pun").display()
    );
    routes.extend(release("v9.9.5", &archive(Some(&replaced)), None));
    // A response that ends before its Content-Length.
    routes.push((
        format!(
            "/shipfastlabs/pun/releases/download/v9.9.4/punphp-{}.tar.gz",
            target()
        ),
        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\ncut short".to_vec(),
    ));
    (serve(routes), new)
}

#[test]
fn fails_without_a_release() {
    let (home, empty, pun) = setup("none");
    // The Composer package's binary is named pun-<version>-<target> (composer/bin/pun).
    let composer = install(&home, "vendor/bin", &format!("pun-{CURRENT}-{}", target()));
    let (err, code) = run(&composer, None, &empty, &[], &[]);
    assert!(
        err.contains(
            "this pun was installed with Composer; update it with `composer update shipfastlabs/pun`"
        ),
        "{err}"
    );
    assert_eq!(code, Some(2));

    let (err, code) = run(&pun, None, &empty, &[], &["latest"]);
    assert!(err.contains("not a release version: vlatest"), "{err}");
    assert_eq!(code, Some(2));

    // Neither the API nor github.com answers.
    let server = serve(vec![]);
    let (err, code) = run(&pun, Some(&server), &empty, &[], &[]);
    assert!(
        err.contains(&format!(
            "download failed: {server}/shipfastlabs/pun/releases/latest"
        )),
        "{err}"
    );
    assert_eq!(code, Some(2));
}

#[test]
fn finds_the_latest_release() {
    let (_home, empty, pun) = setup("latest");
    let check = |routes: Vec<(String, Vec<u8>)>, args: &[&str]| {
        run(&pun, Some(&serve(routes)), &empty, &[], args)
    };
    let api = "/repos/shipfastlabs/pun/releases";

    let latest = format!(r#"{{"tag_name": "v{CURRENT}", "prerelease": false}}"#);
    let (err, code) = check(
        vec![(
            format!("{api}/latest"),
            response("200 OK", latest.as_bytes()),
        )],
        &[],
    );
    assert_eq!(
        (err.as_str(), code),
        (
            format!("pun {CURRENT} is up to date (the latest release is {CURRENT})\n").as_str(),
            Some(0)
        )
    );

    // GitHub's releases/latest skips pre-releases, so with none stable the newest one comes from the list.
    let list = br#"[{"tag_name": "v99.0.0-beta.1", "prerelease": true}]"#;
    let (err, code) = check(
        vec![(format!("{api}?per_page=1"), response("200 OK", list))],
        &["--check"],
    );
    assert_eq!(
        (err.as_str(), code),
        (
            format!("pun 99.0.0-beta.1 is out (this is {CURRENT}); run `pun self-update` to install it\n").as_str(),
            Some(0)
        )
    );

    // When the API's rate limit is used up: github.com's releases/latest, which redirects to the newest published
    // release. Its feed already lists v99.2.0, a tag whose release is still building; that one is not offered.
    let feed = r#"<entry><link rel="alternate" href="https://github.com/shipfastlabs/pun/releases/tag/v99.2.0"/></entry>"#;
    let redirect = |to: &str| -> Vec<(String, Vec<u8>)> {
        vec![
            (
                format!("{api}/latest"),
                response("403 rate limit exceeded", b"{}"),
            ),
            (
                "/shipfastlabs/pun/releases.atom".into(),
                response("200 OK", feed.as_bytes()),
            ),
            (
                "/shipfastlabs/pun/releases/latest".into(),
                format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .into_bytes(),
            ),
        ]
    };
    let (err, code) = check(
        redirect("https://github.com/shipfastlabs/pun/releases/tag/v99.1.0"),
        &["--check"],
    );
    assert_eq!(
        (err.as_str(), code),
        (
            format!("pun 99.1.0 is out (this is {CURRENT}); run `pun self-update` to install it\n")
                .as_str(),
            Some(0)
        )
    );
    // A repository without releases: the release list is empty, and github.com redirects to the releases page.
    let mut none = redirect("https://github.com/shipfastlabs/pun/releases");
    none.push((format!("{api}?per_page=1"), response("200 OK", b"[]")));
    let (err, code) = check(none, &[]);
    assert!(
        err.contains("could not find the latest release; name one: `pun self-update v0.1.0`"),
        "{err}"
    );
    assert_eq!(code, Some(2));
}

#[test]
fn rejects_a_download_that_does_not_verify() {
    let (home, empty, pun) = setup("verify");
    let (url, new) = releases(&home);
    let server = Some(url.as_str());
    let name = format!("punphp-{}.tar.gz", target());

    let (err, code) = run(&pun, server, &empty, &[], &["9.9.4"]);
    assert!(
        err.contains(&format!(
            "download failed: {url}/shipfastlabs/pun/releases/download/v9.9.4/{name}"
        )),
        "{err}"
    );
    assert_eq!(code, Some(2));

    let (err, code) = run(&pun, server, &empty, &[], &["9.9.8"]);
    assert!(
        err.contains(&format!(
            "checksum mismatch for {name} (expected {}, got {}); the download may be corrupt",
            "0".repeat(64),
            sha256(&new)
        )),
        "{err}"
    );
    assert_eq!(code, Some(2));

    let required = [("PUN_REQUIRE_ATTESTATION", "1")];
    let (err, code) = run(&pun, server, &empty, &required, &["9.9.9"]);
    assert!(
        err.starts_with(&format!(
            "Downloading pun 9.9.9 for {}\n ✔ verified the SHA-256 checksum\n",
            target()
        )),
        "{err}"
    );
    assert!(
        err.contains("PUN_REQUIRE_ATTESTATION=1 needs the GitHub CLI (gh), signed in"),
        "{err}"
    );
    assert_eq!(code, Some(2));

    let (err, code) = run(&pun, server, &fake_gh(&home, 1), &[], &["v9.9.9"]);
    assert!(
        err.contains(&format!("the build attestation of {name} does not verify; the download may have been tampered with")),
        "{err}"
    );
    assert_eq!(code, Some(2));
    assert_eq!(files(&home.join("tmp")), Vec::<String>::new());

    let (err, code) = run(&pun, server, &empty, &[], &["9.9.7"]);
    assert!(
        err.contains(" · skipped the build attestation (needs a signed-in gh)\n"),
        "{err}"
    );
    assert!(
        err.contains(&format!("unpacking {name}: it holds no pun binary")),
        "{err}"
    );
    assert_eq!(code, Some(2));
    assert_eq!(files(&home.join("bin")), ["pun"]);
}

#[test]
fn keeps_the_running_pun_when_the_new_one_fails() {
    let (home, empty, pun) = setup("keep");
    let (server, _) = releases(&home);
    let server = Some(server.as_str());

    let (err, code) = run(&pun, server, &empty, &[], &["9.9.6"]);
    assert!(
        err.contains("the downloaded pun reports 'pun 1.2.3', not 'pun 9.9.6'"),
        "{err}"
    );
    assert_eq!(code, Some(2));
    assert_eq!(files(&home.join("bin")), ["pun"]);

    let moved = install(&home, "moved", "pun");
    let (err, code) = run(&moved, server, &empty, &[], &["9.9.5"]);
    assert!(
        err.contains(&format!("can't replace {}", moved.display())),
        "{err}"
    );
    assert_eq!(code, Some(2));
    assert_eq!(files(&home.join("moved")), ["pun"]);

    // Without write access to its directory (root has it anyway, so the case needs a user without it).
    let locked = install(&home, "locked", "pun");
    let dir = locked.parent().unwrap();
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
    if std::fs::write(dir.join("probe"), "").is_err() {
        let (err, code) = run(&locked, server, &empty, &[], &["9.9.9"]);
        assert!(
            err.contains(&format!("can't write to {}", dir.display())),
            "{err}"
        );
        assert_eq!(code, Some(2));
    }
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

#[test]
fn installs_a_verified_release() {
    let (home, _, pun) = setup("install");
    let (server, new) = releases(&home);
    let tmp = home.join("tmp");

    let (err, code) = run(&pun, Some(&server), &fake_gh(&home, 0), &[], &["9.9.9"]);
    assert_eq!(
        (err.as_str(), code),
        (
            format!(
                "Downloading pun 9.9.9 for {}\n ✔ verified the SHA-256 checksum\n ✔ verified the GitHub build attestation\nUpdated pun {CURRENT} → 9.9.9 at {}\n",
                target(),
                pun.display()
            )
            .as_str(),
            Some(0)
        )
    );
    let out = Command::new(&pun).arg("--version").output().unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "pun 9.9.9\n");
    assert_eq!(files(&home.join("bin")), ["pun"]);
    // The same call as docs/public/install's verify_attestation, on the archive as downloaded.
    let log = std::fs::read_to_string(home.join("gh-0.log")).unwrap();
    let file = log.lines().nth(2).unwrap().to_string();
    assert_eq!(
        log,
        format!(
            "attestation\nverify\n{file}\n--repo\nshipfastlabs/pun\n--signer-workflow\ngithub.com/shipfastlabs/pun/.github/workflows/release.yml\n--source-ref\nrefs/tags/v9.9.9\n--deny-self-hosted-runners\n"
        )
    );
    assert!(file.starts_with(&tmp.display().to_string()), "{file}");
    assert_eq!(std::fs::read(home.join("gh-0.log.archive")).unwrap(), new);
    // The archive gh reads sits in a directory only this user can enter, new for this run.
    let dir = std::fs::read_to_string(home.join("gh-0.log.dir")).unwrap();
    assert!(dir.starts_with("drwx------"), "{dir}");
    assert_eq!(files(&tmp), Vec::<String>::new());
}
