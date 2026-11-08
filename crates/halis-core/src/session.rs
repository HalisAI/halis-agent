use crate::error::{CoreError, Result};
use crate::message::{Msg, TurnStats, Usage};
use crate::util;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Record {
    Meta {
        model: String,
        cwd: String,
        started: String,
    },
    Msg {
        msg: Msg,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    Turn {
        stats: TurnStats,
    },
}

pub struct Session {
    pub cwd: PathBuf,
    pub model_label: String,
    pub id: String,
    messages: Vec<Msg>,
    path: Option<PathBuf>,
    file: Option<std::fs::File>,
}

impl Session {
    pub fn create(cwd: PathBuf, model_label: &str) -> Result<Self> {
        let id = format!("{}-{}", util::timestamp(), short_id());
        let path = crate::config::Config::sessions_dir()
            .ok()
            .map(|d| d.join(format!("{id}.jsonl")));
        let mut session = Self {
            cwd,
            model_label: model_label.to_string(),
            id,
            messages: Vec::new(),
            path: path.clone(),
            file: None,
        };
        if let Some(p) = &path {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| CoreError::Config(format!("{}: {e}", parent.display())))?;
            }
            session.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .map_err(|e| CoreError::Config(format!("{}: {e}", p.display())))?,
            );
            let record = Record::Meta {
                model: session.model_label.clone(),
                cwd: session.cwd.to_string_lossy().into_owned(),
                started: util::timestamp(),
            };
            session.append(&record);
        }
        Ok(session)
    }

    pub fn in_memory(cwd: PathBuf, model_label: &str) -> Self {
        Self {
            cwd,
            model_label: model_label.to_string(),
            id: "memory".to_string(),
            messages: Vec::new(),
            path: None,
            file: None,
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| CoreError::Config(format!("{}: {e}", path.display())))?;
        let mut model_label = String::from("unknown");
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut messages = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Record>(line) {
                Ok(Record::Meta { model, cwd: c, .. }) => {
                    model_label = model;
                    if !c.is_empty() {
                        cwd = PathBuf::from(c);
                    }
                }
                Ok(Record::Msg { msg, .. }) => messages.push(msg),
                Ok(Record::Turn { .. }) | Err(_) => {}
            }
        }
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "resumed".to_string());
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|e| CoreError::Config(format!("{}: {e}", path.display())))?;
        Ok(Self {
            cwd,
            model_label,
            id,
            messages,
            path: Some(path.to_path_buf()),
            file: Some(file),
        })
    }

    pub fn load_latest(prefix: Option<&str>) -> Result<Self> {
        let dir = crate::config::Config::sessions_dir()?;
        if !dir.exists() {
            return Err(CoreError::Config("no saved sessions yet".into()));
        }
        let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
        for entry in std::fs::read_dir(&dir)
            .map_err(|e| CoreError::Config(format!("{}: {e}", dir.display())))?
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            let is_match = path.extension().map(|e| e == "jsonl").unwrap_or(false)
                && prefix
                    .map(|p| {
                        path.file_stem()
                            .map(|s| s.to_string_lossy().starts_with(p))
                            .unwrap_or(false)
                    })
                    .unwrap_or(true);
            if !is_match {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, path));
            }
        }
        let (_, path) =
            best.ok_or_else(|| CoreError::Config("no matching session found".into()))?;
        Self::load(&path)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn messages(&self) -> &[Msg] {
        &self.messages
    }

    pub fn push(&mut self, msg: Msg, usage: Option<Usage>) {
        self.append(&Record::Msg {
            msg: msg.clone(),
            usage,
        });
        self.messages.push(msg);
    }

    pub fn push_turn(&mut self, stats: &TurnStats) {
        self.append(&Record::Turn { stats: *stats });
    }

    fn append(&mut self, record: &Record) {
        let Some(file) = &mut self.file else {
            return;
        };
        if let Ok(mut line) = serde_json::to_string(record) {
            line.push('\n');
            let _ = file.write_all(line.as_bytes());
        }
    }
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..6].to_string()
}
