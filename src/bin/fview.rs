//! Optional iced-based GUI: grep a big CSV file and browse the matching rows.
//!
//! This binary is only built when the `gui` feature is enabled, so the normal
//! `fvalidate` tool keeps compiling with no GUI dependency:
//!
//! ```text
//! cargo run --release --features gui --bin fview
//! cargo run --release --features gui --bin fview -- data.csv
//! cargo run --release --features gui --bin fview -- data.csv -d '\t' --case-sensitive
//! ```
//!
//! The file is optional: with no path the window opens on a welcome screen with
//! an **Open CSV…** button that opens a native file picker (`rfd`).
//!
//! Layout:
//! * the top bar holds the regex filter and the profile controls;
//! * a second bar holds the attribute filter and, when attributes are hidden,
//!   a chip per hidden attribute (click a chip to show the attribute again).
//!   Hidden chips are sorted alphabetically so large attribute lists stay
//!   navigable; the attribute filter narrows the hidden list to the matching
//!   names (and highlights the matching chips in the main view);
//! * every matching row is rendered as a set of `attribute = value` chips, each
//!   with a mute icon that hides that attribute from all rows and moves its name
//!   into the top bar.
//!
//! Profiles: the set of currently visible attributes can be saved under a name
//! and re-applied later. Profiles are persisted as TOML in the platform config
//! directory (`<config>/fview/profiles.toml`).

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};

use clap::{Parser, ValueEnum};
use iced::widget::scrollable::AbsoluteOffset;
use iced::widget::text::Wrapping;
use iced::widget::{
    button, column, container, horizontal_rule, pick_list, row, scrollable, text, text_input, Row,
    Space,
};
use iced::{Background, Border, Center, Element, Fill, Length, Subscription, Task, Theme};
use iced_fonts::{Bootstrap, BOOTSTRAP_FONT, BOOTSTRAP_FONT_BYTES};
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use simd_csv::ByteRecord;

const BUFFER_CAPACITY: usize = 64 * 1024;
/// Spacing between chips, in px.
const CHIP_SPACING: f32 = 8.0;
/// Non-text width of a chip: padding + mute icon + inner spacing.
const CHIP_CHROME: f32 = 48.0;
/// Non-text height of a chip: vertical padding + border.
const CHIP_CHROME_V: f32 = 8.0;
/// Height of a single line of chip text.
const CHIP_LINE_HEIGHT: f32 = 16.0;
/// A chip may wrap to at most this many lines; longer values are clipped.
/// Capping the height is what lets every data stripe have a fixed height, which
/// in turn makes the virtual scrolling below exact.
const MAX_CHIP_LINES: usize = 2;
/// Vertical padding of a data stripe (kept in sync with `container.padding`).
const STRIPE_PADDING: f32 = 8.0;
/// Spacing between the chip lines inside a stripe.
const CHIP_LINE_SPACING: f32 = 6.0;
/// Rows rendered above and below the viewport so scrolling does not flash gaps.
const OVERSCAN_ROWS: usize = 3;
/// Preferred chip width used to decide how many chips fit on a line.
const TARGET_CHIP_WIDTH: f32 = 280.0;
/// Rough width of one character at size 13, used to decide text wrapping.
const CHAR_WIDTH: f32 = 7.2;

#[derive(Parser, Debug, Clone)]
#[command(name = "fview", about = "Grep and browse rows of a big CSV file (GUI)")]
struct Args {
    /// CSV file to view. Optional: without it, use the Open button.
    path: Option<PathBuf>,

    /// Field delimiter (single byte; use '\t' for a tab).
    #[arg(short = 'd', long, default_value = ",")]
    delimiter: String,

    /// Make the filter regex case-sensitive (case-insensitive by default).
    #[arg(short = 's', long, default_value_t = false)]
    case_sensitive: bool,

    /// Maximum number of matching rows to display (default 100).
    #[arg(short = 'n', long, default_value_t = 100)]
    limit: usize,

    /// Rendering backend. `auto` prefers the GPU (wgpu) and falls back to the
    /// CPU renderer (tiny-skia) when no GPU is available; the other values
    /// force a specific backend.
    #[arg(long, value_enum, default_value_t = Backend::Auto)]
    backend: Backend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Backend {
    /// Prefer the GPU, fall back to the CPU renderer.
    Auto,
    /// Force the wgpu (GPU) renderer.
    Wgpu,
    /// Force the tiny-skia (CPU) renderer.
    TinySkia,
}

#[derive(Debug, Clone)]
struct ScanResult {
    rows: Vec<Vec<String>>,
    truncated: bool,
    /// Number of data rows actually read from the file. When `truncated` is
    /// false this is the total number of rows in the file.
    rows_read: usize,
}

/// A named set of visible attributes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ProfileConfig {
    #[serde(default)]
    visible: Vec<String>,
}

/// The whole persisted configuration file (`<config>/fview/profiles.toml`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Config {
    #[serde(default)]
    profiles: BTreeMap<String, ProfileConfig>,
}

/// Resolve the path of the TOML profile store.
fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("fview").join("profiles.toml"))
}

/// Load the profile store, falling back to an empty set on any I/O or parse
/// error (a broken config should never stop the viewer from opening).
fn load_config() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

/// Persist the profile store as pretty TOML, creating the directory if needed.
fn store_config(config: &Config) -> Result<(), String> {
    let Some(path) = config_path() else {
        return Err("cannot determine a config directory".into());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let text =
        toml::to_string_pretty(config).map_err(|e| format!("cannot serialize profiles: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Case-insensitive substring test used by the attribute filter. An empty
/// filter never matches (used for highlighting) and never hides (used for the
/// hidden list), so callers handle the empty case explicitly where needed.
fn attr_matches(filter: &str, name: &str) -> bool {
    let filter = filter.trim();
    !filter.is_empty() && name.to_lowercase().contains(&filter.to_lowercase())
}

/// Note shown when the hidden-attribute list is collapsed, so the count stays
/// visible without spending vertical space on the chips.
fn hidden_note(count: usize) -> String {
    match count {
        1 => "1 hidden attribute available — use the Hidden button to reveal it".to_string(),
        _ => format!(
            "{count} hidden attributes available — use the Hidden button to reveal them"
        ),
    }
}

/// Status line for the current scan. `rows_read` is the number of data rows
/// read; when the scan was not truncated it is the total number of rows in the
/// file.
fn status_text(matched: usize, rows_read: usize, truncated: bool) -> String {
    if truncated {
        format!("showing first {matched} matching rows (more available) · {rows_read} rows read")
    } else if matched == rows_read {
        format!("{rows_read} rows")
    } else {
        format!("{matched} matching rows of {rows_read} total")
    }
}

/// Layout geometry derived from the window width: chips per line, the maximum
/// chip width, and how many hidden-attribute chips fit on a line.
fn chip_layout(width: f32) -> (usize, f32, usize) {
    let available = (width - 28.0).max(200.0);
    let columns = ((available / TARGET_CHIP_WIDTH).floor() as usize).max(1);
    let chip_max = ((available - (columns.saturating_sub(1) as f32) * CHIP_SPACING)
        / columns as f32)
        .max(120.0);
    let hidden_columns = ((available / 170.0).floor() as usize).max(1);
    (columns, chip_max, hidden_columns)
}

/// Height reserved for one line of chips, tall enough for the maximum number
/// of wrapped lines a chip may show.
fn chip_line_box() -> f32 {
    MAX_CHIP_LINES as f32 * CHIP_LINE_HEIGHT + CHIP_CHROME_V
}

/// Fixed height of a data stripe showing `lines` lines of chips. Used both to
/// place rows and to give each stripe exactly that height, keeping virtual
/// scrolling stable.
fn stripe_height(lines: usize) -> f32 {
    let lines = lines.max(1);
    lines as f32 * chip_line_box()
        + (lines - 1) as f32 * CHIP_LINE_SPACING
        + 2.0 * STRIPE_PADDING
}

#[derive(Debug, Clone)]
enum Message {
    OpenFile,
    FileChosen(Option<PathBuf>),
    FilterChanged(String),
    RunFilter,
    /// Hide an attribute (column index) from the rows.
    Mute(usize),
    /// Show a previously hidden attribute again.
    Unmute(usize),
    UnmuteAll,
    /// Hide every attribute at once, so a few can be picked back.
    MuteAll,
    /// Expand or collapse the list of hidden attribute chips.
    ToggleHidden,
    /// The attribute search box changed: highlight matching chips and narrow
    /// the hidden attribute list.
    AttributeFilterChanged(String),
    /// A saved profile was picked from the dropdown.
    ProfileSelected(String),
    /// Clear the selected profile (attributes stay as they are).
    ClearProfile,
    /// Overwrite the selected profile with the current visible attributes.
    SaveCurrentProfile,
    /// Open the "save as new profile" name prompt.
    BeginSaveNewProfile,
    NewProfileNameChanged(String),
    ConfirmSaveNewProfile,
    CancelSaveNewProfile,
    /// `(generation, result)`; stale generations are ignored.
    ScanFinished(u64, Result<ScanResult, String>),
    /// The window was resized; used to wrap chips and to size the virtual list.
    Resized(f32, f32),
    /// The scroll position changed; drives the virtual row window.
    Scrolled(scrollable::Viewport),
}

struct Viewer {
    path: Option<PathBuf>,
    delimiter: u8,
    case_sensitive: bool,
    limit: usize,
    headers: Vec<String>,
    muted: HashSet<usize>,
    /// Whether the list of hidden attribute chips in the top bar is expanded.
    show_hidden: bool,
    filter: String,
    rows: Vec<Vec<String>>,
    truncated: bool,
    /// Number of data rows read by the last completed scan.
    rows_read: usize,
    error: Option<String>,
    scanning: bool,
    dirty: bool,
    generation: u64,
    window_width: f32,
    /// Stable id of the row scrollable, so the view can jump back to the top.
    scroll_id: scrollable::Id,
    /// Vertical scroll offset of the row list, in px.
    scroll_offset: f32,
    /// Height of the row viewport, in px.
    viewport_height: f32,
    /// Attribute search: highlights matching chips in the main view and narrows
    /// the hidden attribute list to the matching names.
    attribute_filter: String,
    /// Saved profiles, keyed by name.
    profiles: BTreeMap<String, ProfileConfig>,
    /// Profile currently applied, if any.
    current_profile: Option<String>,
    /// Whether the "save as new profile" name prompt is open.
    naming_profile: bool,
    new_profile_name: String,
    /// Short feedback message about profile actions (shown next to the controls).
    profile_status: Option<String>,
}

impl Viewer {
    fn new(args: Args) -> (Self, Task<Message>) {
        let delimiter = parse_delimiter(&args.delimiter).unwrap_or(b',');

        let mut viewer = Viewer {
            path: None,
            delimiter,
            case_sensitive: args.case_sensitive,
            limit: args.limit.max(1),
            headers: Vec::new(),
            muted: HashSet::new(),
            show_hidden: false,
            filter: String::new(),
            rows: Vec::new(),
            truncated: false,
            rows_read: 0,
            error: None,
            scanning: false,
            dirty: false,
            generation: 0,
            window_width: 1200.0,
            scroll_id: scrollable::Id::unique(),
            scroll_offset: 0.0,
            viewport_height: 720.0,
            attribute_filter: String::new(),
            profiles: load_config().profiles,
            current_profile: None,
            naming_profile: false,
            new_profile_name: String::new(),
            profile_status: None,
        };

        let task = match args.path {
            Some(path) => viewer.load_file(path),
            None => Task::none(),
        };

        (viewer, task)
    }

    /// Open a native file picker on a background task.
    fn pick_file() -> Task<Message> {
        Task::perform(
            async {
                rfd::AsyncFileDialog::new()
                    .add_filter("CSV / TSV", &["csv", "tsv", "txt"])
                    .pick_file()
                    .await
                    .map(|handle| handle.path().to_path_buf())
            },
            Message::FileChosen,
        )
    }

    /// Switch to a new file: reset the per-file state, read its headers and
    /// start scanning. Any in-flight scan is invalidated by the generation bump.
    fn load_file(&mut self, path: PathBuf) -> Task<Message> {
        self.generation += 1;
        self.scanning = false;
        self.dirty = false;
        self.rows.clear();
        self.truncated = false;
        self.rows_read = 0;
        self.muted.clear();
        self.show_hidden = false;
        self.error = None;
        self.current_profile = None;
        self.naming_profile = false;
        self.new_profile_name.clear();
        self.profile_status = None;
        self.path = Some(path.clone());

        match read_headers(&path, self.delimiter) {
            Ok(headers) => {
                self.headers = headers;
                self.start_scan()
            }
            Err(message) => {
                self.headers.clear();
                self.error = Some(message);
                Task::none()
            }
        }
    }

    /// Names of the attributes currently visible (the active set).
    fn visible_names(&self) -> Vec<String> {
        self.headers
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.muted.contains(index))
            .map(|(_, name)| name.clone())
            .collect()
    }

    /// Apply a saved profile: attributes listed in it become visible, every
    /// other attribute is treated as hidden.
    fn apply_profile(&mut self, name: &str) {
        let Some(profile) = self.profiles.get(name).cloned() else {
            self.profile_status = Some(format!("unknown profile “{name}”"));
            return;
        };
        let visible: HashSet<&str> = profile.visible.iter().map(String::as_str).collect();
        self.muted = self
            .headers
            .iter()
            .enumerate()
            .filter(|(_, header)| !visible.contains(header.as_str()))
            .map(|(index, _)| index)
            .collect();
        self.current_profile = Some(name.to_string());
        self.profile_status = Some(format!("applied “{name}”"));
    }

    /// Persist the current profile set to the TOML store.
    fn persist(&self) -> Result<(), String> {
        store_config(&Config {
            profiles: self.profiles.clone(),
        })
    }

    /// Overwrite the profile that is currently selected with the attributes
    /// that are visible right now.
    fn save_current_profile(&mut self) {
        let Some(name) = self.current_profile.clone() else {
            return;
        };
        let visible = self.visible_names();
        self.profiles.insert(name.clone(), ProfileConfig { visible });
        match self.persist() {
            Ok(()) => self.profile_status = Some(format!("saved “{name}”")),
            Err(message) => self.profile_status = Some(message),
        }
    }

    /// Save the visible attributes under the name typed in the prompt.
    fn save_new_profile(&mut self) {
        let name = self.new_profile_name.trim().to_string();
        if name.is_empty() {
            self.profile_status = Some("enter a profile name".into());
            return;
        }
        let visible = self.visible_names();
        self.profiles.insert(name.clone(), ProfileConfig { visible });
        match self.persist() {
            Ok(()) => {
                self.current_profile = Some(name.clone());
                self.naming_profile = false;
                self.new_profile_name.clear();
                self.profile_status = Some(format!("saved “{name}”"));
            }
            Err(message) => self.profile_status = Some(message),
        }
    }

    /// Kick off a background scan. If one is already running we just mark the
    /// state dirty, which coalesces bursts of typing into a single re-scan.
    fn start_scan(&mut self) -> Task<Message> {
        let Some(path) = self.path.clone() else {
            return Task::none();
        };
        if self.scanning {
            self.dirty = true;
            return Task::none();
        }

        self.error = None;
        self.scanning = true;
        self.dirty = false;
        self.generation += 1;
        let generation = self.generation;
        let delimiter = self.delimiter;
        let pattern = self.filter.clone();
        let case_sensitive = self.case_sensitive;
        let limit = self.limit;

        Task::perform(
            async move { scan(path, delimiter, pattern, case_sensitive, limit) },
            move |result| Message::ScanFinished(generation, result),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OpenFile => Self::pick_file(),
            Message::FileChosen(Some(path)) => self.load_file(path),
            Message::FileChosen(None) => Task::none(),
            Message::FilterChanged(value) => {
                self.filter = value;
                self.start_scan()
            }
            Message::RunFilter => self.start_scan(),
            Message::Mute(index) => {
                self.muted.insert(index);
                self.profile_status = None;
                Task::none()
            }
            Message::Unmute(index) => {
                self.muted.remove(&index);
                self.profile_status = None;
                Task::none()
            }
            Message::UnmuteAll => {
                self.muted.clear();
                self.profile_status = None;
                Task::none()
            }
            Message::MuteAll => {
                self.muted = (0..self.headers.len()).collect();
                self.profile_status = None;
                Task::none()
            }
            Message::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                Task::none()
            }
            Message::AttributeFilterChanged(value) => {
                self.attribute_filter = value;
                Task::none()
            }
            Message::ProfileSelected(name) => {
                self.apply_profile(&name);
                Task::none()
            }
            Message::ClearProfile => {
                self.current_profile = None;
                self.profile_status = None;
                Task::none()
            }
            Message::SaveCurrentProfile => {
                self.save_current_profile();
                Task::none()
            }
            Message::BeginSaveNewProfile => {
                self.naming_profile = true;
                self.new_profile_name.clear();
                self.profile_status = None;
                Task::none()
            }
            Message::NewProfileNameChanged(value) => {
                self.new_profile_name = value;
                Task::none()
            }
            Message::ConfirmSaveNewProfile => {
                self.save_new_profile();
                Task::none()
            }
            Message::CancelSaveNewProfile => {
                self.naming_profile = false;
                self.new_profile_name.clear();
                self.profile_status = None;
                Task::none()
            }
            Message::Resized(width, height) => {
                // Keep the virtual viewport fresh so a taller window renders more
                // rows without waiting for the next scroll event.
                self.viewport_height = height.max(1.0);
                // Only rebuild the chip layout when the number of chips per line
                // actually changes; resize events fire continuously while a
                // window edge is dragged.
                let (old_columns, _, old_hidden) = chip_layout(self.window_width);
                let (new_columns, _, new_hidden) = chip_layout(width);
                if old_columns != new_columns || old_hidden != new_hidden {
                    self.window_width = width;
                }
                Task::none()
            }
            Message::Scrolled(viewport) => {
                self.scroll_offset = viewport.absolute_offset().y;
                self.viewport_height = viewport.bounds().height.max(1.0);
                Task::none()
            }
            Message::ScanFinished(generation, result) => {
                if generation != self.generation {
                    return Task::none();
                }
                match result {
                    Ok(scan) => {
                        self.rows = scan.rows;
                        self.truncated = scan.truncated;
                        self.rows_read = scan.rows_read;
                        self.error = None;
                    }
                    Err(message) => self.error = Some(message),
                }
                self.scanning = false;
                self.scroll_offset = 0.0;
                // Jump the row list back to the top for the new result set.
                let reset = scrollable::scroll_to(
                    self.scroll_id.clone(),
                    AbsoluteOffset { x: 0.0, y: 0.0 },
                );
                if self.dirty {
                    self.dirty = false;
                    Task::batch([reset, self.start_scan()])
                } else {
                    reset
                }
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        iced::window::resize_events()
            .map(|(_id, size)| Message::Resized(size.width, size.height))
    }

    fn view(&self) -> Element<'_, Message> {
        // No file yet: welcome screen with an Open button.
        if self.path.is_none() {
            return container(
                column![
                    text("No CSV file opened").size(22),
                    text("Grep and browse rows of a large CSV file.").size(14),
                    button(
                        row![
                            text(char::from(Bootstrap::FolderFill))
                                .font(BOOTSTRAP_FONT)
                                .size(16),
                            text("Open CSV…").size(16),
                        ]
                        .spacing(8)
                        .align_y(Center),
                    )
                    .on_press(Message::OpenFile)
                    .padding([10, 22]),
                ]
                .spacing(16)
                .align_x(Center),
            )
            .center_x(Fill)
            .center_y(Fill)
            .into();
        }

        let open = button(
            row![
                text(char::from(Bootstrap::FolderFill))
                    .font(BOOTSTRAP_FONT)
                    .size(14),
                text("Open…"),
            ]
            .spacing(6)
            .align_y(Center),
        )
        .on_press(Message::OpenFile)
        .padding([8, 14]);

        let file_name = self
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();

        let filter = text_input("regex filter, e.g. \\d{4}-\\d{2}", &self.filter)
            .on_input(Message::FilterChanged)
            .on_submit(Message::RunFilter)
            .padding(8)
            .size(15)
            .width(Fill);

        let search = button(
            row![
                text(char::from(Bootstrap::Search)).font(BOOTSTRAP_FONT),
                text("Search"),
            ]
            .spacing(6)
            .align_y(Center),
        )
        .on_press(Message::RunFilter)
        .padding([8, 14]);

        let status: Element<'_, Message> = if let Some(error) = &self.error {
            row![
                text(char::from(Bootstrap::ExclamationTriangle))
                    .font(BOOTSTRAP_FONT)
                    .color([0.85, 0.25, 0.2]),
                text(error.as_str()).color([0.85, 0.25, 0.2]),
            ]
            .spacing(6)
            .align_y(Center)
            .into()
        } else if self.scanning {
            text("scanning…").into()
        } else {
            text(status_text(self.rows.len(), self.rows_read, self.truncated)).into()
        };

        let top = row![
            open,
            text(file_name).size(13),
            text("Filter:").size(15),
            filter,
            search,
            status
        ]
        .spacing(10)
        .align_y(Center)
        .padding(10);

        // Hidden attributes live in the top bar; clicking restores them.
        let (columns, chip_max, hidden_columns) = chip_layout(self.window_width);

        let mut hidden_bar = column![].spacing(4).padding([4, 10]);
        let mut controls = Row::new().spacing(6).align_y(Center);
        controls = controls.push(
            button(
                row![
                    text(char::from(Bootstrap::EyeSlash))
                        .font(BOOTSTRAP_FONT)
                        .size(13),
                    text("Hide all").size(13),
                ]
                .spacing(5)
                .align_y(Center),
            )
            .on_press(Message::MuteAll)
            .padding([3, 8])
            .style(button::secondary),
        );
        if !self.muted.is_empty() {
            // Collapsible list of hidden attributes: long lists would otherwise
            // push the data rows off screen.
            let caret = if self.show_hidden {
                Bootstrap::CaretDown
            } else {
                Bootstrap::CaretRight
            };
            controls = controls.push(
                button(
                    row![
                        text(char::from(caret)).font(BOOTSTRAP_FONT).size(13),
                        text(format!("Hidden ({})", self.muted.len())).size(13),
                    ]
                    .spacing(5)
                    .align_y(Center),
                )
                .on_press(Message::ToggleHidden)
                .padding([3, 8])
                .style(button::secondary),
            );
            controls = controls.push(
                button(text("show all").size(13))
                    .on_press(Message::UnmuteAll)
                    .padding([3, 8])
                    .style(button::text),
            );
        }
        // Attribute filter: highlights matching chips in the main view and
        // narrows the hidden attribute list below to the matching names.
        controls = controls.push(text("Attributes:").size(13));
        controls = controls.push(
            text_input("filter attributes…", &self.attribute_filter)
                .on_input(Message::AttributeFilterChanged)
                .padding(6)
                .size(13)
                .width(Length::Fixed(200.0)),
        );

        // Profile controls: pick a saved profile, overwrite it, or save the
        // current visible set under a new name.
        let profile_names: Vec<String> = self.profiles.keys().cloned().collect();
        controls = controls.push(text("Profile:").size(13));
        controls = controls.push(
            pick_list(
                profile_names,
                self.current_profile.clone(),
                Message::ProfileSelected,
            )
            .placeholder("none")
            .padding(6)
            .text_size(13),
        );
        if self.current_profile.is_some() {
            controls = controls.push(
                button(text("Save").size(13))
                    .on_press(Message::SaveCurrentProfile)
                    .padding([3, 8])
                    .style(button::primary),
            );
            controls = controls.push(
                button(text("clear").size(13))
                    .on_press(Message::ClearProfile)
                    .padding([3, 8])
                    .style(button::text),
            );
        }
        controls = controls.push(
            button(text("Save as new…").size(13))
                .on_press(Message::BeginSaveNewProfile)
                .padding([3, 8])
                .style(button::secondary),
        );
        if let Some(status) = &self.profile_status {
            controls = controls.push(text(status.as_str()).size(12));
        }
        hidden_bar = hidden_bar.push(controls);

        // Prompt for the name of a new profile.
        if self.naming_profile {
            hidden_bar = hidden_bar.push(
                row![
                    text("New profile name:").size(13),
                    text_input("profile name", &self.new_profile_name)
                        .on_input(Message::NewProfileNameChanged)
                        .on_submit(Message::ConfirmSaveNewProfile)
                        .padding(6)
                        .size(13)
                        .width(Length::Fixed(200.0)),
                    button(text("Save").size(13))
                        .on_press(Message::ConfirmSaveNewProfile)
                        .padding([3, 8])
                        .style(button::primary),
                    button(text("Cancel").size(13))
                        .on_press(Message::CancelSaveNewProfile)
                        .padding([3, 8])
                        .style(button::text),
                ]
                .spacing(6)
                .align_y(Center),
            );
        }

        // The hidden attribute names are sorted alphabetically so a large
        // attribute list stays easy to scan. The list is collapsed by default
        // (a long list would otherwise push the rows off screen); it opens when
        // the user toggles it or searches for an attribute. When collapsed only
        // a note with the count is shown.
        if !self.muted.is_empty() {
            let searching = !self.attribute_filter.trim().is_empty();
            if !self.show_hidden && !searching {
                hidden_bar = hidden_bar.push(text(hidden_note(self.muted.len())).size(13));
            } else {
                let mut indices: Vec<usize> = self.muted.iter().copied().collect();
                indices.sort_by(|a, b| {
                    let left = self.headers.get(*a).map(String::as_str).unwrap_or("");
                    let right = self.headers.get(*b).map(String::as_str).unwrap_or("");
                    left.to_lowercase()
                        .cmp(&right.to_lowercase())
                        .then_with(|| a.cmp(b))
                });
                let mut line = Row::new().spacing(6).align_y(Center);
                let mut count = 0usize;
                let mut shown = 0usize;
                for index in indices {
                    let Some(name) = self.headers.get(index) else {
                        continue;
                    };
                    if searching && !attr_matches(&self.attribute_filter, name) {
                        continue;
                    }
                    if count == hidden_columns {
                        hidden_bar = hidden_bar.push(line);
                        line = Row::new().spacing(6).align_y(Center);
                        count = 0;
                    }
                    line = line.push(
                        button(
                            row![
                                text(char::from(Bootstrap::Eye)).font(BOOTSTRAP_FONT).size(13),
                                text(name).size(13),
                            ]
                            .spacing(5)
                            .align_y(Center),
                        )
                        .on_press(Message::Unmute(index))
                        .padding([3, 8])
                        .style(button::secondary),
                    );
                    count += 1;
                    shown += 1;
                }
                if shown > 0 {
                    hidden_bar = hidden_bar.push(line);
                } else {
                    hidden_bar =
                        hidden_bar.push(text("no hidden attributes match the filter").size(13));
                }
            }
        }

        let all_hidden = !self.headers.is_empty() && self.muted.len() >= self.headers.len();

        let mut list = column![].spacing(0).padding(0);
        if self.rows.is_empty() {
            let message = if self.scanning {
                "scanning…"
            } else {
                "no rows match the filter"
            };
            list = list.push(container(text(message).size(14)).padding(12));
        } else if all_hidden {
            list = list.push(
                container(
                    text("All attributes hidden — reveal the Hidden list above, then click an attribute to display it.")
                        .size(14),
                )
                .padding(12),
            );
        } else {
            // Virtual scrolling: only the stripes intersecting the viewport (plus
            // a small overscan) are built, so a long list costs the same per frame
            // as a short one. Every stripe is given the same fixed height, which
            // keeps the computed offsets exact.
            let visible_headers = self
                .headers
                .iter()
                .enumerate()
                .filter(|(index, _)| !self.muted.contains(index))
                .count();
            let line_count = visible_headers.div_ceil(columns).max(1);
            let row_height = stripe_height(line_count);
            let total_rows = self.rows.len();
            let viewport = self.viewport_height.max(1.0);

            let first = ((self.scroll_offset / row_height).floor() as usize)
                .saturating_sub(OVERSCAN_ROWS)
                .min(total_rows);
            let last = ((((self.scroll_offset + viewport) / row_height).ceil() as usize)
                + OVERSCAN_ROWS
                + 1)
                .min(total_rows)
                .max(first);

            if first > 0 {
                list = list.push(Space::with_height(Length::Fixed(
                    first as f32 * row_height,
                )));
            }

            for (position, values) in self.rows[first..last].iter().enumerate() {
                let index = first + position;
                let visible: Vec<usize> = (0..values.len())
                    .filter(|column| !self.muted.contains(column))
                    .collect();
                let mut block = column![].spacing(CHIP_LINE_SPACING);
                for chunk in visible.chunks(columns) {
                    let mut line = Row::new().spacing(CHIP_SPACING);
                    for &column in chunk {
                        let header = self.headers.get(column).map(String::as_str).unwrap_or("?");
                        let highlight = attr_matches(&self.attribute_filter, header);
                        line = line.push(chip(header, &values[column], column, chip_max, highlight));
                    }
                    // Clip each chip line to a fixed height so a very long value
                    // cannot make one stripe taller than the rest.
                    block = block.push(
                        container(line)
                            .height(Length::Fixed(chip_line_box()))
                            .clip(true),
                    );
                }
                list = list.push(
                    container(block)
                        .width(Fill)
                        .height(Length::Fixed(row_height))
                        .clip(true)
                        .padding([STRIPE_PADDING, 12.0])
                        .style(stripe_style(index % 2 == 1)),
                );
            }

            if last < total_rows {
                list = list.push(Space::with_height(Length::Fixed(
                    (total_rows - last) as f32 * row_height,
                )));
            }
        }

        column![
            top,
            hidden_bar,
            horizontal_rule(1).style(divider_style),
            scrollable(list)
                .id(self.scroll_id.clone())
                .on_scroll(Message::Scrolled)
                .height(Fill)
                .width(Fill)
        ]
        .spacing(0)
        .into()
    }
}

/// Divider between the toolbar and the rows. Uses the theme's text color so it
/// stays distinct from both the plain and the striped (`background.weak`) row
/// backgrounds that the default `Rule` color blends into.
fn divider_style(theme: &Theme) -> iced::widget::rule::Style {
    let palette = theme.extended_palette();
    iced::widget::rule::Style {
        color: palette.background.base.text,
        width: 1,
        radius: 0.0.into(),
        fill_mode: iced::widget::rule::FillMode::Full,
    }
}

/// Alternating background used to stripe the data rows.
fn stripe_style(active: bool) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        if !active {
            return container::Style::default();
        }
        let weak = theme.extended_palette().background.weak.color;
        container::Style {
            background: Some(Background::Color(weak)),
            ..container::Style::default()
        }
    }
}

/// Chip style: a transparent background (the row color shows through) with a
/// border, so a chip never blends into the plain or the striped row background.
/// Highlighted chips (those matching the attribute filter) get an accent
/// background and border.
fn chip_style(theme: &Theme, highlight: bool) -> container::Style {
    let palette = theme.extended_palette();
    if highlight {
        return container::Style {
            background: Some(Background::Color(palette.primary.weak.color)),
            border: Border {
                color: palette.primary.strong.color,
                width: 1.5,
                radius: 8.0.into(),
            },
            ..container::Style::default()
        };
    }
    container::Style {
        background: None,
        border: Border {
            color: palette.background.strong.color,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..container::Style::default()
    }
}

/// A single `attribute = value` chip with a mute icon. `highlight` marks chips
/// whose attribute name matches the attribute filter.
fn chip<'a>(
    header: &'a str,
    value: &'a str,
    index: usize,
    max_width: f32,
    highlight: bool,
) -> Element<'a, Message> {
    let label_text = format!("{header} = {value}");
    let content_width = (max_width - CHIP_CHROME).max(60.0);
    let label = if estimate_chip_width(&label_text) > max_width {
        text(label_text)
            .size(13)
            .width(Length::Fixed(content_width))
            .wrapping(Wrapping::Word)
    } else {
        text(label_text).size(13)
    };

    let mute = button(text(char::from(Bootstrap::EyeSlash)).font(BOOTSTRAP_FONT).size(14))
        .on_press(Message::Mute(index))
        .padding(2)
        .style(button::text);

    container(row![label, mute].spacing(6).align_y(Center))
        .padding([3, 8])
        .style(move |theme| chip_style(theme, highlight))
        .into()
}

/// Rough estimate of a chip's width, used to decide whether its label must
/// wrap. Slightly overestimates so chips err on the side of wrapping.
fn estimate_chip_width(label: &str) -> f32 {
    label.chars().count() as f32 * CHAR_WIDTH + CHIP_CHROME
}

/// Read the header row once so chips can be labelled before the first scan.
fn read_headers(path: &Path, delimiter: u8) -> Result<Vec<String>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut builder = simd_csv::ReaderBuilder::with_capacity(BUFFER_CAPACITY);
    builder.delimiter(delimiter).has_headers(true);
    let mut reader = builder.from_reader(file);

    let headers = reader
        .byte_headers()
        .map_err(|e| format!("cannot read headers of {}: {e}", path.display()))?;

    Ok(headers
        .iter()
        .map(|cell| String::from_utf8_lossy(cell).into_owned())
        .collect())
}

/// Stream the whole file, count every match and keep the first `limit` rows.
/// Runs on a background thread through `Task::perform`.
fn scan(
    path: PathBuf,
    delimiter: u8,
    pattern: String,
    case_sensitive: bool,
    limit: usize,
) -> Result<ScanResult, String> {
    let regex = if pattern.trim().is_empty() {
        None
    } else {
        Some(
            RegexBuilder::new(&pattern)
                .case_insensitive(!case_sensitive)
                .build()
                .map_err(|e| format!("invalid regex: {e}"))?,
        )
    };

    let file = File::open(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut builder = simd_csv::ReaderBuilder::with_capacity(BUFFER_CAPACITY);
    builder
        .delimiter(delimiter)
        .has_headers(true)
        .flexible(true);
    let mut reader = builder.from_reader(file);

    let mut record = ByteRecord::new();
    let mut rows = Vec::new();
    let mut truncated = false;
    let mut rows_read = 0usize;

    loop {
        match reader.read_byte_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => return Err(format!("error reading {}: {e}", path.display())),
        }
        rows_read += 1;

        let is_match = match &regex {
            None => true,
            Some(regex) => record
                .iter()
                .any(|cell| regex.is_match(&String::from_utf8_lossy(cell))),
        };

        if is_match {
            if rows.len() < limit {
                rows.push(
                    record
                        .iter()
                        .map(|cell| String::from_utf8_lossy(cell).into_owned())
                        .collect(),
                );
            } else {
                // Stop scanning: only the first `limit` matching rows are shown.
                truncated = true;
                break;
            }
        }
    }

    Ok(ScanResult {
        rows,
        truncated,
        rows_read,
    })
}

fn parse_delimiter(raw: &str) -> Result<u8, String> {
    match raw {
        "\\t" | "tab" | "TAB" => Ok(b'\t'),
        other => {
            let bytes = other.as_bytes();
            if bytes.len() == 1 {
                Ok(bytes[0])
            } else {
                Err(format!("delimiter must be a single byte, got '{other}'"))
            }
        }
    }
}

fn main() -> iced::Result {
    let args = Args::parse();

    // iced selects the compositor from `ICED_BACKEND` (or automatically when it
    // is unset, which is `wgpu` followed by `tiny-skia`).
    match args.backend {
        Backend::Auto => {}
        Backend::Wgpu => std::env::set_var("ICED_BACKEND", "wgpu"),
        Backend::TinySkia => std::env::set_var("ICED_BACKEND", "tiny-skia"),
    }

    iced::application("fview — CSV viewer", Viewer::update, Viewer::view)
        .subscription(Viewer::subscription)
        .font(BOOTSTRAP_FONT_BYTES)
        .window_size((1200.0, 820.0))
        .run_with(move || Viewer::new(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_through_toml() {
        let mut config = Config::default();
        config.profiles.insert(
            "compact".into(),
            ProfileConfig {
                visible: vec!["id".into(), "name".into()],
            },
        );
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("[profiles.compact]"));
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.profiles["compact"].visible, vec!["id", "name"]);
    }

    #[test]
    fn attr_matches_is_case_insensitive_and_empty_never_matches() {
        assert!(attr_matches("NAME", "full_name"));
        assert!(attr_matches("  name ", "full_name"));
        assert!(!attr_matches("", "full_name"));
        assert!(!attr_matches("   ", "full_name"));
        assert!(!attr_matches("age", "full_name"));
    }

    #[test]
    fn hidden_note_pluralizes() {
        assert_eq!(hidden_note(1).matches("attribute").count(), 1);
        assert!(hidden_note(1).contains("1 hidden attribute available"));
        assert!(hidden_note(42).contains("42 hidden attributes available"));
    }

    #[test]
    fn status_text_reports_rows_read() {
        assert_eq!(
            status_text(100, 101, true),
            "showing first 100 matching rows (more available) · 101 rows read"
        );
        assert_eq!(status_text(7, 500, false), "7 matching rows of 500 total");
        assert_eq!(status_text(500, 500, false), "500 rows");
    }

    #[test]
    fn scan_counts_rows_read_and_stops_at_limit() {
        let path = std::env::temp_dir().join(format!("fview-scan-{}.csv", std::process::id()));
        std::fs::write(&path, "a,b\n1,2\n3,4\n5,6\n").unwrap();

        // The limit is hit mid-file, so only part of it is read.
        let limited = scan(path.clone(), b',', String::new(), false, 2).unwrap();
        assert_eq!(limited.rows.len(), 2);
        assert!(limited.truncated);
        assert_eq!(limited.rows_read, 3);

        // A large enough limit reads the whole file.
        let full = scan(path.clone(), b',', String::new(), false, 10).unwrap();
        assert_eq!(full.rows.len(), 3);
        assert!(!full.truncated);
        assert_eq!(full.rows_read, 3);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stripe_height_grows_with_line_count() {
        // Virtual offsets rely on a stable, positive height.
        assert!(stripe_height(1) > 0.0);
        assert!(stripe_height(3) > stripe_height(2));
        assert!(stripe_height(2) > stripe_height(1));
        // Degenerate input must not collapse to zero height.
        assert_eq!(stripe_height(0), stripe_height(1));
    }

    #[test]
    fn chip_layout_is_always_usable() {
        for width in [0.0, 100.0, 600.0, 1200.0, 4000.0] {
            let (columns, chip_max, hidden_columns) = chip_layout(width);
            assert!(columns >= 1);
            assert!(hidden_columns >= 1);
            assert!(chip_max >= 120.0);
        }
        // A wider window fits at least as many chips per line.
        assert!(chip_layout(2400.0).0 >= chip_layout(800.0).0);
    }
}
