#![windows_subsystem = "windows"]

mod capture;
mod detection;
mod overlay;
mod pin;
mod replay;
mod stats;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use gpui::{
    App, Application, Bounds, ClipboardItem, Context, Div, FocusHandle, KeyDownEvent, MouseButton,
    PromptButton, PromptLevel, Stateful, TitlebarOptions, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};

use capture::Event;
use stats::{Stats, Store, TrackingOptions};

struct Tracker {
    stats: Arc<Mutex<Stats>>,
    store: Option<Store>,
    storage_error: Option<&'static str>,
    status: &'static str,
    overlay_ready: bool,
    copied: bool,
    amount: String,
    amount_focus: FocusHandle,
    manual_in_average: bool,
    reset_pending: bool,
    rebuilding: bool,
    rebuild_status: Option<String>,
    replay_requests: mpsc::Sender<()>,
    pin: pin::Pin,
    tracking: TrackingOptions,
    live_enabled: Arc<AtomicBool>,
}

impl Tracker {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (store, stats, storage_error) = match Store::open() {
            Ok((store, stats)) => (Some(store), stats, None),
            Err(error) => {
                eprintln!("Could not open saved stats: {error}");
                (
                    None,
                    Stats::default(),
                    Some("Could not open saved stats. Check for another running tracker."),
                )
            }
        };
        let stats = Arc::new(Mutex::new(stats));
        let tracking = store
            .as_ref()
            .map(Store::tracking_options)
            .unwrap_or_default();
        let live_enabled = Arc::new(AtomicBool::new(tracking.live));
        let history = replay::SharedHistory::default();
        let overlay_ready = store.as_ref().is_some_and(|store| {
            overlay::start(
                stats.clone(),
                store.directory().join("overlay.json"),
                history.clone(),
            )
            .inspect_err(|error| eprintln!("Could not start OBS overlay: {error}"))
            .is_ok()
        });
        let (send, receive) = mpsc::channel();
        let (replay_requests, rebuild_requests) = mpsc::channel();
        if let Some(store) = &store {
            let events = send.clone();
            let archive = store.directory().join("treasures.json");
            let recording = stats.lock().unwrap().recording();
            let _ = thread::Builder::new()
                .name("fortnite-replays".into())
                .spawn(move || {
                    replay::watch(events, history, archive, recording, rebuild_requests)
                });
        }
        let enabled = live_enabled.clone();
        let started = thread::Builder::new()
            .name("fortnite-capture".into())
            .spawn(move || capture::watch(send, enabled))
            .is_ok();
        let timer = cx.background_executor().clone();

        cx.spawn(async move |view, cx| {
            loop {
                timer.timer(Duration::from_millis(80)).await;
                match receive.try_recv() {
                    Ok(first) => {
                        let updated = view.update(cx, |tracker, cx| {
                            let previous = tracker.snapshot();
                            let mut next = previous;
                            for event in std::iter::once(first).chain(receive.try_iter()) {
                                match event {
                                    Event::Status(status) => tracker.status = status,
                                    Event::Chest if tracker.tracking.live => next.add_live(),
                                    Event::MatchStarted if tracker.tracking.live => {
                                        next.observe_battle_bus()
                                    }
                                    Event::Chest | Event::MatchStarted => {}
                                    Event::ReplayStarted(id) => next.observe_replay_start(id),
                                    Event::ReplayFinished(id, duration_ms, count) => {
                                        next.observe_replay_finish(id, duration_ms, count);
                                    }
                                    Event::ReplayTotals(totals) => {
                                        tracker.rebuilding = false;
                                        tracker.rebuild_status = None;
                                        if totals.matches == 0 {
                                            tracker.rebuild_status = Some(
                                                "No verified completed replays found. Stats kept."
                                                    .into(),
                                            );
                                        } else {
                                            let rebuilt = next.rebuilt_from_replays(&totals);
                                            if tracker.save(rebuilt) {
                                                next = rebuilt;
                                                tracker.amount.clear();
                                                tracker.rebuild_status = Some(format!(
                                                    "Rebuilt from {} matches · {} skipped · {}-replay limit",
                                                    totals.matches, totals.skipped, replay::REBUILD_LIMIT,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                            if next != tracker.snapshot() {
                                tracker.save(next);
                            }
                            cx.notify();
                        });
                        if updated.is_err() {
                            break;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
        })
        .detach();

        cx.observe_window_activation(window, |tracker, window, cx| {
            if !tracker.reset_pending {
                tracker.pin.update(window, cx);
                cx.notify();
            }
        })
        .detach();
        Self {
            stats,
            store,
            storage_error,
            status: if started {
                "Starting screen capture…"
            } else {
                "Could not start screen capture"
            },
            overlay_ready,
            copied: false,
            amount: String::new(),
            amount_focus: cx.focus_handle(),
            manual_in_average: false,
            reset_pending: false,
            rebuilding: false,
            rebuild_status: None,
            replay_requests,
            pin: pin::Pin::default(),
            tracking,
            live_enabled,
        }
    }

    fn snapshot(&self) -> Stats {
        *self.stats.lock().unwrap()
    }

    fn set_tracking(&mut self, next: TrackingOptions) {
        if let Some(store) = &self.store {
            if let Err(error) = store.save_tracking_options(next) {
                eprintln!("Could not save tracking options: {error}");
                self.storage_error = Some("Could not save tracking options.");
                return;
            }
        }
        self.tracking = next;
        self.live_enabled.store(next.live, Ordering::Relaxed);
        self.storage_error = None;
    }

    fn save(&mut self, next: Stats) -> bool {
        let Some(store) = &self.store else {
            return false;
        };
        if let Err(error) = store.save(next) {
            eprintln!("Could not save stats: {error}");
            self.storage_error = Some("Could not save the update. Check the local stats folder.");
            return false;
        }
        *self.stats.lock().unwrap() = next;
        self.storage_error = None;
        true
    }

    fn add(&mut self, amount: u64) -> bool {
        if self.rebuilding {
            return false;
        }
        let mut next = self.snapshot();
        if !next.add(amount, self.manual_in_average) {
            self.storage_error = Some("This amount would exceed the maximum total.");
            return false;
        }
        self.save(next)
    }

    fn confirm_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.store.is_none() || self.reset_pending || self.rebuilding {
            return;
        }
        self.reset_pending = true;
        let warning = format!(
            "Reset to zero clears your counts. Use replays rebuilds lifetime stats from up to your latest {} completed Fortnite replays.",
            replay::REBUILD_LIMIT,
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            "Reset all chest stats?",
            Some(&warning),
            &[
                PromptButton::cancel("Cancel"),
                PromptButton::ok("Use replays"),
                PromptButton::new("Reset to zero"),
            ],
            cx,
        );
        cx.spawn(async move |view, cx| {
            let choice = answer.await;
            let _ = view.update(cx, |tracker, cx| {
                tracker.reset_pending = false;
                match choice {
                    Ok(1) => {
                        tracker.rebuilding = tracker.replay_requests.send(()).is_ok();
                        tracker.rebuild_status = Some(
                            if tracker.rebuilding {
                                "Rebuilding stats from local replays…"
                            } else {
                                "Could not read local replays. Stats kept."
                            }
                            .into(),
                        );
                    }
                    Ok(2) if tracker.save(Stats::default()) => {
                        tracker.amount.clear();
                        tracker.rebuild_status = None;
                    }
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn amount(&self) -> Option<u64> {
        self.amount.parse().ok().filter(|amount| *amount > 0)
    }

    fn add_amount(&mut self) {
        if let Some(amount) = self.amount() {
            if self.add(amount) {
                self.amount.clear();
            }
        }
    }

    fn amount_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        if key.modifiers.control {
            match key.key.as_str() {
                "a" => self.amount.clear(),
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        let text = text.trim();
                        if text.bytes().all(|byte| byte.is_ascii_digit())
                            && text.parse::<u64>().is_ok()
                        {
                            self.amount = text.into();
                        }
                    }
                }
                _ => return,
            }
        } else {
            match key.key.as_str() {
                "backspace" => {
                    self.amount.pop();
                }
                "delete" | "escape" => self.amount.clear(),
                "enter" => self.add_amount(),
                _ => {
                    let text = key.key_char.as_deref().unwrap_or(&key.key);
                    if text.len() != 1
                        || !text.as_bytes()[0].is_ascii_digit()
                        || self.amount.len() >= 20
                    {
                        return;
                    }
                    self.amount.push_str(text);
                }
            }
        }
        cx.notify();
    }
}

impl Render for Tracker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let stats = self.snapshot();
        if self.pin.compact() {
            return div()
                .size_full()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .bg(rgb(0x101722))
                .text_color(rgb(0xf6f8fc))
                .child(stat("This match", stats.current.to_string()))
                .child(stat("All time", stats.all_time.to_string()))
                .child(stat("Avg. per match", stats.average_label()));
        }
        let muted = rgb(0xa5afc0);
        let focus = self.amount_focus.is_focused(window);
        let amount_text = if self.amount.is_empty() {
            "Amount".into()
        } else {
            self.amount.clone()
        };
        let can_edit = self.store.is_some() && !self.rebuilding;
        let content = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(if self.tracking.live {
                        self.status
                    } else {
                        "Live tracking off · Checking saved replays"
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(stat("This match", stats.current.to_string()))
                    .child(stat("All time", stats.all_time.to_string()))
                    .child(stat("Avg. per match", stats.average_label())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        button("increment", "+1 chest", can_edit).on_click(cx.listener(
                            |tracker, _, _, cx| {
                                tracker.add(1);
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        div()
                            .id("amount")
                            .track_focus(&self.amount_focus)
                            .min_w(px(92.0))
                            .max_w(px(180.0))
                            .overflow_hidden()
                            .border_1()
                            .border_color(if focus { rgb(0xffd464) } else { rgb(0x334054) })
                            .rounded_md()
                            .px_3()
                            .py_2()
                            .text_sm()
                            .text_color(if self.amount.is_empty() {
                                muted
                            } else {
                                rgb(0xf6f8fc)
                            })
                            .cursor_text()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|tracker, _, window, cx| {
                                    tracker.amount_focus.focus(window);
                                    cx.notify();
                                }),
                            )
                            .on_key_down(cx.listener(Tracker::amount_key))
                            .child(amount_text),
                    )
                    .child(
                        button(
                            "add-amount",
                            "Add amount",
                            can_edit && self.amount().is_some(),
                        )
                        .on_click(cx.listener(|tracker, _, _, cx| {
                            tracker.add_amount();
                            cx.notify();
                        })),
                    )
                    .child(
                        button("new-match", "New match", can_edit).on_click(cx.listener(
                            |tracker, _, _, cx| {
                                if tracker.rebuilding {
                                    return;
                                }
                                let mut next = tracker.snapshot();
                                next.start_match();
                                tracker.save(next);
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .id("include-manual")
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_sm()
                            .text_color(muted)
                            .cursor_pointer()
                            .on_click(cx.listener(|tracker, _, _, cx| {
                                tracker.manual_in_average = !tracker.manual_in_average;
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size_4()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(if self.manual_in_average {
                                        rgb(0xffd464)
                                    } else {
                                        rgb(0x66768c)
                                    })
                                    .text_color(rgb(0xffd464))
                                    .child(if self.manual_in_average { "✓" } else { "" }),
                            )
                            .child("Include manual additions in the average"),
                    )
                    .when(can_edit, |row| {
                        row.child(
                            button("reset-all", "Reset all stats", true)
                                .text_color(rgb(0xffb6ae))
                                .on_click(cx.listener(|tracker, _, window, cx| {
                                    tracker.confirm_reset(window, cx);
                                })),
                        )
                    }),
            )
            .when_some(self.storage_error, |view, error| {
                view.child(div().text_sm().text_color(rgb(0xffbd7a)).child(error))
            })
            .when_some(self.rebuild_status.clone(), |view, status| {
                view.child(div().text_sm().text_color(muted).child(status))
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .border_t_1()
                    .border_color(rgb(0x334054))
                    .pt_3()
                    .when(self.overlay_ready, |area| {
                        area.child(div().text_sm().text_color(muted).child(overlay::URL))
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        button(
                                            "copy",
                                            if self.copied {
                                                "Copied"
                                            } else {
                                                "Copy OBS URL"
                                            },
                                            true,
                                        )
                                        .on_click(
                                            cx.listener(|tracker, _, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    overlay::URL.into(),
                                                ));
                                                tracker.copied = true;
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .child(
                                        button("edit-overlay", "Edit OBS overlay", true)
                                            .on_click(|_, _, cx| cx.open_url(overlay::EDITOR_URL)),
                                    )
                                    .child(
                                        button("treasures", "Treasure history", true).on_click(
                                            |_, _, cx| cx.open_url(overlay::TREASURES_URL),
                                        ),
                                    ),
                            )
                    })
                    .when(!self.overlay_ready && can_edit, |area| {
                        area.child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child("OBS overlay unavailable · Check port 17333 and restart"),
                        )
                    }),
            );

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x101722))
            .text_color(rgb(0xf6f8fc))
            .child(
                div()
                    .id("tracker-content")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(content),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap_2()
                    .px_5()
                    .py_3()
                    .border_t_1()
                    .border_color(rgb(0x334054))
                    .child(
                        button(
                            "pin",
                            if self.pin.enabled {
                                "Pin on top: On"
                            } else {
                                "Pin on top: Off"
                            },
                            true,
                        )
                        .on_click(cx.listener(|tracker, _, window, cx| {
                            tracker.pin.enabled = !tracker.pin.enabled;
                            tracker.pin.update(window, cx);
                            cx.notify();
                        })),
                    )
                    .child(
                        button(
                            "live",
                            if self.tracking.live {
                                "Live tracking: On"
                            } else {
                                "Live tracking: Off"
                            },
                            true,
                        )
                        .on_click(cx.listener(|tracker, _, _, cx| {
                            tracker.set_tracking(TrackingOptions {
                                live: !tracker.tracking.live,
                            });
                            cx.notify();
                        })),
                    ),
            )
    }
}

fn stat(label: &'static str, value: String) -> Div {
    div()
        .flex()
        .flex_1()
        .min_w_0()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_md()
        .bg(rgb(0x1c2635))
        .child(div().text_sm().text_color(rgb(0xa5afc0)).child(label))
        .child(
            div()
                .truncate()
                .text_2xl()
                .text_color(rgb(0xffd464))
                .child(value),
        )
}

fn button(id: &'static str, label: &'static str, enabled: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(0x263246))
        .text_sm()
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.bg(rgb(0x3a4a64)))
        })
        .when(!enabled, |button| button.text_color(rgb(0x8792a4)))
        .child(label)
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(540.0), px(500.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Fortnite Chest Tracker".into()),
                    ..Default::default()
                }),
                is_resizable: false,
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Tracker::new(window, cx)),
        )
        .expect("Could not open the tracker window");

        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
