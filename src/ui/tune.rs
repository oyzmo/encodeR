//! "Tune in mpv" for the lip-sync popover: at most one mpv window, whose
//! delay changes flow into the track's offset and every live spin button
//! for that track, and back to mpv when a spin button is used.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::mpv;
use crate::window::App;

struct Tune {
    serial: u64,
    job: u64,
    index: usize,
    session: mpv::Session,
}

/// The widgets of one offset popover, so mpv events can reach it.
struct Widgets {
    job: u64,
    index: usize,
    spin: glib::WeakRef<gtk::SpinButton>,
    button: glib::WeakRef<gtk::Button>,
}

thread_local! {
    static TUNE: RefCell<Option<Tune>> = const { RefCell::new(None) };
    static WIDGETS: RefCell<Vec<Widgets>> = const { RefCell::new(Vec::new()) };
    static SERIAL: Cell<u64> = const { Cell::new(0) };
}

fn tuning(job: u64, index: usize) -> bool {
    TUNE.with_borrow(|t| t.as_ref().is_some_and(|t| t.job == job && t.index == index))
}

/// Tracks a popover's spin and tune button (dead ones are pruned).
pub fn register(job: u64, index: usize, spin: &gtk::SpinButton, button: &gtk::Button) {
    WIDGETS.with_borrow_mut(|w| {
        w.retain(|w| w.spin.upgrade().is_some());
        w.push(Widgets {
            job,
            index,
            spin: spin.downgrade(),
            button: button.downgrade(),
        });
    });
    refresh_buttons();
}

/// A spin button changed: move mpv along if it's tuning that track.
pub fn spin_changed(job: u64, index: usize, ms: i64) {
    TUNE.with_borrow(|t| {
        if let Some(t) = t.as_ref().filter(|t| t.job == job && t.index == index) {
            t.session.set_delay(ms);
        }
    });
}

/// Closes mpv if it's tuning `job` (any job for `None`).
pub fn stop(job: Option<u64>) {
    let old = TUNE.with_borrow_mut(|t| {
        if t.as_ref().is_some_and(|t| job.is_none_or(|j| j == t.job)) {
            t.take()
        } else {
            None
        }
    });
    if old.is_some() {
        drop(old);
        refresh_buttons();
    }
}

/// The tune button: opens mpv on the track, or closes it when already open.
pub fn toggle(app: &Rc<App>, job: u64, index: usize) {
    if tuning(job, index) {
        stop(None);
        return;
    }
    let (input, aid, offset) = {
        let st = app.state.borrow();
        let Some(j) = st.job(job).filter(|j| j.state.editable()) else {
            return;
        };
        let Some(aid) = j
            .probe
            .as_ref()
            .and_then(|p| p.audio.iter().position(|a| a.index == index))
        else {
            return;
        };
        let offset = j
            .selection
            .audio
            .iter()
            .find(|c| c.index == index)
            .map_or(0, |c| c.offset_ms);
        (j.input.clone(), aid + 1, offset)
    };
    stop(None);
    let (tx, rx) = async_channel::unbounded();
    let session = match mpv::Session::start(&input, aid, offset, tx) {
        Ok(s) => s,
        Err(e) => {
            app.toast(&format!("Couldn't start mpv: {e}"));
            return;
        }
    };
    let serial = SERIAL.get() + 1;
    SERIAL.set(serial);
    TUNE.set(Some(Tune {
        serial,
        job,
        index,
        session,
    }));
    refresh_buttons();
    let app = app.clone();
    glib::spawn_future_local(async move {
        while let Ok(ev) = rx.recv().await {
            match ev {
                mpv::Event::Delay(ms) => apply(&app, job, index, ms),
                mpv::Event::Closed => break,
            }
        }
        // mpv was closed by the user (or by us): forget it if it's still ours.
        let ours = TUNE.with_borrow(|t| t.as_ref().is_some_and(|t| t.serial == serial));
        if ours {
            stop(None);
            // The timing rows' subtitles show the final delay.
            app.rebuild_editor();
        }
    });
}

/// mpv's delay changed: set it on the live spin buttons, which store it on
/// the job; with none open, store it directly.
fn apply(app: &Rc<App>, job: u64, index: usize, ms: i64) {
    let spins: Vec<gtk::SpinButton> = WIDGETS.with_borrow(|w| {
        w.iter()
            .filter(|w| w.job == job && w.index == index)
            .filter_map(|w| w.spin.upgrade())
            .collect()
    });
    if spins.is_empty() {
        app.edit(job, |j| {
            if let Some(c) = j.selection.audio.iter_mut().find(|c| c.index == index) {
                c.offset_ms = ms;
            }
        });
    }
    for s in spins {
        if s.value().round() as i64 != ms {
            s.set_value(ms as f64);
        }
    }
}

fn refresh_buttons() {
    WIDGETS.with_borrow(|w| {
        for w in w {
            if let Some(b) = w.button.upgrade() {
                b.set_label(if tuning(w.job, w.index) {
                    "Close mpv"
                } else {
                    "Tune in mpv"
                });
            }
        }
    });
}
