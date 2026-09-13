//! Investigation only. A passing characterization test is NOT an S01 pass.
#![cfg(target_os = "macos")]
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
#[test]
fn characterize_kernel_sandbox_after_open_directory_is_moved() {
    let path = std::env::temp_dir().join(format!(
        "directory-ui-sandbox-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&path).unwrap();
    let f = Fixture(path.canonicalize().unwrap());
    let approved = f.0.join("approved");
    let outside = f.0.join("outside");
    fs::create_dir_all(approved.join("child")).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("negative-control"), [0]).unwrap();
    let binary = f.0.join("reader");
    let compilation = Command::new("/usr/bin/clang")
        .args(["-Wno-deprecated-declarations", "-o"])
        .arg(&binary)
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sandbox_reader.c"))
        .output()
        .unwrap();
    assert!(
        compilation.status.success(),
        "test probe compilation failed"
    );
    let mut child = Command::new(&binary)
        .arg(&approved)
        .arg(outside.join("negative-control"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(
        ready.trim(),
        "ready outside_lstat=-1",
        "sandbox must be active and deny external metadata"
    );
    fs::rename(approved.join("child"), outside.join("child")).unwrap();
    fs::write(outside.join("child/external-marker"), [1]).unwrap();
    child.stdin.take().unwrap().write_all(b"g").unwrap();
    let mut output = String::new();
    stdout.read_to_string(&mut output).unwrap();
    assert!(child.wait().unwrap().success());
    println!("Sandbox characterization: {}", output.trim());
    assert_eq!(
        output.trim(),
        "result metadata=-1 errno=1 marker=1",
        "re-evaluate S01 if the OS changes the characterized behavior"
    );
}
