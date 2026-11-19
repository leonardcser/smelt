use super::*;
use std::path::{Path, PathBuf};
use std::process::Output;

/// Runs `script` with `sh -c` inside a sandbox that may write only `writable`.
/// `None` when this machine has no backend, so the test has nothing to check.
fn run(writable: &[&Path], script: &str) -> Option<Output> {
    let policy = Policy {
        writable: Some(paths(writable)),
        ..Policy::default()
    };
    run_with(&policy, script)
}

fn paths(paths: &[&Path]) -> Vec<PathBuf> {
    paths.iter().map(|path| path.to_path_buf()).collect()
}

fn run_with(policy: &Policy, script: &str) -> Option<Output> {
    if let Err(reason) = backend() {
        eprintln!("sandbox unavailable, skipping: {reason}");
        return None;
    }
    let output = command("sh", policy)
        .unwrap()
        .arg("-c")
        .arg(script)
        .output()
        .unwrap();
    Some(output)
}

fn succeeded(output: &Output) -> bool {
    output.status.success()
}

#[test]
fn writes_inside_a_writable_directory_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let script = format!(
        "mkdir {0}/sub && echo hi > {0}/sub/a && mv {0}/sub/a {0}/b && rm {0}/b && rmdir {0}/sub",
        dir.path().display()
    );
    let Some(output) = run(&[dir.path()], &script) else {
        return;
    };
    assert!(succeeded(&output), "{output:?}");
}

#[test]
fn writes_outside_writable_directories_fail() {
    let allowed = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let target = other.path().join("a");
    let Some(output) = run(
        &[allowed.path()],
        &format!("echo hi > {}", target.display()),
    ) else {
        return;
    };
    assert!(!succeeded(&output));
    assert!(!target.exists());
}

#[test]
fn deleting_outside_writable_directories_fails() {
    let other = tempfile::tempdir().unwrap();
    let target = other.path().join("keep");
    std::fs::write(&target, "data").unwrap();
    let Some(output) = run(&[], &format!("rm -f {}", target.display())) else {
        return;
    };
    assert!(!succeeded(&output));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "data");
}

#[test]
fn reads_stay_unrestricted() {
    let other = tempfile::tempdir().unwrap();
    let target = other.path().join("a");
    std::fs::write(&target, "visible").unwrap();
    let Some(output) = run(&[], &format!("cat {}", target.display())) else {
        return;
    };
    assert!(succeeded(&output));
    assert_eq!(output.stdout, b"visible");
}

#[test]
fn standard_streams_and_null_device_stay_writable() {
    let Some(output) = run(
        &[],
        "echo out > /dev/stdout && echo err > /dev/stderr && echo x > /dev/null",
    ) else {
        return;
    };
    assert!(succeeded(&output), "{output:?}");
    assert_eq!(output.stdout, b"out\n");
    assert_eq!(output.stderr, b"err\n");
}

#[test]
fn child_processes_inherit_the_sandbox() {
    let other = tempfile::tempdir().unwrap();
    let target = other.path().join("a");
    let script = format!("sh -c 'sh -c \"echo hi > {}\"'", target.display());
    let Some(output) = run(&[], &script) else {
        return;
    };
    assert!(!succeeded(&output));
    assert!(!target.exists());
}

#[test]
fn missing_writable_paths_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let Some(output) = run(
        &[&missing, dir.path()],
        &format!("touch {}/a", dir.path().display()),
    ) else {
        return;
    };
    assert!(succeeded(&output), "{output:?}");
}

#[test]
fn hidden_paths_can_be_neither_read_nor_written() {
    let home = tempfile::tempdir().unwrap();
    let secrets = home.path().join("secrets");
    let open = home.path().join("open");
    std::fs::create_dir(&secrets).unwrap();
    std::fs::create_dir(&open).unwrap();
    std::fs::write(secrets.join("key"), "secret").unwrap();
    std::fs::write(open.join("notes"), "visible").unwrap();
    let policy = Policy {
        hidden: vec![secrets.clone()],
        ..Policy::default()
    };
    let read_secret = format!("cat {}", secrets.join("key").display());
    let Some(output) = run_with(&policy, &read_secret) else {
        return;
    };
    assert!(!succeeded(&output));
    assert!(output.stdout.is_empty());

    let write_secret = format!("echo x > {}", secrets.join("new").display());
    let output = run_with(&policy, &write_secret).unwrap();
    assert!(!succeeded(&output));
    assert!(!secrets.join("new").exists());

    // Writes stay unrestricted outside hidden paths.
    let other = format!(
        "cat {} && echo x > {}",
        open.join("notes").display(),
        open.join("new").display()
    );
    let output = run_with(&policy, &other).unwrap();
    assert!(succeeded(&output), "{output:?}");
    assert_eq!(output.stdout, b"visible");
    assert!(open.join("new").exists());
}

#[test]
fn visible_paths_stay_readable_inside_hidden_ones() {
    let home = tempfile::tempdir().unwrap();
    let ssh = home.path().join("ssh");
    std::fs::create_dir(&ssh).unwrap();
    std::fs::write(ssh.join("id"), "secret").unwrap();
    std::fs::write(ssh.join("known_hosts"), "hosts").unwrap();
    let policy = Policy {
        writable: Some(Vec::new()),
        hidden: vec![ssh.clone()],
        visible: vec![ssh.join("known_hosts")],
        ..Policy::default()
    };
    let Some(output) = run_with(
        &policy,
        &format!("cat {}", ssh.join("known_hosts").display()),
    ) else {
        return;
    };
    assert!(succeeded(&output), "{output:?}");
    assert_eq!(output.stdout, b"hosts");
    let output = run_with(&policy, &format!("cat {}", ssh.join("id").display())).unwrap();
    assert!(!succeeded(&output));
}

#[test]
fn hidden_paths_inside_writable_ones_stay_hidden_without_blocking_new_entries() {
    let workspace = tempfile::tempdir().unwrap();
    let ws = workspace.path();
    std::fs::create_dir(ws.join("secrets")).unwrap();
    std::fs::write(ws.join("secrets/key"), "secret").unwrap();
    std::fs::write(ws.join(".env"), "KEY=secret").unwrap();
    let policy = Policy {
        writable: Some(vec![ws.to_path_buf()]),
        hidden: vec![ws.join("secrets"), ws.join(".env")],
        ..Policy::default()
    };
    let script = format!(
        "echo x > {0}/new && mkdir {0}/target && grep -r KEY {0}",
        ws.display()
    );
    let Some(output) = run_with(&policy, &script) else {
        return;
    };
    assert!(ws.join("new").exists(), "{output:?}");
    assert!(ws.join("target").is_dir(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    for secret in ["secrets/key", ".env"] {
        let script = format!("cat {0} || echo x >> {0}", ws.join(secret).display());
        let output = run_with(&policy, &script).unwrap();
        assert!(!succeeded(&output), "{secret}: {output:?}");
    }
    assert_eq!(
        std::fs::read_to_string(ws.join(".env")).unwrap(),
        "KEY=secret"
    );
}

#[test]
fn hidden_environment_variables_are_removed() {
    let policy = Policy {
        hidden_env: vec!["SMELT_SANDBOX_TEST_KEY".into()],
        ..Policy::default()
    };
    if backend().is_err() {
        return;
    }
    // Inherited, as a key exported in the user's shell is. nextest runs
    // each test in its own process.
    std::env::set_var("SMELT_SANDBOX_TEST_KEY", "secret");
    std::env::set_var("SMELT_SANDBOX_TEST_KEEP", "kept");
    let output = command("sh", &policy)
        .unwrap()
        .arg("-c")
        .arg("echo \"[$SMELT_SANDBOX_TEST_KEY][$SMELT_SANDBOX_TEST_KEEP]\"")
        .output()
        .unwrap();
    assert_eq!(output.stdout, b"[][kept]\n");
}
