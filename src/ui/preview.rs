//! Crop preview: one still frame per sample point with the crop area outlined.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::crop;
use crate::queue::CropState;
use crate::settings::cache_dir;
use crate::window::App;
use crate::worker;

pub fn show(app: &Rc<App>, id: u64) {
    let (probe, found, tools) = {
        let st = app.state.borrow();
        let Some(j) = st.job(id) else { return };
        let Some(p) = j.probe.clone() else { return };
        let found = match &j.crop {
            CropState::Found(c) => Some(*c),
            _ => None,
        };
        (p, found, st.tools.clone())
    };
    let (w, h) = probe.video.as_ref().map_or((0, 0), |v| (v.width, v.height));
    let points = crop::sample_starts(probe.duration, crop::SAMPLES, crop::SAMPLE_SECONDS);
    let picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Contain)
        .vexpand(true)
        .build();
    let spinner = adw::Spinner::new();
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.set_size_request(48, 48);
    overlay.add_overlay(&spinner);
    let caption = gtk::Label::new(None);
    caption.add_css_class("dim-label");
    let prev = gtk::Button::from_icon_name("go-previous-symbolic");
    let next = gtk::Button::from_icon_name("go-next-symbolic");
    prev.set_tooltip_text(Some("Previous sample"));
    next.set_tooltip_text(Some("Next sample"));
    let nav = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    nav.set_halign(gtk::Align::Center);
    nav.append(&prev);
    nav.append(&caption);
    nav.append(&next);
    let summary = gtk::Label::new(Some(&match found {
        Some(c) => crop::describe(&c, w, h),
        None => "No crop detected; showing the full frame".into(),
    }));
    summary.add_css_class("heading");
    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.append(&summary);
    body.append(&overlay);
    body.append(&nav);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::new());
    tv.set_content(Some(&body));
    let dialog = adw::Dialog::builder()
        .title("Crop Preview")
        .content_width(1000)
        .content_height(680)
        .child(&tv)
        .build();

    let index = Rc::new(Cell::new(points.len() / 2));
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let load = {
        let (picture, spinner, caption, index, points) = (
            picture.clone(),
            spinner.clone(),
            caption.clone(),
            index.clone(),
            points.clone(),
        );
        Rc::new(move || {
            let i = index.get();
            let at = points[i] + crop::SAMPLE_SECONDS / 2.0;
            caption.set_label(&format!(
                "Sample {} of {} · {}",
                i + 1,
                points.len(),
                crate::ui::row::duration_text(at)
            ));
            spinner.set_visible(true);
            let out = dir.join(format!("preview-{}-{id}-{i}.png", std::process::id()));
            let (tools, probe, out2) = (tools.clone(), probe.clone(), out.clone());
            let (tx, rx) = async_channel::bounded::<Result<(), String>>(1);
            std::thread::spawn(move || {
                let _ = tx.send_blocking(worker::preview_frame(
                    &tools,
                    &probe,
                    at,
                    found.as_ref(),
                    &out2,
                ));
            });
            let (picture, spinner) = (picture.clone(), spinner.clone());
            glib::spawn_future_local(async move {
                if let Ok(r) = rx.recv().await {
                    spinner.set_visible(false);
                    if r.is_ok() {
                        picture.set_filename(Some(&out));
                    }
                }
            });
        })
    };
    {
        let (index, load, n) = (index.clone(), load.clone(), points.len());
        prev.connect_clicked(move |_| {
            index.set((index.get() + n - 1) % n);
            load();
        });
    }
    {
        let (index, load, n) = (index.clone(), load.clone(), points.len());
        next.connect_clicked(move |_| {
            index.set((index.get() + 1) % n);
            load();
        });
    }
    load();
    dialog.present(Some(&app.window));
}
