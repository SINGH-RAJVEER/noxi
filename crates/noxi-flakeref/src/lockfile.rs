//! `flake.lock` files (`src/libflake/lockfile.cc`): a graph of locked
//! nodes whose edges are node references or `follows` paths, written as
//! version 7 JSON with nlohmann's two-space layout.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::error::{Error, Result, warn};
use crate::flake_ref::FlakeRef;
use crate::input::{
	Attr, FetchSettings, attrs_to_json, json_to_attrs, warn_unlocked_with_nar_hash,
};

/// A path of input names from the root, such as `["dwarffs", "nixpkgs"]`.
pub type InputAttrPath = Vec<String>;

pub fn print_input_attr_path(path: &[String]) -> String {
	path.join("/")
}

/// `parseInputAttrPath`.
pub fn parse_input_attr_path(s: &str) -> Result<InputAttrPath> {
	s.split('/')
		.filter(|elem| !elem.is_empty())
		.map(|elem| {
			if crate::input::is_flake_id(elem) {
				Ok(elem.to_owned())
			} else {
				Err(Error::new(format!(
					"invalid flake input attribute path element '{elem}'"
				)))
			}
		})
		.collect()
}

pub type NodeId = usize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edge {
	Node(NodeId),
	Follows(InputAttrPath),
}

#[derive(Clone, Debug)]
pub struct LockedNode {
	pub locked_ref: FlakeRef,
	pub original_ref: FlakeRef,
	pub is_flake: bool,
	pub parent_input_attr_path: Option<InputAttrPath>,
}

#[derive(Clone, Debug, Default)]
pub struct Node {
	pub inputs: BTreeMap<String, Edge>,
	/// `None` for the root node.
	pub locked: Option<LockedNode>,
}

/// A lock file; node 0 is the root.
#[derive(Clone, Debug)]
pub struct LockFile {
	pub nodes: Vec<Node>,
}

impl Default for LockFile {
	fn default() -> Self {
		Self {
			nodes: vec![Node::default()],
		}
	}
}

pub const ROOT: NodeId = 0;

impl LockedNode {
	/// Check a node read from a lock file and mark it final.
	fn from_json(settings: &FetchSettings, json: &serde_json::Value) -> Result<Self> {
		let get_ref = |attr: &str, info: Option<&str>| -> Result<FlakeRef> {
			let Some(value) = json.get(attr) else {
				return Err(Error::new(format!(
					"attribute '{attr}' missing in lock file"
				)));
			};
			let mut attrs = json_to_attrs(value)?;
			if let Some(info) = info
				&& let Some(info) = json.get(info)
			{
				attrs.extend(json_to_attrs(info)?);
			}
			FlakeRef::from_attrs(settings, &attrs)
		};
		let mut locked_ref = get_ref("locked", Some("info"))?;
		let original_ref = get_ref("original", None)?;
		let is_flake = json.get("flake").and_then(|f| f.as_bool()).unwrap_or(true);
		let parent_input_attr_path = match json.get("parent") {
			Some(serde_json::Value::Array(items)) => Some(
				items
					.iter()
					.map(|item| item.as_str().unwrap_or_default().to_owned())
					.collect(),
			),
			_ => None,
		};
		if !locked_ref.input.is_locked(settings) && locked_ref.input.is_relative().is_none() {
			if locked_ref.input.get_nar_hash()?.is_some() {
				warn_unlocked_with_nar_hash("Lock file entry", &locked_ref.to_string());
			} else {
				return Err(Error::new(format!(
					"Lock file contains unlocked input '{}'. Use '--allow-dirty-locks' to accept this lock file.",
					crate::json::dump(
						&serde_json::Value::Object(attrs_to_json(&locked_ref.input.attrs)?),
						None
					)
				)));
			}
		}
		locked_ref
			.input
			.attrs
			.insert("__final".into(), Attr::Bool(true));
		Ok(Self {
			locked_ref,
			original_ref,
			is_flake,
			parent_input_attr_path,
		})
	}
}

impl LockFile {
	/// Parse a lock file; `path` is only used in messages.
	pub fn parse(settings: &FetchSettings, contents: &str, path: &str) -> Result<Self> {
		let json: serde_json::Value = serde_json::from_str(contents)
			.map_err(|e| Error::new(format!("Could not parse '{path}': {e}")))?;
		let version = json.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
		if !(5..=7).contains(&version) {
			return Err(Error::new(format!(
				"lock file '{path}' has unsupported version {version}"
			)));
		}
		let root_key = json
			.get("root")
			.and_then(|r| r.as_str())
			.ok_or_else(|| Error::new(format!("lock file '{path}' lacks a root")))?
			.to_owned();
		let nodes_json = json.get("nodes").cloned().unwrap_or_default();
		let mut lock_file = LockFile::default();
		let mut node_map: HashMap<String, NodeId> = HashMap::new();
		node_map.insert(root_key.clone(), ROOT);

		let mut stack: Vec<(NodeId, serde_json::Value)> =
			vec![(ROOT, nodes_json.get(&root_key).cloned().unwrap_or_default())];
		while let Some((node, json_node)) = stack.pop() {
			let Some(inputs) = json_node.get("inputs").and_then(|i| i.as_object()) else {
				continue;
			};
			for (name, value) in inputs {
				if let Some(items) = value.as_array() {
					let path = items
						.iter()
						.map(|item| item.as_str().unwrap_or_default().to_owned())
						.collect();
					lock_file.nodes[node]
						.inputs
						.insert(name.clone(), Edge::Follows(path));
					continue;
				}
				let key = value.as_str().unwrap_or_default().to_owned();
				let child = match node_map.get(&key) {
					Some(&child) => child,
					None => {
						let Some(json_node2) = nodes_json.get(&key) else {
							return Err(Error::new(format!(
								"lock file references missing node '{key}'"
							)));
						};
						let locked = LockedNode::from_json(settings, json_node2)?;
						lock_file.nodes.push(Node {
							inputs: BTreeMap::new(),
							locked: Some(locked),
						});
						let child = lock_file.nodes.len() - 1;
						node_map.insert(key, child);
						stack.push((child, json_node2.clone()));
						child
					}
				};
				if child == ROOT {
					return Err(Error::new("lock file contains cycle to root node"));
				}
				lock_file.nodes[node]
					.inputs
					.insert(name.clone(), Edge::Node(child));
			}
		}
		Ok(lock_file)
	}

	pub fn add_node(&mut self, node: Node) -> NodeId {
		self.nodes.push(node);
		self.nodes.len() - 1
	}

	/// `LockFile::toJSON`: node keys are input names, made unique with
	/// `_2`, `_3`... in depth-first order.
	pub fn to_json(&self) -> Result<(serde_json::Value, HashMap<NodeId, String>)> {
		let mut nodes = serde_json::Map::new();
		let mut node_keys: HashMap<NodeId, String> = HashMap::new();
		let mut keys: BTreeSet<String> = BTreeSet::new();
		let root = self.dump_node("root", ROOT, &mut nodes, &mut node_keys, &mut keys)?;
		let mut json = serde_json::Map::new();
		json.insert("nodes".into(), serde_json::Value::Object(nodes));
		json.insert("root".into(), root.into());
		json.insert("version".into(), 7.into());
		Ok((serde_json::Value::Object(json), node_keys))
	}

	fn dump_node(
		&self,
		key: &str,
		node: NodeId,
		nodes: &mut serde_json::Map<String, serde_json::Value>,
		node_keys: &mut HashMap<NodeId, String>,
		keys: &mut BTreeSet<String>,
	) -> Result<String> {
		if let Some(key) = node_keys.get(&node) {
			return Ok(key.clone());
		}
		let mut key = key.to_owned();
		if !keys.insert(key.clone()) {
			for n in 2.. {
				let candidate = format!("{key}_{n}");
				if keys.insert(candidate.clone()) {
					key = candidate;
					break;
				}
			}
		}
		node_keys.insert(node, key.clone());

		let mut n = serde_json::Map::new();
		let data = &self.nodes[node];
		if !data.inputs.is_empty() {
			let mut inputs = serde_json::Map::new();
			for (name, edge) in &data.inputs {
				let value = match edge {
					Edge::Node(child) => serde_json::Value::String(
						self.dump_node(name, *child, nodes, node_keys, keys)?,
					),
					Edge::Follows(path) => serde_json::Value::Array(
						path.iter()
							.map(|p| serde_json::Value::String(p.clone()))
							.collect(),
					),
				};
				inputs.insert(name.clone(), value);
			}
			n.insert("inputs".into(), serde_json::Value::Object(inputs));
		}
		if let Some(locked) = &data.locked {
			n.insert(
				"original".into(),
				serde_json::Value::Object(attrs_to_json(&locked.original_ref.to_attrs())?),
			);
			let mut locked_json = attrs_to_json(&locked.locked_ref.to_attrs())?;
			locked_json.remove("__final");
			n.insert("locked".into(), serde_json::Value::Object(locked_json));
			if !locked.is_flake {
				n.insert("flake".into(), false.into());
			}
			if let Some(parent) = &locked.parent_input_attr_path {
				n.insert(
					"parent".into(),
					serde_json::Value::Array(
						parent
							.iter()
							.map(|p| serde_json::Value::String(p.clone()))
							.collect(),
					),
				);
			}
		}
		nodes.insert(key.clone(), serde_json::Value::Object(n));
		Ok(key)
	}

	/// The lock file text, without the trailing newline CppNix adds when
	/// writing it.
	pub fn to_string_with_keys(&self) -> Result<(String, HashMap<NodeId, String>)> {
		let (json, keys) = self.to_json()?;
		Ok((crate::json::dump(&json, Some(2)), keys))
	}

	pub fn render(&self) -> Result<String> {
		Ok(self.to_string_with_keys()?.0)
	}

	/// `LockFile::operator==`: equal JSON.
	pub fn same_as(&self, other: &LockFile) -> Result<bool> {
		Ok(self.to_json()?.0 == other.to_json()?.0)
	}

	/// `LockFile::findInput`: follow a path from the root, resolving
	/// `follows` edges.
	pub fn find_input(&self, path: &[String]) -> Result<Option<NodeId>> {
		let mut visited = Vec::new();
		self.do_find(path, &mut visited)
	}

	fn do_find(&self, path: &[String], visited: &mut Vec<InputAttrPath>) -> Result<Option<NodeId>> {
		if let Some(found) = visited.iter().position(|p| p.as_slice() == path) {
			let mut cycle: Vec<String> = visited[found..]
				.iter()
				.map(|p| print_input_attr_path(p))
				.collect();
			cycle.push(print_input_attr_path(path));
			return Err(Error::new(format!(
				"follow cycle detected: [{}]",
				cycle.join(" -> ")
			)));
		}
		visited.push(path.to_vec());
		let mut pos = ROOT;
		for elem in path {
			match self.nodes[pos].inputs.get(elem) {
				Some(Edge::Node(child)) => pos = *child,
				Some(Edge::Follows(follows)) => match self.do_find(follows, visited)? {
					Some(node) => pos = node,
					None => return Ok(None),
				},
				None => return Ok(None),
			}
		}
		Ok(Some(pos))
	}

	/// `LockFile::getAllInputs`.
	pub fn get_all_inputs(&self) -> BTreeMap<InputAttrPath, Edge> {
		let mut done = BTreeSet::new();
		let mut res = BTreeMap::new();
		self.collect_inputs(&[], ROOT, &mut done, &mut res);
		res
	}

	fn collect_inputs(
		&self,
		prefix: &[String],
		node: NodeId,
		done: &mut BTreeSet<NodeId>,
		res: &mut BTreeMap<InputAttrPath, Edge>,
	) {
		if !done.insert(node) {
			return;
		}
		for (id, edge) in &self.nodes[node].inputs {
			let mut path = prefix.to_vec();
			path.push(id.clone());
			res.entry(path.clone()).or_insert_with(|| edge.clone());
			if let Edge::Node(child) = edge {
				self.collect_inputs(&path, *child, done, res);
			}
		}
	}

	/// `LockFile::check`: every `follows` target must exist.
	pub fn check(&self) -> Result<()> {
		for (path, edge) in self.get_all_inputs() {
			if let Edge::Follows(follows) = edge
				&& !follows.is_empty()
				&& self.find_input(&follows)?.is_none()
			{
				return Err(Error::new(format!(
					"input '{}' follows a non-existent input '{}'",
					print_input_attr_path(&path),
					print_input_attr_path(&follows)
				)));
			}
		}
		Ok(())
	}

	/// `LockFile::isUnlocked`: the first node that is not locked and final.
	pub fn is_unlocked(&self, settings: &FetchSettings) -> Result<Option<FlakeRef>> {
		let mut seen = BTreeSet::new();
		let mut order = Vec::new();
		let mut stack = vec![ROOT];
		while let Some(node) = stack.pop() {
			if !seen.insert(node) {
				continue;
			}
			order.push(node);
			for edge in self.nodes[node].inputs.values().rev() {
				if let Edge::Node(child) = edge {
					stack.push(*child);
				}
			}
		}
		order.sort_unstable();
		for node in order {
			if node == ROOT {
				continue;
			}
			let Some(locked) = &self.nodes[node].locked else {
				continue;
			};
			let input = &locked.locked_ref.input;
			let considered_locked = input.is_locked(settings)
				|| (settings.allow_dirty_locks && input.get_nar_hash()?.is_some());
			if (!considered_locked || !input.is_final()) && input.is_relative().is_none() {
				return Ok(Some(locked.locked_ref.clone()));
			}
		}
		Ok(None)
	}

	fn describe_edge(&self, edge: &Edge) -> String {
		match edge {
			Edge::Node(node) => {
				let locked = &self.nodes[*node]
					.locked
					.as_ref()
					.expect("non-root")
					.locked_ref;
				let mut s = format!("'{locked}'");
				if let Ok(Some(last_modified)) = locked.input.get_last_modified() {
					s.push_str(&format!(" ({})", format_date(last_modified)));
				}
				s
			}
			Edge::Follows(path) => format!("follows '{}'", print_input_attr_path(path)),
		}
	}

	fn edges_equal(&self, a: &Edge, other: &LockFile, b: &Edge) -> bool {
		match (a, b) {
			(Edge::Node(n1), Edge::Node(n2)) => {
				let l1 = &self.nodes[*n1]
					.locked
					.as_ref()
					.expect("non-root")
					.locked_ref;
				let l2 = &other.nodes[*n2]
					.locked
					.as_ref()
					.expect("non-root")
					.locked_ref;
				l1 == l2
			}
			(Edge::Follows(f1), Edge::Follows(f2)) => f1 == f2,
			_ => false,
		}
	}

	/// `LockFile::diff`, with ANSI colours as CppNix emits them.
	pub fn diff(old: &LockFile, new: &LockFile) -> String {
		let old_flat: Vec<_> = old.get_all_inputs().into_iter().collect();
		let new_flat: Vec<_> = new.get_all_inputs().into_iter().collect();
		let (mut i, mut j) = (0, 0);
		let mut res = String::new();
		while i < old_flat.len() || j < new_flat.len() {
			if j < new_flat.len() && (i == old_flat.len() || old_flat[i].0 > new_flat[j].0) {
				res.push_str(&format!(
					"\u{2022} \x1b[32;1mAdded input '{}':\x1b[0m\n    {}\n",
					print_input_attr_path(&new_flat[j].0),
					new.describe_edge(&new_flat[j].1)
				));
				j += 1;
			} else if i < old_flat.len() && (j == new_flat.len() || old_flat[i].0 < new_flat[j].0) {
				res.push_str(&format!(
					"\u{2022} \x1b[31;1mRemoved input '{}'\x1b[0m\n",
					print_input_attr_path(&old_flat[i].0)
				));
				i += 1;
			} else {
				if !old.edges_equal(&old_flat[i].1, new, &new_flat[j].1) {
					res.push_str(&format!(
						"\u{2022} \x1b[1mUpdated input '{}':\x1b[0m\n    {}\n  \u{2192} {}\n",
						print_input_attr_path(&old_flat[i].0),
						old.describe_edge(&old_flat[i].1),
						new.describe_edge(&new_flat[j].1)
					));
				}
				i += 1;
				j += 1;
			}
		}
		res
	}
}

/// `%Y-%m-%d` of a Unix time, in UTC.
pub fn format_date(time: u64) -> String {
	let (y, m, d, ..) = civil_from_unix(time);
	format!("{y:04}-{m:02}-{d:02}")
}

/// `%Y%m%d%H%M%S` of a Unix time, in UTC, as `lastModifiedDate` uses.
pub fn format_last_modified_date(time: u64) -> String {
	let (y, m, d, hh, mm, ss) = civil_from_unix(time);
	format!("{y:04}{m:02}{d:02}{hh:02}{mm:02}{ss:02}")
}

fn civil_from_unix(time: u64) -> (i64, u32, u32, u64, u64, u64) {
	let days = (time / 86400) as i64;
	let secs = time % 86400;
	// Howard Hinnant's days-to-civil algorithm.
	let z = days + 719468;
	let era = z.div_euclid(146097);
	let doe = z - era * 146097;
	let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
	let y = yoe + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
	let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
	let y = if m <= 2 { y + 1 } else { y };
	(y, m, d, secs / 3600, secs % 3600 / 60, secs % 60)
}

/// Warn about an override for an input that does not exist.
pub fn warn_override_for_missing(prefix: &[String], name: &str) {
	warn(&format!(
		"input '{}' has an override for a non-existent input '{name}'",
		print_input_attr_path(prefix)
	));
}

#[cfg(test)]
mod tests {
	use super::*;

	const LOCK: &str = r#"{
  "nodes": {
    "a": {
      "inputs": {
        "b": [
          "b"
        ]
      },
      "locked": {
        "lastModified": 1,
        "narHash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        "path": "/tmp/a",
        "type": "path"
      },
      "original": {
        "path": "/tmp/a",
        "type": "path"
      }
    },
    "b": {
      "flake": false,
      "locked": {
        "lastModified": 2,
        "narHash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        "path": "/tmp/b",
        "type": "path"
      },
      "original": {
        "path": "/tmp/b",
        "type": "path"
      }
    },
    "root": {
      "inputs": {
        "a": "a",
        "b": "b"
      }
    }
  },
  "root": "root",
  "version": 7
}"#;

	#[test]
	fn round_trips_byte_for_byte() {
		let settings = FetchSettings {
			flakes: true,
			..FetchSettings::default()
		};
		let lock = LockFile::parse(&settings, LOCK, "flake.lock").unwrap();
		assert_eq!(lock.render().unwrap(), LOCK);
		assert_eq!(
			lock.find_input(&["a".into(), "b".into()]).unwrap(),
			lock.find_input(&["b".into()]).unwrap()
		);
		lock.check().unwrap();
	}

	#[test]
	fn formats_dates() {
		assert_eq!(format_last_modified_date(1577836800), "20200101000000");
		assert_eq!(format_date(1792713716), "2026-10-23");
	}
}
