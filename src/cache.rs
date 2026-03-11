use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use heed::{
    Database, Env, EnvOpenOptions,
    types::{SerdeJson, Str},
};
use serde::{Deserialize, Serialize};

const METADATA_DB: &str = "metadata";
const VALUES_DB: &str = "values";

#[derive(Clone)]
pub struct HeedCache {
    env: Env,
    metadata: Database<Str, SerdeJson<CacheEntry>>,
    values: Database<Str, SerdeJson<CacheEntry>>,
}

impl HeedCache {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        fs::create_dir_all(path.as_ref())?;

        let env = unsafe { EnvOpenOptions::new().max_dbs(8).open(path.as_ref())? };

        let mut wtxn = env.write_txn()?;
        let metadata = env.create_database(&mut wtxn, Some(METADATA_DB))?;
        let values = env.create_database(&mut wtxn, Some(VALUES_DB))?;
        wtxn.commit()?;

        Ok(Self {
            env,
            metadata,
            values,
        })
    }

    pub fn get_metadata(
        &self,
        key: &str,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        self.get(&self.metadata, key)
    }

    pub fn put_metadata(
        &self,
        key: &str,
        body: &str,
        ttl_seconds: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.put(&self.metadata, key, body, ttl_seconds)
    }

    pub fn get_values(
        &self,
        key: &str,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        self.get(&self.values, key)
    }

    pub fn put_values(
        &self,
        key: &str,
        body: &str,
        ttl_seconds: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.put(&self.values, key, body, ttl_seconds)
    }

    fn get(
        &self,
        db: &Database<Str, SerdeJson<CacheEntry>>,
        key: &str,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        let entry = db.get(&rtxn, key)?;

        Ok(entry
            .filter(|entry| entry.expires_at_unix_seconds > now_unix_seconds())
            .map(|entry| entry.body))
    }

    fn put(
        &self,
        db: &Database<Str, SerdeJson<CacheEntry>>,
        key: &str,
        body: &str,
        ttl_seconds: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let entry = CacheEntry {
            body: body.to_string(),
            expires_at_unix_seconds: now_unix_seconds() + ttl_seconds,
        };

        let mut wtxn = self.env.write_txn()?;
        db.put(&mut wtxn, key, &entry)?;
        wtxn.commit()?;
        Ok(())
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct CacheEntry {
    body: String,
    expires_at_unix_seconds: u64,
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time drifted before UNIX_EPOCH")
        .as_secs()
}
