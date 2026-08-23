//! MC C1 end-to-end tests over the built binary (docs/09 §8): argv grammar
//! (`/` splitting, --inputs, exit codes 0/1/2), stdin/stdout plumbing,
//! multi-input legs, and the head-of-a-FIFO early-stop — the process must
//! exit after one quad while a writer is still producing.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn graphy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_graphy"))
}

/// Run with the given stdin, returning (exit code, stdout, stderr).
fn run_pipe(args: &[&str], stdin: &str) -> (i32, String, String) {
    let mut child = graphy()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary spawns");
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(stdin.as_bytes())
        .expect("stdin writes");
    let out = child.wait_with_output().expect("binary runs");
    (
        out.status.code().expect("exit code"),
        String::from_utf8(out.stdout).expect("utf-8 stdout"),
        String::from_utf8(out.stderr).expect("utf-8 stderr"),
    )
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let dir =
            std::env::temp_dir().join(format!("graphy-cli-pipe-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write temp file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const NQ: &str = "\
<http://x/a> <http://x/p> \"1\" .
<http://x/a> <http://x/q> \"2\" .
<http://x/b> <http://x/p> \"3\" <http://x/g> .
";

#[test]
fn read_scribe_roundtrip_via_stdin() {
    let (code, stdout, stderr) = run_pipe(&["read", "-c", "nq", "/", "scribe"], NQ);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, NQ);
}

#[test]
fn missing_serializer_appends_scribe() {
    let (code, stdout, _) = run_pipe(&["read", "-c", "nq", "/", "skip", "1"], NQ);
    assert_eq!(code, 0);
    assert_eq!(stdout.lines().count(), 2);
}

#[test]
fn stdin_defaults_to_trig() {
    let trig = "@prefix ex: <http://x/> .\nex:g { ex:s ex:p ex:o . }\n";
    let (code, stdout, stderr) = run_pipe(&["read", "/", "count"], trig);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout.trim(), "1");
}

#[test]
fn pretty_write_compacts_with_input_prefixes() {
    let ttl = "@prefix ex: <http://x/> .\nex:s a ex:C ; ex:p \"v\", \"w\" .\n";
    let (code, stdout, _) = run_pipe(&["read", "-c", "ttl", "/", "write", "-c", "ttl"], ttl);
    assert_eq!(code, 0);
    assert_eq!(
        stdout,
        "@prefix ex: <http://x/> .\n\nex:s a ex:C ;\n\tex:p \"v\", \"w\" .\n"
    );
}

#[test]
fn multi_input_merge_and_concat() {
    let dir = TempDir::new("multi");
    let a = dir.file("a.nq", "<http://x/1> <http://x/p> \"a\" .\n");
    let b = dir.file("b.nq", "<http://x/2> <http://x/p> \"b\" .\n");
    let (a, b) = (a.to_str().expect("utf-8"), b.to_str().expect("utf-8"));

    let (code, stdout, _) = run_pipe(
        &["read", "/", "concat", "/", "scribe", "--inputs", a, b],
        "",
    );
    assert_eq!(code, 0);
    assert_eq!(
        stdout,
        "<http://x/1> <http://x/p> \"a\" .\n<http://x/2> <http://x/p> \"b\" .\n"
    );

    let (code, stdout, _) = run_pipe(&["read", "/", "merge", "/", "count", "--inputs", a, b], "");
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "2");
}

#[test]
fn usage_errors_exit_2() {
    let dir = TempDir::new("usage");
    let a = dir.file("a.nq", "");
    let b = dir.file("b.nq", "");
    let (a, b) = (a.to_str().expect("utf-8"), b.to_str().expect("utf-8"));

    // Several inputs, no junction.
    let (code, _, stderr) = run_pipe(&["read", "/", "scribe", "--inputs", a, b], "");
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("concat or merge"), "stderr: {stderr}");

    // Future verb names its increment.
    let (code, _, stderr) = run_pipe(&["read", "/", "filter", "-x", "; a", "/", "count"], "");
    assert_eq!(code, 2);
    assert!(stderr.contains("C2"), "stderr: {stderr}");

    // Terminal mid-pipeline.
    let (code, _, stderr) = run_pipe(&["read", "/", "count", "/", "skip", "1"], "");
    assert_eq!(code, 2);
    assert!(stderr.contains("end a pipeline"), "stderr: {stderr}");

    // Stray slash.
    let (code, _, _) = run_pipe(&["read", "/", "/", "scribe"], "");
    assert_eq!(code, 2);

    // Unknown option on a verb.
    let (code, _, stderr) = run_pipe(&["read", "--bogus", "/", "count"], "");
    assert_eq!(code, 2);
    assert!(stderr.contains("--bogus"), "stderr: {stderr}");
}

#[test]
fn sizes_accept_scientific_notation() {
    // `skip 4e6 / head 2e6` is a documented graphy.js idiom.
    let (code, stdout, stderr) =
        run_pipe(&["read", "-c", "nq", "/", "skip", "1e0", "/", "count"], NQ);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout.trim(), "2");

    let (code, _, stderr) = run_pipe(
        &["read", "-c", "nq", "/", "head", "2.5e0", "/", "count"],
        NQ,
    );
    assert_eq!(code, 2);
    assert!(stderr.contains("non-negative integer"), "stderr: {stderr}");
}

#[test]
fn parse_errors_exit_1() {
    let (code, _, stderr) = run_pipe(&["read", "-c", "nq", "/", "count"], "not rdf at all\n");
    assert_eq!(code, 1);
    assert!(stderr.contains("line 1"), "stderr: {stderr}");
}

#[test]
fn relax_reports_warnings_and_continues() {
    let dirty = "<http://x/s> <http://x/p> \"ok\" .\nbroken line here\n<http://x/s2> <http://x/p> \"ok2\" .\n";
    let (code, stdout, stderr) = run_pipe(&["read", "-c", "nq", "-r", "/", "count"], dirty);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout.trim(), "2");
    assert!(stderr.contains("warning"), "stderr: {stderr}");
}

#[test]
fn help_surfaces() {
    let out = graphy()
        .args(["help", "pipeline"])
        .output()
        .expect("binary runs");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("chain commands with"));

    let out = graphy()
        .args(["help", "head"])
        .output()
        .expect("binary runs");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("cancels upstream"));

    // -h inside a stage prints that stage's help and exits 0.
    let (code, stdout, _) = run_pipe(&["read", "/", "head", "-h", "/", "scribe"], "");
    assert_eq!(code, 0);
    assert!(stdout.contains("slice the quad stream"));
}

/// The C1 exit-bar test: `head 1` on a FIFO must end the process after one
/// quad while the writer is still producing — bounded input I/O, observable
/// as a broken pipe on the writer side long before the writer's cap.
#[cfg(unix)]
#[test]
fn head_stops_reading_a_fifo() {
    let dir = TempDir::new("fifo");
    let fifo = dir.0.join("stream.nq");
    let status = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo exists on unix");
    assert!(status.success());

    let child = graphy()
        .args([
            "read",
            "-c",
            "nq",
            "/",
            "head",
            "1",
            "/",
            "scribe",
            "--inputs",
            fifo.to_str().expect("utf-8"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary spawns");

    // Opening the FIFO for writing blocks until the reader opens it.
    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&fifo)
        .expect("fifo opens once the pipeline reads");
    let line = b"<http://x/s> <http://x/p> \"v\" .\n";
    const CAP: usize = 10_000_000;
    let mut written = 0usize;
    while written < CAP {
        match writer.write_all(line) {
            Ok(()) => written += 1,
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => break,
            Err(e) => panic!("unexpected fifo error: {e}"),
        }
    }
    drop(writer);
    assert!(
        written < CAP,
        "pipeline never hung up — head did not cancel upstream"
    );
    // One 256 KiB chunk ≈ 8k lines of this shape; anything in that region
    // proves bounded reads (10M lines ≈ 320 MB would take visibly long).
    assert!(written < 100_000, "wrote {written} lines before the hangup");

    let out = child.wait_with_output().expect("pipeline exits");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "<http://x/s> <http://x/p> \"v\" .\n"
    );
}
