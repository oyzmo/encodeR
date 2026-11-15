//! Editor group for a job's cover art and poster.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::cover::CoverAction;
use crate::ffmpeg::probe::ProbeInfo;
use crate::help;
use crate::queue::Job;
use crate::ui::widgets::switch_row;
use crate::window::App;

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .build();
    b.add_css_class("flat");
    b
}

type ThumbKey = (std::path::PathBuf, Option<std::time::SystemTime>);

thread_local! {
    /// Decoded cover thumbnails, so rebuilding the editor doesn't decode again.
    static THUMBS: std::cell::RefCell<std::collections::HashMap<ThumbKey, gtk::gdk::Texture>> =
        Default::default();
}

/// Show a small thumbnail of `path` in `pic`: decoded at thumbnail size on a
/// worker thread (a big poster would stall the UI), then cached.
fn load_thumb(pic: &gtk::Picture, path: &std::path::Path) {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let key: ThumbKey = (path.to_path_buf(), mtime);
    if let Some(t) = THUMBS.with(|c| c.borrow().get(&key).cloned()) {
        pic.set_paintable(Some(&t));
        return;
    }
    let pic = pic.downgrade();
    let file = path.to_path_buf();
    glib::spawn_future_local(async move {
        let decoded = gtk::gio::spawn_blocking(move || {
            let pb = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(&file, 64, 96, true).ok()?;
            let format = if pb.has_alpha() {
                gtk::gdk::MemoryFormat::R8g8b8a8
            } else {
                gtk::gdk::MemoryFormat::R8g8b8
            };
            Some((
                pb.read_pixel_bytes(),
                pb.width(),
                pb.height(),
                pb.rowstride(),
                format,
            ))
        })
        .await
        .ok()
        .flatten();
        let Some((bytes, w, h, stride, format)) = decoded else {
            return;
        };
        let texture: gtk::gdk::Texture =
            gtk::gdk::MemoryTexture::new(w, h, format, &bytes, stride as usize).upcast();
        THUMBS.with(|c| c.borrow_mut().insert(key, texture.clone()));
        if let Some(pic) = pic.upgrade() {
            pic.set_paintable(Some(&texture));
        }
    });
}

/// Change the cover choice after the current signal handler returns (the
/// editor is rebuilt, which destroys the widget that fired).
fn later(app: &Rc<App>, id: u64, f: impl FnOnce(&mut crate::cover::CoverChoice) + 'static) {
    let a = app.clone();
    glib::idle_add_local_once(move || a.edit_cover(id, f));
}

/// Keep / replace / remove the embedded picture, crop it to 2:3, and save it
/// next to the result as a poster.
pub fn cover_group(app: &Rc<App>, id: u64, job: &Job, probe: &ProbeInfo) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::new();
    g.set_title("Cover Art");
    g.set_description(Some("Drop a picture on the window to use it"));
    let c = &job.cover;

    let row = adw::ActionRow::builder()
        .title("Cover art")
        .subtitle(glib::markup_escape_text(&c.summary(probe)))
        .tooltip_text(help::COVER)
        .build();
    if let CoverAction::Replace(path) = &c.action {
        let pic = gtk::Picture::new();
        load_thumb(&pic, path);
        pic.set_content_fit(gtk::ContentFit::Cover);
        pic.set_size_request(32, 48);
        pic.set_can_shrink(true);
        pic.set_valign(gtk::Align::Center);
        pic.add_css_class("card");
        row.add_prefix(&pic);
        row.set_tooltip_text(Some(&path.display().to_string()));
    }
    let choose = gtk::Button::builder()
        .label("Choose…")
        .tooltip_text("Pick a picture for the cover")
        .valign(gtk::Align::Center)
        .build();
    {
        let a = app.clone();
        choose.connect_clicked(move |_| a.pick_cover(id));
    }
    row.add_suffix(&choose);
    if c.action != CoverAction::Keep {
        let undo = icon_button(
            "edit-undo-symbolic",
            if probe.covers.is_empty() {
                "Back to no cover art"
            } else {
                "Back to the source's cover art"
            },
        );
        let a = app.clone();
        undo.connect_clicked(move |_| later(&a, id, |c| c.action = CoverAction::Keep));
        row.add_suffix(&undo);
    }
    if c.has_picture(probe) {
        let remove = icon_button("user-trash-symbolic", "No cover art in the output");
        let a = app.clone();
        remove.connect_clicked(move |_| later(&a, id, |c| c.action = CoverAction::Remove));
        row.add_suffix(&remove);
    }
    g.add(&row);

    let picture = c.has_picture(probe);
    let poster_name = app
        .state
        .borrow()
        .poster_for(job)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let poster = switch_row(
        "Also save as poster",
        &if picture {
            format!(
                "{} next to the video, the artwork Plex shows",
                glib::markup_escape_text(&poster_name)
            )
        } else {
            "No picture to save".into()
        },
        help::SAVE_POSTER,
        c.save_poster,
    );
    poster.set_sensitive(picture);
    {
        let a = app.clone();
        poster.connect_active_notify(move |r| {
            let v = r.is_active();
            later(&a, id, move |c| c.save_poster = v);
        });
    }
    g.add(&poster);

    // Cropping applies to a picture that's converted: a new cover, or a poster.
    let converted = matches!(c.action, CoverAction::Replace(_)) || (picture && c.save_poster);
    let crop = switch_row(
        "Crop to poster shape",
        match c.action {
            CoverAction::Replace(_) => "2:3, centred; for the cover and the poster",
            _ if converted => "2:3, centred; for the poster (the source's cover stays as it is)",
            _ => "For a new cover or a poster",
        },
        help::POSTER_CROP,
        c.poster_crop,
    );
    crop.set_sensitive(converted);
    {
        let a = app.clone();
        crop.connect_active_notify(move |r| {
            let v = r.is_active();
            later(&a, id, move |c| c.poster_crop = v);
        });
    }
    g.add(&crop);
    g
}
