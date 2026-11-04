//! Sessions and horses: the small mutable objects of the store.
//!
//! A session is a head node in the DAG, a binding to a harness (harness,
//! native id, version, container) and a lineage: the horses it passed
//! through, each with the hash of its DNA. A horse is a DNA (TOML in the
//! store) and a namespace with its sessions. A new paddock run gets a new
//! horse by default.

use super::store::{Hash, Node, Store};
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

/// The harness that a session belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub harness: String,
    /// The id of the session in the harness. A fork gets its id when a
    /// codec projects it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    pub version: String,
    pub container: String,
}

/// One horse in the lineage of a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageStep {
    pub horse: String,
    pub dna: Hash,
    pub since_ms: u64,
}

/// Where a fork comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkOf {
    pub session: String,
    pub node: Hash,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub horse: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<Hash>,
    pub binding: Binding,
    /// The native session header as a blob, when the harness has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_header: Option<Hash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_of: Option<ForkOf>,
    pub lineage: Vec<LineageStep>,
    pub created_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Horse {
    pub id: String,
    pub dna: Hash,
    pub sessions: Vec<String>,
    pub created_ms: u64,
}

/// Ids of sessions and horses are file names: only these characters.
fn check_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        bail!("not a horse or session id: {id}");
    }
    Ok(())
}

impl Store {
    /// Make a horse with this DNA. The DNA must be TOML.
    pub fn new_horse(&self, dna: &str) -> Result<Horse> {
        dna.parse::<toml::Table>().context("the DNA is not TOML")?;
        let h = Horse {
            id: super::new_id("h")?,
            dna: self.put_blob("toml", dna.as_bytes())?,
            sessions: Vec::new(),
            created_ms: super::now_ms(),
        };
        let _l = self.lock()?;
        self.save_horse(&h)?;
        Ok(h)
    }

    fn save_horse(&self, h: &Horse) -> Result<()> {
        check_id(&h.id)?;
        self.write_ref(
            &format!("horses/{}.json", h.id),
            &serde_json::to_vec_pretty(h)?,
        )
    }

    fn save_session(&self, s: &Session) -> Result<()> {
        check_id(&s.id)?;
        self.write_ref(
            &format!("sessions/{}.json", s.id),
            &serde_json::to_vec_pretty(s)?,
        )
    }

    pub fn horse(&self, id: &str) -> Result<Horse> {
        check_id(id)?;
        let b = self
            .read_ref(&format!("horses/{id}.json"))?
            .with_context(|| format!("no horse {id}"))?;
        serde_json::from_slice(&b).with_context(|| format!("horse {id} is damaged"))
    }

    pub fn session(&self, id: &str) -> Result<Session> {
        check_id(id)?;
        let b = self
            .read_ref(&format!("sessions/{id}.json"))?
            .with_context(|| format!("no session {id}"))?;
        serde_json::from_slice(&b).with_context(|| format!("session {id} is damaged"))
    }

    pub fn horses(&self) -> Result<Vec<Horse>> {
        self.list_refs("horses")?
            .iter()
            .filter_map(|n| n.strip_suffix(".json"))
            .map(|id| self.horse(id))
            .collect()
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        self.list_refs("sessions")?
            .iter()
            .filter_map(|n| n.strip_suffix(".json"))
            .map(|id| self.session(id))
            .collect()
    }

    /// The session bound to this native id of a harness.
    pub fn session_by_native(&self, harness: &str, native_id: &str) -> Result<Option<Session>> {
        Ok(self.sessions()?.into_iter().find(|s| {
            s.binding.harness == harness && s.binding.native_id.as_deref() == Some(native_id)
        }))
    }

    /// Make a session in a horse. It joins the namespace of the horse.
    pub fn new_session(&self, horse: &str, binding: Binding) -> Result<Session> {
        let _l = self.lock()?;
        let mut h = self.horse(horse)?;
        let s = Session {
            id: super::new_id("s")?,
            horse: h.id.clone(),
            head: None,
            binding,
            native_header: None,
            fork_of: None,
            lineage: vec![LineageStep {
                horse: h.id.clone(),
                dna: h.dna.clone(),
                since_ms: super::now_ms(),
            }],
            created_ms: super::now_ms(),
        };
        self.save_session(&s)?;
        h.sessions.push(s.id.clone());
        self.save_horse(&h)?;
        Ok(s)
    }

    /// Change a session under the writer lock.
    pub fn update_session(&self, id: &str, f: impl FnOnce(&mut Session)) -> Result<Session> {
        let _l = self.lock()?;
        let mut s = self.session(id)?;
        f(&mut s);
        self.save_session(&s)?;
        Ok(s)
    }

    /// The nodes of a session, root first.
    pub fn session_nodes(&self, s: &Session) -> Result<Vec<Node>> {
        self.chain(s.head.as_ref())
    }

    /// Find a node of a session by its hash, a hash prefix of at least 6
    /// digits, or its native key (`seq:12`).
    pub fn resolve_node(&self, s: &Session, spec: &str) -> Result<Hash> {
        let nodes = self.session_nodes(s)?;
        let found: Vec<&Node> = if spec.contains(':') {
            nodes
                .iter()
                .filter(|n| n.meta.key.as_deref() == Some(spec))
                .collect()
        } else if spec.len() >= 6 {
            nodes
                .iter()
                .filter(|n| n.hash.as_str().starts_with(spec))
                .collect()
        } else {
            bail!("give a node as a hash of at least 6 digits or as a key such as seq:12");
        };
        match found.as_slice() {
            [n] => Ok(n.hash.clone()),
            [] => bail!("session {} has no node {spec}", s.id),
            _ => bail!("{spec} is not unique in session {}", s.id),
        }
    }

    /// Fork: a new session whose head is node `at` of `source`. The source
    /// does not change. The fork goes into `horse`, or into the horse of the
    /// source. It gets a native id when a codec projects it.
    pub fn fork(&self, source: &str, at: &Hash, horse: Option<&str>) -> Result<Session> {
        let src = self.session(source)?;
        if !self.session_nodes(&src)?.iter().any(|n| &n.hash == at) {
            bail!("node {} is not in session {source}", at.short());
        }
        let _l = self.lock()?;
        let mut h = self.horse(horse.unwrap_or(&src.horse))?;
        let mut lineage = src.lineage.clone();
        if lineage.last().map(|l| (&l.horse, &l.dna)) != Some((&h.id, &h.dna)) {
            lineage.push(LineageStep {
                horse: h.id.clone(),
                dna: h.dna.clone(),
                since_ms: super::now_ms(),
            });
        }
        let s = Session {
            id: super::new_id("s")?,
            horse: h.id.clone(),
            head: Some(at.clone()),
            binding: Binding {
                native_id: None,
                ..src.binding.clone()
            },
            native_header: src.native_header.clone(),
            fork_of: Some(ForkOf {
                session: src.id.clone(),
                node: at.clone(),
            }),
            lineage,
            created_ms: super::now_ms(),
        };
        self.save_session(&s)?;
        h.sessions.push(s.id.clone());
        self.save_horse(&h)?;
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::Origin;
    use super::*;

    const O: Origin = Origin {
        harness: "dsh",
        container: "test-jsonl",
        version: "1",
    };

    fn binding() -> Binding {
        Binding {
            harness: "dsh".into(),
            native_id: Some("session-1".into()),
            version: "1".into(),
            container: "test-jsonl".into(),
        }
    }

    fn three_rows(s: &Store) -> Vec<Hash> {
        let a = s.put_native(&O, None, "seq:0", b"a").unwrap();
        let b = s.put_native(&O, Some(&a), "seq:1", b"b").unwrap();
        let c = s.put_native(&O, Some(&b), "seq:2", b"c").unwrap();
        vec![a, b, c]
    }

    #[test]
    fn a_session_lives_in_the_namespace_of_its_horse() {
        let t = tempfile::tempdir().unwrap();
        let st = Store::open(t.path()).unwrap();
        let h = st.new_horse(super::super::DEFAULT_DNA).unwrap();
        let s = st.new_session(&h.id, binding()).unwrap();
        let h = st.horse(&h.id).unwrap();
        assert_eq!(h.sessions, vec![s.id.clone()]);
        assert_eq!(s.lineage.len(), 1);
        assert_eq!(s.lineage[0].dna, h.dna);
        assert_eq!(
            st.get(&h.dna).unwrap().raw,
            super::super::DEFAULT_DNA.as_bytes()
        );
        assert_eq!(
            st.session_by_native("dsh", "session-1")
                .unwrap()
                .unwrap()
                .id,
            s.id
        );
        assert!(st
            .session_by_native("claude", "session-1")
            .unwrap()
            .is_none());
        // A new run gets a new horse.
        let h2 = st.new_horse(super::super::DEFAULT_DNA).unwrap();
        assert_ne!(h.id, h2.id);
        assert_eq!(h.dna, h2.dna);
        assert!(st.new_horse("not = [toml").is_err());
    }

    #[test]
    fn fork_at_a_node_keeps_the_source_and_records_lineage() {
        let t = tempfile::tempdir().unwrap();
        let st = Store::open(t.path()).unwrap();
        let h = st.new_horse("origin = \"a\"\n").unwrap();
        let rows = three_rows(&st);
        let s = st.new_session(&h.id, binding()).unwrap();
        let s = st
            .update_session(&s.id, |s| s.head = Some(rows[2].clone()))
            .unwrap();
        assert_eq!(st.resolve_node(&s, "seq:1").unwrap(), rows[1]);
        assert_eq!(
            st.resolve_node(&s, &rows[1].as_str()[..8]).unwrap(),
            rows[1]
        );
        assert!(st.resolve_node(&s, "seq:9").is_err());
        assert!(st.resolve_node(&s, "ab").is_err());

        let f = st.fork(&s.id, &rows[1], None).unwrap();
        assert_eq!(f.head.as_ref(), Some(&rows[1]));
        assert_eq!(f.binding.native_id, None);
        assert_eq!(f.binding.harness, "dsh");
        assert_eq!(f.fork_of.as_ref().unwrap().session, s.id);
        assert_eq!(f.lineage, s.lineage);
        assert_eq!(st.session_nodes(&f).unwrap().len(), 2);
        assert_eq!(st.session(&s.id).unwrap().head.as_ref(), Some(&rows[2]));

        // Into another horse: the lineage gets the new horse and its DNA.
        let h2 = st.new_horse("origin = \"b\"\n").unwrap();
        let g = st.fork(&f.id, &rows[0], Some(&h2.id)).unwrap();
        assert_eq!(g.lineage.len(), 2);
        assert_eq!(g.lineage[1].horse, h2.id);
        assert_eq!(g.lineage[1].dna, h2.dna);
        assert_eq!(st.horse(&h2.id).unwrap().sessions, vec![g.id.clone()]);

        // A node from elsewhere is refused.
        let other = st.put_native(&O, None, "seq:0", b"z").unwrap();
        assert!(st.fork(&s.id, &other, None).is_err());
        assert_eq!(st.sessions().unwrap().len(), 3);
    }

    #[test]
    fn ids_are_file_names() {
        let t = tempfile::tempdir().unwrap();
        let st = Store::open(t.path()).unwrap();
        assert!(st.session("../x").is_err());
        assert!(st.horse("A").is_err());
    }
}
