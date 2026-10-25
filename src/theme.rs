//! The Bindle palette (sage / cream / cocoa / tan) on top of libadwaita.
//! Text pairs are checked against WCAG AA in `dev/contrast.py`.

use gtk::gdk;

const COMMON: &str = "
.badge { border-radius: 999px; padding: 1px 8px; font-size: 0.8em; font-weight: bold; }
.command-view { font-size: 0.9em; background: transparent; }
";

const LIGHT: &str = "
:root {
  --window-bg-color: #FBF5EA; --window-fg-color: #4A3428;
  --view-fg-color: #4A3428;
  --headerbar-bg-color: #F4E7D3; --headerbar-fg-color: #4A3428;
  --sidebar-bg-color: #F4E7D3; --sidebar-fg-color: #4A3428;
  --dialog-bg-color: #FBF5EA; --dialog-fg-color: #4A3428;
  --accent-bg-color: #7FA07A; --accent-fg-color: #2E2019; --accent-color: #8A6A50;
}
.badge { background: #DCE6DA; color: #4A3428; }
";

const DARK: &str = "
:root {
  --window-bg-color: #2E2019; --window-fg-color: #F4E7D3;
  --view-bg-color: #2E2019; --view-fg-color: #F4E7D3;
  --headerbar-bg-color: #2E2019; --headerbar-fg-color: #F4E7D3;
  --sidebar-bg-color: #2E2019; --sidebar-fg-color: #F4E7D3;
  --dialog-bg-color: #2E2019; --dialog-fg-color: #F4E7D3;
  --popover-bg-color: #3A2A21; --popover-fg-color: #F4E7D3;
  --accent-bg-color: #7FA07A; --accent-fg-color: #2E2019; --accent-color: #7FA07A;
}
.badge { background: #4A3428; color: #F4E7D3; }
";

pub fn install() {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let common = gtk::CssProvider::new();
    common.load_from_string(COMMON);
    gtk::style_context_add_provider_for_display(
        &display,
        &common,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let scheme = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(
        &display,
        &scheme,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let manager = adw::StyleManager::default();
    let apply = move |m: &adw::StyleManager| {
        scheme.load_from_string(if m.is_dark() { DARK } else { LIGHT })
    };
    apply(&manager);
    manager.connect_dark_notify(apply);
}
