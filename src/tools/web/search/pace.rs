//! Spacing and bot-check cooldowns for keyless DuckDuckGo searches.
//!
//! The state lives in a file so every thread and every Yawl process on the
//! machine shares one pace, since DuckDuckGo judges them together. It holds
//! only timestamps, never queries. When the file cannot be used, the pace
//! falls back to this process alone.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::BackendError;
use crate::tools::web::interrupted;

/// Time between the starts of two searches. At 3 seconds a model rewriting
/// one query drew the bot check in most test runs; at about 9 it did not.
const INTERVAL_MS: u64 = 8_000;
const BASE_COOLDOWN_MS: u64 = 60_000;
const MAX_COOLDOWN_MS: u64 = 15 * 60_000;
/// How long a past bot check keeps counting toward a longer cooldown.
const STRIKE_MEMORY_MS: u64 = 60 * 60_000;
/// Longest believable queue of reserved slots, which is many subagents at once.
/// Anything later came from a clock that has since moved back.
const MAX_QUEUE_MS: u64 = 3 * 60_000;
const LOCK_WAIT: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(50);

/// The last state this process saw, for when the file cannot be read or written.
static MEMORY: Mutex<Option<(PathBuf, PaceState)>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct PaceState {
    last_slot_ms: u64,
    blocked_until_ms: u64,
    strikes: u32,
}

impl PaceState {
    fn sanitize(&mut self, now: u64) {
        self.blocked_until_ms = self.blocked_until_ms.min(now + MAX_COOLDOWN_MS);
        if self.last_slot_ms > now + MAX_QUEUE_MS {
            self.last_slot_ms = now;
        }
    }

    /// Keeps what this process knows that the file does not, such as a
    /// cooldown recorded while the file was locked or could not be written.
    fn include(&mut self, remembered: PaceState) {
        self.last_slot_ms = self.last_slot_ms.max(remembered.last_slot_ms);
        if remembered.blocked_until_ms > self.blocked_until_ms {
            self.blocked_until_ms = remembered.blocked_until_ms;
            self.strikes = remembered.strikes;
        }
    }

    fn blocked_for(&self, now: u64) -> Option<Duration> {
        (self.blocked_until_ms > now).then(|| Duration::from_millis(self.blocked_until_ms - now))
    }

    /// Reserves the next request time at least `INTERVAL_MS` after the
    /// previous one, or reports how long searches remain paused.
    fn claim(&mut self, now: u64) -> Result<u64, Duration> {
        if let Some(remaining) = self.blocked_for(now) {
            return Err(remaining);
        }
        let slot = if self.last_slot_ms == 0 {
            now
        } else {
            (self.last_slot_ms + INTERVAL_MS).max(now)
        };
        self.last_slot_ms = slot;
        Ok(slot)
    }

    /// Starts a cooldown that doubles with each bot check in a row. Requests
    /// already in flight when the first one hit do not count again.
    fn block(&mut self, now: u64) -> Duration {
        if let Some(remaining) = self.blocked_for(now) {
            return remaining;
        }
        if now.saturating_sub(self.blocked_until_ms) > STRIKE_MEMORY_MS {
            self.strikes = 0;
        }
        let doublings = self.strikes.min(16);
        self.strikes = self.strikes.saturating_add(1);
        let cooldown = BASE_COOLDOWN_MS
            .saturating_mul(1 << doublings)
            .min(MAX_COOLDOWN_MS);
        self.blocked_until_ms = now + cooldown;
        Duration::from_millis(cooldown)
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn read_state(file: &mut File) -> PaceState {
    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() {
        return PaceState::default();
    }
    serde_json::from_str(&text).unwrap_or_default()
}

fn write_state(file: &mut File, state: &PaceState) -> std::io::Result<()> {
    let text = serde_json::to_string(state).map_err(std::io::Error::other)?;
    // Written before truncating, so a full disk or a crash leaves the old state
    // or a mix that fails to parse, never an empty file that looks like no
    // cooldown at all.
    file.seek(SeekFrom::Start(0))?;
    file.write_all(text.as_bytes())?;
    file.set_len(text.len() as u64)
}

pub(super) struct Pace {
    path: PathBuf,
}

impl Pace {
    pub(super) fn new(state_dir: &Path) -> Self {
        Pace {
            path: state_dir.join("duckduckgo-pace.json"),
        }
    }

    fn open_locked(&self) -> Option<File> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).ok()?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .ok()?;
        let started = Instant::now();
        // Other holders keep the lock for microseconds. Waiting forever on a
        // stopped process would be worse than pacing alone for one search.
        loop {
            match file.try_lock() {
                Ok(()) => return Some(file),
                Err(TryLockError::WouldBlock) if started.elapsed() < LOCK_WAIT => {
                    std::thread::sleep(POLL);
                }
                // A filesystem without locks will not grow them by waiting.
                Err(_) => return None,
            }
        }
    }

    fn update<T>(&self, change: impl FnOnce(&mut PaceState, u64) -> T) -> T {
        let mut memory = MEMORY.lock().unwrap_or_else(PoisonError::into_inner);
        let now = unix_ms();
        let remembered = memory
            .as_ref()
            .filter(|(path, _)| *path == self.path)
            .map(|(_, state)| *state)
            .unwrap_or_default();
        let mut file = self.open_locked();
        let mut state = file.as_mut().map_or(remembered, read_state);
        state.include(remembered);
        state.sanitize(now);
        let output = change(&mut state, now);
        // The pace is best effort; a failed write must not fail the search.
        if let Some(file) = &mut file {
            let _ = write_state(file, &state);
        }
        *memory = Some((self.path.clone(), state));
        output
    }

    /// Gives back a slot that will not be used, so later searches do not wait
    /// for it. A later claim has already moved past it if there was one.
    fn release(&self, slot: u64) {
        self.update(|state, _| {
            if state.last_slot_ms == slot {
                state.last_slot_ms = slot.saturating_sub(INTERVAL_MS);
            }
        });
    }

    /// Waits for this request's slot, checking for cancellation while it
    /// sleeps. Fails at once while a cooldown is running.
    pub(super) fn wait_for_slot(&self) -> Result<(), BackendError> {
        let slot = self
            .update(|state, now| state.claim(now))
            .map_err(BackendError::Paused)?;
        loop {
            if let Err(error) = interrupted() {
                self.release(slot);
                return Err(BackendError::Failed(error));
            }
            let now = unix_ms();
            if now >= slot {
                break;
            }
            std::thread::sleep(Duration::from_millis(slot - now).min(POLL));
        }
        // Another thread may have hit the bot check while this one waited.
        match self.update(|state, now| state.blocked_for(now)) {
            Some(remaining) => {
                self.release(slot);
                Err(BackendError::Paused(remaining))
            }
            None => Ok(()),
        }
    }

    pub(super) fn record_block(&self) -> Duration {
        self.update(|state, now| state.block(now))
    }

    pub(super) fn record_success(&self) {
        // A request that was already in flight says nothing about a cooldown
        // that started after it left.
        self.update(|state, now| {
            if state.blocked_for(now).is_none() {
                state.strikes = 0;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: u64 = 1_000_000_000;

    #[test]
    fn requests_are_spaced_from_the_previous_slot() {
        let mut state = PaceState::default();
        assert_eq!(state.claim(START), Ok(START));
        assert_eq!(state.claim(START), Ok(START + INTERVAL_MS));
        assert_eq!(state.claim(START), Ok(START + INTERVAL_MS * 2));
        let later = START + 60_000;
        assert_eq!(state.claim(later), Ok(later));
    }

    #[test]
    fn repeated_bot_checks_lengthen_the_cooldown_up_to_a_limit() {
        let mut state = PaceState::default();
        let mut now = START;
        for expected in [60_000, 120_000, 240_000, 480_000, 900_000, 900_000] {
            assert_eq!(state.block(now), Duration::from_millis(expected));
            assert_eq!(
                state.claim(now + 1_000),
                Err(Duration::from_millis(expected - 1_000))
            );
            now += expected;
        }
    }

    #[test]
    fn requests_in_flight_do_not_extend_a_running_cooldown() {
        let mut state = PaceState::default();
        state.block(START);
        assert_eq!(state.block(START + 1_000), Duration::from_millis(59_000));
        assert_eq!(state.strikes, 1);
    }

    #[test]
    fn old_bot_checks_stop_counting() {
        let mut state = PaceState::default();
        state.block(START);
        let cooldown = state.block(START + BASE_COOLDOWN_MS + STRIKE_MEMORY_MS + 1);
        assert_eq!(cooldown, Duration::from_millis(BASE_COOLDOWN_MS));
    }

    #[test]
    fn a_clock_moved_back_cannot_leave_searches_stuck() {
        let mut state = PaceState {
            last_slot_ms: START * 2,
            blocked_until_ms: START * 2,
            strikes: 1,
        };
        state.sanitize(START);
        assert_eq!(
            state.blocked_for(START),
            Some(Duration::from_millis(MAX_COOLDOWN_MS))
        );
        assert_eq!(state.last_slot_ms, START, "not a long silent wait");
        state.blocked_until_ms = 0;
        assert_eq!(state.claim(START + 1), Ok(START + INTERVAL_MS));
    }

    #[test]
    fn cooldowns_reach_other_processes_through_the_file() {
        let dir = crate::test_support::unique_temp_dir("search-pace");
        assert_eq!(
            Pace::new(&dir).record_block(),
            Duration::from_millis(BASE_COOLDOWN_MS)
        );
        // A second `Pace` reads only the file, as another process would.
        assert!(matches!(
            Pace::new(&dir).wait_for_slot(),
            Err(BackendError::Paused(_))
        ));
        let saved = fs::read_to_string(dir.join("duckduckgo-pace.json")).expect("state file");
        assert!(saved.contains("blocked_until_ms"), "{saved}");
    }

    #[test]
    fn a_success_during_a_cooldown_does_not_forget_the_strike() {
        let dir = crate::test_support::unique_temp_dir("search-pace-inflight");
        let pace = Pace::new(&dir);
        pace.record_block();
        pace.record_success();
        pace.update(|state, now| {
            assert_eq!(state.strikes, 1);
            // Once the cooldown is over, the next bot check doubles it.
            state.blocked_until_ms = now - 1;
        });
        assert_eq!(
            pace.record_block(),
            Duration::from_millis(BASE_COOLDOWN_MS * 2)
        );
        pace.update(|state, now| state.blocked_until_ms = now - 1);
        pace.record_success();
        pace.update(|state, _| assert_eq!(state.strikes, 0));
    }

    #[test]
    fn a_cooldown_survives_the_file_being_locked_when_it_was_recorded() {
        let dir = crate::test_support::unique_temp_dir("search-pace-locked");
        let pace = Pace::new(&dir);
        pace.update(|_, _| {});
        let holder = File::open(dir.join("duckduckgo-pace.json")).expect("state file");
        holder.try_lock().expect("hold the lock");
        pace.record_block();
        holder.unlock().expect("release the lock");
        assert!(matches!(pace.wait_for_slot(), Err(BackendError::Paused(_))));
    }

    #[test]
    fn writes_never_leave_an_empty_file_behind() {
        let dir = crate::test_support::unique_temp_dir("search-pace-write");
        let pace = Pace::new(&dir);
        pace.record_block();
        let first = fs::read_to_string(dir.join("duckduckgo-pace.json")).expect("state file");
        pace.update(|state, _| state.strikes = 0);
        let second = fs::read_to_string(dir.join("duckduckgo-pace.json")).expect("state file");
        assert_ne!(first, second);
        let parsed: PaceState = serde_json::from_str(&second).expect("whole JSON, no leftovers");
        assert_eq!(parsed.strikes, 0);
    }

    #[test]
    fn a_slot_nobody_used_is_given_back() {
        let dir = crate::test_support::unique_temp_dir("search-pace-release");
        let pace = Pace::new(&dir);
        let first = pace.update(|state, now| state.claim(now)).expect("slot");
        let second = pace.update(|state, now| state.claim(now)).expect("slot");
        pace.release(second);
        pace.update(|state, _| assert_eq!(state.last_slot_ms, first));
        // One that a later claim moved past stays.
        let third = pace.update(|state, now| state.claim(now)).expect("slot");
        pace.release(first);
        pace.update(|state, _| assert_eq!(state.last_slot_ms, third));
    }

    #[test]
    fn a_corrupt_state_file_starts_over() {
        let dir = crate::test_support::unique_temp_dir("search-pace-corrupt");
        fs::create_dir_all(&dir).expect("directory");
        fs::write(dir.join("duckduckgo-pace.json"), "not json").expect("state file");
        assert!(Pace::new(&dir).wait_for_slot().is_ok());
    }

    #[test]
    fn an_unusable_state_directory_still_paces_this_process() {
        let dir = crate::test_support::unique_temp_dir("search-pace-blocked");
        fs::create_dir_all(&dir).expect("directory");
        let not_a_directory = dir.join("file");
        fs::write(&not_a_directory, "").expect("file");
        let pace = Pace::new(&not_a_directory);
        assert!(pace.open_locked().is_none());
        pace.update(|state, now| state.last_slot_ms = now);
        let reserved = pace.update(|state, _| state.last_slot_ms);
        assert!(reserved > 0);
    }
}
