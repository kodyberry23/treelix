//! treelix — an nvim-tree-style terminal file explorer for the Helix editor.

mod app;
mod clipboard;
mod config;
mod editor;
mod git;
mod ipc;
mod keymap;
mod marks;
mod render;
mod theme;
mod tree;
mod ui_overlays;
mod watcher;

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::App;
use config::Config;
use theme::Theme;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if matches!(
        args.get(1).map(String::as_str),
        Some("--version") | Some("-V")
    ) {
        println!("treelix {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    // `treelix reveal <path>` — client mode.
    if args.get(1).map(String::as_str) == Some("reveal") {
        let Some(path) = args.get(2) else {
            eprintln!("usage: treelix reveal <path>");
            return ExitCode::from(2);
        };
        return match ipc::send_reveal(path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(1),
        };
    }

    let opts = match parse_args(&args[1..]) {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    if opts.help {
        print_help();
        return ExitCode::SUCCESS;
    }

    let mut config = Config::load();
    if let Some(t) = opts.theme {
        config.theme = t;
    }
    let theme = Theme::load(&config.theme);

    let root = opts
        .root
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let root = root.canonicalize().unwrap_or(root);

    if let Err(e) = run_tui(root, config, theme) {
        eprintln!("treelix: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

struct Opts {
    root: Option<PathBuf>,
    theme: Option<String>,
    help: bool,
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut opts = Opts {
        root: None,
        theme: None,
        help: false,
    };
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-h" | "--help" => opts.help = true,
            "--root" => {
                i += 1;
                let v = args.get(i).ok_or("--root requires a value")?;
                opts.root = Some(PathBuf::from(v));
            }
            "--theme" => {
                i += 1;
                let v = args.get(i).ok_or("--theme requires a value")?;
                opts.theme = Some(v.clone());
            }
            s if s.starts_with("--root=") => {
                opts.root = Some(PathBuf::from(&s["--root=".len()..]));
            }
            s if s.starts_with("--theme=") => {
                opts.theme = Some(s["--theme=".len()..].to_string());
            }
            s if s.starts_with('-') => return Err(format!("unknown flag: {s}")),
            s => opts.root = Some(PathBuf::from(s)),
        }
        i += 1;
    }
    Ok(opts)
}

fn print_help() {
    println!(
        "treelix — nvim-tree-style file explorer for Helix\n\n\
         USAGE:\n  \
         treelix [--root <dir>] [--theme <name>]\n  \
         treelix reveal <path>      reveal a path in a running instance\n\n\
         OPTIONS:\n  \
         --root <dir>    root directory (default: cwd)\n  \
         --theme <name>  theme name, or 'helix' to derive from Helix\n  \
         -V, --version   print version\n  \
         -h, --help      show this help\n\n\
         Press g? inside treelix for keybindings."
    );
}

fn run_tui(root: PathBuf, config: Config, theme: Theme) -> Result<()> {
    let mouse = config.mouse;
    let mut terminal = setup_terminal(mouse)?;
    // Restore the terminal even if the app panics.
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal(mouse);
        eprintln!("{info}");
    }));

    let mut app = App::new(root, config, theme);
    let result = app.run(&mut terminal);

    restore_terminal(mouse)?;
    result
}

fn setup_terminal(mouse: bool) -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    if mouse {
        execute!(stdout, EnableMouseCapture)?;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;
    Ok(terminal)
}

fn restore_terminal(mouse: bool) -> Result<()> {
    let mut stdout = io::stdout();
    if mouse {
        let _ = execute!(stdout, DisableMouseCapture);
    }
    execute!(stdout, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}
