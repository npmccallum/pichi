// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Retained caller identity. Missing monitor results remain unknown and pinned.

use super::instance::Instance;
use anyhow::{Context, Result, bail};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use conglobate::artifact::Digest;
use madrillo::{CleanupResult, Completion, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Directories holding an unpublished or retiring entry. Only a holder of the
/// image-index lock creates or consumes them, so a leftover is abandoned.
const TRANSIENT: [&str; 2] = [".creating-", ".retired-"];

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    version: u16,
    id: Uuid,
    images: Vec<Digest>,
    endpoint: PathBuf,
}

#[derive(Deserialize)]
struct Version {
    version: u16,
}

#[derive(Debug)]
pub(crate) struct Records {
    pub(super) path: PathBuf,
    pub(super) directory: Dir,
}

/// A transient workload record: `build`, and `run` before durable instances.
#[derive(Debug)]
pub(crate) struct Record {
    identity: Identity,
    pub(crate) run: Run,
}

/// One retained entry of the workload directory.
#[derive(Debug)]
pub(crate) enum Entry {
    Legacy(Record),
    Instance(Instance),
}

/// One boot's monitor state, diagnostics and control endpoint.
#[derive(Debug)]
pub(crate) struct Run {
    path: PathBuf,
    directory: Dir,
    endpoint: PathBuf,
}

impl Records {
    /// Build guests select their own inputs from the read-only image cache.
    /// Their retained output directory also marks that whole-cache lifetime;
    /// only confirmed complete workload cleanup releases it for collection.
    pub(crate) fn build_cache_in_use(graphroot: &Path) -> Result<bool> {
        let Some(records) = Self::existing(graphroot)? else {
            return Ok(false);
        };
        for entry in records.list()? {
            let Entry::Legacy(record) = entry else {
                continue;
            };
            match record.run.directory.open_dir_nofollow("output") {
                Ok(_) => {
                    if !record.complete()? {
                        return Ok(true);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(false)
    }

    pub(crate) fn open(graphroot: &Path) -> Result<Self> {
        let path = Self::location(graphroot)?;
        let parent = Dir::open_ambient_dir(
            path.parent().context("workload state parent")?,
            cap_std::ambient_authority(),
        )?;
        #[cfg(windows)]
        Self::private_directory(&parent)?;
        match parent.create_dir_with(
            path.file_name().context("workload state name")?,
            &Self::private_builder(),
        ) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        Self::from_parent(path, &parent)?.context("missing workload directory")
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn private_builder() -> cap_std::fs::DirBuilder {
        let options = cap_std::fs::DirBuilder::new();
        #[cfg(unix)]
        let options = {
            use cap_std::fs::DirBuilderExt;
            let mut options = options;
            options.mode(0o700);
            options
        };
        options
    }

    pub(crate) fn existing(graphroot: &Path) -> Result<Option<Self>> {
        let path = match Self::location(graphroot) {
            Ok(path) => path,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let parent = match Dir::open_ambient_dir(
            path.parent().context("workload state parent")?,
            cap_std::ambient_authority(),
        ) {
            Ok(parent) => parent,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Self::from_parent(path, &parent)
    }

    // Cache aliases share one identity. Keep state outside the graphroot:
    // build guests mount that image cache read-only and must not see consoles.
    fn location(graphroot: &Path) -> Result<PathBuf> {
        let root = match std::fs::canonicalize(graphroot) {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = graphroot
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                std::fs::canonicalize(parent)?.join(
                    graphroot
                        .file_name()
                        .context("cache needs a directory name")?,
                )
            }
            Err(error) => return Err(error.into()),
        };
        let mut name = root
            .file_name()
            .context("cache needs a directory name")?
            .to_os_string();
        name.push(".workloads");
        Ok(root.with_file_name(name))
    }

    pub(crate) fn pins_at(graphroot: &Path) -> Result<Vec<Digest>> {
        match Self::existing(graphroot)? {
            Some(records) => records.pins(),
            None => Ok(Vec::new()),
        }
    }

    fn from_parent(path: PathBuf, parent: &Dir) -> Result<Option<Self>> {
        let directory =
            match parent.open_dir_nofollow(path.file_name().context("workload state name")?) {
                Ok(directory) => directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
        Self::private_directory(&directory)?;
        Ok(Some(Self { path, directory }))
    }

    #[cfg(unix)]
    fn private_directory(directory: &Dir) -> Result<()> {
        use cap_std::fs::{MetadataExt, PermissionsExt};
        let metadata = directory.dir_metadata()?;
        if metadata.uid() != nix::unistd::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            bail!("workload directory must be private and owned by the invoking user");
        }
        Ok(())
    }

    #[cfg(windows)]
    fn private_directory(directory: &Dir) -> Result<()> {
        use win_custody::{ObjectSecurityDescriptor, OwnedSid};
        let descriptor = ObjectSecurityDescriptor::query_kernel_object(directory)?;
        let user = OwnedSid::from_string(&win_custody::current_user_sid_string()?)?;
        let system = OwnedSid::from_string("S-1-5-18")?;
        let administrators = OwnedSid::from_string("S-1-5-32-544")?;
        let creator = OwnedSid::from_string("S-1-3-0")?;
        let trusted = [&user, &system, &administrators];
        let owner = descriptor.owner()?;
        let acl = descriptor.dacl()?;
        let private = trusted.iter().any(|sid| owner.matches(sid))
            && acl.allowed_aces().all(|ace| {
                ace.mask() == 0
                    || trusted.iter().any(|sid| ace.trustee_matches(sid))
                    || (ace.trustee_matches(&creator) && ace.flags() & 0x08 != 0)
            })
            && acl.allowed_aces().any(|ace| {
                ace.flags() & 0x07 == 0x03
                    && (ace.mask() & 0x1000_0000 != 0 || ace.mask() & 0x001f_01ff == 0x001f_01ff)
                    && (ace.trustee_matches(&user) || ace.trustee_matches(&creator))
            });
        if !private {
            bail!("workload parent must grant private, inheritable user access");
        }
        Ok(())
    }

    /// Caller holds the image-index lock through resolution and this write.
    pub(crate) fn reserve(&self, images: Vec<Digest>) -> Result<Record> {
        let id = Uuid::new_v4();
        self.create(images, Run::endpoint_for(id)?, id)
    }

    /// Caller holds the image-index lock until the identity is durably written.
    pub(super) fn create(
        &self,
        images: Vec<Digest>,
        endpoint: PathBuf,
        id: Uuid,
    ) -> Result<Record> {
        if images.is_empty() || images.len() > 1024 {
            bail!("a workload must pin between 1 and 1024 images");
        }
        let name = id.to_string();
        let identity = Identity {
            version: 1,
            id,
            images,
            endpoint,
        };
        self.directory
            .create_dir_with(&name, &Self::private_builder())?;
        let directory = self.directory.open_dir_nofollow(&name)?;
        let mut file = directory.open_with(
            "identity.json",
            OpenOptions::new().write(true).create_new(true),
        )?;
        serde_json::to_writer(&mut file, &identity)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            directory.open(".")?.sync_all()?;
            self.directory.open(".")?.sync_all()?;
        }
        Ok(Record {
            run: Run {
                path: self.path.join(name),
                directory,
                endpoint: identity.endpoint.clone(),
            },
            identity,
        })
    }

    pub(super) fn get(&self, id: Uuid) -> Result<Entry> {
        let name = id.to_string();
        let directory = self.directory.open_dir_nofollow(&name)?;
        let path = self.path.join(name);
        // A concurrent removal can retire the entry between the two opens:
        // that is an absent instance, like a missing directory.
        let bytes =
            Run::read_bytes(&directory, "identity.json", 1024 * 1024)?.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "missing workload identity")
            })?;
        if serde_json::from_slice::<Version>(&bytes)?.version != 1 {
            return Instance::read(&bytes, id, path, directory).map(Entry::Instance);
        }
        let identity: Identity = serde_json::from_slice(&bytes)?;
        if identity.id != id || identity.images.is_empty() || identity.images.len() > 1024 {
            bail!("invalid workload identity");
        }
        Ok(Entry::Legacy(Record {
            run: Run {
                path,
                directory,
                endpoint: identity.endpoint.clone(),
            },
            identity,
        }))
    }

    /// Read an entry, or `None` if a concurrent removal retired it; its
    /// files can disappear while it is read.
    pub(super) fn get_published(&self, id: Uuid) -> Result<Option<Entry>> {
        match self.get(id) {
            Ok(entry) => Ok(Some(entry)),
            Err(_) if !self.contains(id)? => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether `id` is still published under its own name.
    fn contains(&self, id: Uuid) -> Result<bool> {
        match self.directory.symlink_metadata(id.to_string()) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn list(&self) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for entry in self.directory.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().context("invalid workload directory name")?;
            if TRANSIENT.iter().any(|prefix| name.starts_with(prefix)) {
                continue;
            }
            let id = name.parse::<Uuid>()?;
            if !entry.file_type()?.is_dir() {
                bail!("workload identity must be a directory");
            }
            if let Some(entry) = self.get_published(id)? {
                entries.push(entry);
            }
        }
        entries.sort_by_key(Entry::id);
        Ok(entries)
    }

    pub(super) fn pins(&self) -> Result<Vec<Digest>> {
        let mut pins = Vec::new();
        for entry in self.list()? {
            match entry {
                // An instance needs its image until it is removed.
                Entry::Instance(instance) => pins.push(instance.root().clone()),
                Entry::Legacy(record) => {
                    if !record.complete()? {
                        pins.extend(record.identity.images);
                    }
                }
            }
        }
        Ok(pins)
    }

    /// Remove an entry whose payload is gone. Caller holds the image-index
    /// lock: the identity, its name and its pins disappear in one rename.
    pub(super) fn retire(&self, id: Uuid) -> Result<()> {
        let retired = format!("{}{id}", TRANSIENT[1]);
        match Self::patiently(|| {
            self.directory
                .rename(id.to_string(), &self.directory, &retired)
        }) {
            Ok(()) => {}
            // A concurrent removal already retired it.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        #[cfg(unix)]
        self.directory.open(".")?.sync_all()?;
        // Retired: the shell's deletion can no longer change any outcome.
        if let Err(error) = self.reap(&retired) {
            log::warn!("{id} was removed, but {retired} remains: {error:#}");
        }
        Ok(())
    }

    /// Retry a deletion or rename briefly. On Windows, an observer's handle
    /// to the target refuses it for as long as the handle is open.
    pub(super) fn patiently<T>(
        mut operation: impl FnMut() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        #[cfg(windows)]
        {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            loop {
                match operation() {
                    Err(error)
                        if matches!(error.raw_os_error(), Some(5 | 32))
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    result => return result,
                }
            }
        }
        #[cfg(not(windows))]
        operation()
    }

    /// Delete abandoned transient directories while holding the image-index
    /// lock, so no live creation or retirement owns them. Failures warn.
    pub(super) fn reap_abandoned(&self) {
        let names = match self.directory.entries() {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                .filter(|name| TRANSIENT.iter().any(|prefix| name.starts_with(prefix)))
                .collect::<Vec<_>>(),
            Err(error) => {
                log::warn!("listing abandoned workload directories: {error}");
                return;
            }
        };
        for name in names {
            if let Err(error) = self.reap(&name) {
                log::warn!("removing abandoned {name}: {error:#}");
            }
        }
    }

    /// Only what Pichi, its monitor and Dillo write is removed; anything else
    /// fails closed. Dillo probes hole punching with a `.tmp*` sibling of the
    /// disk, which a killed VMM leaves behind.
    pub(super) fn reap(&self, name: &str) -> Result<()> {
        let directory = self.directory.open_dir_nofollow(name)?;
        // Check everything first: a refused leftover stays whole.
        let mut owned = Vec::new();
        for entry in directory.entries()? {
            let entry = entry?;
            let file = entry.file_name();
            let kind = entry.file_type()?;
            let known = match file.to_str() {
                Some("identity.json" | "identity.json.tmp" | "instance.lock" | "storage.qcow2") => {
                    kind.is_file()
                }
                Some(probe) if probe.starts_with(".tmp") => kind.is_file(),
                _ => false,
            };
            if !known {
                bail!("unexpected {} in {name}", file.to_string_lossy());
            }
            owned.push(file);
        }
        for file in owned {
            Self::patiently(|| directory.remove_file(&file))?;
        }
        // Windows cannot remove a directory while a handle to it is open, and
        // a file another process still has open is only pending deletion.
        drop(directory);
        Self::patiently(|| self.directory.remove_dir(name))?;
        Ok(())
    }

    /// Open the workload directory containing `path` for a callback.
    pub(super) fn at(path: &Path) -> Result<Self> {
        let parent = Dir::open_ambient_dir(
            path.parent().context("workload root parent")?,
            cap_std::ambient_authority(),
        )?;
        Self::from_parent(path.to_owned(), &parent)?.context("missing workload directory")
    }
}

impl Entry {
    pub(super) fn id(&self) -> Uuid {
        match self {
            Self::Legacy(record) => record.id(),
            Self::Instance(instance) => instance.id(),
        }
    }
}

impl Record {
    pub(super) fn at(path: &Path) -> Result<Self> {
        let id = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("workload directory must name an ID")?
            .parse()?;
        match Records::at(path.parent().context("workload directory parent")?)?.get(id)? {
            Entry::Legacy(record) => Ok(record),
            Entry::Instance(_) => bail!("expected a transient workload record"),
        }
    }

    pub(crate) fn storage_directory(&self) -> PathBuf {
        self.run.path.join("storage")
    }

    fn complete(&self) -> Result<bool> {
        Ok(
            matches!(self.run.completion()?, Some(result) if result.cleanup == CleanupResult::Complete),
        )
    }

    pub(crate) fn release_runtime_files(&self) -> Result<()> {
        if self.complete()? {
            self.run.release_endpoint()?;
            match self.run.directory.remove_dir_all("storage") {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(crate) fn id(&self) -> Uuid {
        self.identity.id
    }
    pub(super) fn images(&self) -> &[Digest] {
        &self.identity.images
    }

    pub(crate) fn create_build_output(&self) -> Result<PathBuf> {
        self.run.directory.create_dir("output")?;
        Ok(self.run.path.join("output"))
    }

    pub(crate) fn release_build_output(&self) -> Result<()> {
        if self.complete()? {
            match self.run.directory.remove_dir_all("output") {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

impl Run {
    /// Reopen only the boot previously selected by an observer.
    pub(super) fn reopen(path: &Path, endpoint: PathBuf) -> Result<Self> {
        let parent = path.parent().context("boot parent")?;
        if parent.file_name() != Some("runs".as_ref()) {
            return Ok(Record::at(path)?.run);
        }
        let instance = parent.parent().context("instance directory")?;
        let id = instance
            .file_name()
            .and_then(|name| name.to_str())
            .context("instance ID")?
            .parse()?;
        let attempt = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("boot ID")?
            .parse()?;
        let records = Records::at(instance.parent().context("workload directory")?)?;
        match records.get(id)? {
            Entry::Instance(instance) => instance.run_of(attempt, endpoint),
            Entry::Legacy(_) => bail!("expected an instance record"),
        }
    }

    pub(super) fn new(path: PathBuf, directory: Dir, endpoint: PathBuf) -> Self {
        Self {
            path,
            directory,
            endpoint,
        }
    }

    /// A fresh control address. Unix respects the caller's native temporary
    /// directory, including sandbox restrictions; a compact UUID leaves room
    /// for macOS socket paths and the resize suffix. Both peers authenticate
    /// the invoking user.
    pub(super) fn endpoint_for(id: Uuid) -> Result<PathBuf> {
        #[cfg(unix)]
        let endpoint = std::path::absolute(std::env::temp_dir())?.join(id.simple().to_string());
        #[cfg(windows)]
        let endpoint = PathBuf::from(format!(r"\\.\pipe\pichi-{id}"));
        Ok(endpoint)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn endpoint(&self) -> &Path {
        &self.endpoint
    }
    pub(super) fn resize_endpoint(&self) -> PathBuf {
        let mut endpoint = self.endpoint.as_os_str().to_os_string();
        endpoint.push(".resize");
        endpoint.into()
    }

    pub(super) fn state(&self) -> PathBuf {
        self.path.join("monitor")
    }

    /// A forced Dillo exit cannot run its socket-name destructor. Callers
    /// remove only its owned socket, after confirmed scope cleanup.
    pub(super) fn release_endpoint(&self) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            let endpoint = self.resize_endpoint();
            match std::fs::symlink_metadata(&endpoint) {
                Ok(metadata)
                    if metadata.file_type().is_socket()
                        && metadata.uid() == nix::unistd::geteuid().as_raw() =>
                {
                    match std::fs::remove_file(endpoint) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                // Never remove what this boot did not create.
                Ok(_) => bail!("unexpected file at console resize endpoint"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(super) fn console_log(&self) -> Result<std::fs::File> {
        let monitor = self.directory.open_dir_nofollow("monitor")?;
        Ok(Self::open_file(&monitor, "console.log")?
            .context("workload has no retained console log")?
            .into_std())
    }

    pub(super) fn monitor_diagnostic(&self) -> Result<String> {
        let Some(file) = Self::open_file(&self.directory, "monitor.err")? else {
            return Ok(String::new());
        };
        let mut bytes = Vec::new();
        file.take(8193).read_to_end(&mut bytes)?;
        let truncated = bytes.len() > 8192;
        bytes.truncate(8192);
        let mut text = String::from_utf8_lossy(&bytes).trim().to_owned();
        if truncated {
            text.push_str(" [truncated]");
        }
        Ok(text)
    }

    /// Remove the boot's stale resize socket once its result confirms
    /// complete cleanup. Housekeeping only; failures warn.
    pub(super) fn tidy(&self) {
        match self.completion() {
            Ok(Some(result)) if result.cleanup == CleanupResult::Complete => {
                if let Err(error) = self.release_endpoint() {
                    log::warn!("removing stale resize socket: {error:#}");
                }
            }
            Ok(_) => {}
            Err(error) => log::warn!("reading {} result: {error:#}", self.path.display()),
        }
    }

    pub(crate) fn completion(&self) -> Result<Option<Completion>> {
        let monitor = match self.directory.open_dir_nofollow("monitor") {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(bytes) = Self::read_bytes(&monitor, "result.json", 1024 * 1024)? else {
            return Ok(None);
        };
        let result: Completion = serde_json::from_slice(&bytes)?;
        if result.version != PROTOCOL_VERSION {
            bail!("unsupported retained monitor result version");
        }
        Ok(Some(result))
    }

    pub(super) fn read_bytes(directory: &Dir, name: &str, limit: u64) -> Result<Option<Vec<u8>>> {
        let Some(file) = Self::open_file(directory, name)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len())? > limit {
            bail!("workload record exceeds size limit");
        }
        Ok(Some(bytes))
    }

    fn open_file(directory: &Dir, name: &str) -> Result<Option<cap_std::fs::File>> {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(nix::libc::O_NONBLOCK);
        }
        let file = match directory.open_with(name, &options) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            bail!("workload record must be a regular file");
        }
        Ok(Some(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cli::{PruneArgs, RmiArgs},
        config::{Config, StorageConfig},
    };
    use conglobate::artifact::{
        ConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, PmiDescriptor,
    };
    use madrillo::{CompletionCause, LeaderResult, OomEvidence};
    use pichi_storage::{BlobStore, FilesystemBlobStore, FilesystemTagDb, TagDb};
    use sha2::{Digest as _, Sha256};

    #[test]
    fn storage_cleanup_requires_complete_evidence_and_is_idempotent() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache)?;
        let record = Records::open(&cache)?.reserve(vec![Digest::Sha256([1; 32])])?;
        #[cfg(unix)]
        let _resize = {
            use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};
            ListenerOptions::new()
                .name(
                    record
                        .run
                        .resize_endpoint()
                        .to_fs_name::<GenericFilePath>()?,
                )
                .create_sync()?
        };
        std::fs::create_dir(record.storage_directory())?;
        let backing = record.storage_directory().join("storage.raw");
        std::fs::write(&backing, b"guest data")?;
        Record::at(record.run.path())?.release_runtime_files()?;
        assert!(backing.exists(), "missing result must retain storage");
        std::fs::create_dir(record.run.state())?;
        for cleanup in [
            CleanupResult::Unknown,
            CleanupResult::Incomplete {
                errors: vec!["helper still running".into()],
            },
            CleanupResult::Complete,
        ] {
            let result = Completion {
                version: PROTOCOL_VERSION,
                leader: LeaderResult::Exit { code: 0 },
                cause: CompletionCause::LeaderExit,
                exited_at_unix_ms: 0,
                cleanup,
                oom: OomEvidence::NoneObserved,
                log_errors: Vec::new(),
                monitor_errors: Vec::new(),
                monitor_warnings: Vec::new(),
                output_truncated: false,
            };
            std::fs::write(
                record.run.state().join("result.json"),
                serde_json::to_vec(&result)?,
            )?;
            for _ in 0..2 {
                Record::at(record.run.path())?.release_runtime_files()?;
                assert_eq!(backing.exists(), result.cleanup != CleanupResult::Complete);
                #[cfg(unix)]
                assert_eq!(
                    record.run.resize_endpoint().exists(),
                    result.cleanup != CleanupResult::Complete
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn image_gc_preserves_pins_and_guest_selected_build_inputs() -> Result<()> {
        for build in [false, true] {
            let root = tempfile::tempdir()?;
            let graphroot = root.path().join("cache");
            let config = Config {
                storage: StorageConfig {
                    graphroot: Some(graphroot.clone()),
                    ..Default::default()
                },
                ..Default::default()
            };
            let blobs = FilesystemBlobStore::new(&graphroot);
            let tags = FilesystemTagDb::open(&graphroot)?;
            let records = Records::open(&graphroot)?;
            let mut images = Vec::new();
            let mut layers = Vec::new();
            for payload in [b"appliance".as_slice(), b"build source".as_slice()] {
                let layer = Digest::Sha256(Sha256::digest(payload).into());
                blobs.put_blob(&layer, payload).await?;
                let manifest = Manifest {
                    schema_version: 2,
                    media_type: "application/vnd.oci.image.manifest.v1+json".into(),
                    artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
                    config: ConfigDescriptor::canonical(),
                    layers: vec![Layer::Pmi(PmiDescriptor {
                        digest: layer.to_string(),
                        size: payload.len() as u64,
                    })],
                    annotations: std::collections::BTreeMap::default(),
                };
                let bytes = serde_json::to_vec(&manifest)?;
                let image = Digest::Sha256(Sha256::digest(&bytes).into());
                blobs.put_blob(&image, &bytes).await?;
                images.push(image);
                layers.push(layer);
            }
            tags.set_tag("docker.io/library/appliance:test", &images[0])
                .await?;
            let pins = if build {
                images[..1].to_vec()
            } else {
                images.clone()
            };
            let record = records.create(pins, root.path().join("control"), Uuid::new_v4())?;
            if build {
                record.create_build_output()?;
            }
            assert!(
                !record
                    .run
                    .state()
                    .starts_with(std::fs::canonicalize(&graphroot)?),
                "build's cache mount must not include console state"
            );
            crate::cmd::rmi::run(
                RmiArgs {
                    references: vec!["appliance:test".into()],
                    force: false,
                },
                &config,
            )
            .await?;
            assert_eq!(tags.list_tags().await?, Vec::new());
            for digest in images.iter().chain(&layers) {
                assert!(
                    blobs.blob_exists(digest).await,
                    "rmi removed a workload image: {digest}"
                );
            }
            let mut result = Completion {
                version: PROTOCOL_VERSION,
                leader: LeaderResult::Exit { code: 0 },
                cause: CompletionCause::LeaderExit,
                exited_at_unix_ms: 0,
                cleanup: CleanupResult::Unknown,
                oom: OomEvidence::NoneObserved,
                log_errors: Vec::new(),
                monitor_errors: Vec::new(),
                monitor_warnings: Vec::new(),
                output_truncated: false,
            };
            std::fs::create_dir(record.run.state())?;
            for cleanup in [
                CleanupResult::Unknown,
                CleanupResult::Incomplete {
                    errors: vec!["helper still running".into()],
                },
            ] {
                result.cleanup = cleanup;
                std::fs::write(
                    record.run.state().join("result.json"),
                    serde_json::to_vec(&result)?,
                )?;
                crate::cmd::prune::run(PruneArgs { dry_run: false }, &config).await?;
                for digest in images.iter().chain(&layers) {
                    assert!(
                        blobs.blob_exists(digest).await,
                        "prune removed a workload image: {digest}"
                    );
                }
            }
            result.cleanup = CleanupResult::Complete;
            std::fs::write(
                record.run.state().join("result.json"),
                serde_json::to_vec(&result)?,
            )?;
            crate::cmd::prune::run(PruneArgs { dry_run: false }, &config).await?;
            for digest in images.iter().chain(&layers) {
                assert!(
                    !blobs.blob_exists(digest).await,
                    "completed workload retained {digest}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn missing_records_are_read_only_and_partial_publication_fails_closed() -> Result<()> {
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        assert_eq!(Records::pins_at(&graphroot)?, Vec::new());
        assert!(!Records::location(&graphroot)?.exists());
        std::fs::create_dir(&graphroot)?;
        let records = Records::open(&graphroot)?;
        let partial = Uuid::new_v4().to_string();
        records.directory.create_dir(&partial)?;
        assert!(
            records.pins().is_err(),
            "a crash during identity publication must stop GC"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replaced_workload_directory_and_completion_links_fail_closed() -> Result<()> {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let state = Records::location(&graphroot)?;
        symlink(outside.path(), &state)?;
        assert!(Records::pins_at(&graphroot).is_err());
        std::fs::remove_file(&state)?;
        let records = Records::open(&graphroot)?;
        let image = Digest::Sha256([0; 32]);
        let record = records.create(vec![image], root.path().join("control"), Uuid::new_v4())?;
        symlink(outside.path(), record.run.state())?;
        assert!(record.run.completion().is_err());
        Ok(())
    }
}
