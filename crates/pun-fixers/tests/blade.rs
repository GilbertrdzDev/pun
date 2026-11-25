extern crate punphp_fixers as pun_fixers;
use std::path::{Path, PathBuf};
use std::process::Command;

fn node_modules(crate_dir: &Path) -> Option<PathBuf> {
    let main = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(crate_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .and_then(|git| Some(git.parent()?.join("crates/pun-fixers/node_modules")));
    [Some(crate_dir.join("node_modules")), main]
        .into_iter()
        .flatten()
        .find(|m| m.join("prettier-plugin-blade").exists())
        .map(|m| m.canonicalize().unwrap())
}

fn pipeline() -> pun_format::Pipeline {
    pun_format::Pipeline::new(&pipeline_rules(), &pun_fixers::all).unwrap()
}

fn pipeline_rules() -> Vec<pun_config::Rule> {
    let (_, json) = pun_config::PRESETS
        .iter()
        .find(|(n, _)| *n == "laravel")
        .unwrap();
    let preset: serde_json::Value = serde_json::from_str(json).unwrap();
    let mut rules: Vec<pun_config::Rule> = preset["rules_as_configured"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, v)| **v != serde_json::Value::Bool(false))
        .map(|(name, v)| pun_config::Rule {
            name: name.clone(),
            options: if v.is_boolean() {
                serde_json::Value::Null
            } else {
                v.clone()
            },
        })
        .collect();
    rules.push(pun_config::Rule {
        name: "Pint/laravel_blade".into(),
        options: serde_json::Value::Null,
    });
    rules
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files(&path, out);
        } else if path.to_string_lossy().ends_with(".blade.php") {
            out.push(path);
        }
    }
}

fn run(pipeline: &pun_format::Pipeline, file: &Path, code: &[u8]) -> Vec<u8> {
    match pipeline.run(file, code) {
        Ok(fixed) => fixed.map_or_else(|| code.to_vec(), |f| f.code),
        Err(e) => panic!("{}: {e:?}", file.display()),
    }
}

#[test]
fn oracle_blade_fixtures() {
    if !pun_fixers::all()
        .iter()
        .any(|f| f.name() == "Pint/laravel_blade")
    {
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let modules = node_modules(root).unwrap_or_else(|| {
        panic!(
            "no prettier-plugin-blade in this or the main checkout: run `npm install` in {}",
            root.display()
        )
    });
    let project = std::env::temp_dir().join(format!("pun-blade-project-{}", std::process::id()));
    std::fs::create_dir_all(&project).unwrap();
    let link = project.join("node_modules");
    let _ = std::fs::remove_file(&link);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&modules, &link).unwrap();
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_dir(&modules, &link).is_err() {
            eprintln!("skipping blade oracle: Windows could not link node_modules");
            return;
        }
    }
    let pipeline = pipeline();
    let html: &[u8] = b"<div>\n<p>x</p></div>\n";
    let bare = std::env::temp_dir().join(format!("pun-blade-test-{}", std::process::id()));
    std::fs::create_dir_all(&bare).unwrap();
    std::env::set_current_dir(&bare).unwrap();
    let blade = pipeline
        .fixers()
        .find(|f| f.name() == "Pint/laravel_blade")
        .unwrap();
    let mut tokens = pun_tokens::Tokens::from_code(html);
    let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        blade.fix(Path::new("a.blade.php"), &mut tokens);
    }))
    .unwrap_err();
    let error = error.downcast_ref::<String>().unwrap();
    assert!(
        error.contains("Error: Cannot find module 'prettier'"),
        "{error}"
    );
    assert_eq!(tokens.generate_code(), html);
    std::env::set_current_dir(&project).unwrap();
    assert_eq!(
        pun_fixers::prettier::probe(&[
            "prettier",
            "prettier-plugin-blade",
            "prettier-plugin-tailwindcss",
            "pun-missing-package",
            "fs",
        ]),
        Some(vec![
            Some("3.10.1".into()),
            Some("3.3.3".into()),
            Some("0.8.1".into()),
            None,
            Some(String::new()),
        ])
    );
    assert_eq!(run(&pipeline, Path::new("a.php"), html), html);
    let mut all = Vec::new();
    files(&root.join("tests/blade"), &mut all);
    all.sort();
    let mut fail = Vec::new();
    for input in &all {
        let rel = input.strip_prefix(root).unwrap();
        let code = std::fs::read(input).unwrap();
        let expected = std::fs::read(format!("{}.expected", input.display())).unwrap();
        let got = run(&pipeline, rel, &code);
        if got != expected {
            fail.push(format!(
                "{} (input)\n{}",
                rel.display(),
                String::from_utf8_lossy(&got)
            ));
        }
        let again = run(&pipeline, rel, &expected);
        if again != expected {
            fail.push(format!(
                "{} (expected changed)\n{}",
                rel.display(),
                String::from_utf8_lossy(&again)
            ));
        }
    }
    let mut blade_only = pun_format::Pipeline::new(
        &[pun_config::Rule {
            name: "Pint/laravel_blade".into(),
            options: serde_json::Value::Null,
        }],
        &pun_fixers::all,
    )
    .unwrap();
    let embedded: &[u8] = b"<div>\n    @php\n        $a = array(1,2);\n        if($a){ $b = 1; }\n    @endphp\n</div>\n";
    let body = b"        if ($a) {\n            $b = 1;\n        }\n    @endphp\n</div>\n";
    let file = Path::new("a.blade.php");
    assert_eq!(
        run(&blade_only, file, embedded),
        [&b"<div>\n    @php\n        $a = [1, 2];\n"[..], body].concat()
    );
    blade_only.set_preset("psr12");
    assert_eq!(
        run(&blade_only, file, embedded),
        [&b"<div>\n    @php\n        $a = array(1,2);\n"[..], body].concat()
    );
    std::env::set_current_dir(root).unwrap();
    std::fs::remove_dir_all(&project).unwrap();
    std::fs::remove_dir_all(&bare).unwrap();
    assert!(
        fail.is_empty(),
        "{} of {} failed:\n{}",
        fail.len(),
        all.len() * 2,
        fail.join("\n")
    );
}

/// The golden files outside tests/blade/tailwind whose expected output sorts Tailwind classes.
const SORTS_CLASSES: &[&str] = &[
    "attributes/single-collapse",
    "html/li-in-li",
    "html/li-in-template",
    "kitchen-sink/dashboard",
    "kitchen-sink/everything",
    "kitchen-sink/teams",
];

/// `[format] blade = "native"` against the same golden files, without Node. Tailwind class sorting is not ported,
/// so the fixtures that sort classes are left out.
#[test]
fn native_blade_fixtures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut rules = pipeline_rules();
    for r in rules.iter_mut().filter(|r| r.name == "Pint/laravel_blade") {
        r.options = serde_json::json!({ "engine": "native" });
    }
    let pipeline = pun_format::Pipeline::new(&rules, &pun_fixers::all).unwrap();
    let mut all = Vec::new();
    files(&root.join("tests/blade"), &mut all);
    let blade = root.join("tests/blade");
    all.retain(|f| {
        let rel = f.strip_prefix(&blade).unwrap().to_string_lossy();
        let name = rel.strip_suffix(".blade.php").unwrap();
        !rel.starts_with("tailwind/") && !SORTS_CLASSES.contains(&name)
    });
    all.sort();
    let mut fail = Vec::new();
    for input in &all {
        let rel = input.strip_prefix(root).unwrap();
        let code = std::fs::read(input).unwrap();
        let expected = std::fs::read(format!("{}.expected", input.display())).unwrap();
        for (what, from) in [("input", &code), ("expected changed", &expected)] {
            let got = run(&pipeline, rel, from);
            if got != expected {
                fail.push(format!(
                    "{} ({what})\n{}",
                    rel.display(),
                    String::from_utf8_lossy(&got)
                ));
            }
        }
    }
    assert!(!all.is_empty());
    assert!(
        fail.is_empty(),
        "{} of {} failed:\n{}",
        fail.len(),
        all.len() * 2,
        fail.join("\n")
    );
}

#[test]
fn failed_preload_rejects_the_probe() {
    if std::env::var_os("PUN_BLADE_PROBE_CHILD").is_some() {
        assert_eq!(pun_fixers::prettier::probe(&["prettier"]), None);
        return;
    }
    for (name, script) in [
        (
            "closed",
            "process.stderr.write('preload failed'); process.exit(1);",
        ),
        (
            "invalid",
            "process.stdout.write('not json\\n'); process.exit(0);",
        ),
        (
            "silent",
            "Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0);",
        ),
    ] {
        let dir = std::env::temp_dir().join(format!("pun-probe-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let preload = dir.join("preload.cjs");
        std::fs::write(
            &preload,
            format!("require('fs').writeFileSync('started', 'yes'); {script}"),
        )
        .unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "failed_preload_rejects_the_probe"])
            .env("PUN_BLADE_PROBE_CHILD", "1")
            .env("NODE_OPTIONS", format!("--require={}", preload.display()))
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(result.status.success(), "{name}: {result:?}");
        assert_eq!(std::fs::read(dir.join("started")).unwrap(), b"yes");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
