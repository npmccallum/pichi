// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Durable instances: frozen configuration, one private qcow2 disk and a
//! lifetime lock held by every process of the current boot.

use super::record::{Entry, Records, Run};
use crate::cmd::writable_storage::InstanceDisk;
use anyhow::{Context, Result, bail, ensure};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use conglobate::artifact::Digest;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

const VERSION: u16 = 2;
const IDENTITY: &str = "identity.json";
const LOCK: &str = "instance.lock";
const DISK: &str = "storage.qcow2";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    version: u16,
    id: Uuid,
    root: Digest,
    manifest: Digest,
    cpus: u32,
    memory_mib: u32,
    storage_bytes: u64,
    tty: bool,
    interactive: bool,
    attempt: Option<Attempt>,
    removing: bool,
}

/// The selected boot.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Attempt {
    id: Uuid,
    endpoint: PathBuf,
}

/// Configuration fixed at creation.
#[derive(Debug)]
pub(crate) struct Spec {
    pub(crate) root: Digest,
    pub(crate) manifest: Digest,
    pub(crate) cpus: u32,
    pub(crate) memory_mib: u32,
    pub(crate) storage_bytes: u64,
    pub(crate) tty: bool,
    pub(crate) interactive: bool,
}

#[derive(Debug)]
pub(crate) struct Instance {
    identity: Identity,
    path: PathBuf,
    directory: Dir,
}

/// Derived, never stored.
#[derive(Debug)]
pub(crate) enum State {
    Created,
    Running,
    Exited,

    /// No boot process remains, but its result is missing or unreadable.
    Unknown,
    Removing,
}

/// Exclusive ownership of an instance. Start transfers it to the boot.
#[derive(Debug)]
pub(crate) struct Lease(File);

impl Instance {
    /// Publish a new instance. Caller holds the image-index lock, which
    /// serializes image pins and transient directories.
    pub(crate) async fn create(records: &Records, spec: Spec) -> Result<Self> {
        records.reap_abandoned();
        let id = Uuid::new_v4();
        let staging = format!(".creating-{id}");
        records
            .directory
            .create_dir_with(&staging, &Records::private_builder())?;
        let identity = Identity {
            version: VERSION,
            id,
            root: spec.root,
            manifest: spec.manifest,
            cpus: spec.cpus,
            memory_mib: spec.memory_mib,
            storage_bytes: spec.storage_bytes,
            tty: spec.tty,
            interactive: spec.interactive,
            attempt: None,
            removing: false,
        };
        let built: Result<()> = async {
            let directory = records.directory.open_dir_nofollow(&staging)?;
            let new = || {
                let mut options = OpenOptions::new();
                options.read(true).write(true).create_new(true);
                options
            };
            let disk = directory.open_with(DISK, &new())?.into_std();
            let bytes = identity.storage_bytes;
            tokio::task::spawn_blocking(move || InstanceDisk::create(disk, bytes))
                .await
                .context("creating instance disk")??;
            directory.open_with(LOCK, &new())?.sync_all()?;
            let mut file = directory.open_with(IDENTITY, &new())?;
            serde_json::to_writer(&mut file, &identity)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            #[cfg(unix)]
            directory.open(".")?.sync_all()?;
            // Windows cannot rename a directory while handles inside it are open.
            drop((file, directory));
            records
                .directory
                .rename(&staging, &records.directory, id.to_string())?;
            Ok(())
        }
        .await;
        if let Err(error) = built {
            if let Err(cleanup) = records.reap(&staging) {
                log::warn!("removing unpublished {staging}: {cleanup:#}");
            }
            return Err(error);
        }
        // Published: a later failure must not hide the instance's ID.
        let directory = async {
            #[cfg(unix)]
            records.directory.open(".")?.sync_all()?;
            Ok::<_, anyhow::Error>(records.directory.open_dir_nofollow(id.to_string())?)
        }
        .await
        .with_context(|| format!("instance {id} was published but cannot be reopened"))?;
        Ok(Self {
            identity,
            path: records.path.join(id.to_string()),
            directory,
        })
    }

    pub(super) fn read(bytes: &[u8], id: Uuid, path: PathBuf, directory: Dir) -> Result<Self> {
        let identity: Identity =
            serde_json::from_slice(bytes).context("invalid instance record")?;
        ensure!(
            identity.version == VERSION,
            "unsupported workload record version {}; upgrade pichi",
            identity.version
        );
        ensure!(
            identity.id == id
                && identity.cpus != 0
                && identity.memory_mib != 0
                && identity.storage_bytes != 0,
            "invalid instance record"
        );
        Ok(Self {
            identity,
            path,
            directory,
        })
    }

    /// Re-read the record, for example after acquiring the lease.
    pub(crate) fn reread(self) -> Result<Self> {
        let bytes = Run::read_bytes(&self.directory, IDENTITY, 1024 * 1024)?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing instance record")
        })?;
        Self::read(&bytes, self.identity.id, self.path, self.directory)
    }

    fn save(&self) -> Result<()> {
        let temporary = format!("{IDENTITY}.tmp");
        let mut file = self.directory.open_with(
            &temporary,
            OpenOptions::new().write(true).create(true).truncate(true),
        )?;
        serde_json::to_writer(&mut file, &self.identity)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        self.directory
            .rename(&temporary, &self.directory, IDENTITY)?;
        #[cfg(unix)]
        self.directory.open(".")?.sync_all()?;
        Ok(())
    }

    pub(crate) fn id(&self) -> Uuid {
        self.identity.id
    }
    pub(crate) fn root(&self) -> &Digest {
        &self.identity.root
    }
    pub(crate) fn manifest(&self) -> &Digest {
        &self.identity.manifest
    }
    pub(crate) fn cpus(&self) -> u32 {
        self.identity.cpus
    }
    pub(crate) fn memory_mib(&self) -> u32 {
        self.identity.memory_mib
    }
    pub(crate) fn tty(&self) -> bool {
        self.identity.tty
    }
    pub(crate) fn interactive(&self) -> bool {
        self.identity.interactive
    }
    pub(crate) fn removing(&self) -> bool {
        self.identity.removing
    }
    pub(crate) fn attempt(&self) -> Option<Uuid> {
        self.identity.attempt.as_ref().map(|attempt| attempt.id)
    }
    pub(crate) fn disk(&self) -> PathBuf {
        self.path.join(DISK)
    }

    /// The selected boot, if the instance was ever started.
    pub(crate) fn run(&self) -> Result<Option<Run>> {
        let Some(attempt) = &self.identity.attempt else {
            return Ok(None);
        };
        self.run_of(attempt.id, attempt.endpoint.clone()).map(Some)
    }

    /// Reopen the boot an observer selected, even after another boot starts.
    pub(super) fn run_of(&self, id: Uuid, endpoint: PathBuf) -> Result<Run> {
        let name = id.to_string();
        let directory = self
            .directory
            .open_dir_nofollow("runs")?
            .open_dir_nofollow(&name)?;
        Ok(Run::new(
            self.path.join("runs").join(name),
            directory,
            endpoint,
        ))
    }

    pub(crate) fn state(&self) -> Result<State> {
        if self.identity.removing {
            return Ok(State::Removing);
        }
        let Some(run) = self.run()? else {
            return Ok(State::Created);
        };
        if self.live()? {
            return Ok(State::Running);
        }
        // The lock is free, so no process of this boot remains to change it.
        Ok(match run.completion() {
            Ok(Some(_)) => State::Exited,
            _ => State::Unknown,
        })
    }

    /// Take exclusive ownership without waiting on a live boot. Observers
    /// probe the lock only briefly, so a short retry absorbs them.
    pub(crate) fn lease(&self) -> Result<Lease> {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match self.try_lease()? {
                Some(lease) => return Ok(lease),
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                None => bail!("instance is running or busy"),
            }
        }
    }

    #[cfg(unix)]
    fn try_lease(&self) -> Result<Option<Lease>> {
        let file = self.lock_file(true)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Lease(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    #[cfg(windows)]
    fn try_lease(&self) -> Result<Option<Lease>> {
        use cap_std::fs::OpenOptionsExt as _;
        // Sharing restrictions follow the open file object, including handles
        // transferred to the monitor and VMM; byte-range locks cannot.
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .share_mode(0)
            .follow(FollowSymlinks::No);
        match self.directory.open_with(LOCK, &options) {
            Ok(file) => Ok(Some(Lease(file.into_std()))),
            // ERROR_SHARING_VIOLATION
            Err(error) if error.raw_os_error() == Some(32) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Whether any process holds the lifetime lock. Never waits.
    #[cfg(unix)]
    pub(crate) fn live(&self) -> Result<bool> {
        match self.lock_file(false)?.try_lock_shared() {
            Ok(()) => Ok(false),
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    /// Probes share with each other but not with a lease, which shares nothing.
    #[cfg(windows)]
    pub(crate) fn live(&self) -> Result<bool> {
        use cap_std::fs::OpenOptionsExt as _;
        let mut options = OpenOptions::new();
        // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
        options.read(true).share_mode(7).follow(FollowSymlinks::No);
        match self.directory.open_with(LOCK, &options) {
            Ok(_) => Ok(false),
            // ERROR_SHARING_VIOLATION
            Err(error) if error.raw_os_error() == Some(32) => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(unix)]
    fn lock_file(&self, write: bool) -> Result<File> {
        let mut options = OpenOptions::new();
        options.read(true).write(write).follow(FollowSymlinks::No);
        let file = self.directory.open_with(LOCK, &options)?;
        ensure!(file.metadata()?.is_file(), "instance lock must be a file");
        Ok(file.into_std())
    }

    /// Reopen the disk without repair before a boot may write to it.
    pub(crate) async fn check_disk(&self, _lease: &Lease) -> Result<()> {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = self.directory.open_with(DISK, &options)?.into_std();
        ensure!(file.metadata()?.is_file(), "instance disk must be a file");
        let bytes = self.identity.storage_bytes;
        tokio::task::spawn_blocking(move || InstanceDisk::check(file, bytes))
            .await?
            .with_context(|| {
                format!(
                    "instance {} disk {} failed its consistency check; it was not repaired",
                    self.identity.id,
                    self.disk().display()
                )
            })
    }

    /// Create a fresh boot's run directory. It is not the selected boot
    /// until `select_attempt`.
    pub(crate) fn prepare_attempt(&self, _lease: &Lease) -> Result<(Run, Attempt)> {
        let id = Uuid::new_v4();
        let endpoint = Run::endpoint_for(id)?;
        match self
            .directory
            .create_dir_with("runs", &Records::private_builder())
        {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let runs = self.directory.open_dir_nofollow("runs")?;
        runs.create_dir_with(id.to_string(), &Records::private_builder())?;
        #[cfg(unix)]
        runs.open(".")?.sync_all()?;
        let name = id.to_string();
        let run = Run::new(
            self.path.join("runs").join(&name),
            runs.open_dir_nofollow(&name)?,
            endpoint.clone(),
        );
        Ok((run, Attempt { id, endpoint }))
    }

    /// Select the boot, once its monitor listens.
    pub(crate) fn select_attempt(&mut self, _lease: &Lease, attempt: Attempt) -> Result<()> {
        // The lease is free only after the previous result was published.
        // Remove any resize socket left by that boot.
        if let Some(previous) = self.run()? {
            previous.tidy();
        }
        self.identity.attempt = Some(attempt);
        self.save()
    }

    /// Delete the disk and boot history, then retire the record. Removal
    /// intent is durable first, so an interrupted removal can be resumed but
    /// never started again.
    pub(crate) async fn remove(
        mut self,
        lease: Lease,
        records: &Records,
        layout: &pichi_storage::CacheLayout,
    ) -> Result<()> {
        if !self.identity.removing {
            // Let the last boot's monitor finish recording into its run
            // directory before that directory is deleted; then nothing else
            // will remove its stale resize socket.
            if let Some(run) = self.run()? {
                run.tidy();
            }
            self.identity.removing = true;
            self.save()?;
        }
        match Records::patiently(|| self.directory.remove_file(DISK)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // Treat only a missing run-history directory as already removed;
        // an incomplete recursive deletion must retain removal intent.
        match Records::patiently(|| match self.directory.remove_dir_all("runs") {
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && self.directory.symlink_metadata("runs").is_ok() =>
            {
                Err(std::io::Error::other(
                    "run history changed while it was deleted",
                ))
            }
            result => result,
        }) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // Windows cannot rename a directory while handles to it or inside
        // it are open. Removal intent already prevents another start.
        let id = self.identity.id;
        drop((lease, self));
        layout
            .with_index_lock(|| async { records.retire(id) })
            .await
    }

    /// Exit callback for one boot: remove its stale resize socket after
    /// confirmed cleanup. Never touches the disk, record or lock.
    pub(super) fn release(path: &Path) -> Result<()> {
        let attempt: Uuid = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("run directory must name an attempt")?
            .parse()?;
        let instance = path
            .parent()
            .and_then(Path::parent)
            .context("run directory must be inside an instance")?;
        let id = instance
            .file_name()
            .and_then(|name| name.to_str())
            .context("instance directory must name an ID")?
            .parse()?;
        let records = Records::at(instance.parent().context("instance parent")?)?;
        let Entry::Instance(instance) = records.get(id)? else {
            bail!("expected an instance record");
        };
        if instance.attempt() != Some(attempt) {
            return Ok(());
        }
        let run = instance.run()?.context("missing selected run")?;
        if run
            .completion()?
            .is_some_and(|result| result.cleanup == madrillo::CleanupResult::Complete)
        {
            run.release_endpoint()?;
        }
        Ok(())
    }
}

impl Lease {
    /// One handle for the monitor to hold until cleanup and one for the VMM,
    /// so the lock outlives every process of the boot.
    pub(crate) fn clone_boot(&self) -> Result<(File, File)> {
        Ok((self.0.try_clone()?, self.0.try_clone()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Spec {
        fn fixture() -> Self {
            Self {
                root: Digest::Sha256([1; 32]),
                manifest: Digest::Sha256([2; 32]),
                cpus: 1,
                memory_mib: 128,
                storage_bytes: 2 * 1024 * 1024,
                tty: false,
                interactive: false,
            }
        }
    }

    #[tokio::test]
    async fn disk_and_pins_outlive_every_boot_handle() -> Result<()> {
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = Records::open(&graphroot)?;
        let mut instance = Instance::create(&records, Spec::fixture()).await?;
        assert!(matches!(instance.state()?, State::Created));
        assert_eq!(records.pins()?, vec![Digest::Sha256([1; 32])]);
        assert!(instance.disk().is_file());
        assert!(matches!(records.get(instance.id())?, Entry::Instance(_)));
        let lease = instance.lease()?;
        instance.check_disk(&lease).await?;
        let (run, attempt) = instance.prepare_attempt(&lease)?;
        assert!(
            instance.attempt().is_none(),
            "preparation must not select a boot"
        );
        instance.select_attempt(&lease, attempt)?;
        let selected = instance.attempt();
        let (monitor, vm) = lease.clone_boot()?;
        drop(lease);
        assert!(instance.live()?);
        drop(monitor);
        assert!(
            instance.live()?,
            "a VM must retain exclusion after its monitor dies"
        );
        drop(vm);
        assert!(!instance.live()?);
        assert!(
            matches!(instance.state()?, State::Unknown),
            "a lost monitor must not fabricate an exit status"
        );
        assert_eq!(records.pins()?, vec![Digest::Sha256([1; 32])]);
        assert!(instance.disk().is_file());
        let instance = instance.reread()?;
        assert_eq!(instance.attempt(), selected);
        assert_eq!(
            instance.run()?.context("selected boot")?.endpoint(),
            run.endpoint()
        );
        let lease = instance.lease()?;
        instance.check_disk(&lease).await?;
        Ok(())
    }

    #[tokio::test]
    async fn interrupted_removal_retains_intent_and_image_pin() -> Result<()> {
        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = Records::open(&graphroot)?;
        let instance = Instance::create(&records, Spec::fixture()).await?;
        let id = instance.id();
        let disk = instance.disk();
        // Make payload removal fail after intent is durable. This is a test
        // instance's own disk; no external file is imported or removed.
        std::fs::remove_file(&disk)?;
        std::fs::create_dir(&disk)?;
        let lease = instance.lease()?;
        let layout = pichi_storage::CacheLayout {
            graphroot: graphroot.clone(),
            runroot: root.path().join("run"),
            mode: pichi_storage::Mode::Rootless,
        };
        assert!(instance.remove(lease, &records, &layout).await.is_err());
        let Entry::Instance(instance) = records.get(id)? else {
            bail!("instance disappeared")
        };
        assert!(instance.removing());
        assert!(matches!(instance.state()?, State::Removing));
        assert_eq!(records.pins()?, vec![Digest::Sha256([1; 32])]);
        std::fs::remove_dir(&disk)?;
        let lease = instance.lease()?;
        instance.remove(lease, &records, &layout).await?;
        assert!(records.list()?.is_empty());
        assert!(records.pins()?.is_empty());
        assert!(!records.path().join(id.to_string()).exists());
        Ok(())
    }

    #[tokio::test]
    async fn blocking_observers_release_directories_before_removal() -> Result<()> {
        use interprocess::local_socket::{GenericFilePath, ListenerOptions, tokio::prelude::*};
        for operation in ["wait", "stop", "attach"] {
            let root = tempfile::tempdir()?;
            let graphroot = root.path().join("cache");
            std::fs::create_dir(&graphroot)?;
            let records = Records::open(&graphroot)?;
            let mut instance = Instance::create(&records, Spec::fixture()).await?;
            let id = instance.id();
            let lease = instance.lease()?;
            let (run, mut attempt) = instance.prepare_attempt(&lease)?;
            if cfg!(unix) {
                attempt.endpoint = root.path().join("observer.sock");
            }
            let endpoint = attempt.endpoint.clone();
            instance.select_attempt(&lease, attempt)?;
            drop((run, lease));
            let listener = ListenerOptions::new()
                .name(endpoint.to_fs_name::<GenericFilePath>()?)
                .create_tokio()?;
            let observer = super::super::Workload::new(records.get(id)?)?;
            let observing = async move {
                match operation {
                    "wait" => observer.wait().await.map(|_| ()),
                    "stop" => observer.stop(Duration::ZERO).await.map(|_| ()),
                    _ => observer.attach(false).await.map(|_| ()),
                }
            };
            tokio::pin!(observing);
            // Hold the connection without responding, so the CLI operation
            // remains pending while removal needs every directory released.
            let connection = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::select! {
                    connection = listener.accept() => Ok::<_, anyhow::Error>(connection?),
                    result = &mut observing => bail!("observer ended before connecting: {result:?}"),
                }
            }).await??;
            let lease = instance.lease()?;
            let layout = pichi_storage::CacheLayout {
                graphroot,
                runroot: root.path().join("run"),
                mode: pichi_storage::Mode::Rootless,
            };
            instance.remove(lease, &records, &layout).await?;
            assert!(records.list()?.is_empty());
            assert!(records.pins()?.is_empty());
            assert_eq!(records.directory.entries()?.count(), 0);
            drop(connection);
        }
        Ok(())
    }

    #[tokio::test]
    async fn disconnected_waiter_reads_its_original_boot_after_restart() -> Result<()> {
        use interprocess::local_socket::{GenericFilePath, ListenerOptions, tokio::prelude::*};
        use madrillo::{CleanupResult, Completion, CompletionCause, LeaderResult, OomEvidence};

        let root = tempfile::tempdir()?;
        let graphroot = root.path().join("cache");
        std::fs::create_dir(&graphroot)?;
        let records = Records::open(&graphroot)?;
        let mut instance = Instance::create(&records, Spec::fixture()).await?;
        let lease = instance.lease()?;
        let (first, mut attempt) = instance.prepare_attempt(&lease)?;
        if cfg!(unix) {
            attempt.endpoint = root.path().join("waiter.sock");
        }
        let listener = ListenerOptions::new()
            .name(attempt.endpoint.as_path().to_fs_name::<GenericFilePath>()?)
            .create_tokio()?;
        instance.select_attempt(&lease, attempt)?;
        let observer = super::super::Workload::new(records.get(instance.id())?)?;
        let waiting = observer.wait();
        tokio::pin!(waiting);
        let connection = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                connection = listener.accept() => Ok::<_, anyhow::Error>(connection?),
                result = &mut waiting => bail!("waiter ended before connecting: {result:?}"),
            }
        })
        .await??;

        // Complete the observed boot and select a different one before the
        // waiter sees EOF. Its fallback must still report the first result.
        let (second, attempt) = instance.prepare_attempt(&lease)?;
        for (run, code) in [(&first, 37), (&second, 42)] {
            let result = Completion {
                version: madrillo::PROTOCOL_VERSION,
                leader: LeaderResult::Exit { code },
                cause: CompletionCause::LeaderExit,
                exited_at_unix_ms: 0,
                cleanup: CleanupResult::Complete,
                oom: OomEvidence::NoneObserved,
                log_errors: Vec::new(),
                monitor_errors: Vec::new(),
                monitor_warnings: Vec::new(),
                output_truncated: false,
            };
            std::fs::create_dir(run.state())?;
            std::fs::write(
                run.state().join("result.json"),
                serde_json::to_vec(&result)?,
            )?;
        }
        instance.select_attempt(&lease, attempt)?;
        drop(connection);
        let result = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await??
            .context("missing observed result")?;
        assert_eq!(result.code()?, 37);
        assert_eq!(
            instance
                .run()?
                .context("selected boot")?
                .completion()?
                .context("selected result")?
                .leader,
            LeaderResult::Exit { code: 42 },
        );
        Ok(())
    }
}
