//! Stable diagnostic codes. A code never changes meaning once published;
//! retired codes stay in the table. `noxi explain CODE` prints the entry.
//!
//! The letter names the area: `R` resolving and updating sources, `E`
//! evaluation, `F` fetching, `S` substitution, `B` building, `A`
//! activation and verification, `D` the daemon.

/// One diagnostic code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Code {
	pub id: &'static str,
	/// One line: what went wrong.
	pub summary: &'static str,
	/// Why it happens and what to do, as plain paragraphs.
	pub explanation: &'static str,
}

pub const RESOLVE_SOURCE: &str = "R0001";
pub const UPDATE_INPUTS: &str = "R0002";
pub const EVALUATION: &str = "E0001";
pub const BUILD: &str = "B0001";
pub const BUILD_RESULT: &str = "B0002";
pub const ACTIVATION_REFUSED: &str = "A0001";
pub const PREPARE_ACTIVATION: &str = "A0002";
pub const ACTIVATION_FAILED: &str = "A0003";
pub const VERIFY: &str = "A0004";
pub const ROLLBACK_TARGET: &str = "A0005";

pub const CODES: &[Code] = &[
	Code {
		id: RESOLVE_SOURCE,
		summary: "the configuration source could not be resolved",
		explanation: "noxi could not find the configuration to build: the flake directory or file does not exist, has no lock file where one is required, or does not provide the selected attribute.\n\nNothing was built or activated. Check the --flake or --file argument, and for a flake without a #HOST fragment, that nixosConfigurations has an entry for this machine's host name.",
	},
	Code {
		id: UPDATE_INPUTS,
		summary: "flake inputs could not be updated",
		explanation: "--update re-locks flake inputs before evaluation, and it failed: an input could not be fetched, does not exist, or the lock file could not be written.\n\nNothing was built or activated, and the existing lock file is unchanged. Fix the input reference, check network access, or rebuild without --update.",
	},
	Code {
		id: EVALUATION,
		summary: "the configuration failed to evaluate",
		explanation: "Evaluating the Nix expression for the system failed before any build started: a syntax error, an undefined variable or option, a type error, an assertion, or infinite recursion.\n\nThe diagnostic points at the source location when one is known. Nothing was built or activated.",
	},
	Code {
		id: BUILD,
		summary: "a derivation failed to build",
		explanation: "A builder exited unsuccessfully. The diagnostic names the derivation, the step it failed in, and an excerpt of its output; the full log is kept and `noxi logs` shows it.\n\nNothing was activated. Builds that had already finished stay in the store and are reused by the next attempt.",
	},
	Code {
		id: BUILD_RESULT,
		summary: "the build finished but its result could not be used",
		explanation: "The build succeeded, but noxi could not record its result: the out-link or temporary directory could not be created, or the result is not where the build said it would be.\n\nNothing was activated. Check permissions on the out-link location and TMPDIR.",
	},
	Code {
		id: ACTIVATION_REFUSED,
		summary: "the action needs root",
		explanation: "test, boot, switch and rollback change the system profile or the running system, which only root may do. noxi checks this before reading or building anything.\n\nRun the command as root, or use `noxi system build` to build without activating.",
	},
	Code {
		id: PREPARE_ACTIVATION,
		summary: "activation could not be prepared",
		explanation: "The build succeeded, but the new generation could not be set up for activation: the snapshot journal could not be written, the closure has no activation program, or the profile could not be updated.\n\nThe diagnostic says whether the profile changed. The running system was not touched.",
	},
	Code {
		id: ACTIVATION_FAILED,
		summary: "activation failed",
		explanation: "The activation program (switch-to-configuration) exited unsuccessfully. Activation is not atomic: units may already have been stopped, started or restarted before the failure.\n\nThe diagnostic says whether the running system and the boot default changed. Inspect failed units with `journalctl -u UNIT -b`, and return to the previous generation with `noxi system rollback`.",
	},
	Code {
		id: VERIFY,
		summary: "the system state after activation is not what was requested",
		explanation: "After activation, noxi reads back the profile, the running system and the boot default. They did not match what the action should have produced.\n\nThe report shows the observed state; trust it over the requested action.",
	},
	Code {
		id: ROLLBACK_TARGET,
		summary: "there is no generation to roll back to",
		explanation: "Rollback selects the newest generation older than the current one, or the one given with --to. That generation does not exist.\n\nList the profile's generations, and pass an existing one with --to.",
	},
];

/// The entry for `id`, ignoring ASCII case.
pub fn lookup(id: &str) -> Option<&'static Code> {
	CODES.iter().find(|code| code.id.eq_ignore_ascii_case(id))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn codes_are_unique_and_well_formed() {
		let mut seen = std::collections::HashSet::new();
		for code in CODES {
			assert!(seen.insert(code.id), "duplicate {}", code.id);
			let (area, number) = code.id.split_at(1);
			assert!("REFSBAD".contains(area), "{}", code.id);
			assert!(number.len() == 4 && number.bytes().all(|b| b.is_ascii_digit()));
			assert!(!code.summary.ends_with('.'));
		}
		assert_eq!(lookup("b0001").map(|code| code.id), Some(BUILD));
		assert_eq!(lookup("X9999"), None);
	}
}
