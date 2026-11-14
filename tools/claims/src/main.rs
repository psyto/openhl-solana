//! Re-measures a chapter's "delete this check: is it caught?" table.
//!
//! A chapter states, for each check in the program, what happens when it is
//! deleted: the fuzzer finds it, a test fails, or something else refuses the
//! call instead. Those rows were measured once, by hand. This tool reads the
//! same rows from a `claims.toml`, deletes each check in turn, rebuilds, runs
//! the tests, the fuzzer and the probe, and fails if any row no longer reads
//! the way the chapter says. Run it in CI and the table cannot drift from the
//! code without someone noticing.
//!
//!     cargo run --release --manifest-path tools/claims/Cargo.toml -- \
//!         fuzz/positions/claims.toml [--only <text>] [--no-baseline]

use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Deserialize)]
struct Claims {
    /// The repository root, relative to the claims file. Every path and
    /// command below is taken from there.
    root: String,
    /// Chapters that print the table; every `row` must appear in each.
    docs: Vec<String>,
    /// Run once before anything else (building the harness).
    prepare: Vec<String>,
    /// Rebuilds the program after each deletion.
    build: String,
    /// Runs the tests; failures are read from `test <name> ... FAILED`.
    test: String,
    /// Runs the fuzzer; findings are lines starting `[FUZZ_FINDING] [`.
    fuzz: String,
    fuzz_seconds: u64,
    /// Sends each spec once per variant; lines starting `PROBE `.
    probe: String,
    claim: Vec<Claim>,
}

#[derive(Deserialize)]
struct Claim {
    /// The row as the chapter prints it.
    row: String,
    /// The same row in a translation, where it reads differently.
    #[serde(default)]
    also: Vec<String>,
    file: String,
    /// The deletion is made inside this function, at the first `find` after
    /// its signature.
    in_fn: String,
    find: String,
    replace: String,
    /// Text one of the fuzzer's findings must contain, every piece of it;
    /// absent means no finding.
    fuzz: Option<Vec<String>>,
    /// Tests that must fail; every other test must pass.
    #[serde(default)]
    tests_fail: Vec<String>,
    /// For a row the fuzzer does not catch: what refused the call instead.
    refused: Option<Refused>,
}

#[derive(Deserialize)]
struct Refused {
    /// Picks the probe line: `<spec> | <variant>`.
    probe: String,
    /// Text that line must contain.
    log: String,
}

struct Outcome {
    findings: Vec<String>,
    failed_tests: Vec<String>,
    probe: Vec<String>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: claims <claims.toml> [--only <text>] [--no-baseline]");
        std::process::exit(2);
    };
    let only = args.iter().position(|a| a == "--only").and_then(|i| args.get(i + 1));
    let baseline = !args.iter().any(|a| a == "--no-baseline");

    let text = std::fs::read_to_string(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
    let claims: Claims = toml::from_str(&text).unwrap_or_else(|e| die(&format!("{path}: {e}")));
    let root = Path::new(path).parent().unwrap_or(Path::new(".")).join(&claims.root);
    let root = root.canonicalize().unwrap_or_else(|e| die(&format!("{}: {e}", root.display())));

    // A run stopped half-way leaves a backup; put the original back first.
    for c in &claims.claim {
        restore_backup(&root.join(&c.file));
    }

    let mut bad = 0;
    for doc in &claims.docs {
        let body = std::fs::read_to_string(root.join(doc)).unwrap_or_else(|e| die(&format!("{doc}: {e}")));
        for c in claims.claim.iter().filter(|c| !std::iter::once(&c.row).chain(&c.also).any(|r| body.contains(r.as_str()))) {
            println!("MISSING  {doc} has no row {}", c.row);
            bad += 1;
        }
    }

    for cmd in &claims.prepare {
        run_ok(&root, cmd);
    }
    if baseline {
        run_ok(&root, &claims.build);
        let out = measure(&root, &claims, None);
        if !out.findings.is_empty() || !out.failed_tests.is_empty() {
            println!("BASELINE the program as it is: findings {:?}, failed tests {:?}", out.findings, out.failed_tests);
            std::process::exit(1);
        }
        println!("baseline: no finding, every test passes");
    }

    for c in claims.claim.iter().filter(|c| only.is_none_or(|o| c.row.contains(o.as_str()))) {
        let file = root.join(&c.file);
        let original = std::fs::read_to_string(&file).unwrap_or_else(|e| die(&format!("{}: {e}", c.file)));
        let mutated = delete_check(&original, c).unwrap_or_else(|e| die(&format!("{}: {e}", c.row)));
        let _guard = Restore::new(&file, &original);
        std::fs::write(&file, mutated).unwrap_or_else(|e| die(&format!("{}: {e}", c.file)));
        run_ok(&root, &claims.build);
        let out = measure(&root, &claims, Some(c));
        drop(_guard);

        let problems = compare(c, &out);
        if problems.is_empty() {
            println!("ok       {}", c.row);
        } else {
            bad += 1;
            println!("MISMATCH {}", c.row);
            for p in problems {
                println!("           {p}");
            }
        }
    }
    // Leave the program built as it is in the repository.
    run_ok(&root, &claims.build);

    if bad > 0 {
        println!("{bad} row(s) no longer read the way the chapter says");
        std::process::exit(1);
    }
}

/// Replaces the first `find` inside `in_fn` (up to the next top-level `fn`).
fn delete_check(src: &str, c: &Claim) -> Result<String, String> {
    let start = src.find(&c.in_fn).ok_or(format!("no `{}`", c.in_fn))?;
    let body_end = src[start + 1..].find("\nfn ").map_or(src.len(), |i| start + 1 + i);
    let at = src[start..body_end]
        .find(&c.find)
        .ok_or(format!("`{}` is not in `{}`", c.find, c.in_fn))?;
    let at = start + at;
    Ok(format!("{}{}{}", &src[..at], c.replace, &src[at + c.find.len()..]))
}

fn measure(root: &Path, claims: &Claims, c: Option<&Claim>) -> Outcome {
    let tests = output(root, &claims.test);
    if !tests.contains("test result:") {
        die(&format!("the tests did not run:\n{tests}"));
    }
    let failed_tests = tests
        .lines()
        .filter_map(|l| l.strip_prefix("test ")?.strip_suffix(" ... FAILED"))
        .map(|n| n.rsplit("::").next().unwrap_or(n).to_string())
        .collect();
    let expect = c.and_then(|c| c.fuzz.clone());
    let findings = fuzz(root, &claims.fuzz, claims.fuzz_seconds, expect.as_deref());
    let probe = match c.and_then(|c| c.refused.as_ref()) {
        Some(_) => output(root, &claims.probe).lines().filter(|l| l.starts_with("PROBE ")).map(String::from).collect(),
        None => vec![],
    };
    Outcome { findings, failed_tests, probe }
}

fn compare(c: &Claim, out: &Outcome) -> Vec<String> {
    let mut problems = vec![];
    match &c.fuzz {
        Some(want) if !out.findings.iter().any(|f| matches(f, want)) => {
            problems.push(format!("expected a finding with {want:?}, got {:?}", out.findings));
        }
        None if !out.findings.is_empty() => {
            problems.push(format!("expected no finding, got {:?}", out.findings));
        }
        _ => {}
    }
    let mut want = c.tests_fail.clone();
    want.sort();
    let mut got = out.failed_tests.clone();
    got.sort();
    if want != got {
        problems.push(format!("expected failing tests {want:?}, got {got:?}"));
    }
    if let Some(r) = &c.refused {
        match out.probe.iter().find(|l| l.contains(&r.probe)) {
            None => problems.push(format!("no probe line for `{}`", r.probe)),
            Some(l) if !l.contains("ok=false") || !l.contains(&r.log) => {
                problems.push(format!("expected `{}` refused with `{}`, got: {l}", r.probe, r.log));
            }
            Some(_) => {}
        }
    }
    problems
}

/// Runs the fuzzer for up to `seconds`, stopping early at a finding.
/// Crucible checks its own `--timeout` only every 1,000 iterations
/// (asymmetric-research/crucible#25), so the clock is kept here.
fn matches(finding: &str, want: &[String]) -> bool {
    want.iter().all(|w| finding.contains(w.as_str()))
}

fn fuzz(root: &Path, cmd: &str, seconds: u64, expect: Option<&[String]>) -> Vec<String> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("{cmd} 2>&1"))
        .current_dir(root)
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap_or_else(|e| die(&format!("{cmd}: {e}")));
    let stdout = child.stdout.take().expect("piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut findings = vec![];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if line.starts_with("[FUZZ_FINDING] [") => {
                let hit = expect.is_none_or(|e| matches(&line, e));
                findings.push(line);
                if hit {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break, // the clock ran out, or crucible exited
        }
    }
    // Crucible runs its clients in the same process group.
    let pgid = format!("-{}", child.id());
    let _ = Command::new("kill").args(["-INT", "--", &pgid]).status();
    let stop = Instant::now() + Duration::from_secs(10);
    while child.try_wait().ok().flatten().is_none() {
        if Instant::now() > stop {
            let _ = Command::new("kill").args(["-KILL", "--", &pgid]).status();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    findings.sort();
    findings.dedup();
    findings
}

fn output(root: &Path, cmd: &str) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("{cmd} 2>&1"))
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| die(&format!("{cmd}: {e}")));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run_ok(root: &Path, cmd: &str) {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("{cmd} 2>&1"))
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| die(&format!("{cmd}: {e}")));
    if !out.status.success() {
        die(&format!("`{cmd}` failed:\n{}", String::from_utf8_lossy(&out.stdout)));
    }
}

/// The file being edited, so `die` can put it back.
static EDITING: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn backup_of(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".claims-orig");
    PathBuf::from(name)
}

fn restore_backup(file: &Path) {
    let backup = backup_of(file);
    if backup.exists() {
        std::fs::rename(&backup, file).unwrap_or_else(|e| die(&format!("{}: {e}", backup.display())));
        println!("restored {} from an interrupted run", file.display());
    }
}

/// Puts the original file back when dropped; the backup on disk covers a
/// run that is killed before then.
struct Restore {
    file: PathBuf,
    original: String,
}

impl Restore {
    fn new(file: &Path, original: &str) -> Self {
        std::fs::write(backup_of(file), original).unwrap_or_else(|e| die(&format!("{}: {e}", file.display())));
        *EDITING.lock().unwrap() = Some(file.to_path_buf());
        Restore { file: file.to_path_buf(), original: original.to_string() }
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        if std::fs::write(&self.file, &self.original).is_ok() {
            let _ = std::fs::remove_file(backup_of(&self.file));
        }
        *EDITING.lock().unwrap() = None;
    }
}

fn die(msg: &str) -> ! {
    eprintln!("claims: {msg}");
    if let Some(file) = EDITING.lock().ok().and_then(|mut e| e.take()) {
        restore_backup(&file);
    }
    std::process::exit(2);
}
