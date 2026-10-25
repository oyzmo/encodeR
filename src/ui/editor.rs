//! The job editor: status, checks, output settings, tracks, picture, quality and advanced options.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::checks::Severity;
use crate::crop;
use crate::ffmpeg::estimate::{self, format_bytes};
use crate::ffmpeg::progress::format_eta;
use crate::ffmpeg::vmaf;
use crate::help;
use crate::presets;
use crate::queue::{CropState, EstimateState, Job, JobState, VmafState};
use crate::scan::{Scan, ScanOverride};
use crate::settings::{
    Container, Downscale, MultichannelFallback, RateControl, SizeMode, VideoCodec,
};
use crate::tracks::{self, AudioAction};
use crate::ui::row::duration_text;
use crate::ui::widgets::{DynGroup, combo_row, severity_icon, spin_row, switch_row};
use crate::window::{App, State};

enum TrackKind {
    Audio,
    Sub,
}

struct TrackRow {
    kind: TrackKind,
    index: usize,
    row: adw::ActionRow,
}

pub struct Editor {
    pub job: u64,
    pub root: gtk::Widget,
    scroller: Option<gtk::ScrolledWindow>,
    issues: Option<DynGroup>,
    estimate_row: Option<(adw::ActionRow, adw::Spinner)>,
    audio_usage: Option<adw::ActionRow>,
    tracks: RefCell<Vec<TrackRow>>,
    crop_row: Option<(adw::SwitchRow, gtk::Button)>,
    scan_row: Option<(adw::ComboRow, adw::SwitchRow)>,
    vmaf_row: Option<(adw::ActionRow, gtk::Button, adw::Spinner)>,
    command: Option<gtk::TextBuffer>,
    progress: Option<(gtk::ProgressBar, gtk::Label)>,
}

fn group(title: &str, description: &str) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::new();
    g.set_title(title);
    if !description.is_empty() {
        g.set_description(Some(description));
    }
    g
}

fn status_page(icon: &str, title: &str, desc: &str, spinner: bool) -> gtk::Widget {
    let p = adw::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .description(desc)
        .build();
    if spinner {
        let s = adw::SpinnerPaintable::new(Some(&p));
        p.set_paintable(Some(&s));
    }
    p.upcast()
}

fn video_summary(job: &Job) -> String {
    let Some(p) = &job.probe else {
        return String::new();
    };
    let Some(v) = &p.video else {
        return String::new();
    };
    let mut parts = vec![
        format!("{}×{}", v.width, v.height),
        format!("{} {}", v.codec.to_uppercase(), v.profile)
            .trim()
            .to_string(),
        format!("{}-bit", v.bit_depth),
        format!("{:.3} fps", v.fps).replace(".000", ""),
        duration_text(p.duration),
        format_bytes(p.size_bytes),
    ];
    if let Some(b) = p.video_bit_rate() {
        parts.push(format!("{:.1} Mb/s video", b as f64 / 1e6));
    }
    parts.join(" · ")
}

impl Editor {
    pub fn scroll_position(&self) -> f64 {
        self.scroller
            .as_ref()
            .map_or(0.0, |s| s.vadjustment().value())
    }

    pub fn restore_scroll(&self, v: f64) {
        if let Some(s) = self.scroller.clone() {
            glib::idle_add_local_once(move || s.vadjustment().set_value(v));
        }
    }

    pub fn build(app: &Rc<App>, id: u64) -> Rc<Editor> {
        let st = app.state.borrow();
        let empty = |root: gtk::Widget| {
            Rc::new(Editor {
                job: id,
                root,
                scroller: None,
                issues: None,
                estimate_row: None,
                audio_usage: None,
                tracks: RefCell::new(Vec::new()),
                crop_row: None,
                scan_row: None,
                vmaf_row: None,
                command: None,
                progress: None,
            })
        };
        let Some(job) = st.job(id).cloned() else {
            return empty(status_page(
                "dialog-question-symbolic",
                "No Job Selected",
                "",
                false,
            ));
        };
        if job.state == JobState::Probing {
            return empty(status_page("", "Reading File…", &job.file_name(), true));
        }
        let Some(probe) = job.probe.clone() else {
            return empty(status_page(
                "dialog-error-symbolic",
                "Couldn't Read File",
                job.error.as_deref().unwrap_or("ffprobe failed"),
                false,
            ));
        };
        let caps = st.caps.clone();
        let user_presets = st.prefs.user_presets.clone();
        let vmaf_after = st.prefs.vmaf_after_encode;
        let crop_gpu = st.crop_on_gpu();
        drop(st);

        let editable = job.state.editable();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 24);
        content.set_margin_top(18);
        content.set_margin_bottom(24);
        content.set_margin_start(12);
        content.set_margin_end(12);

        // ---- Status ----
        let status = group(&job.file_name(), &video_summary(&job));
        if let Some(b) = probe.hdr.badge() {
            let l = gtk::Label::new(Some(&b));
            l.add_css_class("badge");
            l.set_valign(gtk::Align::Center);
            status.set_header_suffix(Some(&l));
        }
        let mut progress = None;
        match job.state {
            JobState::Running => {
                let bar = gtk::ProgressBar::new();
                bar.set_valign(gtk::Align::Center);
                bar.set_hexpand(true);
                let label = gtk::Label::builder().xalign(0.0).build();
                label.add_css_class("caption");
                let vb = gtk::Box::new(gtk::Orientation::Vertical, 6);
                vb.set_margin_top(12);
                vb.set_margin_bottom(12);
                vb.set_margin_start(12);
                vb.set_margin_end(12);
                vb.append(&label);
                vb.append(&bar);
                let cancel = gtk::Button::with_label("Cancel");
                cancel.add_css_class("destructive-action");
                cancel.set_valign(gtk::Align::Center);
                cancel.set_tooltip_text(Some("Stop ffmpeg and delete the partial output"));
                let a = app.clone();
                cancel.connect_clicked(move |_| a.cancel_running(id));
                let hb = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                hb.append(&vb);
                hb.append(&cancel);
                hb.set_margin_end(12);
                let row = gtk::ListBoxRow::builder()
                    .child(&hb)
                    .activatable(false)
                    .build();
                status.add(&row);
                progress = Some((bar, label));
            }
            JobState::Done => {
                for line in &job.report {
                    let row = adw::ActionRow::builder().title(line).title_lines(0).build();
                    row.add_prefix(&gtk::Image::from_icon_name(if line.starts_with('⚠') {
                        "dialog-warning-symbolic"
                    } else {
                        "object-select-symbolic"
                    }));
                    status.add(&row);
                }
                if let Some(out) = job.output.clone() {
                    let row = adw::ActionRow::builder()
                        .title(
                            out.file_name()
                                .map(|f| f.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        )
                        .subtitle(
                            out.parent()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default(),
                        )
                        .build();
                    let btn = gtk::Button::with_label("Show in Files");
                    btn.set_valign(gtk::Align::Center);
                    let win = app.window.clone();
                    btn.connect_clicked(move |_| {
                        gtk::FileLauncher::new(Some(&gio::File::for_path(&out)))
                            .open_containing_folder(Some(&win), None::<&gio::Cancellable>, |_| {});
                    });
                    row.add_suffix(&btn);
                    status.add(&row);
                }
                if let Some(d) = job
                    .decode_used
                    .filter(|_| job.settings.codec != VideoCodec::Copy)
                {
                    status.add(
                        &adw::ActionRow::builder()
                            .title(format!("Used {}", d.label()))
                            .build(),
                    );
                }
            }
            JobState::Failed | JobState::Cancelled => {
                let msg = job
                    .error
                    .clone()
                    .unwrap_or_else(|| job.state.label().to_string());
                let row = adw::ActionRow::builder().title(&msg).title_lines(0).build();
                row.add_prefix(&gtk::Image::from_icon_name("dialog-error-symbolic"));
                let retry = gtk::Button::with_label("Retry");
                retry.set_valign(gtk::Align::Center);
                let a = app.clone();
                retry.connect_clicked(move |_| a.retry(id));
                row.add_suffix(&retry);
                status.add(&row);
                if !job.stderr_tail.is_empty() {
                    let exp = adw::ExpanderRow::builder()
                        .title("ffmpeg output")
                        .subtitle(format!("Last {} lines", job.stderr_tail.len()))
                        .build();
                    let label = gtk::Label::builder()
                        .label(job.stderr_tail.join("\n"))
                        .selectable(true)
                        .wrap(true)
                        .wrap_mode(gtk::pango::WrapMode::WordChar)
                        .xalign(0.0)
                        .margin_top(12)
                        .margin_bottom(12)
                        .margin_start(12)
                        .margin_end(12)
                        .build();
                    label.add_css_class("monospace");
                    exp.add_row(&label);
                    status.add(&exp);
                }
            }
            _ => {}
        }
        content.append(&status);

        let issues = DynGroup::new("Checks");
        content.append(&issues.group);

        let settings_box = gtk::Box::new(gtk::Orientation::Vertical, 24);
        settings_box.set_sensitive(editable);
        content.append(&settings_box);
        let s = job.settings.clone();
        let v = probe.video.clone().unwrap_or_default();

        // ---- Output ----
        let out = group("Output", "");
        let all_presets = presets::all(&user_presets);
        let mut names: Vec<String> = all_presets.iter().map(|p| p.name.clone()).collect();
        let custom = !names.contains(&job.preset)
            || presets::find(&user_presets, &job.preset).is_some_and(|p| p.settings != s);
        if custom {
            names.push("Custom".into());
        }
        let current = if custom {
            names.len() - 1
        } else {
            names.iter().position(|n| *n == job.preset).unwrap_or(0)
        };
        let preset_row = combo_row("Preset", help::PRESET, &names, current);
        {
            let a = app.clone();
            let names = names.clone();
            preset_row.connect_selected_notify(move |r| {
                if let Some(name) = names.get(r.selected() as usize).filter(|n| *n != "Custom") {
                    let name = name.clone();
                    let a = a.clone();
                    glib::idle_add_local_once(move || a.apply_preset(id, &name));
                }
            });
        }
        out.add(&preset_row);

        // Codec: one radio row each, so unavailable codecs can say why.
        let codec_exp = adw::ExpanderRow::builder()
            .title("Video codec")
            .subtitle(s.codec.label())
            .tooltip_text(help::CODEC)
            .build();
        let mut first_check: Option<gtk::CheckButton> = None;
        for codec in VideoCodec::ALL {
            let cc = caps.as_ref().map(|c| c.codec(codec));
            let available = cc.as_ref().is_none_or(|c| c.available);
            let mut subtitle = match codec {
                VideoCodec::Hevc => "Best for Apple TV · 10-bit and HDR10".to_string(),
                VideoCodec::Av1 => {
                    "Smallest files · older Apple TVs may need the NAS to transcode".to_string()
                }
                VideoCodec::H264 => "Plays everywhere · bigger files · 8-bit only".to_string(),
                VideoCodec::Copy => {
                    "Keep the video as it is; only change tracks and container".to_string()
                }
            };
            if let Some(reason) = cc.as_ref().and_then(|c| c.reason.clone()) {
                subtitle = reason;
            } else if caps.is_none() && codec != VideoCodec::Copy {
                subtitle.push_str(" · checking GPU…");
            }
            let row = adw::ActionRow::builder()
                .title(codec.label())
                .subtitle(&subtitle)
                .sensitive(available)
                .build();
            row.set_tooltip_text(Some(if codec == VideoCodec::Av1 {
                help::CODEC_AV1
            } else {
                help::CODEC
            }));
            let check = gtk::CheckButton::new();
            check.set_valign(gtk::Align::Center);
            if let Some(f) = &first_check {
                check.set_group(Some(f));
            } else {
                first_check = Some(check.clone());
            }
            check.set_active(s.codec == codec);
            row.add_prefix(&check);
            row.set_activatable_widget(Some(&check));
            let a = app.clone();
            check.connect_toggled(move |c| {
                if c.is_active() {
                    let a = a.clone();
                    glib::idle_add_local_once(move || {
                        a.edit(id, |j| {
                            if j.settings.codec != codec {
                                j.settings.codec = codec;
                                j.settings.qp = codec.default_qp();
                                j.preset = "Custom".into();
                            }
                        });
                        a.rebuild_editor();
                    });
                }
            });
            codec_exp.add_row(&row);
        }
        out.add(&codec_exp);

        let encoding = s.codec != VideoCodec::Copy;
        if encoding {
            let mode_row = adw::ActionRow::builder()
                .title("Size")
                .tooltip_text(help::SIZE_MODE)
                .build();
            let toggles = adw::ToggleGroup::new();
            toggles.add(
                adw::Toggle::builder()
                    .label("Target size")
                    .name("target")
                    .build(),
            );
            toggles.add(
                adw::Toggle::builder()
                    .label("Constant quality")
                    .name("quality")
                    .build(),
            );
            toggles.set_active_name(Some(if s.size_mode == SizeMode::TargetSize {
                "target"
            } else {
                "quality"
            }));
            toggles.set_valign(gtk::Align::Center);
            let a = app.clone();
            toggles.connect_active_name_notify(move |t| {
                let mode = if t.active_name().as_deref() == Some("quality") {
                    SizeMode::ConstantQuality
                } else {
                    SizeMode::TargetSize
                };
                let a = a.clone();
                glib::idle_add_local_once(move || {
                    a.edit(id, |j| j.settings.size_mode = mode);
                    a.rebuild_editor();
                });
            });
            mode_row.add_suffix(&toggles);
            out.add(&mode_row);

            if s.size_mode == SizeMode::TargetSize {
                let target = spin_row(
                    "Target size (GB)",
                    help::TARGET_SIZE,
                    0.5,
                    100.0,
                    0.5,
                    s.target_gb,
                    1,
                );
                let picks = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                picks.add_css_class("linked");
                picks.set_valign(gtk::Align::Center);
                for gb in [2.0, 3.0, 4.0, 5.0] {
                    let b = gtk::Button::with_label(&format!("{gb:.0}"));
                    b.set_tooltip_text(Some(&format!("{gb:.0} GB")));
                    let t = target.clone();
                    b.connect_clicked(move |_| t.set_value(gb));
                    picks.append(&b);
                }
                target.add_suffix(&picks);
                let a = app.clone();
                target.connect_value_notify(move |r| {
                    let v = r.value();
                    a.edit(id, |j| j.settings.target_gb = v);
                });
                out.add(&target);
            } else {
                let q = adw::ActionRow::builder()
                    .title("Quality")
                    .subtitle("Lower = better and bigger")
                    .tooltip_text(help::QUALITY)
                    .build();
                let scale = gtk::Scale::with_range(
                    gtk::Orientation::Horizontal,
                    0.0,
                    s.codec.max_qp() as f64,
                    1.0,
                );
                scale.set_value(s.qp as f64);
                scale.set_digits(0);
                scale.set_draw_value(true);
                scale.set_value_pos(gtk::PositionType::Left);
                scale.set_width_request(220);
                scale.set_valign(gtk::Align::Center);
                let def = s.codec.default_qp() as f64;
                scale.add_mark(def, gtk::PositionType::Bottom, None);
                let a = app.clone();
                scale.connect_value_changed(move |sc| {
                    let v = sc.value().round() as u32;
                    a.edit(id, |j| j.settings.qp = v);
                });
                q.add_suffix(&scale);
                out.add(&q);
            }
        }
        let est = adw::ActionRow::builder()
            .title("Estimated size")
            .tooltip_text(help::ESTIMATE)
            .subtitle_lines(2)
            .build();
        let est_spin = adw::Spinner::new();
        est_spin.set_visible(false);
        est.add_suffix(&est_spin);
        out.add(&est);
        let containers = vec!["MKV".to_string(), "MP4".to_string()];
        let cont = combo_row(
            "Container",
            help::CONTAINER,
            &containers,
            if s.container == Container::Mkv { 0 } else { 1 },
        );
        {
            let a = app.clone();
            cont.connect_selected_notify(move |r| {
                let c = if r.selected() == 0 {
                    Container::Mkv
                } else {
                    Container::Mp4
                };
                a.edit(id, |j| j.settings.container = c);
            });
        }
        out.add(&cont);
        settings_box.append(&out);

        // ---- Picture ----
        let mut crop_row = None;
        let mut scan_row = None;
        if encoding {
            let pic = group("Picture", "");
            let caps_10 = caps.as_ref().is_none_or(|c| c.codec(s.codec).ten_bit);
            let ten = switch_row("10-bit", "", help::TEN_BIT, s.output_ten_bit());
            ten.set_sensitive(s.codec.supports_10bit() && caps_10);
            if !s.codec.supports_10bit() {
                ten.set_subtitle("Not available with H.264");
            } else if !caps_10 {
                ten.set_subtitle("Your driver rejected 10-bit for this codec");
            }
            {
                let a = app.clone();
                ten.connect_active_notify(move |r| {
                    let v = r.is_active();
                    a.edit(id, |j| j.settings.ten_bit = v);
                });
            }
            pic.add(&ten);
            let options = Downscale::options_for(v.height);
            let labels: Vec<String> = options.iter().map(|d| d.label().to_string()).collect();
            let sel = options.iter().position(|d| *d == s.downscale).unwrap_or(0);
            let ds = combo_row("Resolution", help::DOWNSCALE, &labels, sel);
            {
                let a = app.clone();
                ds.connect_selected_notify(move |r| {
                    let d = options
                        .get(r.selected() as usize)
                        .copied()
                        .unwrap_or(Downscale::Keep);
                    a.edit(id, |j| j.settings.downscale = d);
                });
            }
            pic.add(&ds);

            let choices: Vec<String> = ScanOverride::ALL
                .iter()
                .map(|o| o.label().to_string())
                .collect();
            let sc = combo_row(
                "Interlacing",
                help::SCAN,
                &choices,
                ScanOverride::ALL
                    .iter()
                    .position(|o| *o == job.scan_override)
                    .unwrap_or(0),
            );
            sc.set_subtitle_lines(2);
            {
                let a = app.clone();
                sc.connect_selected_notify(move |r| {
                    let o = ScanOverride::ALL[r.selected() as usize];
                    a.edit(id, |j| j.scan_override = o);
                });
            }
            pic.add(&sc);
            let dbl = switch_row(
                "Double frame rate",
                "",
                help::DOUBLE_RATE,
                s.deinterlace_double_rate,
            );
            {
                let a = app.clone();
                dbl.connect_active_notify(move |r| {
                    let v = r.is_active();
                    a.edit(id, |j| j.settings.deinterlace_double_rate = v);
                });
            }
            pic.add(&dbl);
            scan_row = Some((sc, dbl));

            let cr = switch_row("Crop black bars", "", help::CROP, job.crop_enabled);
            cr.set_tooltip_text(Some(&format!(
                "{} {}",
                help::CROP,
                if crop_gpu {
                    help::CROP_GPU
                } else {
                    help::CROP_CPU
                }
            )));
            let preview = gtk::Button::from_icon_name("image-x-generic-symbolic");
            preview.set_tooltip_text(Some("Preview the crop"));
            preview.add_css_class("flat");
            preview.set_valign(gtk::Align::Center);
            {
                let a = app.clone();
                preview.connect_clicked(move |_| a.crop_preview(id));
            }
            cr.add_suffix(&preview);
            {
                let a = app.clone();
                cr.connect_active_notify(move |r| {
                    let v = r.is_active();
                    a.edit(id, |j| j.crop_enabled = v);
                });
            }
            pic.add(&cr);
            crop_row = Some((cr, preview));

            if let Some(d) = probe.hdr.dovi {
                let has_tool = caps.as_ref().is_some_and(|c| c.dovi_tool);
                let blocker = job.dv_blocker(has_tool);
                let subtitle = blocker
                    .map(str::to_string)
                    .unwrap_or_else(|| match d.profile {
                        7 => "Experimental · profile 7 is converted to 8.1".into(),
                        p => format!("Experimental · keeps profile {p}"),
                    });
                let dv = switch_row(
                    "Keep Dolby Vision",
                    &subtitle,
                    if has_tool {
                        help::DV_PRESERVE
                    } else {
                        help::DV_MISSING
                    },
                    job.dv_preserve && blocker.is_none(),
                );
                dv.set_sensitive(blocker.is_none());
                let a = app.clone();
                dv.connect_active_notify(move |r| {
                    let v = r.is_active();
                    a.edit(id, |j| j.dv_preserve = v);
                });
                pic.add(&dv);
            }
            settings_box.append(&pic);
        }

        // ---- Audio ----
        let audio = group("Audio", "");
        let atv = switch_row(
            "Apple TV compatible audio only",
            "Convert TrueHD, DTS and Opus; copy the rest",
            help::APPLE_AUDIO,
            s.apple_tv_audio,
        );
        {
            let a = app.clone();
            atv.connect_active_notify(move |r| {
                let v = r.is_active();
                let a = a.clone();
                glib::idle_add_local_once(move || {
                    a.edit(id, |j| {
                        j.settings.apple_tv_audio = v;
                        if let Some(p) = &j.probe {
                            j.selection.reset_actions(p, &j.settings);
                        }
                    });
                    a.rebuild_editor();
                });
            });
        }
        audio.add(&atv);
        let down = switch_row(
            "Downmix to stereo AAC",
            "Fold 5.1 and 7.1 tracks to AAC 2.0",
            help::DOWNMIX,
            s.stereo_downmix,
        );
        {
            let a = app.clone();
            down.connect_active_notify(move |r| {
                let v = r.is_active();
                let a = a.clone();
                glib::idle_add_local_once(move || {
                    a.edit(id, |j| {
                        j.settings.stereo_downmix = v;
                        if let Some(p) = &j.probe {
                            j.selection.reset_actions(p, &j.settings);
                        }
                    });
                    a.rebuild_editor();
                });
            });
        }
        audio.add(&down);
        let fb_labels: Vec<String> = MultichannelFallback::ALL
            .iter()
            .map(|f| f.label().to_string())
            .collect();
        let fb = combo_row(
            "Multichannel conversion",
            help::FALLBACK,
            &fb_labels,
            MultichannelFallback::ALL
                .iter()
                .position(|f| *f == s.multichannel_fallback)
                .unwrap_or(0),
        );
        fb.set_sensitive(s.apple_tv_audio && !s.stereo_downmix);
        {
            let a = app.clone();
            fb.connect_selected_notify(move |r| {
                let f = MultichannelFallback::ALL[r.selected() as usize];
                let a = a.clone();
                glib::idle_add_local_once(move || {
                    a.edit(id, |j| {
                        j.settings.multichannel_fallback = f;
                        if let Some(p) = &j.probe {
                            j.selection.reset_actions(p, &j.settings);
                        }
                    });
                    a.rebuild_editor();
                });
            });
        }
        audio.add(&fb);
        let usage = adw::ActionRow::builder().title("Selected audio").build();
        audio.add(&usage);
        settings_box.append(&audio);

        // ---- Tracks, grouped by language ----
        let track_rows = RefCell::new(Vec::new());
        for g in &job.groups {
            let pg = group(
                &g.name,
                &format!("{} audio · {} subtitle", g.audio.len(), g.subs.len()),
            );
            for (rank, r) in g.audio.iter().enumerate() {
                let Some(a) = probe.audio_by_index(r.index) else {
                    continue;
                };
                let Some(choice) = job.selection.audio.iter().find(|c| c.index == r.index) else {
                    continue;
                };
                let mut title = tracks::audio_label(a);
                if let Some(t) = &a.title {
                    title = format!("{title} — {t}");
                }
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&title))
                    .subtitle_lines(3)
                    .build();
                row.set_tooltip_text(Some(&format!(
                    "{}: {}",
                    if rank == 0 {
                        "Ranked first".to_string()
                    } else {
                        format!("Ranked #{}", rank + 1)
                    },
                    r.reasons.join(", ")
                )));
                let check = gtk::CheckButton::new();
                check.set_active(choice.selected);
                check.set_valign(gtk::Align::Center);
                row.add_prefix(&check);
                let index = r.index;
                {
                    let ap = app.clone();
                    check.connect_toggled(move |c| {
                        let v = c.is_active();
                        ap.edit(id, |j| {
                            if let Some(ch) =
                                j.selection.audio.iter_mut().find(|ch| ch.index == index)
                            {
                                ch.selected = v;
                            }
                        });
                    });
                }
                let mut choices = AudioAction::choices(a.channels);
                if !choices.contains(&choice.action) {
                    choices.insert(1, choice.action);
                }
                let labels: Vec<String> = choices.iter().map(|c| c.label(a.channels)).collect();
                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                let dd = gtk::DropDown::from_strings(&refs);
                dd.set_selected(
                    choices
                        .iter()
                        .position(|c| *c == choice.action)
                        .unwrap_or(0) as u32,
                );
                dd.set_valign(gtk::Align::Center);
                dd.set_tooltip_text(Some("Copy keeps the track as it is; converting makes it smaller or playable on Apple TV"));
                {
                    let ap = app.clone();
                    dd.connect_selected_notify(move |d| {
                        let act = choices[d.selected() as usize];
                        ap.edit(id, |j| {
                            if let Some(ch) =
                                j.selection.audio.iter_mut().find(|ch| ch.index == index)
                            {
                                ch.action = act;
                            }
                        });
                    });
                }
                row.add_suffix(&dd);
                if !tracks::apple_compatible(&a.codec) {
                    let warn = gtk::Image::from_icon_name("dialog-warning-symbolic");
                    warn.set_tooltip_text(Some(&format!(
                        "Apple TV can't play {} natively",
                        tracks::codec_name(&a.codec, &a.profile)
                    )));
                    warn.add_css_class("warning");
                    row.add_suffix(&warn);
                }
                pg.add(&row);
                track_rows.borrow_mut().push(TrackRow {
                    kind: TrackKind::Audio,
                    index,
                    row,
                });
            }
            let subs_exp = adw::ExpanderRow::builder()
                .title("Subtitles")
                .subtitle(format!(
                    "{} of {} selected",
                    g.subs
                        .iter()
                        .filter(|r| job
                            .selection
                            .subs
                            .iter()
                            .any(|c| c.index == r.index && c.selected))
                        .count(),
                    g.subs.len()
                ))
                .expanded(app.subs_expanded.borrow().contains(&g.name))
                .build();
            {
                let ap = app.clone();
                let name = g.name.clone();
                subs_exp.connect_expanded_notify(move |e| {
                    let mut open = ap.subs_expanded.borrow_mut();
                    if e.is_expanded() {
                        open.insert(name.clone());
                    } else {
                        open.remove(&name);
                    }
                });
            }
            for (rank, r) in g.subs.iter().enumerate() {
                let Some(sub) = probe.subtitle_by_index(r.index) else {
                    continue;
                };
                let Some(choice) = job.selection.subs.iter().find(|c| c.index == r.index) else {
                    continue;
                };
                let kind = tracks::sub_kind(sub);
                let mut title = format!("Subtitles · {}", tracks::sub_kind_label(kind));
                if let Some(t) = &sub.title {
                    title = format!("{title} — {t}");
                }
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&title))
                    .subtitle_lines(2)
                    .build();
                row.set_tooltip_text(Some(&format!(
                    "{}: {}",
                    if rank == 0 {
                        "Ranked first".to_string()
                    } else {
                        format!("Ranked #{}", rank + 1)
                    },
                    r.reasons.join(", ")
                )));
                let check = gtk::CheckButton::new();
                check.set_active(choice.selected);
                check.set_valign(gtk::Align::Center);
                row.add_prefix(&check);
                let index = r.index;
                {
                    let ap = app.clone();
                    let exp = subs_exp.clone();
                    let group: Vec<usize> = g.subs.iter().map(|r| r.index).collect();
                    check.connect_toggled(move |c| {
                        let v = c.is_active();
                        ap.edit(id, |j| {
                            if let Some(ch) =
                                j.selection.subs.iter_mut().find(|ch| ch.index == index)
                            {
                                ch.selected = v;
                            }
                            let n = j
                                .selection
                                .subs
                                .iter()
                                .filter(|c| c.selected && group.contains(&c.index))
                                .count();
                            exp.set_subtitle(&format!("{n} of {} selected", group.len()));
                        });
                    });
                }
                let fmt = gtk::Label::new(Some(&tracks::codec_name(&sub.codec, "")));
                fmt.add_css_class("dim-label");
                row.add_suffix(&fmt);
                if !tracks::is_text_subtitle(&sub.codec) {
                    let warn = gtk::Image::from_icon_name("dialog-warning-symbolic");
                    warn.set_tooltip_text(Some(help::PGS));
                    warn.add_css_class("warning");
                    row.add_suffix(&warn);
                }
                subs_exp.add_row(&row);
                track_rows.borrow_mut().push(TrackRow {
                    kind: TrackKind::Sub,
                    index,
                    row,
                });
            }
            if !g.subs.is_empty() {
                pg.add(&subs_exp);
            }
            settings_box.append(&pg);
        }

        // ---- Quality check ----
        let mut vmaf_row = None;
        if encoding {
            let qg = group("Quality Check", "");
            let row = adw::ActionRow::builder()
                .title("VMAF score")
                .subtitle_lines(3)
                .build();
            let has_vmaf = caps.as_ref().is_some_and(|c| c.libvmaf);
            row.set_tooltip_text(Some(if has_vmaf {
                if probe.hdr.is_hdr() {
                    help::VMAF_HDR
                } else {
                    help::VMAF
                }
            } else {
                help::VMAF_MISSING
            }));
            let spin = adw::Spinner::new();
            spin.set_visible(false);
            let btn = gtk::Button::with_label("Check");
            btn.set_valign(gtk::Align::Center);
            btn.set_sensitive(has_vmaf);
            {
                let a = app.clone();
                btn.connect_clicked(move |b| {
                    if b.label().as_deref() == Some("Cancel") {
                        a.cancel_vmaf();
                    } else {
                        a.start_vmaf(id);
                    }
                });
            }
            row.add_suffix(&spin);
            row.add_suffix(&btn);
            qg.add(&row);
            let after = switch_row(
                "Check after encoding",
                "Score the finished file too",
                help::VMAF_AFTER,
                vmaf_after,
            );
            after.set_sensitive(has_vmaf);
            {
                let a = app.clone();
                after.connect_active_notify(move |r| {
                    a.state.borrow_mut().prefs.vmaf_after_encode = r.is_active();
                    a.save_prefs();
                });
            }
            qg.add(&after);
            settings_box.append(&qg);
            vmaf_row = Some((row, btn, spin));
        }

        // ---- Advanced ----
        let adv = group("Advanced", "");
        if encoding {
            let exp = adw::ExpanderRow::builder()
                .title("Encoder options")
                .subtitle("Rate control, speed, keyframes, B-frames")
                .build();
            let rcs: Vec<RateControl> = RateControl::ALL
                .into_iter()
                .filter(|rc| {
                    caps.as_ref()
                        .is_none_or(|c| c.rate_control_ok(s.codec, *rc))
                        || *rc == s.rate_control
                })
                .collect();
            let labels: Vec<String> = rcs
                .iter()
                .map(|rc| match rc {
                    RateControl::Auto => {
                        format!("Automatic ({})", s.effective_rate_control().label())
                    }
                    r => r.label().to_string(),
                })
                .collect();
            let rc_row = combo_row(
                "Rate control",
                help::RATE_CONTROL,
                &labels,
                rcs.iter().position(|r| *r == s.rate_control).unwrap_or(0),
            );
            {
                let a = app.clone();
                rc_row.connect_selected_notify(move |r| {
                    let rc = rcs[r.selected() as usize];
                    let a = a.clone();
                    glib::idle_add_local_once(move || {
                        a.edit(id, |j| j.settings.rate_control = rc);
                        a.rebuild_editor();
                    });
                });
            }
            exp.add_row(&rc_row);
            let eff = s.effective_rate_control();
            if eff.uses_bitrate() && !s.bitrate_from_target() {
                let br = spin_row(
                    "Bitrate (kb/s)",
                    help::BITRATE,
                    300.0,
                    200_000.0,
                    100.0,
                    s.bitrate_kbps as f64,
                    0,
                );
                let a = app.clone();
                br.connect_value_notify(move |r| {
                    let v = r.value() as u32;
                    a.edit(id, |j| j.settings.bitrate_kbps = v);
                });
                exp.add_row(&br);
            }
            if eff.uses_bitrate() && eff != RateControl::Cbr && !s.bitrate_from_target() {
                let mr = spin_row(
                    "Max bitrate (kb/s)",
                    help::MAXRATE,
                    0.0,
                    400_000.0,
                    100.0,
                    s.maxrate_kbps as f64,
                    0,
                );
                mr.set_subtitle("0 = 1.5 × bitrate");
                let a = app.clone();
                mr.connect_value_notify(move |r| {
                    let v = r.value() as u32;
                    a.edit(id, |j| j.settings.maxrate_kbps = v);
                });
                exp.add_row(&mr);
            }
            let levels: Vec<String> = std::iter::once("Driver default".to_string())
                .chain((1..=7).map(|n| n.to_string()))
                .collect();
            let cl = combo_row(
                "Speed / quality level",
                help::COMPRESSION,
                &levels,
                s.compression_level.map_or(0, |n| n as usize),
            );
            {
                let a = app.clone();
                cl.connect_selected_notify(move |r| {
                    let v = (r.selected() > 0).then_some(r.selected());
                    a.edit(id, |j| j.settings.compression_level = v);
                });
            }
            exp.add_row(&cl);
            let gop = spin_row(
                "Keyframe interval (frames)",
                help::GOP,
                0.0,
                1200.0,
                1.0,
                s.gop.unwrap_or(0) as f64,
                0,
            );
            gop.set_subtitle(&format!(
                "0 = automatic ({} frames ≈ 2 s)",
                (v.fps * 2.0).round() as u32
            ));
            {
                let a = app.clone();
                gop.connect_value_notify(move |r| {
                    let v = r.value() as u32;
                    a.edit(id, |j| j.settings.gop = (v > 0).then_some(v));
                });
            }
            exp.add_row(&gop);
            let bf_labels: Vec<String> = std::iter::once("Driver default".to_string())
                .chain((0..=4).map(|n| n.to_string()))
                .collect();
            let bf = combo_row(
                "B-frames",
                help::BFRAMES,
                &bf_labels,
                s.bframes.map_or(0, |n| n as usize + 1),
            );
            {
                let a = app.clone();
                bf.connect_selected_notify(move |r| {
                    let v = (r.selected() > 0).then(|| r.selected() - 1);
                    a.edit(id, |j| j.settings.bframes = v);
                });
            }
            exp.add_row(&bf);
            adv.add(&exp);
        }
        let meta = switch_row(
            "Keep metadata and chapters",
            "",
            help::METADATA,
            s.keep_metadata,
        );
        {
            let a = app.clone();
            meta.connect_active_notify(move |r| {
                let v = r.is_active();
                a.edit(id, |j| j.settings.keep_metadata = v);
            });
        }
        adv.add(&meta);
        let extra = adw::EntryRow::builder()
            .title("Extra ffmpeg arguments")
            .text(&s.extra_args)
            .tooltip_text(help::EXTRA_ARGS)
            .show_apply_button(true)
            .build();
        {
            let a = app.clone();
            extra.connect_apply(move |e| {
                let text = e.text().to_string();
                if let Err(err) = crate::ffmpeg::command::split_extra(&text) {
                    a.toast(&err);
                    return;
                }
                a.edit(id, |j| j.settings.extra_args = text);
            });
        }
        adv.add(&extra);
        settings_box.append(&adv);

        // ---- Command ----
        let cmd_group = group("Command", "");
        cmd_group.set_tooltip_text(Some(help::COMMAND));
        let buffer = gtk::TextBuffer::new(None);
        let view = gtk::TextView::builder()
            .buffer(&buffer)
            .editable(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(12)
            .bottom_margin(12)
            .left_margin(12)
            .right_margin(12)
            .build();
        view.add_css_class("command-view");
        let frame = gtk::Frame::new(None);
        frame.set_child(Some(&view));
        frame.add_css_class("card");
        let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy.add_css_class("flat");
        copy.set_tooltip_text(Some("Copy command"));
        {
            let buffer = buffer.clone();
            let a = app.clone();
            copy.connect_clicked(move |b| {
                let (s, e) = buffer.bounds();
                b.clipboard().set_text(&buffer.text(&s, &e, false));
                a.toast("Command copied");
            });
        }
        cmd_group.set_header_suffix(Some(&copy));
        cmd_group.add(&frame);
        content.append(&cmd_group);

        // ---- Actions ----
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        actions.set_halign(gtk::Align::Center);
        let save = gtk::Button::with_label("Save as Preset…");
        save.add_css_class("pill");
        let a = app.clone();
        save.connect_clicked(move |_| a.save_preset_dialog(id));
        let all = gtk::Button::with_label("Apply to All Queued Jobs");
        all.add_css_class("pill");
        all.set_tooltip_text(Some(
            "Copy these settings to every job that hasn't started yet",
        ));
        let a = app.clone();
        all.connect_clicked(move |_| a.apply_to_all(id));
        actions.append(&save);
        actions.append(&all);
        actions.set_sensitive(editable);
        content.append(&actions);

        let clamp = adw::Clamp::builder()
            .maximum_size(820)
            .tightening_threshold(600)
            .child(&content)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&clamp)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let ed = Rc::new(Editor {
            job: id,
            root: scroller.clone().upcast(),
            scroller: Some(scroller),
            issues: Some(issues),
            estimate_row: Some((est, est_spin)),
            audio_usage: Some(usage),
            tracks: track_rows,
            crop_row,
            scan_row,
            vmaf_row,
            command: Some(buffer),
            progress,
        });
        ed.refresh(app);
        ed.update_progress(&app.state.borrow());
        ed
    }

    pub fn update_progress(&self, st: &State) {
        let (Some((bar, label)), Some(job)) = (&self.progress, st.job(self.job)) else {
            return;
        };
        let Some(p) = &job.probe else { return };
        let mut parts = vec![job.stage.clone().unwrap_or_else(|| "Encoding".into())];
        if let Some(pr) = &job.progress {
            bar.set_fraction(pr.fraction(p.duration));
            parts.push(format!("{:.1}%", pr.fraction(p.duration) * 100.0));
            if let Some(eta) = pr.eta(p.duration) {
                parts.push(format!("{} left", format_eta(eta)));
            }
            if let Some(s) = pr.speed {
                parts.push(format!("{s:.2}× speed"));
            }
            if let Some(f) = pr.fps {
                parts.push(format!("{f:.0} fps"));
            }
        }
        label.set_label(&parts.join(" · "));
    }

    /// Update derived rows in place (no widget rebuild).
    pub fn refresh(&self, app: &Rc<App>) {
        let st = app.state.borrow();
        let Some(job) = st.job(self.job) else { return };
        let Some(p) = &job.probe else { return };
        let issues = st.issues(job);
        let kbps = st.video_kbps(job);

        if let Some(g) = &self.issues {
            g.clear();
            for i in &issues {
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&i.message))
                    .title_lines(0)
                    .build();
                row.add_prefix(&severity_icon(i.severity));
                if let Some(fix) = i.fix {
                    let b = gtk::Button::with_label(&fix.label());
                    b.set_valign(gtk::Align::Center);
                    if i.severity == Severity::Strong {
                        b.add_css_class("suggested-action");
                    }
                    let a = app.clone();
                    let id = self.job;
                    b.connect_clicked(move |_| {
                        let a = a.clone();
                        glib::idle_add_local_once(move || a.apply_fix(id, fix));
                    });
                    b.set_sensitive(job.state.editable());
                    row.add_suffix(&b);
                }
                g.add(&row);
            }
            g.group.set_visible(!g.is_empty());
        }

        if let Some((row, spin)) = &self.estimate_row {
            spin.set_visible(job.estimate == EstimateState::Running);
            let mut text = match &job.estimate {
                EstimateState::None => "—".to_string(),
                EstimateState::Running => "Encoding samples…".to_string(),
                EstimateState::Ready(r) => estimate::describe(*r, p.size_bytes),
                EstimateState::Failed(e) => format!("Estimate unavailable: {e}"),
            };
            if let Some(k) = kbps {
                text = format!("Video {:.1} Mb/s · {text}", k as f64 / 1000.0);
            }
            row.set_subtitle(&text);
            let rc = job.settings.effective_rate_control();
            row.set_tooltip_text(Some(
                if job.settings.codec == VideoCodec::Copy
                    || (rc.uses_bitrate() && rc != RateControl::Qvbr)
                {
                    help::ESTIMATE_BITRATE
                } else {
                    help::ESTIMATE
                },
            ));
        }

        if let Some(row) = &self.audio_usage {
            let bytes = job.selection.audio_bytes(p);
            let n = job.selection.selected_audio().count();
            let mut t = format!(
                "{n} track{} · {}",
                if n == 1 { "" } else { "s" },
                format_bytes(bytes)
            );
            if job.settings.size_mode == SizeMode::TargetSize
                && job.settings.codec != VideoCodec::Copy
            {
                t.push_str(&format!(
                    " · {:.0}% of the target",
                    bytes as f64 / (job.settings.target_gb * estimate::GB) * 100.0
                ));
            }
            row.set_subtitle(&t);
        }

        let first_audio = job.selection.selected_audio().next().map(|c| c.index);
        for tr in self.tracks.borrow().iter() {
            let mut parts = Vec::new();
            let mut note = None;
            match tr.kind {
                TrackKind::Audio => {
                    let (Some(a), Some(c)) = (
                        p.audio_by_index(tr.index),
                        job.selection.audio.iter().find(|c| c.index == tr.index),
                    ) else {
                        continue;
                    };
                    parts.push(tracks::language_name(a.language.as_deref()));
                    if let Some(b) = a.bit_rate {
                        parts.push(format!("{:.0} kb/s", b as f64 / 1000.0));
                    }
                    parts.push(format!(
                        "≈ {}",
                        format_bytes(tracks::output_audio_bytes(a, c.action, p.duration))
                    ));
                    if c.selected && first_audio == Some(tr.index) {
                        parts.push("default track".into());
                    }
                    if tracks::is_commentary(a) {
                        parts.push("commentary".into());
                    }
                    if tracks::is_audio_description(a) {
                        parts.push("audio description".into());
                    }
                    note = tracks::conversion_note(a, c.action);
                }
                TrackKind::Sub => {
                    let Some(s) = p.subtitle_by_index(tr.index) else {
                        continue;
                    };
                    parts.push(tracks::language_name(s.language.as_deref()));
                    parts.push(if tracks::is_text_subtitle(&s.codec) {
                        "text".into()
                    } else {
                        "image-based".into()
                    });
                    parts.push(format!(
                        "≈ {}",
                        format_bytes(tracks::subtitle_bytes(s, p.duration))
                    ));
                    if s.disposition.default {
                        parts.push("default".into());
                    }
                    if job.settings.container == Container::Mp4 {
                        note = Some("Dropped in MP4 output".into());
                    }
                }
            }
            let mut sub = glib::markup_escape_text(&parts.join(" · ")).to_string();
            if let Some(n) = note {
                sub.push('\n');
                sub.push_str(&glib::markup_escape_text(&n));
            }
            tr.row.set_subtitle(&sub);
        }

        if let Some((row, dbl)) = &self.scan_row {
            let fps = p.video.as_ref().map_or(0.0, |v| v.fps);
            let plan = job.scan_plan();
            let found = match (&job.crop, job.scan_detected) {
                (CropState::Detecting | CropState::Unknown, _) => "Analysing…".to_string(),
                (_, Some(d)) => format!("Detected: {}", d.label()),
                (_, None) => "Detection inconclusive; using the stream flags".to_string(),
            };
            row.set_subtitle(&format!("{found}\n{}", plan.describe(fps)));
            let deint = matches!(plan, Scan::Deinterlace { .. });
            dbl.set_visible(deint);
            dbl.set_subtitle(&format!(
                "{:.0} fps instead of {:.0}; smoother motion",
                fps * 2.0,
                fps
            ));
        }

        if let Some((row, preview)) = &self.crop_row {
            let v = p.video.as_ref();
            let (w, h) = v.map_or((0, 0), |v| (v.width, v.height));
            let (text, available) = match &job.crop {
                CropState::Unknown => ("Not checked".to_string(), false),
                CropState::Detecting => ("Detecting…".to_string(), false),
                CropState::None => ("No black bars found".to_string(), false),
                CropState::Found(c) => (crop::describe(c, w, h), true),
                CropState::Failed(e) => (format!("Detection failed: {e}"), false),
            };
            let text = if job.dv_preserve && available {
                format!("{text} · {}", help::CROP_DV)
            } else {
                text
            };
            row.set_subtitle(&text);
            row.set_sensitive(available && !job.dv_preserve);
            preview.set_sensitive(true);
        }

        if let Some((row, btn, spin)) = &self.vmaf_row {
            let running = job.vmaf == VmafState::Running;
            spin.set_visible(running);
            btn.set_label(if running { "Cancel" } else { "Check" });
            let codec_ok = st
                .caps
                .as_ref()
                .is_some_and(|c| c.codec(job.settings.codec).available);
            if !running && st.caps.as_ref().is_some_and(|c| c.libvmaf) {
                btn.set_sensitive(codec_ok);
            }
            row.set_subtitle(&match &job.vmaf {
                VmafState::None if !codec_ok => "Needs a working VA-API encoder".to_string(),
                VmafState::None => {
                    "Encode the sample segments and compare them with the source".to_string()
                }
                VmafState::Running => {
                    "Encoding and scoring samples (CPU, may take a while)…".to_string()
                }
                VmafState::Ready(r) => vmaf::describe(*r, p.hdr.is_hdr()),
                VmafState::Failed(e) => format!("Failed: {e}"),
            });
        }

        if let Some(buf) = &self.command {
            buf.set_text(&st.command_text(job));
        }
    }
}
