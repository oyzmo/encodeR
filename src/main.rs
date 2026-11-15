mod changelog;
mod checks;
mod cli;
mod crop;
mod ffmpeg;
mod hdr;
mod help;
mod presets;
mod queue;
mod scan;
mod settings;
mod subs;
mod theme;
mod tracks;
mod ui;
mod window;
mod worker;

use std::path::PathBuf;

use adw::prelude::*;

const USAGE: &str = "Usage: encoder [FILE…]
       encoder --print-command FILE [--preset NAME] [--out DIR] [--subs SRT]…
       encoder --encode FILE [--preset NAME] [--out DIR] [--subs SRT]…
       encoder --add-subs VIDEO [SRT…] [--embed] [--no-sync]
       encoder --version";

fn run_cli(mode: &str, rest: &[String]) -> gtk::glib::ExitCode {
    if mode == "--add-subs" {
        return match cli::add_subs(rest) {
            Ok(()) => gtk::glib::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                gtk::glib::ExitCode::FAILURE
            }
        };
    }
    let result = cli::parse_options(rest).and_then(|o| {
        if mode == "--encode" {
            cli::encode(&o)
        } else {
            cli::print_command(&o)
        }
    });
    match result {
        Ok(()) => gtk::glib::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            gtk::glib::ExitCode::FAILURE
        }
    }
}

fn main() -> gtk::glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut files = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--version" | "-v" => {
                println!("encoder {}", window::VERSION);
                return gtk::glib::ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return gtk::glib::ExitCode::SUCCESS;
            }
            "--print-command" | "--encode" | "--add-subs" => {
                return run_cli(&args[i], &args[i + 1..]);
            }
            a if a.starts_with('-') => {
                eprintln!("Unknown option {a}\n{USAGE}");
                return gtk::glib::ExitCode::FAILURE;
            }
            a => files.push(PathBuf::from(a)),
        }
        i += 1;
    }

    let app = adw::Application::builder()
        .application_id(window::APP_ID)
        // Single instance: a second launch passes its files to the open window.
        .flags(gtk::gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.connect_startup(|_| {
        gtk::Window::set_default_icon_name(window::APP_ID);
        // Lets `cargo run` find the icon before it's installed.
        if let Some(d) = gtk::gdk::Display::default() {
            gtk::IconTheme::for_display(&d)
                .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
        }
        theme::install();
        std::thread::spawn(ffmpeg::estimate::sweep_cache);
    });
    app.connect_activate(|app| window::build(app, Vec::new()));
    app.connect_open(|app, files, _| {
        window::build(app, files.iter().filter_map(|f| f.path()).collect());
    });
    // Options are handled above; only the file names go to GApplication,
    // which forwards them to a running instance.
    let mut gargs = vec![args[0].clone()];
    gargs.extend(files.iter().map(|f| f.to_string_lossy().into_owned()));
    app.run_with_args(&gargs)
}
