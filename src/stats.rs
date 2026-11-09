use std::{
    env, fs,
    fs::{File, OpenOptions},
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
struct PendingMatch {
    replay: Option<u64>,
    bus: Option<u64>,
    current: u64,
    live: Option<u64>,
}

#[derive(Clone, Copy)]
pub struct ReplayCount {
    pub minimum: u64,
    pub verified: Option<u64>,
}

#[derive(Default)]
pub struct ReplayTotals {
    pub chests: u64,
    pub matches: u64,
    pub skipped: u64,
    pub through: Option<u64>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(default)]
pub struct TrackingOptions {
    pub live: bool,
}

impl Default for TrackingOptions {
    fn default() -> Self {
        Self { live: true }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Stats {
    pub current: u64,
    pub all_time: u64,
    pub completed_matches: u64,
    #[serde(default)]
    excluded_current: u64,
    #[serde(default)]
    excluded_all_time: u64,
    match_active: bool,
    last_bus_start: Option<u64>,
    #[serde(default)]
    active_replay: Option<u64>,
    #[serde(default)]
    last_replay: Option<u64>,
    #[serde(default)]
    live_current: Option<u64>,
    #[serde(default)]
    pending: [Option<PendingMatch>; 8],
    #[serde(default)]
    replay_checked: [Option<u64>; 8],
}

impl Stats {
    pub fn average(self) -> Option<f64> {
        (self.completed_matches > 0).then(|| {
            let excluded = self.excluded_all_time - self.excluded_current;
            (self.all_time - self.current - excluded) as f64 / self.completed_matches as f64
        })
    }

    pub fn average_label(self) -> String {
        self.average()
            .map_or_else(|| "—".into(), |average| format!("{average:.1}"))
    }

    pub fn recording(self) -> Option<u64> {
        self.active_replay
    }

    pub fn rebuilt_from_replays(self, totals: &ReplayTotals) -> Self {
        let active_replay = self
            .active_replay
            .filter(|id| totals.through.is_none_or(|last| *id > last));
        Self {
            all_time: totals.chests,
            completed_matches: totals.matches,
            last_replay: active_replay.or(totals.through),
            match_active: active_replay.is_some(),
            active_replay,
            live_current: Some(0),
            ..Self::default()
        }
    }

    pub fn add(&mut self, amount: u64, include_in_average: bool) -> bool {
        let Some(total) = self.all_time.checked_add(amount) else {
            return false;
        };
        self.all_time = total;
        self.current += amount;
        if include_in_average {
            self.match_active = true;
        } else {
            self.excluded_current += amount;
            self.excluded_all_time += amount;
        }
        true
    }

    pub fn add_live(&mut self) {
        if self.current == 0 {
            self.live_current = Some(0);
        }
        if self.add(1, true) {
            if let Some(live) = &mut self.live_current {
                *live += 1;
            }
        }
    }

    pub fn start_match(&mut self) {
        if self.match_active && (self.active_replay.is_some() || self.last_bus_start.is_some()) {
            self.pending.rotate_right(1);
            self.pending[0] = Some(PendingMatch {
                replay: self.active_replay,
                bus: self.last_bus_start,
                current: self.current,
                live: self.live_current,
            });
        }
        self.close_match();
        self.match_active = true;
    }

    fn close_match(&mut self) {
        if self.match_active {
            self.completed_matches = self.completed_matches.saturating_add(1);
        }
        self.current = 0;
        self.excluded_current = 0;
        self.live_current = Some(0);
        self.match_active = false;
        self.active_replay = None;
        self.last_bus_start = None;
    }

    pub fn observe_battle_bus(&mut self) {
        let now = unix_now();
        // Reopening the tracker on the same bus should keep the current match.
        if self
            .last_bus_start
            .is_some_and(|last| (last..last.saturating_add(40)).contains(&now))
        {
            return;
        }
        let replay_just_started = self
            .active_replay
            .is_some_and(|id| now.abs_diff(crate::replay::replay_time(id)) <= 120);
        if !replay_just_started {
            self.start_match();
        }
        self.last_bus_start = Some(now);
    }

    pub fn observe_replay_start(&mut self, id: u64) {
        if self.last_replay.is_some_and(|last| id <= last) {
            return;
        }
        let started = crate::replay::replay_time(id);
        let same_match = self.active_replay.is_none()
            && self.match_active
            && self
                .last_bus_start
                .is_some_and(|bus| (started.saturating_sub(10)..=started + 120).contains(&bus));
        if !same_match {
            self.start_match();
        }
        self.active_replay = Some(id);
        self.last_replay = Some(id);
    }

    pub fn observe_replay_finish(&mut self, id: u64, duration_ms: u32, count: Option<ReplayCount>) {
        if self.replay_checked.contains(&Some(id)) {
            return;
        }
        let started = crate::replay::replay_time(id);
        let ended = started + u64::from(duration_ms) / 1000;
        let bus_matches = |bus: u64| {
            (started.saturating_sub(10)..=started + 120).contains(&bus)
                && bus.saturating_add(5) <= ended
        };
        let previous = self
            .pending
            .iter()
            .position(|entry| entry.is_some_and(|entry| entry.replay == Some(id)))
            .or_else(|| {
                let mut candidates = self.pending.iter().enumerate().filter(|(_, entry)| {
                    entry.is_some_and(|entry| {
                        entry.replay.is_none() && entry.bus.is_some_and(bus_matches)
                    })
                });
                let (index, _) = candidates.next()?;
                candidates.next().is_none().then_some(index)
            });
        let finalized_only = self.active_replay.is_none()
            && self.match_active
            && self.last_replay.is_none_or(|last| id > last)
            && unix_now().abs_diff(ended) <= 300
            && self.last_bus_start.is_some_and(bus_matches);
        if self.active_replay == Some(id) || (finalized_only && previous.is_none()) {
            self.reconcile_current(count);
            let finished_count = self.current;
            if count.is_none() {
                self.pending.rotate_right(1);
                self.pending[0] = Some(PendingMatch {
                    replay: Some(id),
                    bus: self.last_bus_start,
                    current: finished_count,
                    live: self.live_current,
                });
            }
            self.close_match();
        } else if let Some(index) = previous {
            if count.is_none() {
                return;
            }
            let entry = self.pending[index].take().unwrap();
            self.adjust_total(entry.current, entry.live, count);
        } else if self.active_replay.is_none()
            && !self.match_active
            && self.last_replay.is_none_or(|last| id > last)
            && unix_now().abs_diff(ended) <= 300
            && let Some(verified) = count.and_then(|count| count.verified)
        {
            self.match_active = true;
            self.reconcile_current(Some(ReplayCount {
                minimum: verified,
                verified: Some(verified),
            }));
            self.close_match();
        } else {
            return;
        }
        self.last_replay = Some(self.last_replay.unwrap_or(0).max(id));
        if count.is_some() {
            self.replay_checked.rotate_right(1);
            self.replay_checked[0] = Some(id);
        }
    }

    fn reconcile_current(&mut self, count: Option<ReplayCount>) {
        let current = self.current;
        self.current = self.adjust_total(current, self.live_current, count);
    }

    fn adjust_total(
        &mut self,
        previous: u64,
        live: Option<u64>,
        count: Option<ReplayCount>,
    ) -> u64 {
        let corrected = count.map_or(previous, |count| {
            count.verified.zip(live).map_or_else(
                || previous.max(count.minimum),
                |(verified, live)| previous.saturating_sub(live).saturating_add(verified),
            )
        });
        if let Some(total) = self
            .all_time
            .checked_sub(previous)
            .and_then(|total| total.checked_add(corrected))
        {
            self.all_time = total;
            corrected
        } else {
            previous
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub struct Store {
    directory: PathBuf,
    _lock: File,
}

impl Store {
    pub fn open() -> io::Result<(Self, Stats)> {
        let local = env::var_os("LOCALAPPDATA")
            .ok_or_else(|| io::Error::other("LOCALAPPDATA is unavailable"))?;
        Self::open_in(PathBuf::from(local).join("FortniteChestTracker"))
    }

    fn open_in(directory: PathBuf) -> io::Result<(Self, Stats)> {
        fs::create_dir_all(&directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("stats.lock"))?;
        lock.try_lock().map_err(io::Error::from)?;
        let stats: Stats = match File::open(directory.join("stats.json")) {
            Ok(file) => serde_json::from_reader(file)?,
            Err(error) if error.kind() == ErrorKind::NotFound => Stats::default(),
            Err(error) => return Err(error),
        };
        if stats.current > stats.all_time
            || (!stats.match_active && stats.current != stats.excluded_current)
            || stats.excluded_current > stats.current
            || stats
                .live_current
                .is_some_and(|live| live > stats.current - stats.excluded_current)
            || stats.excluded_all_time < stats.excluded_current
            || stats.excluded_all_time - stats.excluded_current > stats.all_time - stats.current
        {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "Invalid chest totals",
            ));
        }
        Ok((
            Self {
                directory,
                _lock: lock,
            },
            stats,
        ))
    }

    pub fn save(&self, stats: Stats) -> io::Result<()> {
        save_json(&self.directory.join("stats.json"), &stats)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn tracking_options(&self) -> TrackingOptions {
        File::open(self.directory.join("tracking.json"))
            .ok()
            .and_then(|file| serde_json::from_reader(file).ok())
            .unwrap_or_default()
    }

    pub fn save_tracking_options(&self, options: TrackingOptions) -> io::Result<()> {
        save_json(&self.directory.join("tracking.json"), &options)
    }
}

pub fn save_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("Storage directory is unavailable"))?;
    let file = NamedTempFile::new_in(directory)?;
    serde_json::to_writer(file.as_file(), value)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_matches_keep_the_total_and_include_zero_chest_matches() {
        let mut stats = Stats::default();
        stats.start_match();
        assert_eq!(stats.average(), None);
        assert!(stats.add(6, true));
        stats.start_match();
        assert_eq!(
            (stats.current, stats.all_time, stats.average()),
            (0, 6, Some(6.0))
        );
        stats.start_match();
        assert!(stats.add(2, true));
        assert_eq!(
            (stats.current, stats.all_time, stats.average()),
            (2, 8, Some(3.0))
        );
    }

    #[test]
    fn stats_survive_restart_and_one_instance_owns_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        let (store, mut stats) = Store::open_in(path.clone()).unwrap();
        assert!(Store::open_in(path.clone()).is_err());
        stats.observe_battle_bus();
        assert!(stats.add(12, true));
        assert!(stats.add(3, false));
        store.save(stats).unwrap();
        drop(store);

        let (store, mut loaded) = Store::open_in(path.clone()).unwrap();
        assert_eq!(loaded, stats);
        loaded.observe_battle_bus();
        assert_eq!(loaded, stats);
        loaded.start_match();
        store.save(loaded).unwrap();
        drop(store);
        assert_eq!(Store::open_in(path).unwrap().1, loaded);
    }

    #[test]
    fn excluded_additions_only_change_the_displayed_counts() {
        let mut stats: Stats = serde_json::from_str(
            r#"{"current":4,"all_time":10,"completed_matches":1,"match_active":true,"last_bus_start":null}"#,
        )
        .unwrap();
        assert_eq!(stats.average(), Some(6.0));
        assert!(stats.add(5, false));
        assert!(stats.add(2, true));
        assert_eq!((stats.current, stats.all_time), (11, 17));
        stats.start_match();
        assert_eq!(
            (stats.current, stats.all_time, stats.average()),
            (0, 17, Some(6.0))
        );
    }

    #[test]
    fn excluded_additions_before_a_match_do_not_create_a_completed_match() {
        let mut stats = Stats::default();
        assert!(stats.add(3, false));
        stats.start_match();
        assert_eq!(
            (stats.current, stats.all_time, stats.average()),
            (0, 3, None)
        );
        assert!(stats.add(8, true));
        stats.start_match();
        assert_eq!(stats.average(), Some(8.0));
    }

    #[test]
    fn recorded_match_finishes_once_and_only_adds_missing_chests() {
        let mut stats = Stats::default();
        let id = 621_355_968_000_000_000 + (unix_now() - 60) * 10_000_000;
        stats.observe_replay_start(id);
        assert!(stats.add(3, true));
        assert!(stats.add(1, false));
        stats.observe_battle_bus();
        assert_eq!(stats.current, 4);
        let count = ReplayCount {
            minimum: 6,
            verified: None,
        };
        stats.observe_replay_finish(id, 60_000, Some(count));
        assert_eq!(
            (stats.current, stats.all_time, stats.average()),
            (0, 6, Some(5.0))
        );
        stats.observe_replay_start(id);
        stats.observe_replay_finish(id, 60_000, Some(count));
        assert_eq!(stats.completed_matches, 1);
    }

    #[test]
    fn a_late_verified_replay_corrects_live_counts_without_reopening_the_match() {
        let mut stats = Stats::default();
        let id = 621_355_968_000_000_000 + (unix_now() - 60) * 10_000_000;
        stats.observe_replay_start(id);
        stats.add_live();
        stats.add_live();
        assert!(stats.add(1, false));
        stats.start_match();
        stats.add_live();
        let report = ReplayCount {
            minimum: 1,
            verified: Some(1),
        };
        stats.observe_replay_finish(id, 60_000, Some(report));
        assert_eq!(
            (stats.current, stats.all_time, stats.average()),
            (1, 3, Some(1.0))
        );
    }

    #[test]
    fn rebuilding_replaces_totals_and_does_not_count_the_same_replay_twice() {
        let mut stats = Stats::default();
        let id = 621_355_968_000_000_000 + (unix_now() - 60) * 10_000_000;
        assert!(stats.add(50, false));
        let mut stats = stats.rebuilt_from_replays(&ReplayTotals {
            chests: 6,
            matches: 3,
            skipped: 1,
            through: Some(id),
        });
        let report = ReplayCount {
            minimum: 1,
            verified: Some(1),
        };
        stats.observe_replay_start(id);
        stats.observe_replay_finish(id, 60_000, Some(report));
        assert_eq!(
            (stats.current, stats.all_time, stats.average()),
            (0, 6, Some(2.0))
        );
    }
}
