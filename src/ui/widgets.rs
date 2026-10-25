//! Small widget helpers shared by the UI modules.

use std::cell::RefCell;

use adw::prelude::*;

/// A preferences group whose rows can be cleared and rebuilt.
pub struct DynGroup {
    pub group: adw::PreferencesGroup,
    rows: RefCell<Vec<gtk::Widget>>,
}

impl DynGroup {
    pub fn new(title: &str) -> Self {
        let group = adw::PreferencesGroup::new();
        if !title.is_empty() {
            group.set_title(title);
        }
        Self {
            group,
            rows: RefCell::new(Vec::new()),
        }
    }

    pub fn clear(&self) {
        for w in self.rows.borrow_mut().drain(..) {
            self.group.remove(&w);
        }
    }

    pub fn add(&self, w: &impl IsA<gtk::Widget>) {
        self.group.add(w);
        self.rows.borrow_mut().push(w.clone().upcast());
    }

    pub fn is_empty(&self) -> bool {
        self.rows.borrow().is_empty()
    }
}

pub fn combo_row(title: &str, tooltip: &str, items: &[String], selected: usize) -> adw::ComboRow {
    let refs: Vec<&str> = items.iter().map(String::as_str).collect();
    let row = adw::ComboRow::builder()
        .title(title)
        .tooltip_text(tooltip)
        .model(&gtk::StringList::new(&refs))
        .build();
    row.set_selected(selected as u32);
    row
}

pub fn switch_row(title: &str, subtitle: &str, tooltip: &str, active: bool) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title(title)
        .tooltip_text(tooltip)
        .active(active)
        .build();
    if !subtitle.is_empty() {
        row.set_subtitle(subtitle);
    }
    row
}

pub fn spin_row(
    title: &str,
    tooltip: &str,
    min: f64,
    max: f64,
    step: f64,
    value: f64,
    digits: u32,
) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min, max, step);
    row.set_title(title);
    row.set_tooltip_text(Some(tooltip));
    row.set_digits(digits);
    row.set_value(value);
    row
}

pub fn severity_icon(sev: crate::checks::Severity) -> gtk::Image {
    let (icon, class) = match sev {
        crate::checks::Severity::Info => ("dialog-information-symbolic", "accent"),
        crate::checks::Severity::Warning => ("dialog-warning-symbolic", "warning"),
        crate::checks::Severity::Strong => ("dialog-error-symbolic", "error"),
    };
    let img = gtk::Image::from_icon_name(icon);
    img.add_css_class(class);
    img
}
