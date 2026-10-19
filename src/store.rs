use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    #[serde(default)]
    pub subagents: crate::subagents::State,
    #[serde(flatten)]
    pub options: crate::session_options::Options,
    pub id: String,
    pub cwd: PathBuf,
    pub model: String,
    #[serde(default)]
    pub pending_context: Vec<Value>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub permission_policy: Option<crate::permissions::PermissionPolicy>,
    #[serde(default = "default_collaboration_mode")]
    pub collaboration_mode: String,
    #[serde(default)]
    pub tool_receipts: BTreeMap<String, Value>,
    #[serde(default)]
    pub native_command_receipts: BTreeMap<String, Value>,
    #[serde(default)]
    pub tool_grants: std::collections::BTreeSet<String>,
    pub revision: u64,
    pub status: String,
    pub turn: Option<Value>,
    pub approval_policy: String,
    #[serde(default = "crate::guardian::legacy_reviewer")]
    pub approvals_reviewer: String,
    #[serde(default)]
    pub review_config: crate::guardian::ReviewConfig,
    pub budget: Option<Value>,
    #[serde(default)]
    pub additional_directories: Vec<PathBuf>,
    #[serde(default)]
    pub unrestricted: bool,
    #[serde(default)]
    pub usage: Value,
    #[serde(default)]
    pub token_usage: Value,
    pub receipts: BTreeMap<String, Value>,
    #[serde(default)]
    pub steer_receipts: BTreeMap<String, Value>,
    #[serde(default)]
    pub turns: Vec<Value>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub goal: Option<Value>,
    #[serde(default)]
    pub mcp_servers: Value,
}

fn default_collaboration_mode() -> String {
    "default".into()
}

impl Thread {
    pub fn is_plan(&self) -> bool {
        self.collaboration_mode == "plan"
    }
    pub fn requested_permissions(&self) -> Result<crate::permissions::PermissionPolicy> {
        self.permission_policy.clone().map(Ok).unwrap_or_else(||crate::permissions::PermissionPolicy::from_rpc(&serde_json::json!({"sandbox":if self.unrestricted{"danger-full-access"}else{"workspace-write"},"additionalDirectories":self.additional_directories}),&self.cwd))
    }
    pub fn permissions(&self) -> Result<crate::permissions::PermissionPolicy> {
        if self.is_plan() {
            crate::permissions::PermissionPolicy::from_rpc(
                &serde_json::json!({"sandboxPolicy":{"type":"readOnly","networkAccess":false}}),
                &self.cwd,
            )
        } else {
            self.requested_permissions()
        }
    }
    pub fn checkpoint_turn(&mut self) {
        if let Some(turn) = &self.turn {
            if let Some(existing) = self.turns.iter_mut().find(|t| t["id"] == turn["id"]) {
                *existing = turn.clone();
            } else {
                self.turns.push(turn.clone());
            }
        }
        self.updated_at = crate::now();
    }
}

pub struct Store {
    pub root: PathBuf,
    _lock: File,
    ephemeral: std::sync::Mutex<BTreeMap<String, (Thread, tempfile::TempDir)>>,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut directory = std::fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.create(path)?;
        let root = path.canonicalize()?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join("writer.lock"))?;
        lock.try_lock_exclusive()
            .context("another harness owns this state directory")?;
        // A killed process cannot run TempDir cleanup. Never resume its ephemeral sessions.
        for directory in ["ephemeral", "command-leases"] {
            let temporary = root.join(directory);
            if temporary.exists() {
                std::fs::remove_dir_all(&temporary)?;
            }
        }
        std::fs::create_dir_all(root.join("threads"))?;
        Ok(Self {
            root,
            _lock: lock,
            ephemeral: Default::default(),
        })
    }
    fn path(&self, id: &str) -> Result<PathBuf> {
        uuid::Uuid::parse_str(id).context("invalid thread ID")?;
        Ok(self.root.join("threads").join(format!("{id}.json")))
    }
    pub fn save(&self, thread: &Thread) -> Result<()> {
        let path = self.path(&thread.id)?;
        if thread.options.ephemeral {
            let mut sessions = self.ephemeral.lock().unwrap();
            if let Some((saved, _)) = sessions.get_mut(&thread.id) {
                *saved = thread.clone();
            } else {
                let temporary = self.root.join("ephemeral");
                std::fs::create_dir_all(&temporary)?;
                sessions.insert(
                    thread.id.clone(),
                    (
                        thread.clone(),
                        tempfile::Builder::new()
                            .prefix("session-")
                            .tempdir_in(temporary)?,
                    ),
                );
            }
            return Ok(());
        }
        let temp = path.with_extension("tmp");
        let mut file = File::create(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(thread)?)?;
        file.sync_all()?;
        std::fs::rename(temp, path)?;
        File::open(self.root.join("threads"))?.sync_all()?;
        Ok(())
    }
    pub fn load(&self, id: &str) -> Result<Thread> {
        if let Some((thread, _)) = self.ephemeral.lock().unwrap().get(id) {
            return Ok(thread.clone());
        }
        Ok(serde_json::from_slice(&std::fs::read(self.path(id)?)?)?)
    }
    pub fn native_root(&self, id: &str) -> PathBuf {
        self.ephemeral
            .lock()
            .unwrap()
            .get(id)
            .map_or_else(|| self.root.clone(), |(_, dir)| dir.path().to_owned())
    }
    pub fn forget_ephemeral(&self, id: &str) {
        self.ephemeral.lock().unwrap().remove(id);
    }
    pub fn list(&self) -> Result<Vec<Thread>> {
        let mut threads = Vec::new();
        for entry in std::fs::read_dir(self.root.join("threads"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|s| s == "json") {
                threads.push(serde_json::from_slice(&std::fs::read(path)?)?);
            }
        }
        Ok(threads)
    }
    pub fn all(&self) -> Result<Vec<Thread>> {
        let mut threads = self.list()?;
        threads.extend(
            self.ephemeral
                .lock()
                .unwrap()
                .values()
                .map(|(t, _)| t.clone()),
        );
        Ok(threads)
    }
    pub fn event(&self, id: &str, event: &Value) -> Result<()> {
        if self.ephemeral.lock().unwrap().contains_key(id) {
            return Ok(());
        }
        let path = self.path(id)?.with_extension("jsonl");
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{}", event)?;
        file.sync_data()?;
        Ok(())
    }
}
