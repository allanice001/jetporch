use std::process::Command;

fn main() {
    // Populate `src/cli/version.rs` with version info from git,
    // via shell script.
    Command::new("git pull");
    let _ = Command::new("./version.sh")
        .status()
        .expect("git version shell script should succeed");
}
