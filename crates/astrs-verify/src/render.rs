//! Rendering a [`VerificationReport`] for a terminal.
//!
//! The layout answers, in order, the three questions a reader has:
//! *did anything fail*, *what exactly failed and why*, and *what is the
//! result conditional on*. Violations come first and carry their full
//! counterexample; everything that held is a single line each; the caveats
//! close the report so nobody mistakes "no violations" for "proved".

use std::fmt::Write as _;

use crate::counterexample::Counterexample;
use crate::report::{Discharge, ObligationOutcome, VerificationReport};

/// ANSI SGR codes, or empty strings when color is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Palette {
    reset: &'static str,
    bold: &'static str,
    red: &'static str,
    green: &'static str,
    yellow: &'static str,
    dim: &'static str,
}

impl Palette {
    const PLAIN: Self = Self {
        reset: "",
        bold: "",
        red: "",
        green: "",
        yellow: "",
        dim: "",
    };

    const COLOR: Self = Self {
        reset: "\u{1b}[0m",
        bold: "\u{1b}[1m",
        red: "\u{1b}[31m",
        green: "\u{1b}[32m",
        yellow: "\u{1b}[33m",
        dim: "\u{1b}[2m",
    };

    const fn pick(color: bool) -> Self {
        if color { Self::COLOR } else { Self::PLAIN }
    }
}

/// How to render a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderOptions {
    /// Colorize with plain ANSI SGR codes.
    pub color: bool,
    /// Include the SMT-LIB2 system behind each obligation.
    ///
    /// Off by default: it is the artefact that makes a verdict auditable,
    /// but it is long, and a reader who wants it knows to ask.
    pub show_systems: bool,
}

/// Render a report as human-readable text.
///
/// A pure function of the report, so two runs of the same proof produce
/// byte-identical output — which is what makes the report diffable in CI.
#[must_use]
pub fn render_human(report: &VerificationReport, options: &RenderOptions) -> String {
    let palette = Palette::pick(options.color);
    let summary = report.summary();
    let mut out = String::new();

    let _ = writeln!(
        out,
        "{}graph proofs{} — {} node(s), {} channel(s), {}s analysis window{}",
        palette.bold,
        palette.reset,
        report.node_count,
        report.channel_count,
        report.window_seconds,
        if report.profile_applied {
            ", verification profile applied"
        } else {
            ""
        }
    );
    let _ = writeln!(out, "{summary}");
    out.push('\n');

    let violations: Vec<&ObligationOutcome> = report.violations().collect();
    if violations.is_empty() {
        let _ = writeln!(
            out,
            "{}no obligation was violated{}",
            palette.green, palette.reset
        );
    } else {
        for outcome in &violations {
            render_violation(&mut out, outcome, palette);
        }
    }

    out.push('\n');
    let _ = writeln!(out, "{}obligations{}", palette.bold, palette.reset);
    for outcome in &report.obligations {
        let (marker, color) = match &outcome.discharge {
            Discharge::Holds { .. } => ("ok  ", palette.green),
            Discharge::Violated { .. } => ("FAIL", palette.red),
            Discharge::NotAttempted { .. } => ("skip", palette.yellow),
            Discharge::Inconclusive { .. } => ("????", palette.yellow),
        };
        let _ = writeln!(
            out,
            "  {color}{marker}{} {}",
            palette.reset,
            outcome.obligation.heading()
        );
        match &outcome.discharge {
            Discharge::Holds { claim } => {
                let _ = writeln!(out, "       {}{claim}{}", palette.dim, palette.reset);
            }
            Discharge::NotAttempted { reason } => {
                let _ = writeln!(out, "       {}{reason}{}", palette.dim, palette.reset);
            }
            Discharge::Inconclusive { reason } => {
                let _ = writeln!(out, "       {}{reason}{}", palette.dim, palette.reset);
            }
            Discharge::Violated { .. } => {}
        }
        if options.show_systems
            && let Some(system) = &outcome.encoded_system
        {
            for line in system.lines() {
                let _ = writeln!(out, "       {}{line}{}", palette.dim, palette.reset);
            }
        }
    }

    if !report.caveats.is_empty() {
        out.push('\n');
        let _ = writeln!(out, "{}conditional on{}", palette.bold, palette.reset);
        for caveat in &report.caveats {
            let _ = writeln!(out, "  - {caveat}");
        }
    }

    if !report.everything_discharged() {
        out.push('\n');
        let _ = writeln!(
            out,
            "{}not every obligation reached a verdict: this report rules out what it lists, and no more{}",
            palette.yellow, palette.reset
        );
    }

    out
}

fn render_violation(out: &mut String, outcome: &ObligationOutcome, palette: Palette) {
    let Some(counterexample) = outcome.discharge.counterexample() else {
        return;
    };
    let _ = writeln!(
        out,
        "{}{}VIOLATED{} {}",
        palette.bold,
        palette.red,
        palette.reset,
        outcome.obligation.heading()
    );
    let _ = writeln!(out, "  {}", counterexample.headline);
    let _ = writeln!(
        out,
        "  {}({}){}",
        palette.dim,
        counterexample.strength.label(),
        palette.reset
    );
    render_counterexample(out, counterexample, palette);
    out.push('\n');
}

fn render_counterexample(out: &mut String, counterexample: &Counterexample, palette: Palette) {
    if !counterexample.facts.is_empty() {
        let width = counterexample
            .facts
            .iter()
            .map(|fact| fact.label.chars().count())
            .max()
            .unwrap_or(0);
        for fact in &counterexample.facts {
            let _ = writeln!(
                out,
                "    {:width$}  {}",
                fact.label,
                fact.value,
                width = width
            );
        }
    }
    if !counterexample.trace.is_empty() {
        let _ = writeln!(out, "    {}trace{}", palette.dim, palette.reset);
        for step in &counterexample.trace {
            let _ = writeln!(out, "      {} — {}", step.actor, step.detail);
        }
    }
    if let Some(remedy) = &counterexample.remedy {
        let _ = writeln!(out, "    fix: {remedy}");
    }
    if !counterexample.cross_checked {
        let _ = writeln!(
            out,
            "    {}warning: this crate's own replay did not confirm the solver's model; treat it as a lead, not a proof{}",
            palette.yellow, palette.reset
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::counterexample::Strength;
    use crate::obligation::{Obligation, ObligationKind, Subject};
    use crate::report::{Caveat, Discharge, Inconclusive, NotAttempted};

    fn counterexample() -> Counterexample {
        Counterexample {
            headline: "`detector` can never fire".to_string(),
            strength: Strength::Proof,
            facts: vec![
                crate::counterexample::Fact::new("starved channels", "a.x, b.y"),
                crate::counterexample::Fact::new("window", "1s"),
            ],
            trace: vec![crate::counterexample::TraceStep::new(
                "detector",
                "every wired input is starved",
            )],
            remedy: Some("add a timer input".to_string()),
            assignment: vec!["starved!a.x = true".to_string()],
            cross_checked: true,
        }
    }

    fn report() -> VerificationReport {
        VerificationReport {
            node_count: 2,
            channel_count: 2,
            window_seconds: 1,
            profile_applied: false,
            obligations: vec![
                ObligationOutcome {
                    obligation: Obligation::whole_graph(ObligationKind::DeadlockFreedom),
                    discharge: Discharge::Violated {
                        counterexample: Box::new(counterexample()),
                    },
                    encoded_system: Some("(check-sat)".to_string()),
                },
                ObligationOutcome {
                    obligation: Obligation::about(
                        ObligationKind::QueueBoundedness,
                        Subject::Node("detector".to_string()),
                    ),
                    discharge: Discharge::NotAttempted {
                        reason: NotAttempted::NoServiceTime {
                            node: "detector".to_string(),
                        },
                    },
                    encoded_system: None,
                },
                ObligationOutcome {
                    obligation: Obligation::about(
                        ObligationKind::RateConsistency,
                        Subject::Channel("detector.frames".to_string()),
                    ),
                    discharge: Discharge::Holds {
                        claim: "fed within its timeout".to_string(),
                    },
                    encoded_system: Some("(check-sat)".to_string()),
                },
            ],
            caveats: vec![Caveat::NoProfile],
        }
    }

    #[test]
    fn violations_lead_the_report() {
        let text = render_human(&report(), &RenderOptions::default());
        let violation = text.find("VIOLATED").expect("violation rendered");
        let list = text.find("obligations").expect("obligation list rendered");
        assert!(violation < list, "{text}");
    }

    #[test]
    fn a_counterexample_renders_facts_trace_and_fix() {
        let text = render_human(&report(), &RenderOptions::default());
        assert!(text.contains("`detector` can never fire"), "{text}");
        assert!(text.contains("starved channels"), "{text}");
        assert!(text.contains("trace"), "{text}");
        assert!(text.contains("fix: add a timer input"), "{text}");
        assert!(text.contains("(proved)"), "{text}");
    }

    #[test]
    fn skipped_obligations_say_how_to_settle_them() {
        let text = render_human(&report(), &RenderOptions::default());
        assert!(text.contains("nodes.detector.wcet"), "{text}");
        assert!(text.contains("skip"), "{text}");
    }

    #[test]
    fn caveats_and_the_incompleteness_note_close_the_report() {
        let text = render_human(&report(), &RenderOptions::default());
        assert!(text.contains("conditional on"), "{text}");
        assert!(text.contains("no verification profile"), "{text}");
        assert!(text.contains("no more"), "{text}");
    }

    #[test]
    fn a_clean_report_says_so_and_omits_the_warning() {
        let mut clean = report();
        clean.obligations.retain(|o| o.discharge.holds());
        clean.caveats.clear();
        let text = render_human(&clean, &RenderOptions::default());
        assert!(text.contains("no obligation was violated"), "{text}");
        assert!(!text.contains("no more"), "{text}");
        assert!(!text.contains("conditional on"), "{text}");
    }

    #[test]
    fn plain_rendering_has_no_escape_codes() {
        let text = render_human(&report(), &RenderOptions::default());
        assert!(!text.contains('\u{1b}'), "{text}");
    }

    #[test]
    fn color_rendering_adds_escape_codes() {
        let text = render_human(
            &report(),
            &RenderOptions {
                color: true,
                show_systems: false,
            },
        );
        assert!(text.contains('\u{1b}'));
    }

    #[test]
    fn systems_are_shown_only_on_request() {
        let plain = render_human(&report(), &RenderOptions::default());
        assert!(!plain.contains("(check-sat)"), "{plain}");
        let verbose = render_human(
            &report(),
            &RenderOptions {
                color: false,
                show_systems: true,
            },
        );
        assert!(verbose.contains("(check-sat)"), "{verbose}");
    }

    #[test]
    fn an_unconfirmed_model_is_flagged_in_the_output() {
        let mut unconfirmed = report();
        if let Discharge::Violated { counterexample } = &mut unconfirmed.obligations[0].discharge {
            counterexample.cross_checked = false;
        }
        let text = render_human(&unconfirmed, &RenderOptions::default());
        assert!(text.contains("did not confirm"), "{text}");
    }

    #[test]
    fn inconclusive_obligations_render_their_reason() {
        let mut undecided = report();
        undecided.obligations[1].discharge = Discharge::Inconclusive {
            reason: Inconclusive::SolverUnknown,
        };
        let text = render_human(&undecided, &RenderOptions::default());
        assert!(text.contains("could not decide"), "{text}");
    }

    #[test]
    fn rendering_is_deterministic() {
        let report = report();
        let options = RenderOptions::default();
        assert_eq!(
            render_human(&report, &options),
            render_human(&report, &options)
        );
    }
}
