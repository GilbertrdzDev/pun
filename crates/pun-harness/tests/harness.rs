#![cfg(unix)]
//! Runs the harness binary against a throwaway root (`PUN_HARNESS_ROOT`) that borrows the real
//! .oracle/ and scripts/ but has its own two-project corpus. Expected counts and diffs are
//! hand-derived from the corpus below: Pint (laravel preset) turns `array(...)` into `[...]`.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn script(path: &Path, body: &str) {
    write(path, &format!("#!/bin/sh\n{body}\n"));
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// root/{.oracle,scripts} -> real ones; corpus a (pint.json with `exclude`, `node_modules`, a
/// symlink) and b (no pint.json); a scoreboard with one foreign row and a stale row for `a`.
fn root(name: &str) -> PathBuf {
    let r = std::env::temp_dir().join(format!("pun-harness-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&r);
    fs::create_dir_all(&r).unwrap();
    let r = r.canonicalize().unwrap();
    symlink(repo().join(".oracle"), r.join(".oracle")).unwrap();
    symlink(repo().join("scripts"), r.join("scripts")).unwrap();
    write(
        &r.join("corpus/manifest.toml"),
        "[[project]]\nname = \"a\"\n\n[[project]]\nname = \"b\"\n",
    );
    let a = r.join("corpus/a");
    fs::create_dir_all(a.join(".git")).unwrap();
    write(
        &a.join("pint.json"),
        r#"{"preset":"laravel","exclude":["skip"]}"#,
    );
    write(&a.join("app.php"), "<?php\n\n$a = array(1);   \n");
    write(&a.join("ok.php"), "<?php\n\n$a = [1];\n");
    write(&a.join("skip/x.php"), "<?php\n\n$b = array(2);\n");
    write(&a.join("readme.txt"), "hi\n");
    symlink("readme.txt", a.join("notes")).unwrap();
    write(&a.join("node_modules/m/index.js"), "x\n");
    let b = r.join("corpus/b");
    fs::create_dir_all(b.join(".git")).unwrap();
    write(&b.join("b.php"), "<?php\n\n$x = array();\n");
    write(
        &r.join("context/reports/scoreboard.md"),
        "| project | x |\n|---|---|\n| a | stale |\n| zz | 1 | 1 | 1 | 0 | 0 | 0 |\n\n## Benchmarks\n\n| zz | kept |\n",
    );
    r
}

fn harness(root: &Path, pun: &Path, args: &[&str]) -> (String, String, Option<i32>) {
    let out = Command::new(env!("CARGO_BIN_EXE_pun-harness"))
        .arg("format")
        .args(args)
        .env("PUN_HARNESS_ROOT", root)
        .env("PUN_BIN", pun)
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

#[test]
fn full_run_with_failing_pun() {
    let r = root("full");
    let pun = r.join("fake-pun");
    script(&pun, "echo new > new.txt; exit 3");
    let (out, err, code) = harness(&r, &pun, &["--diffs", "1"]);
    let want = "a: 6 files, 4 identical, 2 differing (pint changed 1, pun changed 1)\n\
                --- oracle/app.php\n+++ pun/app.php\n@@ -1,3 +1,3 @@\n <?php\n \n-$a = [1];\n+$a = array(1);   \n\
                b: 2 files, 0 identical, 2 differing (pint changed 1, pun changed 1)\n\
                --- oracle/b.php\n+++ pun/b.php\n@@ -1,3 +1,3 @@\n <?php\n \n-$x = [];\n+$x = array();\n";
    assert_eq!(out, want, "stderr: {err}");
    assert_eq!(code, Some(1));
    assert_eq!(
        err.matches("warning: pun exited exit status: 3").count(),
        2,
        "{err}"
    );
    let board = fs::read_to_string(r.join("context/reports/scoreboard.md")).unwrap();
    assert!(board.ends_with(
        "| project | files | identical | differing | pint changed | pun changed |\n|---|---|---|---|---|---|\n\
         | a | 6 | 4 | 2 | 1 | 1 |\n| b | 2 | 0 | 2 | 1 | 1 |\n| zz | 1 | 1 | 1 | 0 | 0 | 0 |\n\n## Benchmarks\n\n| zz | kept |\n"
    ), "{board}");
    fs::remove_dir_all(&r).unwrap();
}

#[test]
fn single_fixer_identical() {
    let r = root("fixer");
    let pun = r.join("fake-pun");
    script(
        &pun,
        "[ -f app.php ] && printf '<?php\\n\\n$a = [1];   \\n' > app.php; [ -f b.php ] && printf '<?php\\n\\n$x = [];\\n' > b.php; true",
    );
    let before = fs::read_to_string(r.join("context/reports/scoreboard.md")).unwrap();
    let (out, err, code) = harness(&r, &pun, &["--fixer", "array_syntax"]);
    assert_eq!(
        out,
        "a: 5 files, 5 identical, 0 differing (pint changed 1, pun changed 1)\nb: 1 files, 1 identical, 0 differing (pint changed 1, pun changed 1)\n",
        "stderr: {err}"
    );
    assert_eq!(code, Some(0));
    assert_eq!(
        fs::read_to_string(r.join("context/reports/scoreboard.md")).unwrap(),
        before
    );

    fs::remove_file(r.join("context/reports/scoreboard.md")).unwrap();
    let (out, err, code) = harness(
        &r,
        &pun,
        &["--corpus", "b", "--fixer", "no_trailing_whitespace"],
    );
    assert_eq!(
        out,
        "b: 1 files, 0 identical, 1 differing (pint changed 0, pun changed 1)\n--- oracle/b.php\n+++ pun/b.php\n@@ -1,3 +1,3 @@\n <?php\n \n-$x = array();\n+$x = [];\n",
        "stderr: {err}"
    );
    assert_eq!(code, Some(1));
    assert!(!r.join("context/reports/scoreboard.md").exists());
    fs::remove_dir_all(&r).unwrap();
}

#[test]
fn preset_override() {
    let r = root("preset");
    let pun = r.join("fake-pun");
    script(
        &pun,
        "[ \"$*\" = 'migrate --from pint --preset psr12' ] && printf '<?php\\n\\n$a = array(1);\\n' > app.php; true",
    );
    let before = fs::read_to_string(r.join("context/reports/scoreboard.md")).unwrap();
    let (out, err, code) = harness(&r, &pun, &["--corpus", "a", "--preset", "psr12"]);
    assert_eq!(
        out, "a: 5 files, 5 identical, 0 differing (pint changed 1, pun changed 1)\n",
        "stderr: {err}"
    );
    assert_eq!(code, Some(0));
    assert_eq!(
        fs::read_to_string(r.join("context/reports/scoreboard.md")).unwrap(),
        before
    );
    fs::remove_dir_all(&r).unwrap();
}

#[test]
fn projects_without_pint_json_migrate_pint_defaults() {
    let r = root("defaults");
    let pun = r.join("fake-pun");
    let log = r.join("migrate.log");
    script(
        &pun,
        &format!(
            "[ \"$1\" = migrate ] && {{ echo \"$*\" >> {0}; [ \"$4\" = --config ] && cat \"$5\" >> {0}; }}; true",
            log.display()
        ),
    );
    harness(&r, &pun, &[]);
    let got = fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = got.lines().collect();
    assert_eq!(lines.len(), 3, "{got}");
    assert_eq!(lines[0], "migrate --from pint");
    let cfg = lines[1]
        .strip_prefix("migrate --from pint --config ")
        .unwrap();
    assert!(cfg.ends_with("/b/pint-defaults.json"), "{got}");
    assert!(!cfg.contains("/b/pun/"), "{got}");
    assert_eq!(lines[2], "{}");
    fs::remove_dir_all(&r).unwrap();
}

#[test]
fn missing_pieces() {
    let r = root("missing");
    let (out, err, code) = harness(&r, &r.join("no-such-pun"), &["--corpus", "b"]);
    assert_eq!(
        out,
        "b: 1 files, 0 identical, 1 differing (pint changed 1, pun changed 0)\n--- oracle/b.php\n+++ pun/b.php\n@@ -1,3 +1,3 @@\n <?php\n \n-$x = [];\n+$x = array();\n"
    );
    assert_eq!(
        err,
        format!(
            "note: {} missing, treating pun as identity\n",
            r.join("no-such-pun").display()
        )
    );
    assert_eq!(code, Some(1));

    write(&r.join("not-exec"), "");
    let tmp = r.join("tmp");
    fs::create_dir(&tmp).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_pun-harness"))
        .args(["format", "--corpus", "b"])
        .env("PUN_HARNESS_ROOT", &r)
        .env("PUN_BIN", r.join("not-exec"))
        .env("TMPDIR", &tmp)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.starts_with("Error: spawning pun migrate\n"), "{err}");
    assert_eq!(out.status.code(), Some(1));
    let left: Vec<_> = fs::read_dir(&tmp)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "left in {tmp:?}: {left:?}");

    let out = Command::new(env!("CARGO_BIN_EXE_pun-harness"))
        .args(["format", "--corpus", "b"])
        .env("PUN_HARNESS_ROOT", &r)
        .env("PUN_BIN", r.join("no-such-pun"))
        .env("PATH", "")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Error: spawning pint\n"), "{err}");
    assert_eq!(out.status.code(), Some(1));

    fs::remove_dir_all(r.join("corpus/b/.git")).unwrap();
    let (_, err, code) = harness(&r, &r.join("no-such-pun"), &["--corpus", "b"]);
    assert!(
        err.ends_with(&format!(
            "Error: {} missing: run scripts/corpus.sh\n",
            r.join("corpus/b").display()
        )),
        "{err}"
    );
    assert_eq!(code, Some(1));

    fs::remove_file(r.join(".oracle")).unwrap();
    let (_, err, code) = harness(&r, &r.join("no-such-pun"), &[]);
    assert_eq!(
        err,
        format!(
            "Error: {} missing: run scripts/oracle.sh\n",
            r.join(".oracle/pint/builds/pint").display()
        )
    );
    assert_eq!(code, Some(1));
    fs::remove_dir_all(&r).unwrap();
}

/// The scratch copies live in a new owner-only directory, and a manifest name that leaves `corpus/` (and with it the
/// scratch directory, which the run deletes) is refused before anything is copied or removed.
#[test]
fn scratch_is_private_and_names_stay_inside_it() {
    let r = root("scratch");
    let tmp = r.join("tmp");
    fs::create_dir(&tmp).unwrap();
    let pun = r.join("fake-pun");
    let log = r.join("mode.log");
    // Pun runs in <scratch>/<project>/pun.
    script(&pun, &format!("stat -c %a ../.. >> {}", log.display()));
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_pun-harness"))
            .args(["format", "--corpus", "b", "--fixer", "array_syntax"])
            .env("PUN_HARNESS_ROOT", &r)
            .env("PUN_BIN", &pun)
            .env("TMPDIR", &tmp)
            .output()
            .unwrap()
    };
    assert_eq!(run().status.code(), Some(1));
    assert_eq!(
        fs::read_to_string(&log).unwrap().lines().next(),
        Some("700")
    );
    let out = Command::new(env!("CARGO_BIN_EXE_pun-harness"))
        .arg("format")
        .env("PUN_HARNESS_ROOT", &r)
        .env("PUN_BIN", &pun)
        .env("TMPDIR", r.join("no-such-dir"))
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with(&format!(
            "Error: creating {}/pun-harness-",
            r.join("no-such-dir").display()
        )),
        "{err}"
    );

    // `../victim` resolves to a checkout beside corpus/, and from the scratch directory to tmp/victim.
    write(
        &r.join("corpus/manifest.toml"),
        "[[project]]\nname = \"../victim\"\n",
    );
    fs::create_dir_all(r.join("victim/.git")).unwrap();
    write(&tmp.join("victim/keep.txt"), "mine\n");
    let out = Command::new(env!("CARGO_BIN_EXE_pun-harness"))
        .arg("format")
        .env("PUN_HARNESS_ROOT", &r)
        .env("PUN_BIN", &pun)
        .env("TMPDIR", &tmp)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: corpus/manifest.toml: not a corpus name: ../victim\n"
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        fs::read_to_string(tmp.join("victim/keep.txt")).unwrap(),
        "mine\n"
    );
    fs::remove_dir_all(&r).unwrap();
}
