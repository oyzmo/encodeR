//! Editor groups for external subtitles and the job mode.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::help;
use crate::queue::Job;
use crate::subs::{ExtKind, ExternalSub, JobMode, SubsTarget};
use crate::tracks;
use crate::ui::widgets::{combo_row, spin_row, switch_row};
use crate::window::App;

/// "Re-encode or remux" vs "Subtitles only", and where subtitles-only jobs
/// put the files.
pub fn mode_group(app: &Rc<App>, id: u64, job: &Job) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::new();
    let modes = vec![
        "Re-encode or remux".to_string(),
        "Add subtitles only".to_string(),
    ];
    let row = combo_row(
        "Job",
        help::JOB_MODE,
        &modes,
        usize::from(job.mode.subtitles_only()),
    );
    row.set_subtitle(if job.mode.subtitles_only() {
        "The video isn't re-encoded"
    } else {
        "Writes a new file"
    });
    let target = match job.mode {
        JobMode::Subtitles(t) => t,
        JobMode::Encode => app.state.borrow().prefs.subs_target,
    };
    {
        let a = app.clone();
        row.connect_selected_notify(move |r| {
            let mode = if r.selected() == 1 {
                JobMode::Subtitles(target)
            } else {
                JobMode::Encode
            };
            let a = a.clone();
            glib::idle_add_local_once(move || a.set_mode(id, mode));
        });
    }
    g.add(&row);
    if job.mode.subtitles_only() {
        let labels: Vec<String> = SubsTarget::ALL
            .iter()
            .map(|t| t.label().to_string())
            .collect();
        let pos = SubsTarget::ALL
            .iter()
            .position(|t| *t == target)
            .unwrap_or(0);
        let t = combo_row("Subtitles", help::SUBS_TARGET, &labels, pos);
        t.set_subtitle(match target {
            SubsTarget::Sidecar => "Movie.no.srt next to the video; Plex finds it",
            SubsTarget::Embed => "Remuxed into the file, which is then replaced",
        });
        let a = app.clone();
        t.connect_selected_notify(move |r| {
            let mode = JobMode::Subtitles(SubsTarget::ALL[r.selected() as usize]);
            let a = a.clone();
            glib::idle_add_local_once(move || a.set_mode(id, mode));
        });
        g.add(&t);
    }
    g
}

fn summary(e: &ExternalSub, alass: bool) -> String {
    let mut parts = vec![e.language_name(), e.kind.short().to_string()];
    parts.push(
        if e.sync && alass {
            "synced to the video"
        } else {
            "not synced"
        }
        .into(),
    );
    if e.offset_ms != 0 {
        parts.push(format!("{:+.2} s", e.offset_ms as f64 / 1000.0));
    }
    parts.join(" · ")
}

/// Edit external subtitle `k` of job `id`, then rebuild the editor (labels change).
fn edit_ext(app: &Rc<App>, id: u64, k: usize, f: impl FnOnce(&mut ExternalSub) + 'static) {
    let a = app.clone();
    glib::idle_add_local_once(move || {
        a.edit(id, |j| {
            if let Some(e) = j.externals.get_mut(k) {
                f(e);
            }
            j.externals_changed();
        });
        a.reopen_pgs(id);
        a.rebuild_editor();
    });
}

/// The job's SRT files: include, language, kind, sync and offset per file.
pub fn externals_group(app: &Rc<App>, id: u64, job: &Job, alass: bool) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::new();
    g.set_title(if job.mode.subtitles_only() {
        "Subtitle Files"
    } else {
        "External Subtitles"
    });
    g.set_description(Some(match (job.mode.subtitles_only(), alass) {
        (true, true) => "Synced to the video, then saved or embedded",
        (true, false) => "Install alass to sync them automatically",
        (false, true) => "SRT files added to the output, synced to the video first",
        (false, false) => {
            "SRT files added to the output. Install alass to sync them automatically."
        }
    }));
    g.set_tooltip_text(Some(help::EXTERNAL_SUBS));
    let add = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add subtitle files…")
        .valign(gtk::Align::Center)
        .build();
    add.add_css_class("flat");
    {
        let a = app.clone();
        add.connect_clicked(move |_| a.pick_subtitles(id));
    }
    g.set_header_suffix(Some(&add));
    if job.externals.is_empty() {
        g.add(
            &adw::ActionRow::builder()
                .title("No subtitle files")
                .subtitle("Drop .srt files on the window, or use the + button")
                .build(),
        );
        return g;
    }
    let langs = tracks::language_choices();
    let mut lang_labels = vec!["Unknown".to_string()];
    lang_labels.extend(langs.iter().map(|(c, n)| format!("{n} ({c})")));
    for (k, e) in job.externals.iter().enumerate() {
        let row = adw::ExpanderRow::builder()
            .title(glib::markup_escape_text(&e.file_name()))
            .subtitle(summary(e, alass))
            .build();
        row.set_tooltip_text(Some(&e.path.display().to_string()));
        let check = gtk::CheckButton::new();
        check.set_active(e.selected);
        check.set_valign(gtk::Align::Center);
        check.set_tooltip_text(Some("Include this file"));
        {
            let a = app.clone();
            check.connect_toggled(move |c| {
                let v = c.is_active();
                edit_ext(&a, id, k, move |e| e.selected = v);
            });
        }
        row.add_prefix(&check);
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove from the job (the file isn't deleted)")
            .valign(gtk::Align::Center)
            .build();
        remove.add_css_class("flat");
        {
            let a = app.clone();
            remove.connect_clicked(move |_| {
                let a = a.clone();
                glib::idle_add_local_once(move || {
                    a.edit(id, |j| {
                        if k < j.externals.len() {
                            j.externals.remove(k);
                        }
                        j.externals_changed();
                    });
                    a.reopen_pgs(id);
                    a.rebuild_editor();
                });
            });
        }
        row.add_suffix(&remove);

        let current = e
            .language
            .as_deref()
            .and_then(|l| langs.iter().position(|(c, _)| *c == tracks::canonical(l)))
            .map_or(0, |i| i + 1);
        let lang = combo_row("Language", help::SUB_LANGUAGE, &lang_labels, current);
        lang.set_enable_search(true);
        {
            let a = app.clone();
            let codes: Vec<&'static str> = langs.iter().map(|(c, _)| *c).collect();
            lang.connect_selected_notify(move |r| {
                let code = (r.selected() as usize)
                    .checked_sub(1)
                    .and_then(|i| codes.get(i))
                    .map(|c| c.to_string());
                edit_ext(&a, id, k, move |e| e.language = code);
            });
        }
        row.add_row(&lang);

        let kinds: Vec<String> = ExtKind::ALL.iter().map(|k| k.label().to_string()).collect();
        let kind = combo_row(
            "Kind",
            help::SUB_KIND,
            &kinds,
            ExtKind::ALL.iter().position(|x| *x == e.kind).unwrap_or(0),
        );
        {
            let a = app.clone();
            kind.connect_selected_notify(move |r| {
                let v = ExtKind::ALL[r.selected() as usize];
                edit_ext(&a, id, k, move |e| e.kind = v);
            });
        }
        row.add_row(&kind);

        let sync = switch_row(
            "Sync to the video",
            if alass { "" } else { "alass isn't installed" },
            if alass {
                help::SUB_SYNC
            } else {
                help::SUB_SYNC_MISSING
            },
            e.sync,
        );
        sync.set_sensitive(alass);
        {
            let a = app.clone();
            sync.connect_active_notify(move |r| {
                let v = r.is_active();
                edit_ext(&a, id, k, move |e| e.sync = v);
            });
        }
        row.add_row(&sync);

        let offset = spin_row(
            "Offset (seconds)",
            help::SUB_OFFSET,
            -600.0,
            600.0,
            0.1,
            e.offset_ms as f64 / 1000.0,
            2,
        );
        {
            let a = app.clone();
            offset.connect_value_notify(move |r| {
                let ms = (r.value() * 1000.0).round() as i64;
                // Only the summary changes; no rebuild while the user types.
                a.edit(id, |j| {
                    if let Some(e) = j.externals.get_mut(k) {
                        e.offset_ms = ms;
                    }
                });
            });
        }
        row.add_row(&offset);
        g.add(&row);
    }
    g
}
