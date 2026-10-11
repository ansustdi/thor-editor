//! Connection profiles (JSON in the app config dir) and passwords (OS keyring).

use crate::model::ConnectionProfile;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Mutex;

// Pre-rename (DbForge Lite) id, kept so saved passwords stay readable.
const KEYRING_SERVICE: &str = "dbforge-lite";

pub struct ProfileStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl ProfileStore {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(Self { path: dir.join("connections.json"), lock: Mutex::new(()) })
    }

    pub fn load(&self) -> Result<Vec<ConnectionProfile>> {
        let _g = self.lock.lock().unwrap();
        self.read()
    }

    fn read(&self) -> Result<Vec<ConnectionProfile>> {
        if !self.path.exists() {
            return Ok(vec![]);
        }
        let text = std::fs::read_to_string(&self.path)?;
        Ok(serde_json::from_str(&text).context("connections.json is corrupt")?)
    }

    fn write(&self, list: &[ConnectionProfile]) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(list)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn upsert(&self, p: ConnectionProfile) -> Result<()> {
        let _g = self.lock.lock().unwrap();
        let mut list = self.read()?;
        match list.iter_mut().find(|x| x.id == p.id) {
            Some(slot) => *slot = p,
            None => list.push(p),
        }
        self.write(&list)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let _g = self.lock.lock().unwrap();
        let mut list = self.read()?;
        list.retain(|x| x.id != id);
        self.write(&list)?;
        let _ = delete_password(id);
        let _ = delete_password(&format!("{id}#ssh"));
        Ok(())
    }

    pub fn touch(&self, id: &str) -> Result<()> {
        let _g = self.lock.lock().unwrap();
        let mut list = self.read()?;
        if let Some(p) = list.iter_mut().find(|x| x.id == id) {
            p.last_used = Some(chrono::Local::now().to_rfc3339());
        }
        self.write(&list)
    }
}

pub fn set_password(id: &str, password: &str) -> Result<()> {
    keyring::Entry::new(KEYRING_SERVICE, id)?.set_password(password)?;
    Ok(())
}

pub fn get_password(id: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, id).ok()?.get_password().ok()
}

pub fn delete_password(id: &str) -> Result<()> {
    keyring::Entry::new(KEYRING_SERVICE, id)?.delete_credential()?;
    Ok(())
}
