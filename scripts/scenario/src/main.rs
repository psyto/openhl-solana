//! **Mirrored across sibling Fabrknt sandbox engines.** This module's
//! shape (Scenario JSON, list/show/run renderers, run_embedded with
//! sub-process spawn + stdio inherit, `has_shell_metacharacters`,
//! `EmbeddedReport`, CTA footer with `product=` waitlist enrichment)
//! is duplicated nearly verbatim in:
//!
//!   - psyto/rdk → `princeps/bin/princeps/src/scenario.rs`
//!   - psyto/ssr → `cli/src/scenario.rs`
//!
//! When you change behavior shared across them (metachar detection
//! rules, headline rendering, JSON shape, CTA footer text), apply the
//! same change to all three. The decision to keep three copies rather
//! than extract to a shared crate (e.g. `fabrknt-scenario-runner`) is
//! deliberate: the engines live in different repos with no shared
//! workspace, so a crate would need crates.io publication + cross-repo
//! version coordination that doesn't yet pay for itself given the
//! small surface area. Revisit when adding the 4th subprocess-based
//! runner or when the shared surface grows.
//!
//! The 4th Fabrknt runner (`rdk/openhl`) is structurally different —
//! in-process execution via `LiveRethEvmBridge<()>` rather than
//! sub-process spawn — so it shares only the Scenario JSON shape and
//! the CTA footer with these three.
//!
//! ---
//!
//! Sandbox scenario surface for openhl-solana.
//!
//! A *scenario* is a metadata-wrapped recipe of `cargo run -p <script>`
//! invocations that demonstrate one Solana perp behavior end-to-end.
//! Per `fabrknt/website/SANDBOX-PATTERN.md`, the surface implements the
//! 5 elements: (1) pre-baked scenarios in `scenarios/*.json`, (2) ASCII
//! headline + step rendering, (3) parameter dial via per-step args,
//! (4) replay (the JSON IS the replay format — re-running yields the
//! same sequence), (5) CTA footer with three options.
//!
//! v1 spawns each `cargo run -p X -- ...` step as a sub-process with
//! stdio inherited, so each script's own output (CU cost prints,
//! account dumps, etc.) streams live to the operator's terminal.
//! Comment / off-CLI hint lines are skipped and printed as info.
//!
//! v0 (legacy `render_run_v0`) prints only the step list; kept
//! exported for the `--dry-run` flag.

mod account_layout_demo;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Debug, Parser)]
#[command(
    name = "scenario",
    about = "Discover, inspect, and walk through openhl-solana sandbox scenarios"
)]
struct Cli {
    #[command(subcommand)]
    action: Action,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// List all available scenarios with their headlines.
    List {
        /// Directory containing scenario JSON files. Default: `scenarios/`
        /// relative to the current working directory.
        #[arg(long, default_value = "scenarios")]
        dir: PathBuf,
    },
    /// Print one scenario's metadata, description, and step summary.
    Show {
        /// Scenario name (file stem without `.json`).
        name: String,
        #[arg(long, default_value = "scenarios")]
        dir: PathBuf,
    },
    /// Run the scenario: spawn each `cargo run -p X -- …` step as a
    /// sub-process with stdio inherited so each script's CU prints +
    /// account dumps stream live. Pass `--dry-run` to print only the
    /// step list without executing.
    Run {
        /// Scenario name (file stem without `.json`).
        name: String,
        #[arg(long, default_value = "scenarios")]
        dir: PathBuf,
        /// Skip embedded execution; print only the step list.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Standalone account-layout inspection demo. Walks every on-chain
    /// account type defined in `openhl-state` and reports
    /// discriminator + byte size + version. Pure Rust against
    /// `openhl-state` constants — no validator, no deployed program.
    /// Same flow the scenario runner v2 path dispatches for the
    /// `account-layout-demo` scenario.
    AccountLayoutDemo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Scenario {
    name: String,
    category: String,
    description: String,
    headline: String,
    steps: Vec<ScenarioStep>,
    /// v2 Phase 3: optional declarative outcome checks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    expected_outcomes: Vec<ExpectedOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExpectedOutcome {
    name: String,
    description: String,
    check: SolanaCheck,
}

/// Engine-specific check schema for openhl-solana. Externally-tagged
/// JSON so authors write `{"account_types_min": 10}` instead of
/// `{"kind": "account_types_min", "value": 10}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SolanaCheck {
    /// Assert at least N distinct account types are inspected.
    AccountTypesMin(usize),
    /// Assert exactly N account types are inspected.
    AccountTypesExact(usize),
    /// Assert OrderBook total size equals N bytes.
    OrderbookSizeExact(usize),
    /// Assert OrderBook capacity (orders) is exactly N.
    OrderbookCapacityExact(usize),
    /// Assert Slab capacity (orders) is at least N.
    SlabCapacityMin(usize),
    /// Assert Slab packs more orders per byte than the flat OrderBook
    /// (the engineering claim the scenario headline rests on).
    SlabBeatsOrderbookOnDensity,
    /// Assert a specific account type appears with the given byte size.
    AccountTypeSize { type_name: String, expected: usize },
}

#[derive(Debug, Clone)]
enum OutcomeStatus {
    Pass,
    Fail(String),
}

fn evaluate_solana_check(
    check: &SolanaCheck,
    result: &account_layout_demo::AccountLayoutDemoResult,
) -> OutcomeStatus {
    match check {
        SolanaCheck::AccountTypesMin(min) => {
            let observed = result.distinct_account_types();
            if observed >= *min {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed account_types = {observed} (expected ≥ {min})"
                ))
            }
        }
        SolanaCheck::AccountTypesExact(expected) => {
            let observed = result.distinct_account_types();
            if observed == *expected {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!("observed account_types = {observed}"))
            }
        }
        SolanaCheck::OrderbookSizeExact(expected) => {
            if result.orderbook_size == *expected {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed OrderBook size = {} bytes",
                    result.orderbook_size
                ))
            }
        }
        SolanaCheck::OrderbookCapacityExact(expected) => {
            if result.orderbook_capacity == *expected {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed OrderBook capacity = {}",
                    result.orderbook_capacity
                ))
            }
        }
        SolanaCheck::SlabCapacityMin(min) => {
            if result.slab_capacity >= *min {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed Slab capacity = {} (expected ≥ {min})",
                    result.slab_capacity
                ))
            }
        }
        SolanaCheck::SlabBeatsOrderbookOnDensity => {
            if result.slab_capacity > result.orderbook_capacity {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "Slab capacity ({}) ≤ OrderBook capacity ({})",
                    result.slab_capacity, result.orderbook_capacity
                ))
            }
        }
        SolanaCheck::AccountTypeSize { type_name, expected } => {
            match result.entries.iter().find(|e| e.type_name == type_name) {
                Some(e) if e.size_bytes == *expected => OutcomeStatus::Pass,
                Some(e) => OutcomeStatus::Fail(format!(
                    "{type_name} size = {} bytes (expected {expected})",
                    e.size_bytes
                )),
                None => OutcomeStatus::Fail(format!("{type_name} not found")),
            }
        }
    }
}

/// v2 in-process dispatch target.
#[derive(Debug, Clone, Copy)]
enum InProcessTarget {
    AccountLayoutDemo,
}

fn try_parse_in_process(command: &str) -> Option<InProcessTarget> {
    let trimmed = command.trim();
    // Match the documented in-process command: invoking the scenario
    // binary's own AccountLayoutDemo subcommand. Re-routed in-process
    // by the runner so the demo doesn't actually spawn a sub-process.
    if trimmed == "cargo run -p scenario -- account-layout-demo" {
        return Some(InProcessTarget::AccountLayoutDemo);
    }
    None
}

fn is_v2_eligible(scenario: &Scenario) -> bool {
    !scenario.steps.is_empty()
        && scenario
            .steps
            .iter()
            .all(|s| try_parse_in_process(&s.command).is_some())
}

#[derive(Debug, Clone)]
enum StepResult {
    AccountLayoutDemo(account_layout_demo::AccountLayoutDemoResult),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScenarioStep {
    explanation: String,
    command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expect: Option<String>,
}

fn load_from_path(path: &Path) -> Result<Scenario> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let s: Scenario =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    Ok(s)
}

fn list_in(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Err(anyhow!(
            "scenarios directory not found: {}\n\
            run `cargo run -p scenario -- list` from the openhl-solana repo root,\n\
            or pass --dir explicitly.",
            dir.display()
        ));
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    Ok(paths)
}

fn render_list(scenarios: &[(PathBuf, Scenario)]) -> String {
    if scenarios.is_empty() {
        return "No scenarios found.\n".to_string();
    }
    let name_w = scenarios.iter().map(|(_, s)| s.name.len()).max().unwrap_or(8).max(4);
    let cat_w = scenarios
        .iter()
        .map(|(_, s)| s.category.len())
        .max()
        .unwrap_or(8)
        .max(8);

    let mut out = String::new();
    out.push_str(&format!(
        "{:<name_w$}  {:<cat_w$}  Headline\n",
        "Name",
        "Category",
        name_w = name_w,
        cat_w = cat_w,
    ));
    out.push_str(&format!(
        "{:-<name_w$}  {:-<cat_w$}  {:-<60}\n",
        "",
        "",
        "",
        name_w = name_w,
        cat_w = cat_w,
    ));
    for (_, s) in scenarios {
        out.push_str(&format!(
            "{:<name_w$}  {:<cat_w$}  {}\n",
            s.name,
            s.category,
            s.headline,
            name_w = name_w,
            cat_w = cat_w,
        ));
    }
    out.push_str(&cta_footer());
    out
}

fn render_show(scenario: &Scenario, path: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!("─── {} ────────────────────────────────────\n", scenario.headline));
    out.push_str(&format!("name        : {}\n", scenario.name));
    out.push_str(&format!("category    : {}\n", scenario.category));
    out.push_str(&format!("source      : {}\n\n", path.display()));
    out.push_str("description :\n");
    for line in scenario.description.lines() {
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str(&format!("\nsteps       : {} command(s)\n", scenario.steps.len()));
    for (i, step) in scenario.steps.iter().enumerate() {
        out.push_str(&format!("  [{}] {}\n", i + 1, step.explanation));
    }
    out.push_str(&cta_footer());
    out
}

fn render_run_v0(scenario: &Scenario, path: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "─── scenario: {} ────────────────────────────────────\n",
        scenario.name
    ));
    out.push_str(&format!("HEADLINE: {}\n\n", scenario.headline));
    out.push_str("DESCRIPTION:\n");
    for line in scenario.description.lines() {
        out.push_str(&format!("  {line}\n"));
    }

    out.push_str("\nPREREQUISITES:\n");
    out.push_str("  # local validator running:\n");
    out.push_str("  solana-test-validator --reset\n");
    out.push_str("  # build + deploy the on-chain program:\n");
    out.push_str("  cargo build-sbf -p openhl-core\n");
    out.push_str("  solana program deploy target/deploy/openhl_core.so\n");

    out.push_str(&format!("\nSTEPS ({} command(s)):\n", scenario.steps.len()));
    for (i, step) in scenario.steps.iter().enumerate() {
        out.push_str(&format!("\n  Step {} — {}\n", i + 1, step.explanation));
        out.push_str(&format!("    $ {}\n", step.command));
        if let Some(expect) = &step.expect {
            out.push_str(&format!("    # expect output to include: {expect}\n"));
        }
    }

    out.push_str(&format!("\nSOURCE: {}\n", path.display()));
    out.push_str("\nNOTE: v0 prints the step list rather than executing each command\n");
    out.push_str("in-process. Embedded execution with CU-cost aggregation lands in v1.\n");

    out.push_str(&cta_footer());
    out
}

fn cta_footer() -> String {
    let mut out = String::new();
    out.push_str("\nNEXT:\n");
    out.push_str("  • Adopt this engine  : https://github.com/psyto/openhl-solana\n");
    out.push_str("  • Custom build       : https://fabrknt.com/waitlist.html?product=solana-perp&intent=build\n");
    out.push_str("  • Hosted access      : https://fabrknt.com/waitlist.html?product=solana-perp&intent=hosted\n");
    out
}

/// Detect whether `command` contains shell metacharacters that mean it
/// can't be naïvely whitespace-split into argv. Returns true for
/// command chains (`&&`, `||`, `;`) and pipes (`|`).
///
/// **Intentionally excluded**: `<` and `>`. Curated scenarios use
/// `<PLACEHOLDER>` syntax for operator-substituted values; treating
/// those as shell redirects breaks every placeholder step.
fn has_shell_metacharacters(command: &str) -> bool {
    command.contains("&&")
        || command.contains("||")
        || command.contains(';')
        || command.contains('|')
}

#[derive(Debug, Clone, Copy)]
struct EmbeddedReport {
    total: usize,
    skipped: usize,
    passed: usize,
    failed: usize,
    expectations_unverified: usize,
}

/// Embedded execution dispatcher. v2-eligible scenarios (every step
/// matches an [`InProcessTarget`]) take the in-process v2 path with
/// HEADLINE / TIMELINE / DELTA / OUTCOMES / NEXT contract. Other
/// scenarios fall back to v1 sub-process spawn with stdio inherit.
fn run_embedded(scenario: &Scenario, path: &Path) -> Result<EmbeddedReport> {
    if is_v2_eligible(scenario) {
        return run_embedded_v2(scenario, path);
    }
    run_embedded_v1(scenario, path)
}

fn run_embedded_v2(scenario: &Scenario, path: &Path) -> Result<EmbeddedReport> {
    println!(
        "─── scenario: {} ────────────────────────────────────",
        scenario.name
    );
    println!();

    let mut results: Vec<StepResult> = Vec::with_capacity(scenario.steps.len());
    let report = EmbeddedReport {
        total: scenario.steps.len(),
        skipped: 0,
        passed: scenario.steps.len(),
        failed: 0,
        expectations_unverified: 0,
    };

    for step in &scenario.steps {
        let target = try_parse_in_process(&step.command)
            .expect("v2-eligible scenario must have all-in-process steps");
        match target {
            InProcessTarget::AccountLayoutDemo => {
                results.push(StepResult::AccountLayoutDemo(
                    account_layout_demo::run_account_layout_demo_structured(),
                ));
            }
        }
    }

    render_v2_sections(scenario, path, &results, &report);

    Ok(report)
}

fn render_v2_sections(
    scenario: &Scenario,
    path: &Path,
    results: &[StepResult],
    report: &EmbeddedReport,
) {
    let last_account_layout = results.iter().rev().find_map(|r| match r {
        StepResult::AccountLayoutDemo(d) => Some(d),
    });

    let evaluated: Vec<(&ExpectedOutcome, OutcomeStatus)> = scenario
        .expected_outcomes
        .iter()
        .map(|o| {
            let status = if let Some(d) = last_account_layout {
                evaluate_solana_check(&o.check, d)
            } else {
                OutcomeStatus::Fail("no account-layout result available".to_string())
            };
            (o, status)
        })
        .collect();
    let any_failed = evaluated.iter().any(|(_, s)| matches!(s, OutcomeStatus::Fail(_)));
    let has_outcomes = !evaluated.is_empty();

    if has_outcomes && !any_failed {
        println!("HEADLINE ✓: {}", scenario.headline);
    } else if has_outcomes && any_failed {
        println!("HEADLINE ⚠: {}", scenario.headline);
    } else {
        println!("HEADLINE (unverified): {}", scenario.headline);
    }
    println!();

    println!("TIMELINE:");
    for r in results {
        match r {
            StepResult::AccountLayoutDemo(d) => {
                println!(
                    "    inspect    {} on-chain account types via openhl-state constants",
                    d.distinct_account_types()
                );
                println!("    aggregate  {} bytes total (one of each)", d.total_bytes());
                println!(
                    "    compare    OrderBook ({} bytes / {} orders) vs Slab ({} bytes / ~{} orders)",
                    d.orderbook_size, d.orderbook_capacity, d.slab_size, d.slab_capacity
                );
            }
        }
    }
    println!();

    println!("DELTA:");
    for r in results {
        match r {
            StepResult::AccountLayoutDemo(d) => {
                println!("  {:<16}  {:<10}  {:>6}  Version", "Type", "Tag", "Bytes");
                println!(
                    "  {}  {}  {}  {}",
                    "─".repeat(16),
                    "─".repeat(10),
                    "─".repeat(6),
                    "─".repeat(7)
                );
                for e in &d.entries {
                    let v = e.version.map_or("—".to_string(), |v| format!("v{v}"));
                    println!(
                        "  {:<16}  {:<10}  {:>6}  {}",
                        e.type_name, e.discriminator_text, e.size_bytes, v
                    );
                }
                println!();
                println!(
                    "  OrderBook (flat): {} bytes / {} orders → {} bytes/order",
                    d.orderbook_size, d.orderbook_capacity, d.orderbook_bytes_per_order
                );
                println!(
                    "  Slab (critbit):   {} bytes / ~{} orders → ~{} bytes/order",
                    d.slab_size, d.slab_capacity, d.slab_bytes_per_order
                );
            }
        }
    }
    println!();

    println!("OUTCOMES:");
    if evaluated.is_empty() {
        println!("  (no expected_outcomes declared — HEADLINE shown as unverified)");
    } else {
        for (outcome, status) in &evaluated {
            match status {
                OutcomeStatus::Pass => println!("  ✓ {}", outcome.description),
                OutcomeStatus::Fail(why) => println!("  ✗ {} ({why})", outcome.description),
            }
        }
        let passed = evaluated
            .iter()
            .filter(|(_, s)| matches!(s, OutcomeStatus::Pass))
            .count();
        println!();
        println!("  {passed} of {} outcome(s) verified.", evaluated.len());
    }
    println!();

    if report.failed > 0 {
        println!(
            "({} of {} step(s) failed during execution)",
            report.failed, report.total
        );
        println!();
    }
    println!("source: {}", path.display());

    print!("{}", cta_footer());
}

/// v1 embedded execution: walk each step, spawn `cargo run -p X` (or
/// any other shell-style command) as a sub-process with stdio
/// inherited. Comment lines (starting with `#`) are printed as info.
fn run_embedded_v1(scenario: &Scenario, path: &Path) -> Result<EmbeddedReport> {
    println!(
        "─── scenario: {} ────────────────────────────────────",
        scenario.name
    );
    println!("HEADLINE (curator claim): {}", scenario.headline);
    println!();
    println!("DESCRIPTION:");
    for line in scenario.description.lines() {
        println!("  {line}");
    }
    println!();
    println!("PREREQUISITES (operator's responsibility):");
    println!("  solana-test-validator --reset    # in another terminal");
    println!("  cargo build-sbf -p openhl-core");
    println!("  solana program deploy target/deploy/openhl_core.so");
    println!();

    let mut report = EmbeddedReport {
        total: scenario.steps.len(),
        skipped: 0,
        passed: 0,
        failed: 0,
        expectations_unverified: 0,
    };

    for (i, step) in scenario.steps.iter().enumerate() {
        println!(
            "─── Step {} of {} ───────────────────────────",
            i + 1,
            scenario.steps.len()
        );
        println!("  {}", step.explanation);
        println!("  $ {}", step.command);
        if let Some(expect) = &step.expect {
            println!("  # (looking for: {expect})");
        }
        println!();

        let trimmed = step.command.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            println!("  (informational step — no command executed)");
            println!();
            report.skipped += 1;
            continue;
        }

        let has_shell_metas = has_shell_metacharacters(trimmed);

        let mut cmd = if has_shell_metas {
            let mut c = Command::new("sh");
            c.args(["-c", trimmed]);
            c
        } else {
            let argv: Vec<&str> = trimmed.split_whitespace().collect();
            let (program, args) = match argv.split_first() {
                Some((p, a)) => (*p, a.to_vec()),
                None => continue,
            };
            let mut c = Command::new(program);
            c.args(&args);
            c
        };

        let status = cmd.status();

        match status {
            Ok(s) if s.success() => {
                println!();
                println!("  ✓ step {} succeeded (exit 0)", i + 1);
                report.passed += 1;
                if step.expect.is_some() {
                    report.expectations_unverified += 1;
                }
            }
            Ok(s) => {
                println!();
                println!("  ✗ step {} exited {}", i + 1, s.code().unwrap_or(-1));
                report.failed += 1;
            }
            Err(e) => {
                println!();
                println!("  ✗ step {} failed to spawn: {e}", i + 1);
                if has_shell_metas {
                    println!("    (routed via `sh -c` because the command contains shell metacharacters; check that `sh` is available)");
                } else {
                    println!("    (verify the first token of the command is in PATH and the workspace is built)");
                }
                report.failed += 1;
            }
        }
        println!();
    }

    println!("─── verdict ───────────────────────────────────────────");
    println!(
        "{} step(s): {} passed / {} failed / {} skipped (informational)",
        report.total, report.passed, report.failed, report.skipped
    );
    if report.expectations_unverified > 0 {
        println!(
            "{} step(s) declared expected-output substrings; v1 cannot verify these because it inherits stdio (v2 will tee).",
            report.expectations_unverified
        );
    }
    println!("source: {}", path.display());
    print!("{}", cta_footer());

    Ok(report)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.action {
        Action::List { dir } => {
            let paths = list_in(&dir)?;
            let mut loaded: Vec<(PathBuf, Scenario)> = Vec::with_capacity(paths.len());
            for path in paths {
                match load_from_path(&path) {
                    Ok(s) => loaded.push((path, s)),
                    Err(e) => eprintln!("warning: skipping {}: {e:#}", path.display()),
                }
            }
            print!("{}", render_list(&loaded));
        }
        Action::Show { name, dir } => {
            let path = dir.join(format!("{name}.json"));
            let s = load_from_path(&path)?;
            print!("{}", render_show(&s, &path));
        }
        Action::Run { name, dir, dry_run } => {
            let path = dir.join(format!("{name}.json"));
            let s = load_from_path(&path)?;
            if dry_run {
                print!("{}", render_run_v0(&s, &path));
            } else {
                let report = run_embedded(&s, &path)?;
                if report.failed > 0 {
                    return Err(anyhow!(
                        "{} step(s) failed during scenario run",
                        report.failed
                    ));
                }
            }
        }
        Action::AccountLayoutDemo => {
            account_layout_demo::run_account_layout_demo_cli();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_scenario_json() -> &'static str {
        r#"{
            "name": "bring-up",
            "category": "bring-up",
            "description": "Initialize the market and verify state.",
            "headline": "Boot a market, place one order, observe CU cost.",
            "steps": [
                {
                    "explanation": "Allocate the Market account.",
                    "command": "cargo run -p allocate-market",
                    "expect": "Market account allocated"
                }
            ]
        }"#
    }

    #[test]
    fn scenario_round_trips() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        assert_eq!(s.name, "bring-up");
        assert_eq!(s.steps.len(), 1);
        assert_eq!(s.steps[0].expect.as_deref(), Some("Market account allocated"));
    }

    #[test]
    fn render_list_contains_headline_and_cta() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        let path = PathBuf::from("/tmp/bring-up.json");
        let out = render_list(&[(path, s)]);
        assert!(out.contains("bring-up"));
        assert!(out.contains("Boot a market"));
        assert!(out.contains("solana-perp"));
        assert!(out.contains("NEXT:"));
    }

    #[test]
    fn render_show_contains_step_summary() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        let path = PathBuf::from("/tmp/bring-up.json");
        let out = render_show(&s, &path);
        assert!(out.contains("steps       : 1 command(s)"));
        assert!(out.contains("Allocate the Market"));
    }

    #[test]
    fn render_run_v0_includes_prereqs_and_steps() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        let path = PathBuf::from("scenarios/bring-up.json");
        let out = render_run_v0(&s, &path);
        assert!(out.contains("PREREQUISITES"));
        assert!(out.contains("solana-test-validator"));
        assert!(out.contains("Step 1 — Allocate the Market account."));
        assert!(out.contains("$ cargo run -p allocate-market"));
        assert!(out.contains("expect output to include: Market account allocated"));
    }

    /// Regression tests for shell-metachar detection.

    #[test]
    fn metachar_routes_chains_through_sh() {
        assert!(has_shell_metacharacters("cargo run -p a && cargo run -p b"));
        assert!(has_shell_metacharacters("a || b"));
        assert!(has_shell_metacharacters("a; b"));
        assert!(has_shell_metacharacters("a | grep b"));
    }

    /// Regression: openhl-solana scenarios don't currently use angle-
    /// bracket placeholders, but the detection must still match the
    /// sibling runners' behavior so a future scenario like `cargo run
    /// -p deposit -- --account <ACCOUNT_ID>` doesn't break.
    #[test]
    fn metachar_does_not_match_angle_bracket_placeholders() {
        assert!(!has_shell_metacharacters(
            "cargo run -p deposit -- --account <ACCOUNT_ID>"
        ));
        assert!(!has_shell_metacharacters("cmd <input> output"));
    }

    #[test]
    fn metachar_does_not_match_plain_commands() {
        assert!(!has_shell_metacharacters("cargo run -p oracle"));
        assert!(!has_shell_metacharacters("cargo run -p position -- liquidate"));
    }

    /// v2 in-process dispatch + Phase 3 expected_outcomes tests.

    #[test]
    fn try_parse_in_process_matches_account_layout_demo() {
        assert!(matches!(
            try_parse_in_process("cargo run -p scenario -- account-layout-demo"),
            Some(InProcessTarget::AccountLayoutDemo)
        ));
    }

    #[test]
    fn try_parse_in_process_returns_none_for_other_commands() {
        assert!(try_parse_in_process("cargo run -p oracle").is_none());
        assert!(try_parse_in_process("cargo run -p match-cli").is_none());
        assert!(try_parse_in_process("# comment").is_none());
    }

    fn make_scenario(commands: &[&str]) -> Scenario {
        Scenario {
            name: "test".to_string(),
            category: "stress".to_string(),
            description: "test".to_string(),
            headline: "test".to_string(),
            steps: commands
                .iter()
                .map(|c| ScenarioStep {
                    explanation: "step".to_string(),
                    command: (*c).to_string(),
                    expect: None,
                })
                .collect(),
            expected_outcomes: Vec::new(),
        }
    }

    #[test]
    fn is_v2_eligible_true_when_all_steps_in_process() {
        let s = make_scenario(&["cargo run -p scenario -- account-layout-demo"]);
        assert!(is_v2_eligible(&s));
    }

    #[test]
    fn is_v2_eligible_false_when_any_step_is_subprocess() {
        let s = make_scenario(&[
            "cargo run -p scenario -- account-layout-demo",
            "cargo run -p oracle",
        ]);
        assert!(!is_v2_eligible(&s));
    }

    #[test]
    fn evaluate_solana_check_account_types_min() {
        let r = account_layout_demo::run_account_layout_demo_structured();
        assert!(matches!(
            evaluate_solana_check(&SolanaCheck::AccountTypesMin(5), &r),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_solana_check(&SolanaCheck::AccountTypesMin(99), &r),
            OutcomeStatus::Fail(_)
        ));
    }

    #[test]
    fn evaluate_solana_check_slab_beats_orderbook() {
        let r = account_layout_demo::run_account_layout_demo_structured();
        assert!(matches!(
            evaluate_solana_check(&SolanaCheck::SlabBeatsOrderbookOnDensity, &r),
            OutcomeStatus::Pass
        ));
    }

    #[test]
    fn evaluate_solana_check_account_type_size_lookup() {
        let r = account_layout_demo::run_account_layout_demo_structured();
        // Market is 256 bytes per the state crate.
        assert!(matches!(
            evaluate_solana_check(
                &SolanaCheck::AccountTypeSize {
                    type_name: "Market".to_string(),
                    expected: 256
                },
                &r
            ),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_solana_check(
                &SolanaCheck::AccountTypeSize {
                    type_name: "Market".to_string(),
                    expected: 999
                },
                &r
            ),
            OutcomeStatus::Fail(_)
        ));
        assert!(matches!(
            evaluate_solana_check(
                &SolanaCheck::AccountTypeSize {
                    type_name: "NonExistent".to_string(),
                    expected: 0
                },
                &r
            ),
            OutcomeStatus::Fail(_)
        ));
    }

    #[test]
    fn scenario_with_expected_outcomes_round_trips() {
        let json = r#"{
            "name": "t",
            "category": "walkthrough",
            "description": "t",
            "headline": "t",
            "steps": [
                {"explanation": "s", "command": "cargo run -p scenario -- account-layout-demo"}
            ],
            "expected_outcomes": [
                {
                    "name": "n",
                    "description": "d",
                    "check": {"account_types_min": 5}
                },
                {
                    "name": "n2",
                    "description": "d2",
                    "check": "slab_beats_orderbook_on_density"
                }
            ]
        }"#;
        let s: Scenario = serde_json::from_str(json).expect("parse");
        assert_eq!(s.expected_outcomes.len(), 2);
        assert!(matches!(
            s.expected_outcomes[0].check,
            SolanaCheck::AccountTypesMin(5)
        ));
        assert!(matches!(
            s.expected_outcomes[1].check,
            SolanaCheck::SlabBeatsOrderbookOnDensity
        ));
    }

    #[test]
    fn scenario_without_expected_outcomes_still_parses() {
        let json = minimal_scenario_json();
        let s: Scenario = serde_json::from_str(json).expect("parse");
        assert!(s.expected_outcomes.is_empty());
    }
}
