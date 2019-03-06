use core::store::Directory;
use error::*;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

///  An inter process mutex lock.
/// Typical use might look like:<pre class="prettyprint">
///   try (final Lock lock = directory.obtainLock("my.lock")) {
///     // ... code to execute while locked ...
///   }
/// </pre>
///
/// @see Directory#obtainLock(String)
///
pub trait Lock: Sync + Send {
    /// Releases exclusive access.
    ///
    /// Note that exceptions thrown from close may require
    /// human intervention, as it may mean the lock was no
    /// longer valid, or that fs permissions prevent removal
    /// of the lock file, or other reasons.
    ///
    /// @throws LockReleaseFailedException optional specific exception) if
    ///         the lock could not be properly released.
    fn close(&self) -> Result<()>;

    /// Best effort check that this lock is still valid. Locks
    /// could become invalidated externally for a number of reasons,
    /// for example if a user deletes the lock file manually or
    /// when a network filesystem is in use.
    /// @throws IOException if the lock is no longer valid.
    fn ensure_valid(&self) -> Result<()>;
}

/// Base class for Locking implementation. `Directory` uses
/// instances of this class to implement locking.
///
/// Lucene uses `NativeFSLockFactory` by default for
/// `FSDirectory`-based index directories.
///
/// Special care needs to be taken if you change the locking
/// implementation: First be certain that no writer is in fact
/// writing to the index otherwise you can easily corrupt
/// your index. Be sure to do the LockFactory change on all Lucene
/// instances and clean up all leftover lock files before starting
/// the new configuration for the first time. Different implementations
/// can not work together!
///
/// If you suspect that some LockFactory implementation is
/// not working properly in your environment, you can easily
/// test it by using `VerifyingLockFactory`, `LockVerifyServer` and `LockStressTest`.
///
/// @see LockVerifyServer
/// @see LockStressTest
/// @see VerifyingLockFactory
///
pub trait LockFactory {
    ///
    // Return a new obtained Lock instance identified by lockName.
    // @param lockName name of the lock to be created.
    // @throws LockObtainFailedException (optional specific exception) if the lock could
    //         not be obtained because it is currently held elsewhere.
    // @throws IOException if any i/o error occurs attempting to gain the lock
    //
    fn obtain_lock(&self, dir: &Directory, lock_name: &str) -> Result<Box<Lock>>;
}

pub struct NativeFSLock {
    lock: Mutex<String>,
    channel: fs::File,
    real_path: PathBuf,
    lock_held: Arc<Mutex<HashSet<PathBuf>>>,
}

impl NativeFSLock {
    pub fn new(
        lock: Mutex<String>,
        channel: fs::File,
        real_path: PathBuf,
        lock_held: Arc<Mutex<HashSet<PathBuf>>>,
    ) -> NativeFSLock {
        NativeFSLock {
            lock,
            channel,
            real_path,
            lock_held,
        }
    }
}

impl Lock for NativeFSLock {
    fn close(&self) -> Result<()> {
        // NOTE: we don't validate, as unlike SimpleFSLockFactory, we can't break others locks
        // first release the lock, then the channel
        let remove = self.lock_held.lock()?.remove(&self.real_path);
        if !remove {
            bail!(
                "Lock path was cleared but never marked as held: {:?}",
                self.real_path
            );
        }

        Ok(())
    }

    fn ensure_valid(&self) -> Result<()> {
        if !self.lock_held.lock()?.contains(&self.real_path) {
            bail!("Lock path unexpectedly cleared from map");
        }

        let meta = fs::metadata(&self.real_path)?;
        if meta.len() != 0 {
            bail!("Unexpected lock file size");
        }

        Ok(())
    }
}

unsafe impl Send for NativeFSLock {}

unsafe impl Sync for NativeFSLock {}

pub struct NativeFSLockFactory {
    pub lock_held: Arc<Mutex<HashSet<PathBuf>>>,
}

impl Default for NativeFSLockFactory {
    fn default() -> NativeFSLockFactory {
        NativeFSLockFactory {
            lock_held: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

impl LockFactory for NativeFSLockFactory {
    fn obtain_lock(&self, dir: &Directory, lock_name: &str) -> Result<Box<Lock>> {
        let mut real_path = dir.resolve(lock_name);
        real_path.pop();
        let _ = fs::create_dir(&real_path);

        real_path = dir.resolve(lock_name);
        let channel = fs::File::create(&real_path)?;

        self.lock_held.lock()?.insert(real_path.clone());

        Ok(Box::new(NativeFSLock::new(
            Mutex::new(lock_name.to_string()),
            channel,
            real_path,
            Arc::clone(&self.lock_held),
        )))
    }
}
