//! `neteq-parity` CLI.
//!
//! ```text
//! neteq-parity list
//! neteq-parity gen --scenario burst_loss --codec opus-mono --out trace.json
//! neteq-parity run --scenario all --codec l16-16k-mono --candidate cpp
//! neteq-parity run --trace trace.json --candidate rust --json report.json
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use neteq_parity::{
    run_diff, run_single, Codec, Implementation, Result, RunSummary, Scenario, Trace,
};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "neteq-parity",
    about = "Diff our Rust NetEq against Google WebRTC's C++ NetEq"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CandidateArg {
    /// Reference only, no diff.
    None,
    /// C++ vs C++ (harness self-check: must be identical).
    Cpp,
    /// C++ vs our Rust NetEq (needs `--features rust-neteq`).
    Rust,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OpusDecoderArg {
    /// C libopus, the codec the reference uses. Gated: any diff is a NetEq bug.
    Libopus,
    /// `ropus`, the pure-Rust decoder. Report only: its drift from libopus
    /// is measured, never gated (exit code ignores Opus diffs).
    Ropus,
}

#[derive(Subcommand)]
enum Command {
    /// List scenarios and codecs.
    List,
    /// Generate a trace file.
    Gen {
        /// Scenario name.
        #[arg(long)]
        scenario: String,
        /// Codec label, e.g. l16-16k-mono, opus-stereo.
        #[arg(long, default_value = "l16-16k-mono")]
        codec: String,
        /// Generator seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Media duration (default: scenario default).
        #[arg(long)]
        duration_ms: Option<u32>,
        /// Output path.
        #[arg(long)]
        out: PathBuf,
        /// Store only payload sizes; payloads are regenerated on load.
        #[arg(long)]
        no_payloads: bool,
    },
    /// Run a scenario (or a trace file) and report.
    Run {
        /// Scenario name, or `all`.
        #[arg(long, conflicts_with = "trace")]
        scenario: Option<String>,
        /// Load this trace instead of generating one.
        #[arg(long)]
        trace: Option<PathBuf>,
        /// Codec label, or `all`.
        #[arg(long, default_value = "l16-16k-mono")]
        codec: String,
        /// Generator seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Media duration (default: scenario default).
        #[arg(long)]
        duration_ms: Option<u32>,
        /// What to compare the C++ reference against.
        #[arg(long, value_enum, default_value_t = CandidateArg::Rust)]
        candidate: CandidateArg,
        /// Opus decoder on the Rust side (`ropus` is a non-gating report).
        #[arg(long, value_enum, default_value_t = OpusDecoderArg::Libopus)]
        opus_decoder: OpusDecoderArg,
        /// Write the JSON report here (`-` for stdout).
        #[arg(long)]
        json: Option<PathBuf>,
        /// Include per-frame records in the JSON.
        #[arg(long)]
        frames: bool,
    },
}

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

fn parse_codecs(label: &str) -> Result<Vec<Codec>> {
    if label == "all" {
        return Ok(Codec::all());
    }
    Codec::parse(label).map(|c| vec![c]).ok_or_else(|| {
        neteq_parity::HarnessError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unknown codec `{label}` (see `neteq-parity list`)"),
        ))
    })
}

fn real_main() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::List => {
            println!("scenarios:");
            for s in Scenario::ALL {
                println!("  {:<18} {} ms", s.name(), s.default_duration_ms());
            }
            println!("codecs:");
            for c in Codec::all() {
                println!("  {}", c.label());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Gen {
            scenario,
            codec,
            seed,
            duration_ms,
            out,
            no_payloads,
        } => {
            let scenario = Scenario::from_name(&scenario)?;
            let codec = parse_codecs(&codec)?.remove(0);
            let trace = scenario.generate(
                codec,
                seed,
                duration_ms.unwrap_or(scenario.default_duration_ms()),
            )?;
            trace.save(&out, !no_payloads)?;
            println!("wrote {} packets to {}", trace.packets.len(), out.display());
            Ok(ExitCode::SUCCESS)
        }
        Command::Run {
            scenario,
            trace,
            codec,
            seed,
            duration_ms,
            candidate,
            opus_decoder,
            json,
            frames,
        } => {
            let traces: Vec<Trace> = match (trace, scenario) {
                (Some(path), _) => vec![Trace::load(&path)?],
                (None, Some(name)) => {
                    let scenarios = if name == "all" {
                        Scenario::ALL.to_vec()
                    } else {
                        vec![Scenario::from_name(&name)?]
                    };
                    let mut v = Vec::new();
                    for s in scenarios {
                        for c in parse_codecs(&codec)? {
                            v.push(s.generate(
                                c,
                                seed,
                                duration_ms.unwrap_or(s.default_duration_ms()),
                            )?);
                        }
                    }
                    v
                }
                (None, None) => {
                    eprintln!("error: pass --scenario <name|all> or --trace <file>");
                    return Ok(ExitCode::from(2));
                }
            };

            let mut reports: Vec<Value> = Vec::new();
            let mut all_identical = true;
            for trace in &traces {
                match candidate {
                    CandidateArg::None => {
                        let rec = run_single(trace, Implementation::Cpp)?;
                        let summary = RunSummary::new(trace, &rec);
                        print!("{}", summary.summary());
                        let mut v = serde_json::to_value(&summary)?;
                        if frames {
                            v["frames"] = serde_json::to_value(&rec.frames)?;
                        }
                        reports.push(v);
                    }
                    CandidateArg::Cpp | CandidateArg::Rust => {
                        let imp = if candidate == CandidateArg::Cpp {
                            Implementation::Cpp
                        } else if opus_decoder == OpusDecoderArg::Ropus {
                            Implementation::RustRopus
                        } else {
                            Implementation::Rust
                        };
                        let (report, recs) = run_diff(trace, imp)?;
                        print!("{}", report.summary());
                        let non_gating = imp == Implementation::RustRopus
                            && matches!(trace.codec, Codec::Opus { .. });
                        all_identical &= report.identical || non_gating;
                        let mut v = serde_json::to_value(&report)?;
                        if frames {
                            v["reference_frames"] = serde_json::to_value(&recs[0].frames)?;
                            v["candidate_frames"] = serde_json::to_value(&recs[1].frames)?;
                        }
                        reports.push(v);
                    }
                }
            }

            if let Some(path) = json {
                let doc = if reports.len() == 1 {
                    reports.remove(0)
                } else {
                    json!({ "runs": reports })
                };
                let text = serde_json::to_string_pretty(&doc)?;
                if path.as_os_str() == "-" {
                    println!("{text}");
                } else {
                    std::fs::write(&path, text)?;
                    eprintln!("wrote {}", path.display());
                }
            }
            Ok(if all_identical {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
    }
}
