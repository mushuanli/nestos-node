//! Private, per-process JSONL files with bounded size rotation.
use serde::Deserialize;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub directory: Option<PathBuf>,
    pub max_file_bytes: u64,
    pub retained_files: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            directory: None,
            max_file_bytes: 10 * 1024 * 1024,
            retained_files: 5,
        }
    }
}

pub(super) struct FileLog {
    path: PathBuf,
    file: File,
    bytes: u64,
    max_bytes: u64,
    retained: usize,
}
impl FileLog {
    pub fn open(config: &Config, config_file: &Path) -> io::Result<Self> {
        if config.max_file_bytes < 1024 || !(1..=100).contains(&config.retained_files) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "logging requires max_file_bytes >= 1024 and retained_files in 1..=100",
            ));
        }
        let parent = config_file.canonicalize()?.parent().unwrap().to_path_buf();
        let directory = parent.join(config.directory.as_deref().unwrap_or(Path::new("logs")));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        let name = format!(
            "cells-{}-{}.jsonl",
            chrono::Local::now().format("%Y%m%dT%H%M%S%.9f"),
            std::process::id()
        );
        let path = directory.join(name);
        let file = create(&path)?;
        Ok(Self {
            path,
            file,
            bytes: 0,
            max_bytes: config.max_file_bytes,
            retained: config.retained_files,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        let size = line.len() as u64 + 1;
        if self.bytes > 0 && self.bytes.saturating_add(size) > self.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(format!("{line}\n").as_bytes())?;
        self.bytes = self.bytes.saturating_add(size);
        Ok(())
    }
    fn segment(&self, index: usize) -> PathBuf {
        if index == 0 {
            return self.path.clone();
        }
        let mut name = self.path.as_os_str().to_os_string();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }
    fn rotate(&mut self) -> io::Result<()> {
        if self.retained == 1 {
            fs::remove_file(&self.path)?;
        } else {
            for index in (1..self.retained).rev() {
                match fs::rename(self.segment(index - 1), self.segment(index)) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound && index > 1 => (),
                    result => result?,
                }
            }
        }
        self.file = create(&self.path)?;
        self.bytes = 0;
        Ok(())
    }
}
fn create(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
}

#[cfg(test)]
#[path = "log_file_tests.rs"]
mod tests;
