//! A queue row: file name, source summary, status, progress bar and a cancel/remove button.

use adw::prelude::*;

use crate::checks::Severity;
use crate::ffmpeg::estimate::format_bytes;
use crate::ffmpeg::progress::format_eta;
use crate::queue::{Job, JobState};

pub struct JobRow {
    pub row: gtk::ListBoxRow,
    title: gtk::Label,
    info: gtk::Label,
    status: gtk::Label,
    badge: gtk::Label,
    progress: gtk::ProgressBar,
    warn: gtk::Image,
    pub button: gtk::Button,
}

pub fn duration_text(secs: f64) -> String {
    let s = secs.round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

pub fn source_summary(job: &Job) -> String {
    let Some(p) = &job.probe else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(v) = &p.video {
        let res = match v.height {
            h if h >= 2000 => "4K".to_string(),
            h => format!("{h}p"),
        };
        parts.push(format!(
            "{res} {} {}-bit",
            v.codec.to_uppercase(),
            v.bit_depth
        ));
    }
    parts.push(duration_text(p.duration));
    parts.push(format_bytes(p.size_bytes));
    parts.join(" · ")
}

impl JobRow {
    pub fn new() -> Self {
        let title = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        title.add_css_class("heading");
        let info = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        info.add_css_class("dim-label");
        info.add_css_class("caption");
        let badge = gtk::Label::new(None);
        badge.add_css_class("badge");
        badge.set_valign(gtk::Align::Center);
        let status = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        status.add_css_class("caption");
        let progress = gtk::ProgressBar::new();
        let warn = gtk::Image::from_icon_name("dialog-warning-symbolic");
        warn.set_visible(false);
        let button = gtk::Button::from_icon_name("window-close-symbolic");
        button.add_css_class("flat");
        button.add_css_class("circular");
        button.set_valign(gtk::Align::Center);

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        title.set_hexpand(true);
        top.append(&title);
        top.append(&badge);
        top.append(&warn);
        let info_line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        info_line.append(&info);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 3);
        text.set_hexpand(true);
        text.append(&top);
        text.append(&info_line);
        text.append(&status);
        text.append(&progress);
        let outer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        outer.set_margin_top(10);
        outer.set_margin_bottom(10);
        outer.set_margin_start(12);
        outer.set_margin_end(6);
        outer.append(&text);
        outer.append(&button);
        let row = gtk::ListBoxRow::builder().child(&outer).build();
        Self {
            row,
            title,
            info,
            status,
            badge,
            progress,
            warn,
            button,
        }
    }

    pub fn update(&self, job: &Job, worst: Option<(Severity, String)>) {
        self.title.set_label(&job.file_name());
        self.title
            .set_tooltip_text(Some(&job.input.to_string_lossy()));
        self.info.set_label(&source_summary(job));
        let scan_badge = match job.scan_detected {
            Some(crate::scan::ScanType::Interlaced { .. }) => Some("Interlaced"),
            Some(crate::scan::ScanType::Telecine { .. }) => Some("Telecine"),
            _ => None,
        };
        let badge = match (job.probe.as_ref().and_then(|p| p.hdr.badge()), scan_badge) {
            (Some(h), Some(s)) => Some(format!("{h} · {s}")),
            (h, s) => h.or(s.map(str::to_string)),
        };
        self.badge.set_visible(badge.is_some());
        self.badge.set_label(badge.as_deref().unwrap_or(""));
        let mut status = job.state.label().to_string();
        match job.state {
            JobState::Running => {
                if let Some(s) = &job.stage {
                    status = s.clone();
                }
                if let (Some(p), Some(pr)) = (&job.progress, &job.probe) {
                    self.progress.set_fraction(p.fraction(pr.duration));
                    let mut extra = vec![format!("{:.0}%", p.fraction(pr.duration) * 100.0)];
                    if let Some(eta) = p.eta(pr.duration) {
                        extra.push(format!("{} left", format_eta(eta)));
                    }
                    if let Some(sp) = p.speed {
                        extra.push(format!("{sp:.2}×"));
                    }
                    status = format!("{status} · {}", extra.join(" · "));
                }
            }
            JobState::Done => {
                self.progress.set_fraction(1.0);
                if let Some(first) = job.report.first() {
                    status = format!("Done · {first}");
                }
            }
            JobState::Failed => {
                if let Some(e) = &job.error {
                    status = format!("Failed · {e}");
                }
            }
            JobState::Pending if !job.pgs_pending.is_empty() => {
                status = "Waiting for a subtitle choice".into()
            }
            JobState::Pending => {
                if let Some(d) = job.decode_used {
                    status = format!("Waiting · last run used {}", d.label());
                }
            }
            _ => {}
        }
        if job.state != JobState::Running && job.state != JobState::Done {
            self.progress.set_fraction(0.0);
        }
        self.progress
            .set_visible(matches!(job.state, JobState::Running | JobState::Done));
        self.status.set_label(&status);
        self.status.set_tooltip_text(Some(&status));
        self.status.remove_css_class("error");
        self.status.remove_css_class("success");
        match job.state {
            JobState::Failed => self.status.add_css_class("error"),
            JobState::Done => self.status.add_css_class("success"),
            _ => {}
        }
        match worst {
            Some((sev, msg)) if sev >= Severity::Warning => {
                self.warn.set_visible(true);
                self.warn.set_tooltip_text(Some(&msg));
                self.warn.set_css_classes(&[if sev == Severity::Strong {
                    "error"
                } else {
                    "warning"
                }]);
            }
            _ => self.warn.set_visible(false),
        }
        let (icon, tip) = if job.state == JobState::Running {
            ("process-stop-symbolic", "Cancel encoding")
        } else {
            ("window-close-symbolic", "Remove from queue")
        };
        self.button.set_icon_name(icon);
        self.button.set_tooltip_text(Some(tip));
    }
}
