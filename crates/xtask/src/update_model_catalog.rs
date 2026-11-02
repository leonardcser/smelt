//! Regenerate the models.dev snapshot embedded in `smelt-provider`.

use std::path::PathBuf;
use std::process::Command;

const SNAPSHOT: &str = "crates/provider/data/models.catalog";

pub(crate) fn run(args: Vec<String>) {
    let json = match args.first() {
        Some(path) => std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}")),
        None => download(),
    };
    let snapshot =
        smelt_provider::catalog::write_snapshot(&json).expect("input is not a models.dev catalog");
    let path = crate::repo_root().join(SNAPSHOT);
    std::fs::write(&path, &snapshot).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    eprintln!(
        "wrote {} ({} KB, provider-indexed catalog)",
        PathBuf::from(SNAPSHOT).display(),
        snapshot.len() / 1024
    );
}

fn download() -> String {
    let output = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .arg(smelt_provider::catalog::MODELS_API_URL)
        .output()
        .expect("run curl");
    if !output.status.success() {
        panic!(
            "download {}: {}",
            smelt_provider::catalog::MODELS_API_URL,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).expect("catalog is UTF-8")
}
