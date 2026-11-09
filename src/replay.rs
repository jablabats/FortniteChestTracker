use std::{
    collections::{HashMap, HashSet},
    env, fs,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender, TryRecvError},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use serde::{Deserialize, Serialize};
use tempfile::tempfile;

use crate::{
    capture::Event,
    stats::{ReplayCount, ReplayTotals, save_json},
};

const UNIX_EPOCH_TICKS: u64 = 621_355_968_000_000_000;
const MAX_REPLAY_BYTES: u64 = 256 * 1024 * 1024;
pub const REBUILD_LIMIT: usize = 100;

type CompletedReplay = (PathBuf, Header, (u64, SystemTime));

#[derive(Deserialize, Serialize)]
pub struct Treasure {
    seconds: Option<f64>,
    #[serde(default)]
    opening_observed: bool,
    position: Option<[f64; 3]>,
    #[serde(default)]
    inferred_position: bool,
    source: String,
    opener: String,
    loot: Vec<Loot>,
}

#[derive(Deserialize, Serialize)]
struct Loot {
    item: String,
    count: Option<u32>,
    picked_up_by: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct ReplayMatch {
    id: u64,
    name: String,
    recorded_ms: u64,
    duration_ms: u32,
    #[serde(default)]
    verified_chests: Option<u64>,
    #[serde(default)]
    ammo_crates: Option<u64>,
    #[serde(default)]
    decoder_version: u8,
    treasures: Vec<Treasure>,
}

#[derive(Default, Serialize)]
pub struct History {
    status: String,
    matches: Vec<ReplayMatch>,
}

pub type SharedHistory = Arc<Mutex<History>>;

#[derive(Clone, Copy)]
struct Header {
    id: u64,
    duration_ms: u32,
    live: bool,
}

pub fn replay_time(id: u64) -> u64 {
    id.saturating_sub(UNIX_EPOCH_TICKS) / 10_000_000
}

fn file_time_id(time: SystemTime) -> u64 {
    UNIX_EPOCH_TICKS
        + (time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            / 100) as u64
}

fn limit_rebuild(completed: &mut Vec<CompletedReplay>, unreadable: Vec<u64>) -> u64 {
    let mut order: Vec<_> = completed
        .iter()
        .enumerate()
        .map(|(index, (_, info, _))| (info.id, Some(index)))
        .chain(unreadable.into_iter().map(|time| (time, None)))
        .collect();
    order.sort_unstable_by_key(|&(time, index)| (std::cmp::Reverse(time), index));
    order.truncate(REBUILD_LIMIT);
    let scanned = order.len();
    let selected: HashSet<_> = order.into_iter().filter_map(|(_, index)| index).collect();
    let mut index = 0;
    completed.retain(|_| {
        let keep = selected.contains(&index);
        index += 1;
        keep
    });
    (scanned - selected.len()) as u64
}

fn header(mut file: impl Read + Seek) -> io::Result<Header> {
    fn word(file: &mut impl Read) -> io::Result<u32> {
        let mut bytes = [0; 4];
        file.read_exact(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Unsupported Fortnite replay header",
        )
    };
    if word(&mut file)? != 0x1ca2_e27f {
        return Err(invalid());
    }
    let version = word(&mut file)?;
    if !(3..=32).contains(&version) {
        return Err(invalid());
    }
    if version >= 7 {
        let custom_versions = word(&mut file)?;
        if custom_versions > 1024 {
            return Err(invalid());
        }
        file.seek(SeekFrom::Current(i64::from(custom_versions) * 20))?;
    }
    let duration_ms = word(&mut file)?;
    file.seek(SeekFrom::Current(8))?; // Network version and changelist.
    let name_size = word(&mut file)? as i32;
    let bytes = if name_size < 0 {
        -i64::from(name_size) * 2
    } else {
        i64::from(name_size)
    };
    if bytes > 16_384 {
        return Err(invalid());
    }
    file.seek(SeekFrom::Current(bytes))?;
    let live = match word(&mut file)? {
        0 => false,
        1 => true,
        _ => return Err(invalid()),
    };
    let mut timestamp = [0; 8];
    file.read_exact(&mut timestamp)?;
    let id = u64::from_le_bytes(timestamp);
    if id <= UNIX_EPOCH_TICKS {
        return Err(invalid());
    }
    Ok(Header {
        id,
        duration_ms,
        live,
    })
}

#[derive(Deserialize)]
struct Report {
    #[serde(default)]
    treasures: Vec<Treasure>,
    error: Option<String>,
    verified_chests: Option<u64>,
    ammo_crates: Option<u64>,
}

fn decoder() -> Option<PathBuf> {
    env::current_exe()
        .ok()?
        .ancestors()
        .map(|parent| parent.join("replay-tools"))
        .find(|dir| {
            dir.join("bin/ReplayDecoder.exe").is_file()
                || dir
                    .join("node_modules/fortnite-replay-parser/package.json")
                    .is_file()
        })
}

fn decode(directory: &Path, replay: &Path) -> io::Result<Report> {
    let native = directory.join("bin/ReplayDecoder.exe");
    if native.is_file() {
        let mut command = Command::new(native);
        match run_decoder(&mut command, replay) {
            Ok(report) => return Ok(report),
            Err(native_error) => {
                if !directory
                    .join("node_modules/fortnite-replay-parser/package.json")
                    .is_file()
                {
                    return Err(native_error);
                }
                let mut command = Command::new("node.exe");
                command
                    .arg("--max-old-space-size=512")
                    .arg(directory.join("read.cjs"));
                return run_decoder(&mut command, replay).map_err(|_| native_error);
            }
        }
    }
    let mut command = Command::new("node.exe");
    command
        .arg("--max-old-space-size=512")
        .arg(directory.join("read.cjs"));
    run_decoder(&mut command, replay)
}

fn run_decoder(command: &mut Command, replay: &Path) -> io::Result<Report> {
    let mut output = tempfile()?;
    let mut process = command
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .arg(replay)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = process.try_wait()? {
            if !status.success() {
                return Err(io::Error::other("Replay decoder stopped"));
            }
            break;
        }
        if start.elapsed() >= Duration::from_secs(45) {
            let _ = process.kill();
            let _ = process.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Replay decoder timed out",
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
    output.rewind()?;
    let report: Report = serde_json::from_reader(output.take(32 * 1024 * 1024))?;
    if let Some(error) = &report.error {
        Err(io::Error::other(error.clone()))
    } else {
        Ok(report)
    }
}

fn personal_count(treasures: &[Treasure]) -> u64 {
    treasures
        .iter()
        .filter(|entry| entry.opener == "You" && entry.opening_observed)
        .count() as u64
}

pub fn watch(
    events: Sender<Event>,
    history: SharedHistory,
    archive_path: PathBuf,
    recording: Option<u64>,
    rebuild_requests: Receiver<()>,
) {
    let Some(folder) = env::var_os("LOCALAPPDATA")
        .map(|local| PathBuf::from(local).join("FortniteGame/Saved/Demos"))
    else {
        history.lock().unwrap().status = "Fortnite replay folder unavailable.".into();
        return;
    };
    watch_folder(
        &folder,
        events,
        history,
        archive_path,
        recording,
        rebuild_requests,
    );
}

fn watch_folder(
    folder: &Path,
    events: Sender<Event>,
    history: SharedHistory,
    archive_path: PathBuf,
    recording: Option<u64>,
    rebuild_requests: Receiver<()>,
) {
    let matches = match File::open(&archive_path) {
        Ok(file) => serde_json::from_reader::<_, Vec<ReplayMatch>>(file).map_err(io::Error::from),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    };
    let mut imported = match matches {
        Ok(matches) => {
            let imported = matches
                .iter()
                .map(|entry| {
                    (
                        entry.id,
                        (
                            entry.decoder_version,
                            ReplayCount {
                                minimum: personal_count(&entry.treasures),
                                verified: entry.verified_chests,
                            },
                        ),
                    )
                })
                .collect::<HashMap<_, _>>();
            history.lock().unwrap().matches = matches;
            imported
        }
        Err(error) => {
            history.lock().unwrap().status =
                format!("Could not load the saved treasure history. {error}");
            return;
        }
    };
    let mut seen_live = recording.into_iter().collect::<HashSet<_>>();
    let mut finished = HashSet::new();
    let mut attempted = HashSet::new();
    let mut previous = HashMap::new();
    let mut failures = HashSet::new();

    loop {
        let rebuild = match rebuild_requests.try_recv() {
            Ok(()) => true,
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => return,
        };
        if rebuild {
            attempted.clear();
        }
        let script = decoder();
        let decoder_version = if script
            .as_ref()
            .is_some_and(|dir| dir.join("bin/ReplayDecoder.exe").is_file())
        {
            2 // Replay totals now include ammo crates.
        } else {
            0
        };
        let files = match fs::read_dir(folder) {
            Ok(files) => files,
            Err(_) => {
                history.lock().unwrap().status =
                    "No Fortnite replay folder yet. Enable Record Replays in Fortnite.".into();
                if rebuild
                    && events
                        .send(Event::ReplayTotals(ReplayTotals::default()))
                        .is_err()
                {
                    return;
                }
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let mut current = HashMap::new();
        let mut live = Vec::new();
        let mut completed = Vec::new();
        let mut unreadable = Vec::new();
        for entry in files.flatten() {
            let path = entry.path();
            if !path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("replay"))
            {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                if rebuild {
                    unreadable.push(UNIX_EPOCH_TICKS);
                }
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let fingerprint = (
                metadata.len(),
                metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            );
            if metadata.len() > MAX_REPLAY_BYTES {
                if rebuild {
                    unreadable.push(file_time_id(fingerprint.1));
                }
                continue;
            }
            let stable = previous.get(&path) == Some(&fingerprint);
            current.insert(path.clone(), fingerprint);
            let Ok(info) = File::open(&path).and_then(header) else {
                if rebuild {
                    unreadable.push(file_time_id(fingerprint.1));
                }
                continue;
            };
            if info.live
                && fingerprint
                    .1
                    .elapsed()
                    .is_ok_and(|age| age <= Duration::from_secs(180))
            {
                live.push(info);
            } else if !info.live && (stable || rebuild) && info.duration_ms > 0 {
                completed.push((path, info, fingerprint));
            } else if rebuild && !info.live {
                unreadable.push(info.id);
            }
        }
        previous = current;
        let skipped = if rebuild {
            limit_rebuild(&mut completed, unreadable)
        } else {
            0
        };
        // Finish a match seen recording before opening the next match's recording.
        completed.sort_by_key(|(_, info, _)| {
            (!seen_live.contains(&info.id), std::cmp::Reverse(info.id))
        });
        let mut next_live = live.into_iter().max_by_key(|info| info.id);
        let mut results = HashMap::new();
        let mut decoded = 0;
        for (path, info, fingerprint) in completed {
            if !rebuild && decoded == 8 && !seen_live.contains(&info.id) {
                break;
            }
            if !seen_live.remove(&info.id)
                && !announce_recording(&mut next_live, &mut seen_live, &events)
            {
                return;
            }
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let count = if let Some((_, count)) =
                imported.get(&info.id).filter(|(version, count)| {
                    *version >= decoder_version && (!rebuild || count.verified.is_some())
                }) {
                Some(*count)
            } else if let Some(script) = script
                .as_deref()
                .filter(|_| attempted.insert((info.id, fingerprint)))
            {
                decoded += 1;
                history.lock().unwrap().status = format!("Reading {name}…");
                match decode(script, &path) {
                    Ok(report) => {
                        let count = ReplayCount {
                            minimum: personal_count(&report.treasures),
                            verified: report.verified_chests,
                        };
                        let mut log = history.lock().unwrap();
                        let old = log
                            .matches
                            .iter()
                            .position(|entry| entry.id == info.id)
                            .map(|index| log.matches.swap_remove(index));
                        log.matches.push(ReplayMatch {
                            id: info.id,
                            name,
                            recorded_ms: (info.id - UNIX_EPOCH_TICKS) / 10_000,
                            duration_ms: info.duration_ms,
                            treasures: report.treasures,
                            verified_chests: report.verified_chests,
                            ammo_crates: report.ammo_crates,
                            decoder_version,
                        });
                        if let Err(error) = save_json(&archive_path, &log.matches) {
                            log.matches.pop();
                            log.matches.extend(old);
                            eprintln!("Could not save treasure history: {error}");
                            failures.insert(info.id);
                            None
                        } else {
                            imported.insert(info.id, (decoder_version, count));
                            failures.remove(&info.id);
                            Some(count)
                        }
                    }
                    Err(error) => {
                        eprintln!("Could not decode {name}: {error}");
                        failures.insert(info.id);
                        imported.get(&info.id).map(|(_, count)| *count)
                    }
                }
            } else {
                None
            };
            if rebuild {
                let verified = results.entry(info.id).or_insert(None);
                *verified = count.and_then(|count| count.verified).or(*verified);
            }
            if finished.insert(info.id)
                && events
                    .send(Event::ReplayFinished(info.id, info.duration_ms, count))
                    .is_err()
            {
                return;
            }
        }
        if !announce_recording(&mut next_live, &mut seen_live, &events) {
            return;
        }
        if rebuild {
            let mut totals = ReplayTotals {
                skipped,
                ..Default::default()
            };
            for (id, count) in results {
                totals.through = Some(totals.through.unwrap_or(0).max(id));
                if let Some(count) = count {
                    totals.chests = totals.chests.saturating_add(count);
                    totals.matches += 1;
                } else {
                    totals.skipped += 1;
                }
            }
            if events.send(Event::ReplayTotals(totals)).is_err() {
                return;
            }
        }
        let mut log = history.lock().unwrap();
        log.status = if script.is_none() {
            "Replay boundaries are enabled. Build the local replay decoder to enable treasure decoding."
                .into()
        } else {
            format!(
                "Watching local Fortnite replays · {} imported · {} unreadable",
                imported.len(),
                failures.len()
            )
        };
        drop(log);
        thread::sleep(Duration::from_secs(2));
    }
}

fn announce_recording(
    next: &mut Option<Header>,
    seen: &mut HashSet<u64>,
    events: &Sender<Event>,
) -> bool {
    next.take().is_none_or(|info| {
        !seen.insert(info.id) || events.send(Event::ReplayStarted(info.id)).is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn rebuild_uses_the_latest_hundred_files_including_unreadable_ones() {
        let mut completed: Vec<_> = (1..=105)
            .map(|id| {
                (
                    PathBuf::new(),
                    Header {
                        id,
                        duration_ms: 1,
                        live: false,
                    },
                    (1, SystemTime::UNIX_EPOCH),
                )
            })
            .collect();
        assert_eq!(limit_rebuild(&mut completed, vec![0, 106]), 1);
        assert_eq!(completed.len(), 99);
        assert_eq!(completed.first().unwrap().1.id, 7);
        assert_eq!(completed.last().unwrap().1.id, 105);
    }

    #[test]
    fn rebuilding_uses_each_verified_local_match_once() {
        let folder = tempfile::tempdir().unwrap();
        let (request, rebuild_requests) = mpsc::channel();
        let (events, response) = mpsc::channel();
        let mut archive = Vec::new();
        for (name, id, count) in [
            ("match.replay", UNIX_EPOCH_TICKS + 10, 4),
            ("duplicate.replay", UNIX_EPOCH_TICKS + 10, 4),
            ("zero.replay", UNIX_EPOCH_TICKS + 20, 0),
        ] {
            let mut bytes: Vec<u8> = [0x1ca2_e27f_u32, 3, 60_000, 0, 0, 0, 0]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect();
            bytes.extend(id.to_le_bytes());
            fs::write(folder.path().join(name), bytes).unwrap();
            archive.push(ReplayMatch {
                id,
                name: name.into(),
                recorded_ms: 0,
                duration_ms: 60_000,
                verified_chests: Some(count),
                ammo_crates: None,
                decoder_version: 2,
                treasures: Vec::new(),
            });
        }
        fs::write(folder.path().join("invalid.replay"), []).unwrap();
        let archive_path = folder.path().join("treasures.json");
        save_json(&archive_path, &archive).unwrap();
        request.send(()).unwrap();
        drop(request);
        let path = folder.path().to_owned();
        let worker = thread::spawn(move || {
            watch_folder(
                &path,
                events,
                SharedHistory::default(),
                archive_path,
                None,
                rebuild_requests,
            );
        });
        let totals = loop {
            if let Event::ReplayTotals(totals) =
                response.recv_timeout(Duration::from_secs(5)).unwrap()
            {
                break totals;
            }
        };
        assert_eq!((totals.chests, totals.matches, totals.skipped), (4, 2, 1));
        worker.join().unwrap();
    }
}
