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
//! v0 prints the step list with explanations rather than executing in
//! sub-processes. Embedded execution + CU-cost aggregation lands in v1.

use std::fs;
use std::path::{Path, PathBuf};

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
    /// Print the scenario's step-by-step recipe of cargo-run invocations.
    /// (Embedded in-process execution + CU-cost aggregation lands in v1.)
    Run {
        /// Scenario name (file stem without `.json`).
        name: String,
        #[arg(long, default_value = "scenarios")]
        dir: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Scenario {
    name: String,
    category: String,
    description: String,
    headline: String,
    steps: Vec<ScenarioStep>,
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
        Action::Run { name, dir } => {
            let path = dir.join(format!("{name}.json"));
            let s = load_from_path(&path)?;
            print!("{}", render_run_v0(&s, &path));
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
}
