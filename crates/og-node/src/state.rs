//! What the node remembers between runs: the repos it follows and how to
//! reach its peers.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    path::Path,
};

use anyhow::{Context, Result};
use iroh::EndpointAddr;
use og_core::{NodeId, RepoId};
use serde::{Deserialize, Serialize};

const FILE: &str = "state.json";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub repos: BTreeMap<RepoId, RepoState>,
    /// Last known address of each peer.
    #[serde(default)]
    pub peers: BTreeMap<NodeId, EndpointAddr>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RepoState {
    /// Peers we replicate this repo with.
    #[serde(default)]
    pub peers: BTreeSet<NodeId>,
}

impl State {
    pub fn load(home: &Path) -> Result<Self> {
        match fs::read(home.join(FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("corrupt state.json"),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes atomically, so a crash never leaves a truncated file.
    pub fn save(&self, home: &Path) -> Result<()> {
        let tmp = home.join(format!("{FILE}.tmp"));
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&tmp, home.join(FILE))?;
        Ok(())
    }
}
