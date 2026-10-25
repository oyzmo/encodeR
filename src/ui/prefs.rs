//! Preferences dialog: output, device, languages, and user presets.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::ffmpeg::capabilities;
use crate::help;
use crate::presets;
use crate::tracks;
use crate::ui::widgets::combo_row;
use crate::window::App;

pub fn show(app: &Rc<App>) {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Preferences");
    let (prefs, caps) = {
        let st = app.state.borrow();
        (st.prefs.clone(), st.caps.clone())
    };

    let page = adw::PreferencesPage::builder()
        .title("General")
        .icon_name("emblem-system-symbolic")
        .build();

    // Output
    let out = adw::PreferencesGroup::builder().title("Output").build();
    let folder = adw::ActionRow::builder()
        .title("Output folder")
        .tooltip_text(help::OUTPUT_DIR)
        .build();
    let describe = |p: &Option<std::path::PathBuf>| {
        p.as_ref()
            .map_or("Next to the input file".to_string(), |p| {
                p.display().to_string()
            })
    };
    folder.set_subtitle(&describe(&prefs.output_dir));
    let choose = gtk::Button::with_label("Choose…");
    choose.set_valign(gtk::Align::Center);
    let reset = gtk::Button::from_icon_name("edit-clear-symbolic");
    reset.set_tooltip_text(Some("Use the input file's folder"));
    reset.add_css_class("flat");
    reset.set_valign(gtk::Align::Center);
    {
        let a = app.clone();
        let row = folder.clone();
        choose.connect_clicked(move |_| {
            let fd = gtk::FileDialog::builder()
                .title("Output Folder")
                .modal(true)
                .build();
            let a = a.clone();
            let row = row.clone();
            fd.select_folder(
                Some(&a.window.clone()),
                None::<&gio::Cancellable>,
                move |r| {
                    if let Ok(f) = r
                        && let Some(p) = f.path()
                    {
                        row.set_subtitle(&p.display().to_string());
                        a.state.borrow_mut().prefs.output_dir = Some(p);
                        a.save_prefs();
                        a.rebuild_editor();
                    }
                },
            );
        });
    }
    {
        let a = app.clone();
        let row = folder.clone();
        reset.connect_clicked(move |_| {
            a.state.borrow_mut().prefs.output_dir = None;
            row.set_subtitle("Next to the input file");
            a.save_prefs();
            a.rebuild_editor();
        });
    }
    folder.add_suffix(&reset);
    folder.add_suffix(&choose);
    out.add(&folder);
    let suffix = adw::EntryRow::builder()
        .title("File name suffix")
        .text(&prefs.suffix)
        .tooltip_text(help::SUFFIX)
        .build();
    {
        let a = app.clone();
        suffix.connect_changed(move |e| {
            a.state.borrow_mut().prefs.suffix = e.text().to_string();
            a.save_prefs();
        });
    }
    out.add(&suffix);
    page.add(&out);

    // Hardware
    let hw = adw::PreferencesGroup::builder().title("Hardware").build();
    let devices = caps
        .as_ref()
        .map(|c| c.devices.clone())
        .unwrap_or_else(capabilities::render_devices);
    let mut labels = vec!["Automatic".to_string()];
    labels.extend(devices.iter().map(|d| d.label()));
    let current = prefs
        .device
        .as_ref()
        .and_then(|d| devices.iter().position(|x| x.path == *d))
        .map_or(0, |i| i + 1);
    let dev = combo_row("VA-API device", help::DEVICE, &labels, current);
    dev.set_subtitle("Takes effect after restarting the app");
    {
        let a = app.clone();
        dev.connect_selected_notify(move |r| {
            let i = r.selected() as usize;
            a.state.borrow_mut().prefs.device = if i == 0 {
                None
            } else {
                devices.get(i - 1).map(|d| d.path.clone())
            };
            a.save_prefs();
        });
    }
    hw.add(&dev);
    page.add(&hw);

    // Languages
    let lang = adw::PreferencesGroup::builder().title("Tracks").build();
    let langs = adw::EntryRow::builder()
        .title("Preferred languages")
        .text(tracks::format_language_list(&prefs.preferred_languages))
        .tooltip_text(help::LANGUAGES)
        .show_apply_button(true)
        .build();
    {
        let a = app.clone();
        langs.connect_apply(move |e| {
            let (codes, unknown) = tracks::parse_language_list(&e.text());
            if !unknown.is_empty() {
                a.toast(&format!("Unknown language: {}", unknown.join(", ")));
            }
            if codes.is_empty() {
                return;
            }
            e.set_text(&tracks::format_language_list(&codes));
            a.state.borrow_mut().prefs.preferred_languages = codes;
            a.save_prefs();
            let a = a.clone();
            glib::idle_add_local_once(move || a.reprobe_all_tracks());
        });
    }
    lang.add(&langs);
    page.add(&lang);
    dialog.add(&page);

    // Presets
    let ppage = adw::PreferencesPage::builder()
        .title("Presets")
        .icon_name("view-list-symbolic")
        .build();
    let builtin = adw::PreferencesGroup::builder().title("Built-in").build();
    for p in presets::builtins() {
        builtin.add(
            &adw::ActionRow::builder()
                .title(&p.name)
                .subtitle(preset_summary(&p.settings))
                .build(),
        );
    }
    ppage.add(&builtin);
    let user = adw::PreferencesGroup::builder()
        .title("Your presets")
        .description("Save new ones from a job with “Save as Preset…”. Edit a name and press Enter to rename it.")
        .build();
    if prefs.user_presets.is_empty() {
        user.add(
            &adw::ActionRow::builder()
                .title("No saved presets yet")
                .sensitive(false)
                .build(),
        );
    }
    for p in &prefs.user_presets {
        let row = adw::EntryRow::builder()
            .title(preset_summary(&p.settings))
            .text(&p.name)
            .show_apply_button(true)
            .build();
        let del = gtk::Button::from_icon_name("user-trash-symbolic");
        del.add_css_class("flat");
        del.set_valign(gtk::Align::Center);
        del.set_tooltip_text(Some("Delete preset"));
        let old = std::rc::Rc::new(std::cell::RefCell::new(p.name.clone()));
        {
            let a = app.clone();
            let old = old.clone();
            row.connect_apply(move |e| {
                let new = e.text().to_string();
                let r = presets::rename(
                    &mut a.state.borrow_mut().prefs.user_presets,
                    &old.borrow(),
                    &new,
                );
                match r {
                    Ok(()) => {
                        {
                            let mut st = a.state.borrow_mut();
                            let o = old.borrow().clone();
                            for j in st.jobs.iter_mut().filter(|j| j.preset == o) {
                                j.preset = new.trim().to_string();
                            }
                        }
                        *old.borrow_mut() = new.trim().to_string();
                        a.save_prefs();
                        a.rebuild_editor();
                    }
                    Err(err) => a.toast(&err),
                }
            });
        }
        {
            let a = app.clone();
            let group = user.clone();
            let row2 = row.clone();
            del.connect_clicked(move |_| {
                presets::delete(&mut a.state.borrow_mut().prefs.user_presets, &old.borrow());
                group.remove(&row2);
                a.save_prefs();
                a.rebuild_editor();
            });
        }
        row.add_suffix(&del);
        user.add(&row);
    }
    ppage.add(&user);
    dialog.add(&ppage);

    dialog.present(Some(&app.window));
}

fn preset_summary(s: &crate::settings::EncodeSettings) -> String {
    use crate::settings::SizeMode;
    let mut parts = vec![s.codec.label().to_string()];
    if s.output_ten_bit() {
        parts.push("10-bit".into());
    }
    parts.push(match s.size_mode {
        SizeMode::TargetSize => format!("{:.1} GB", s.target_gb),
        SizeMode::ConstantQuality => format!("quality {}", s.qp),
    });
    if let Some(h) = s.downscale.height() {
        parts.push(format!("{h}p"));
    }
    parts.push(format!("{:?}", s.container).to_uppercase());
    parts.join(" · ")
}
