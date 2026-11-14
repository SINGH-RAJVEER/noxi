//! CppNix's `Suggestions`: the names closest to one that was not found.
//!
//! Suggestions from several lookups are merged before they are trimmed, as
//! `InstallableFlake::getCursors` merges those of its candidate paths.
use std::collections::BTreeSet;

/// Candidates ordered by edit distance, then by name.
#[derive(Debug, Default, Clone)]
pub struct Suggestions(BTreeSet<(usize, String)>);

impl Suggestions {
	/// `Suggestions::bestMatches`: every candidate with its distance.
	pub fn best_matches<'a>(candidates: impl IntoIterator<Item = &'a str>, query: &str) -> Self {
		Self(
			candidates
				.into_iter()
				.map(|candidate| {
					(
						levenshtein(query.as_bytes(), candidate.as_bytes()),
						candidate.to_owned(),
					)
				})
				.collect(),
		)
	}

	pub fn extend(&mut self, other: Suggestions) {
		self.0.extend(other.0);
	}

	/// The `Did you mean ...?` line CppNix prints after an error message,
	/// from at most five candidates within an edit distance of two.
	pub fn did_you_mean(&self) -> Option<String> {
		let kept: Vec<&str> = self
			.0
			.iter()
			.take(5)
			.take_while(|(distance, _)| *distance <= 2)
			.map(|(_, name)| name.as_str())
			.collect();
		let shown = match kept.as_slice() {
			[] => return None,
			[one] => (*one).to_owned(),
			[first, rest @ ..] => {
				let mut shown = format!("one of {first}");
				for (index, name) in rest.iter().enumerate() {
					shown.push_str(if index + 1 == rest.len() {
						" or "
					} else {
						", "
					});
					shown.push_str(name);
				}
				shown
			}
		};
		Some(format!("Did you mean {shown}?"))
	}

	/// `message`, followed by the suggestion line when there is one.
	pub fn append_to(&self, message: String) -> String {
		match self.did_you_mean() {
			Some(line) => format!("{message}\n{line}"),
			None => message,
		}
	}
}

/// `levenshteinDistance`, over bytes.
fn levenshtein(first: &[u8], second: &[u8]) -> usize {
	let mut previous: Vec<usize> = (0..=second.len()).collect();
	let mut current = vec![0; second.len() + 1];
	for (i, a) in first.iter().enumerate() {
		current[0] = i + 1;
		for (j, b) in second.iter().enumerate() {
			let substitution = previous[j] + usize::from(a != b);
			current[j + 1] = (previous[j + 1] + 1).min(current[j] + 1).min(substitution);
		}
		std::mem::swap(&mut previous, &mut current);
	}
	previous[second.len()]
}

#[cfg(test)]
mod tests {
	use super::Suggestions;

	#[test]
	fn merged_suggestions_are_trimmed_after_merging() {
		let names = ["foo", "fo1", "fo2", "fooo", "foooo", "fooooo", "fooooo1"];
		let mut suggestions = Suggestions::best_matches(names, "fob");
		suggestions.extend(Suggestions::best_matches(["packages"], "fob"));
		assert_eq!(
			suggestions.did_you_mean().as_deref(),
			Some("Did you mean one of fo1, fo2, foo or fooo?")
		);
		assert_eq!(
			Suggestions::best_matches(["foo"], "bar").did_you_mean(),
			None
		);
	}
}
