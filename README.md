<p align="center">
  <img src="assets/treelix-wordmark.png" alt="treelix" width="640">
</p>

An [nvim-tree](https://github.com/nvim-tree/nvim-tree.lua)-style terminal file
explorer for the [Helix](https://helix-editor.com) editor, written in Rust.

Helix has no plugin system and nothing like nvim-tree. treelix is a standalone
sidebar process — drop it in a [zellij](https://zellij.dev) (or tmux/wezterm)
pane next to Helix and it behaves like nvim-tree: a live file tree with git
status, file-watching auto-reload, file operations, and theme matching.
Selecting a file opens it in your already-running Helix instance.

It was built to replace [broot](https://dystroy.org/broot/) as the file sidebar.

## Features

- **Tree view** with Nerd Font icons, indent guides, dirs-first sorting, lazy
  loading, and a `~`-shortened root header.
- **Git status** (per-file glyphs + colors, propagated to folders) via the
  `git` CLI — staged `✓`, dirty `✗`, untracked `★`, renamed `➜`, deleted,
  conflict, ignored `◌`.
- **Auto-reload**: a debounced filesystem watcher refreshes the tree when files
  change on disk (mirrors nvim-tree's `filesystem_watchers`).
- **File operations**: create, rename, delete (with confirm), trash, cut/copy/
  paste, and copy-path-to-clipboard.
- **Open in Helix**: `<CR>` opens the file in the running Helix over its Unix
  socket (helix-editor/helix PR #13896), with vsplit/hsplit and system-open.
- **Reveal**: a tiny IPC socket lets Helix tell treelix to reveal the current
  buffer (`treelix reveal <path>`), replacing broot's `--listen`/`--send`.
- **Theming**: treelix owns its theme via its own theme file and ships a
  built-in Deep Nord Aurora theme. It can also derive colors from your active
  Helix theme (`theme = "helix"`).

## Install

Requires a Nerd Font. Build from source (needs a Rust toolchain):

```sh
git clone https://github.com/kodyberry23/treelix ~/projects/treelix
cargo install --path ~/projects/treelix   # installs `treelix` into ~/.cargo/bin
```

Or grab a prebuilt macOS binary from the [latest release](https://github.com/kodyberry23/treelix/releases/latest):

```sh
arch=$(uname -m | sed 's/arm64/aarch64/')   # aarch64 or x86_64
curl -fsSL "https://github.com/kodyberry23/treelix/releases/latest/download/treelix-${arch}-apple-darwin.tar.gz" \
  | tar -xz
install "treelix-${arch}-apple-darwin/treelix" ~/.cargo/bin/   # or anywhere on PATH
```

### Releases

Tagged releases (`vX.Y.Z`) build and publish macOS `aarch64`/`x86_64` binaries via
GitHub Actions. To cut one: bump `version` in `Cargo.toml`, then
`git tag vX.Y.Z && git push --tags` (the tag must match the Cargo.toml version).

## Usage

```sh
treelix [--root <dir>] [--theme <name>]   # run the sidebar TUI (root: cwd)
treelix reveal <path>                      # reveal a path in a running instance
```

Press `?` (or `g?`) inside treelix for the keybinding help panel.

### Keybindings (nvim-tree defaults)

| Key | Action |
|---|---|
| `j` / `k`, arrows | down / up |
| `K` / `J` | first / last sibling |
| `>` / `<` | next / previous sibling |
| `<CR>` / `o` | open file / toggle directory |
| `l` / `h` | expand / collapse · go to parent |
| `P` | move cursor to parent |
| `<C-]>` | cd into directory (re-root) |
| `-` | re-root to parent |
| `E` / `W` | expand all / collapse all |
| `L` | toggle group-empty directories |
| `]c` / `[c` | next / previous git change |
| `<Tab>` | preview in Helix (keeps focus in treelix) |
| `<C-v>` / `<C-x>` | open in vsplit / hsplit |
| `s` | system open |
| `a` | create (trailing `/` = directory) |
| `d` / `<Del>` | delete (confirm) |
| `D` | trash |
| `r` / `e` / `u` | rename / rename basename / rename full path |
| `<C-r>` | rename omitting filename |
| `x` / `c` / `p` | cut / copy / paste |
| `y` / `Y` / `gy` | copy filename / relative path / absolute path |
| `<C-k>` | file info popup |
| `m` | toggle bookmark |
| `bd` / `bt` / `bmv` | bulk delete / trash / move bookmarked |
| `v` | select node (multi-select) |
| `f` / `F` | live filter start / clear |
| `Esc` | clear active filter, then selection, then pending keys |
| `S` | search node by name |
| `.` | toggle hidden **and** git-ignored (one combined toggle) |
| `C` | toggle git-clean (changed only) |
| `U` / `B` / `M` | toggle custom / no-buffer / no-bookmark filter |
| `R` | refresh |
| `?` / `g?` | help |
| `q` | quit |

Selection-aware ops: when nodes are multi-selected with `v`, the delete / trash /
cut / copy actions operate on the whole selection. Bulk `bd` / `bt` / `bmv` act on
bookmarked nodes (`m`).

Live filter (`f`): matches **files** by name while keeping folders visible so the
tree stays navigable (nvim-tree's `always_show_folders`; set
`live_filter_show_folders = false` to also hide non-matching folders). Like
nvim-tree, it searches only nodes that are currently loaded — press `E`
(expand-all) first to filter across the whole tree. `Esc` or `F` clears it.

## Configuration

Optional `~/.config/treelix/config.toml`:

```toml
theme = "nord-aurora"   # a treelix theme name, or "helix" to derive from Helix
icons = true            # Nerd Font glyphs (false → ASCII fallbacks)
arrows = false          # show expand/collapse arrows before folder icons
show_hidden = false     # initial state of dotfiles (`.` toggles this + ignored)
show_ignored = false    # initial state of git-ignored entries
sort = "name"           # name | modified | extension | filetype
files_first = false     # place files before directories
group_empty = false     # collapse chains of sole-child dirs into one line
mouse = true            # click to open/cd, scroll to move
bookmarks_persist = false   # persist bookmarks to ~/.config/treelix/bookmarks
live_filter_show_folders = true   # keep folders visible during `f` (filter files only)
exclude = []            # substring patterns hidden when custom filter (U) is on
# special_files = ["cargo.toml", "makefile", "readme.md", ...]   # highlighted
# open_command = "~/projects/helix-files/scripts/dispatch-to-editor.sh {mode} {path}"
```

### Themes

A treelix theme is a TOML file (`~/.config/treelix/themes/<name>.toml`) with a
`[palette]` of `name = "#hex"` and a `[styles]` table mapping treelix elements
(`text`, `directory`, `git_staged`, …) to `"fg"`, `"fg / bg"`, or
`"fg / bg / mods"` specs. See [`themes/nord-aurora.toml`](themes/nord-aurora.toml).

## Opening files in Helix

By default treelix routes `<CR>` to the dotfiles dispatcher
`~/projects/helix-files/scripts/dispatch-to-editor.sh` (which sends `:open`/
`:vsplit` to Helix over its per-session socket and focuses the editor pane,
falling back to spawning a fresh `hx` pane). If that script isn't present,
treelix performs the same dispatch itself, resolving the socket from
`HELIX_SOCKET_PATH` / `$XDG_RUNTIME_DIR/helix/<session>.sock`. Override with
`open_command` or the `TREELIX_DISPATCH_TO_EDITOR` env var.

## Roadmap

**Implemented:** tree render with icons/indent guides, git status (+ folder
propagation), auto-reload/file-watching, full file ops, open-in-Helix
(vsplit/hsplit/preview/system), reveal IPC, theming + Helix-theme import,
hidden/ignored/git-clean/custom/no-buffer/no-bookmark filters, live filter
(`f`/`F`) and search (`S`) via `nucleo`, sort modes + files-first, group-empty
dirs (`L`), git navigation (`[c`/`]c`), sibling navigation (`>`/`<`),
marks/bookmarks (`m`,`M`,`bd`,`bt`,`bmv`, with persistence), visual multi-select
bulk ops, rename variants (`r`/`e`/`u`/`<C-r>`), file-info popup (`<C-k>`),
special-file highlighting, current-file highlight, and mouse support.

**Helix-aware** (the open-file highlight, `no_buffer` filter, and follow-on-reveal
work by Helix telling treelix its current buffer over the reveal socket — see the
[helix-files](https://github.com/kodyberry23/helix-files) integration).

**Not feasible standalone** (depend on the editor's LSP / live buffer set):

- Diagnostics column (`]e` / `[e`, severity icons) — needs Helix's LSP state.
- Window picker (`O`), open-in-place (`<C-e>`), new-tab (`<C-t>`), float window —
  Neovim window/buffer concepts with no standalone analog.
- True modified indicator and a complete `no_buffer` set require Helix's full
  buffer list; treelix approximates `no_buffer` with the files it has opened/
  revealed this session.

## License

MIT
