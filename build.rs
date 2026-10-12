use std::{fs, path::Path, process::Command};

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // No timestamps or dirty-tree marker: identical commits produce identical ids.
    // Don't accidentally use a parent repository when building an unpacked tarball.
    let mut id = "dev".to_owned();
    if Path::new(".git").exists() {
        if Path::new(".git").is_file() {
            println!("cargo:rerun-if-changed=.git");
        }
        if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
            println!("cargo:rerun-if-changed={head}");
            if let Ok(contents) = fs::read_to_string(&head)
                && let Some(reference) = contents.trim().strip_prefix("ref: ")
                && let Some(path) = git(&["rev-parse", "--git-path", reference])
            {
                if Path::new(&path).exists() {
                    println!("cargo:rerun-if-changed={path}");
                } else {
                    // Packed refs have no loose file. Watch the nearest existing
                    // ref directory for its creation, not a missing file (Cargo
                    // would rerun on every build), and the packed refs for updates.
                    let mut parent = Path::new(&path).parent();
                    while let Some(dir) = parent {
                        if dir.exists() {
                            println!("cargo:rerun-if-changed={}", dir.display());
                            break;
                        }
                        parent = dir.parent();
                    }
                    if let Some(packed) = git(&["rev-parse", "--git-path", "packed-refs"])
                        && Path::new(&packed).exists()
                    {
                        println!("cargo:rerun-if-changed={packed}");
                    }
                }
            }
        }
        if let Some(commit) = git(&["rev-parse", "--verify", "HEAD"])
            && commit.len() >= 8
            && commit.bytes().all(|b| b.is_ascii_hexdigit())
        {
            id = commit[..8].to_owned();
        }
    }
    println!("cargo:rustc-env=IRIS_BUILD_ID={id}");
}
