use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, mpsc},
    time::Duration,
};

use dispatch_channel_protocol::{
    ChannelEventAcceptance, ChannelEventBatch, ChannelEventRequest, ChannelIngressRecovery,
    ChannelIngressStorage, PluginMessage, PluginRequestId, event_request_to_jsonrpc,
    parse_jsonrpc_message,
};
use fs2::FileExt;

use crate::{RuntimeError, journal_fs, write_stdout_line};

const MAX_JOURNAL_BYTES: u64 = 8 * 1024 * 1024;
/// Interval for observing stop requests while a receipt is outstanding.
const RECEIPT_POLL_INTERVAL: Duration = Duration::from_millis(100);

struct EventDelivery {
    stdout_lock: Arc<Mutex<()>>,
    journal_path: Mutex<Option<PathBuf>>,
    /// Host head supplied with the latest ingress request, consumed by the next
    /// reconciliation. Local journal files are authoritative afterwards.
    recovery: Mutex<Option<ChannelIngressRecovery>>,
    receipt: Mutex<ReceiptSlot>,
    gate: Mutex<()>,
}

#[derive(Default)]
struct ReceiptSlot {
    waiting: Option<(PluginRequestId, mpsc::Sender<ChannelEventAcceptance>)>,
    input_closed: bool,
}

static DELIVERIES: OnceLock<Mutex<BTreeMap<usize, Arc<EventDelivery>>>> = OnceLock::new();

fn delivery(stdout_lock: &Arc<Mutex<()>>) -> Arc<EventDelivery> {
    let key = Arc::as_ptr(stdout_lock) as usize;
    Arc::clone(
        DELIVERIES
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .expect("channel event deliveries poisoned")
            .entry(key)
            .or_insert_with(|| {
                Arc::new(EventDelivery {
                    stdout_lock: Arc::clone(stdout_lock),
                    journal_path: Mutex::new(None),
                    recovery: Mutex::new(None),
                    receipt: Mutex::new(ReceiptSlot::default()),
                    gate: Mutex::new(()),
                })
            }),
    )
}

fn locked<'a, T>(mutex: &'a Mutex<T>, name: &str) -> Result<MutexGuard<'a, T>, RuntimeError> {
    mutex
        .lock()
        .map_err(|_| RuntimeError::Journal(format!("event {name} lock poisoned")))
}

fn journal_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Journal(error.to_string())
}

pub fn configure_event_storage(
    stdout_lock: &Arc<Mutex<()>>,
    storage: &ChannelIngressStorage,
) -> Result<(), RuntimeError> {
    storage.validate().map_err(journal_error)?;
    let path = PathBuf::from(&storage.journal_path);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir) || part.as_os_str() == ".agent-shell")
    {
        return Err(RuntimeError::Journal(
            "unsafe event journal path".to_string(),
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| RuntimeError::Journal("event journal path has no parent".to_string()))?;
    journal_fs::validate_directory(parent)?;
    for file in [
        path.clone(),
        temporary_path(&path),
        accepted_path(&path),
        temporary_path(&accepted_path(&path)),
        lock_path(&path),
    ] {
        match journal_fs::validate_artifact(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let sender = delivery(stdout_lock);
    let mut configured = locked(&sender.journal_path, "journal path")?;
    if configured
        .as_ref()
        .is_some_and(|existing| existing != &path)
    {
        return Err(RuntimeError::Journal(
            "event journal path changed during plugin lifetime".to_string(),
        ));
    }
    *configured = Some(path);
    *locked(&sender.recovery, "recovery")? = storage.recovery.clone();
    Ok(())
}

/// Route one host receipt to the batch waiting for it.
///
/// A receipt that matches no waiting batch never confirms anything; the
/// pending journal resolves it on replay, so it is reported and ignored.
pub fn route_event_acceptance(
    stdout_lock: &Arc<Mutex<()>>,
    id: PluginRequestId,
    acceptance: ChannelEventAcceptance,
) -> Result<(), RuntimeError> {
    let sender = delivery(stdout_lock);
    let slot = locked(&sender.receipt, "receipt")?;
    match slot.waiting.as_ref() {
        Some((expected_id, response)) if expected_id == &id => {
            // A closed receiver means the batch stopped waiting; replay resolves it.
            let _ = response.send(acceptance);
        }
        _ => eprintln!("ignoring channel event receipt that matches no waiting batch"),
    }
    Ok(())
}

/// Fail any outstanding and future receipt wait after host input ends.
pub fn close_event_input(stdout_lock: &Arc<Mutex<()>>) {
    let sender = delivery(stdout_lock);
    if let Ok(mut slot) = sender.receipt.lock() {
        slot.input_closed = true;
        slot.waiting = None;
    }
}

/// Journal, emit, and confirm one batch. Returns only after the host's exact
/// durable receipt for the batch, or for its unchanged accepted original.
pub fn deliver_channel_event_batch(
    stdout_lock: &Arc<Mutex<()>>,
    batch: ChannelEventBatch,
) -> Result<(), RuntimeError> {
    deliver_channel_event_batch_until(stdout_lock, batch, &|| false)
}

/// Like [`deliver_channel_event_batch`], but stops waiting for a receipt once
/// `stopped` reports true. The original stays journaled for replay.
pub fn deliver_channel_event_batch_until(
    stdout_lock: &Arc<Mutex<()>>,
    batch: ChannelEventBatch,
    stopped: &dyn Fn() -> bool,
) -> Result<(), RuntimeError> {
    let sender = delivery(stdout_lock);
    let _gate = locked(&sender.gate, "delivery")?;
    let path = journal_path(&sender)?;
    let _file_lock = lock_journal(&path)?;
    let replayed = replay_pending_locked(&sender, &path, stopped)?;
    let accepted = read_accepted(&path)?;
    if let Some(head) = accepted.as_ref().filter(|head| head.request.batch == batch) {
        // A no-progress batch replays the exact accepted original instead of
        // naming its own receipt as predecessor.
        if replayed.as_deref() != Some(head.request.batch_digest.as_str()) {
            exchange(&sender, &head.request, stopped)?;
        }
        return Ok(());
    }
    let request = ChannelEventRequest::new(batch, accepted.map(|head| head.acceptance))
        .map_err(journal_error)?;
    persist(&path, &request)?;
    confirm_pending(&sender, &path, &request, stopped)
}

pub fn replay_pending_event_batch(stdout_lock: &Arc<Mutex<()>>) -> Result<(), RuntimeError> {
    replay_pending_event_batch_until(stdout_lock, &|| false)
}

pub fn replay_pending_event_batch_until(
    stdout_lock: &Arc<Mutex<()>>,
    stopped: &dyn Fn() -> bool,
) -> Result<(), RuntimeError> {
    let sender = delivery(stdout_lock);
    let _gate = locked(&sender.gate, "delivery")?;
    let path = journal_path(&sender)?;
    let _file_lock = lock_journal(&path)?;
    replay_pending_locked(&sender, &path, stopped).map(|_| ())
}

/// Reconcile host recovery with the local journal and re-send an unconfirmed
/// original. Returns the digest only when an original was emitted and confirmed.
fn replay_pending_locked(
    sender: &EventDelivery,
    path: &Path,
    stopped: &dyn Fn() -> bool,
) -> Result<Option<String>, RuntimeError> {
    let recovery = locked(&sender.recovery, "recovery")?.take();
    let pending = read_pending(path)?;
    let accepted = reconcile_recovery(path, recovery.as_ref(), pending.as_ref())?;
    let Some(request) = pending else {
        return Ok(None);
    };
    match accepted {
        Some(head) if head.request == request => {
            // The receipt was stored before the pending original was cleared.
            journal_fs::reclaim(path)?;
            return Ok(None);
        }
        Some(head) if request.previous_acceptance.as_ref() != Some(&head.acceptance) => {
            return Err(RuntimeError::Journal(
                "pending event predecessor conflicts with accepted head".to_string(),
            ));
        }
        None if request.previous_acceptance.is_some() => {
            return Err(RuntimeError::Journal(
                "pending event predecessor has no accepted head".to_string(),
            ));
        }
        _ => {}
    }
    confirm_pending(sender, path, &request, stopped)?;
    Ok(Some(request.batch_digest))
}

/// Emit a journaled original, then durably record it with its receipt and
/// effective cursor before clearing the pending journal.
fn confirm_pending(
    sender: &EventDelivery,
    path: &Path,
    request: &ChannelEventRequest,
    stopped: &dyn Fn() -> bool,
) -> Result<(), RuntimeError> {
    let acceptance = exchange(sender, request, stopped)?;
    let previous_state = read_accepted(path)?.and_then(|frame| frame.state);
    let state = request.batch.state.clone().or(previous_state);
    write_accepted(
        path,
        &ChannelIngressRecovery {
            request: request.clone(),
            acceptance,
            state,
        },
    )?;
    // The synced accepted original is retirement authority. Physical deletion
    // only reclaims its payload; restart reconciliation handles a survivor.
    journal_fs::reclaim(path)?;
    Ok(())
}

/// Write one exact original and wait for its matching receipt.
fn exchange(
    sender: &EventDelivery,
    request: &ChannelEventRequest,
    stopped: &dyn Fn() -> bool,
) -> Result<ChannelEventAcceptance, RuntimeError> {
    let (respond, receive) = mpsc::channel();
    {
        let mut slot = locked(&sender.receipt, "receipt")?;
        if slot.input_closed {
            return Err(RuntimeError::Journal(
                "host input closed before event acceptance".to_string(),
            ));
        }
        slot.waiting = Some((request.request_id(), respond));
    }
    let result = (|| {
        let json = event_request_to_jsonrpc(request).map_err(journal_error)?;
        write_stdout_line(&sender.stdout_lock, &json)?;
        loop {
            match receive.recv_timeout(RECEIPT_POLL_INTERVAL) {
                Ok(acceptance) if acceptance.matches(request) => return Ok(acceptance),
                Ok(_) => {
                    return Err(RuntimeError::Journal(
                        "event acceptance does not match original batch".to_string(),
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) if stopped() => {
                    return Err(RuntimeError::Journal(
                        "event acceptance wait stopped".to_string(),
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(RuntimeError::Journal(
                        "host input closed before event acceptance".to_string(),
                    ));
                }
            }
        }
    })();
    locked(&sender.receipt, "receipt")?.waiting = None;
    result
}

fn reconcile_recovery(
    path: &Path,
    recovery: Option<&ChannelIngressRecovery>,
    pending: Option<&ChannelEventRequest>,
) -> Result<Option<ChannelIngressRecovery>, RuntimeError> {
    let accepted = read_accepted(path)?;
    let Some(recovery) = recovery else {
        return Ok(accepted);
    };
    recovery.validate().map_err(journal_error)?;
    let adopt = match accepted.as_ref() {
        Some(current) if current == recovery => false,
        Some(current)
            if current.request == recovery.request && current.acceptance == recovery.acceptance =>
        {
            true
        }
        Some(current)
            if recovery.request.previous_acceptance.as_ref() == Some(&current.acceptance) =>
        {
            if pending.is_some_and(|request| request != &recovery.request) {
                return Err(RuntimeError::Journal(
                    "host recovery conflicts with pending original".to_string(),
                ));
            }
            true
        }
        // The host may lag one receipt behind a head this producer already recorded.
        Some(current)
            if current.request.previous_acceptance.as_ref() == Some(&recovery.acceptance) =>
        {
            false
        }
        Some(_) => {
            return Err(RuntimeError::Journal(
                "host recovery conflicts with local accepted head".to_string(),
            ));
        }
        None => {
            if pending.is_some_and(|request| {
                request != &recovery.request
                    && request.previous_acceptance.as_ref() != Some(&recovery.acceptance)
            }) {
                return Err(RuntimeError::Journal(
                    "host recovery conflicts with pending original".to_string(),
                ));
            }
            true
        }
    };
    if adopt {
        write_accepted(path, recovery)?;
        return Ok(Some(recovery.clone()));
    }
    Ok(accepted)
}

fn read_pending(path: &Path) -> Result<Option<ChannelEventRequest>, RuntimeError> {
    let Some(data) = read_private(path)? else {
        return Ok(None);
    };
    let line = std::str::from_utf8(&data)
        .map_err(|_| RuntimeError::Journal("event journal is not UTF-8".to_string()))?;
    let PluginMessage::EventRequest { id, request } =
        parse_jsonrpc_message(line).map_err(journal_error)?
    else {
        return Err(RuntimeError::Journal(
            "event journal contains a non-event message".to_string(),
        ));
    };
    if id != request.request_id() {
        return Err(RuntimeError::Journal(
            "event journal identity mismatch".to_string(),
        ));
    }
    Ok(Some(request))
}

fn read_accepted(path: &Path) -> Result<Option<ChannelIngressRecovery>, RuntimeError> {
    let Some(data) = read_private(&accepted_path(path))? else {
        return Ok(None);
    };
    let frame: ChannelIngressRecovery = serde_json::from_slice(&data).map_err(journal_error)?;
    frame.validate().map_err(journal_error)?;
    Ok(Some(frame))
}

fn read_private(path: &Path) -> Result<Option<Vec<u8>>, RuntimeError> {
    let file = match journal_fs::open_existing(path, false) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() > MAX_JOURNAL_BYTES {
        return Err(RuntimeError::Journal(
            "unsafe event journal artifact".to_string(),
        ));
    }
    let mut data = Vec::new();
    file.take(MAX_JOURNAL_BYTES + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(RuntimeError::Journal(
            "event journal artifact exceeds bound".to_string(),
        ));
    }
    Ok(Some(data))
}

fn write_accepted(path: &Path, frame: &ChannelIngressRecovery) -> Result<(), RuntimeError> {
    frame.validate().map_err(journal_error)?;
    let data = serde_json::to_vec(frame).map_err(journal_error)?;
    write_replacing(&accepted_path(path), &data)
}

fn persist(path: &Path, request: &ChannelEventRequest) -> Result<(), RuntimeError> {
    let json = event_request_to_jsonrpc(request).map_err(journal_error)?;
    write_replacing(path, json.as_bytes())
}

/// Publish synced `data` with Unix directory sync or Windows write-through move.
fn write_replacing(target: &Path, data: &[u8]) -> Result<(), RuntimeError> {
    if data.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(RuntimeError::Journal(
            "event journal artifact exceeds bound".to_string(),
        ));
    }
    let temporary = temporary_path(target);
    let mut file = journal_fs::create_private_file(&temporary)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    journal_fs::publish(&temporary, target)?;
    Ok(())
}

fn lock_journal(path: &Path) -> Result<File, RuntimeError> {
    let lock = lock_path(path);
    let file = match journal_fs::create_private_file(&lock) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            journal_fs::open_existing(&lock, true)?
        }
        Err(error) => return Err(error.into()),
    };
    // The lockfile is never removed or replaced, so every process locks one inode.
    file.lock_exclusive()?;
    discard_unrenamed(path)?;
    Ok(file)
}

/// Remove temporaries left before their rename. A pending original is emitted
/// only after its rename, and an accepted frame is recoverable from the
/// pending original or host recovery, so neither temporary is authoritative.
fn discard_unrenamed(path: &Path) -> Result<(), RuntimeError> {
    for temporary in [temporary_path(path), temporary_path(&accepted_path(path))] {
        match journal_fs::reclaim(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn temporary_path(path: &Path) -> PathBuf {
    sibling_path(path, ".pending")
}

fn accepted_path(path: &Path) -> PathBuf {
    sibling_path(path, ".accepted")
}

fn lock_path(path: &Path) -> PathBuf {
    sibling_path(path, ".lock")
}

fn journal_path(sender: &EventDelivery) -> Result<PathBuf, RuntimeError> {
    locked(&sender.journal_path, "journal path")?
        .clone()
        .ok_or_else(|| RuntimeError::Journal("event storage was not configured".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch_channel_protocol::{
        IngressState, PluginInput, event_acceptance_to_jsonrpc, parse_plugin_input,
    };
    use serde_json::Value;
    use std::{
        fs,
        process::{Command, Stdio},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dispatch-event-journal-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        crate::create_event_journal_directory(&path).unwrap();
        path
    }

    fn cursor(value: &str) -> IngressState {
        serde_json::from_value(serde_json::json!({
            "mode": "polling", "status": "connected", "metadata": {"cursor": value}
        }))
        .unwrap()
    }

    fn cursor_batch(value: &str) -> ChannelEventBatch {
        ChannelEventBatch {
            events: Vec::new(),
            state: Some(cursor(value)),
            poll_after_ms: None,
        }
    }

    fn accepted_frame(request: &ChannelEventRequest) -> ChannelIngressRecovery {
        ChannelIngressRecovery {
            request: request.clone(),
            acceptance: request.acceptance().unwrap(),
            state: request.batch.state.clone(),
        }
    }

    fn storage(path: &Path, recovery: Option<ChannelIngressRecovery>) -> ChannelIngressStorage {
        ChannelIngressStorage {
            journal_path: path.to_string_lossy().into_owned(),
            recovery,
        }
    }

    /// Runs `deliver` on a thread while this thread plays the host: it records
    /// each emitted original and answers with its exact receipt.
    fn with_host<T: Send + 'static>(
        stdout_lock: &Arc<Mutex<()>>,
        deliver: impl FnOnce() -> T + Send + 'static,
        expected: usize,
    ) -> (T, Vec<ChannelEventRequest>) {
        let sender = delivery(stdout_lock);
        let worker = std::thread::spawn(deliver);
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while seen.len() < expected && std::time::Instant::now() < deadline {
            let waiting = sender
                .receipt
                .lock()
                .unwrap()
                .waiting
                .as_ref()
                .map(|(id, _)| id.clone());
            let Some(id) = waiting else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            let path = journal_path(&sender).unwrap();
            let request = read_pending(&path)
                .unwrap()
                .filter(|request| request.request_id() == id)
                .or_else(|| {
                    read_accepted(&path)
                        .unwrap()
                        .map(|head| head.request)
                        .filter(|request| request.request_id() == id)
                })
                .expect("waiting batch is journaled");
            route_event_acceptance(stdout_lock, id, request.acceptance().unwrap()).unwrap();
            seen.push(request);
            while sender.receipt.lock().unwrap().waiting.is_some()
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if seen.len() < expected {
            // Fail the outstanding receipt wait instead of joining it forever.
            close_event_input(stdout_lock);
        }
        (worker.join().unwrap(), seen)
    }

    #[test]
    fn receipt_fsync_before_pending_removal_recovers_without_reemitting() {
        let root = directory();
        let path = root.join("pending.journal");
        let stdout_lock = Arc::new(Mutex::new(()));
        let request = ChannelEventRequest::new(cursor_batch("42"), None).unwrap();
        let frame = accepted_frame(&request);
        configure_event_storage(&stdout_lock, &storage(&path, Some(frame.clone()))).unwrap();
        persist(&path, &request).unwrap();
        write_accepted(&path, &frame).unwrap();
        // A surviving retired original must not enter the receipt exchange.
        close_event_input(&stdout_lock);
        replay_pending_event_batch(&stdout_lock).unwrap();
        assert!(!path.exists());
        assert_eq!(read_accepted(&path).unwrap(), Some(frame));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn host_recovery_carries_cursor_across_a_state_free_successor() {
        let root = directory();
        let path = root.join("cursor.journal");
        let first = ChannelEventRequest::new(cursor_batch("42"), None).unwrap();
        write_accepted(&path, &accepted_frame(&first)).unwrap();
        let second = ChannelEventRequest::new(
            ChannelEventBatch {
                events: Vec::new(),
                state: None,
                poll_after_ms: Some(5000),
            },
            Some(first.acceptance().unwrap()),
        )
        .unwrap();
        let second_frame = ChannelIngressRecovery {
            acceptance: second.acceptance().unwrap(),
            request: second,
            state: Some(cursor("42")),
        };
        assert_eq!(
            reconcile_recovery(&path, Some(&second_frame), None).unwrap(),
            Some(second_frame.clone())
        );
        assert_eq!(
            read_accepted(&path).unwrap().unwrap().state,
            Some(cursor("42"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn accepted_publication_failure_preserves_prior_cursor_and_pending_original() {
        let root = directory();
        let path = root.join("failed-head.journal");
        let first = ChannelEventRequest::new(cursor_batch("1"), None).unwrap();
        let prior = accepted_frame(&first);
        write_accepted(&path, &prior).unwrap();
        let next =
            ChannelEventRequest::new(cursor_batch("2"), Some(first.acceptance().unwrap())).unwrap();
        persist(&path, &next).unwrap();
        let temporary = temporary_path(&accepted_path(&path));
        drop(journal_fs::create_private_file(&temporary).unwrap());
        assert!(reconcile_recovery(&path, Some(&accepted_frame(&next)), Some(&next)).is_err());
        assert_eq!(read_accepted(&path).unwrap(), Some(prior));
        assert_eq!(read_pending(&path).unwrap(), Some(next));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn no_progress_batch_replays_the_accepted_original_instead_of_succeeding_itself() {
        let root = directory();
        let path = root.join("idle.journal");
        let stdout_lock = Arc::new(Mutex::new(()));
        configure_event_storage(&stdout_lock, &storage(&path, None)).unwrap();
        let lock = Arc::clone(&stdout_lock);
        let (result, first) = with_host(
            &stdout_lock,
            move || deliver_channel_event_batch(&lock, cursor_batch("7")),
            1,
        );
        result.unwrap();
        let lock = Arc::clone(&stdout_lock);
        let (result, replayed) = with_host(
            &stdout_lock,
            move || deliver_channel_event_batch(&lock, cursor_batch("7")),
            1,
        );
        result.unwrap();
        assert_eq!(
            replayed, first,
            "the same original and predecessor are re-sent"
        );
        let lock = Arc::clone(&stdout_lock);
        let (result, next) = with_host(
            &stdout_lock,
            move || deliver_channel_event_batch(&lock, cursor_batch("8")),
            1,
        );
        result.unwrap();
        assert_eq!(
            next[0].previous_acceptance,
            Some(first[0].acceptance().unwrap())
        );
        assert_eq!(
            read_accepted(&path).unwrap().unwrap().state,
            Some(cursor("8"))
        );
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unrenamed_temporaries_are_discarded_and_a_torn_original_is_never_emitted() {
        let root = directory();
        let path = root.join("torn.journal");
        let stdout_lock = Arc::new(Mutex::new(()));
        configure_event_storage(&stdout_lock, &storage(&path, None)).unwrap();
        for temporary in [temporary_path(&path), temporary_path(&accepted_path(&path))] {
            let mut file = journal_fs::create_private_file(&temporary).unwrap();
            file.write_all(br#"{"jsonrpc":"2.0","id":"chan"#).unwrap();
        }
        replay_pending_event_batch(&stdout_lock).unwrap();
        assert!(!temporary_path(&path).exists());
        assert!(!temporary_path(&accepted_path(&path)).exists());
        assert!(read_accepted(&path).unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unreadable_committed_journal_fails_instead_of_succeeding() {
        let root = directory();
        let path = root.join("corrupt.journal");
        let stdout_lock = Arc::new(Mutex::new(()));
        configure_event_storage(&stdout_lock, &storage(&path, None)).unwrap();
        write_replacing(&path, b"not a channel frame").unwrap();
        assert!(deliver_channel_event_batch(&stdout_lock, cursor_batch("1")).is_err());
        assert!(path.exists(), "the unreadable original is retained");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stopped_receipt_wait_keeps_the_original_and_late_receipts_are_ignored() {
        let root = directory();
        let path = root.join("stopped.journal");
        let stdout_lock = Arc::new(Mutex::new(()));
        configure_event_storage(&stdout_lock, &storage(&path, None)).unwrap();
        let error = deliver_channel_event_batch_until(&stdout_lock, cursor_batch("3"), &|| true)
            .unwrap_err();
        assert!(error.to_string().contains("stopped"));
        let pending = read_pending(&path)
            .unwrap()
            .expect("original stays journaled");
        route_event_acceptance(
            &stdout_lock,
            pending.request_id(),
            pending.acceptance().unwrap(),
        )
        .unwrap();
        assert!(
            read_accepted(&path).unwrap().is_none(),
            "a late receipt confirms nothing"
        );
        let lock = Arc::clone(&stdout_lock);
        let (result, replayed) = with_host(
            &stdout_lock,
            move || deliver_channel_event_batch(&lock, cursor_batch("3")),
            1,
        );
        result.unwrap();
        assert_eq!(replayed, vec![pending]);
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn closed_host_input_fails_the_receipt_wait() {
        let root = directory();
        let path = root.join("closed.journal");
        let stdout_lock = Arc::new(Mutex::new(()));
        configure_event_storage(&stdout_lock, &storage(&path, None)).unwrap();
        close_event_input(&stdout_lock);
        assert!(deliver_channel_event_batch(&stdout_lock, cursor_batch("4")).is_err());
        assert!(
            path.exists(),
            "the original stays journaled for the next process"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_host_recovery_does_not_override_a_newer_local_chain() {
        let root = directory();
        let path = root.join("shared.journal");
        let stdout_lock = Arc::new(Mutex::new(()));
        let first = ChannelEventRequest::new(cursor_batch("1"), None).unwrap();
        write_accepted(&path, &accepted_frame(&first)).unwrap();
        configure_event_storage(&stdout_lock, &storage(&path, Some(accepted_frame(&first))))
            .unwrap();
        replay_pending_event_batch(&stdout_lock).unwrap();
        // Another producer process sharing the journal advances it twice.
        let second =
            ChannelEventRequest::new(cursor_batch("2"), Some(first.acceptance().unwrap())).unwrap();
        let third = ChannelEventRequest::new(cursor_batch("3"), Some(second.acceptance().unwrap()))
            .unwrap();
        write_accepted(&path, &accepted_frame(&third)).unwrap();
        let lock = Arc::clone(&stdout_lock);
        let (result, next) = with_host(
            &stdout_lock,
            move || deliver_channel_event_batch(&lock, cursor_batch("4")),
            1,
        );
        result.unwrap();
        assert_eq!(
            next[0].previous_acceptance,
            Some(third.acceptance().unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_receipt_frames_round_trip_through_plugin_input() {
        let request = ChannelEventRequest::new(cursor_batch("9"), None).unwrap();
        let line =
            event_acceptance_to_jsonrpc(&request.request_id(), &request.acceptance().unwrap())
                .unwrap();
        let PluginInput::EventAcceptance { id, acceptance } =
            parse_plugin_input::<Value, Value>(&line).unwrap()
        else {
            panic!("expected a receipt");
        };
        assert_eq!(id, request.request_id());
        assert!(acceptance.matches(&request));
    }

    #[test]
    fn journal_lock_child() {
        let Ok(path) = std::env::var("DISPATCH_JOURNAL_LOCK_CHILD") else {
            return;
        };
        let lock = lock_journal(Path::new(&path)).unwrap();
        let marker = std::env::var("DISPATCH_JOURNAL_LOCK_MARKER").unwrap();
        File::create(&marker).unwrap();
        if let Ok(release) = std::env::var("DISPATCH_JOURNAL_LOCK_RELEASE") {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !Path::new(&release).exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(Path::new(&release).exists());
        }
        drop(lock);
    }

    #[test]
    fn journal_lock_serializes_across_processes_on_one_inode() {
        let root = directory();
        let path = root.join("shared.journal");
        let first_marker = root.join("first.entered");
        let second_marker = root.join("second.entered");
        let release = root.join("release");
        let child = |marker: &Path| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "event_delivery::tests::journal_lock_child",
                    "--nocapture",
                ])
                .env("DISPATCH_JOURNAL_LOCK_CHILD", &path)
                .env("DISPATCH_JOURNAL_LOCK_MARKER", marker)
                .stdout(Stdio::null());
            command
        };
        let mut first = child(&first_marker)
            .env("DISPATCH_JOURNAL_LOCK_RELEASE", &release)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !first_marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(first_marker.exists());
        let identity = journal_fs::file_identity(&lock_path(&path)).unwrap();
        let mut second = child(&second_marker).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!second_marker.exists());
        File::create(&release).unwrap();
        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());
        assert!(second_marker.exists());
        assert_eq!(
            journal_fs::file_identity(&lock_path(&path)).unwrap(),
            identity
        );
        fs::remove_dir_all(root).unwrap();
    }
}
