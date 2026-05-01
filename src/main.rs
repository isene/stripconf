use crust::{Crust, Pane, Input};
use crust::style;
use std::path::PathBuf;
use std::process::{Command, Stdio};

// strip's globals + per-segment fields. The segment grammar is:
//   segment NAME [+EXTRA_GAP_PX] [#RRGGBB] CMD [args...] [INTERVAL_S]
// strip parses INTERVAL by scanning back for trailing digits.

#[derive(Clone)]
struct Segment {
    enabled: bool,                // false → emit with leading `#`
    name: String,                 // strip caps to 31 chars internally
    gap_override: u32,            // 0 = no `+N` prefix
    color: String,                // "" = no `#RRGGBB` prefix → use cfg_fg
    command: String,              // verbatim cmd (may contain `sh -c '...'`)
    interval: u32,                // 0 = static
    comments_above: Vec<String>,  // verbatim lines preserved before this segment
}

struct App {
    top: Pane,
    left: Pane,
    right: Pane,
    status: Pane,
    cat_index: usize,
    item_index: usize,            // Globals: which key. Segments: which segment.
    globals: Globals,
    segments: Vec<Segment>,
    leading_comments: Vec<String>, // file-top comments before first segment/key
    dirty: bool,
    config_path: PathBuf,
}

#[derive(Clone)]
struct Globals {
    height: u32,
    top_offset: u32,
    bg: String,
    fg: String,
    gap: u32,
    font: String,                 // empty = strip default
    char_width: u32,              // 0 = strip default
    baseline: u32,                // 0 = strip default
}

impl Default for Globals {
    fn default() -> Self {
        Globals {
            height: 22,
            top_offset: 0,
            bg: "#000000".into(),
            fg: "#cccccc".into(),
            gap: 8,
            font: String::new(),
            char_width: 0,
            baseline: 0,
        }
    }
}

const GLOBAL_KEYS: &[(&str, &str)] = &[
    ("height",     "bar height in pixels"),
    ("top_offset", "vertical offset from top of screen 0 (px)"),
    ("bg",         "bar background colour (#RRGGBB)"),
    ("fg",         "default segment foreground (#RRGGBB)"),
    ("gap",        "default gap between segments (px)"),
    ("font",       "X core font name (empty = strip default)"),
    ("char_width", "monospace cell width override (0 = default)"),
    ("baseline",   "font baseline offset (0 = default)"),
];

impl App {
    fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let config_path = PathBuf::from(&home).join(".striprc");
        let (cols, rows) = Crust::terminal_size();
        let split = 22u16;
        let lw = split - 1;
        let rx = split + 3;
        let rw = cols.saturating_sub(rx).saturating_sub(1);

        let mut app = App {
            top: Pane::new(1, 1, cols, 1, 0, 236),
            left: Pane::new(2, 3, lw, rows.saturating_sub(4), 255, 0),
            right: Pane::new(rx, 3, rw, rows.saturating_sub(4), 252, 0),
            status: Pane::new(1, rows, cols, 1, 252, 236),
            cat_index: 0,
            item_index: 0,
            globals: Globals::default(),
            segments: Vec::new(),
            leading_comments: Vec::new(),
            dirty: false,
            config_path,
        };
        app.left.border = true;
        app.right.border = true;
        app.load_config();
        app
    }

    // --- config I/O -------------------------------------------------

    fn load_config(&mut self) {
        let content = match std::fs::read_to_string(&self.config_path) {
            Ok(c) => c, Err(_) => return,
        };
        // pending_comments accumulates comment / blank lines. When a
        // segment line lands they become its `comments_above`. Anything
        // still pending at EOF stays in leading_comments (or is trailed
        // back at save time).
        let mut pending: Vec<String> = Vec::new();
        let mut have_first_segment = false;

        for raw in content.lines() {
            let trimmed = raw.trim();
            // Blank / pure-comment line.
            if trimmed.is_empty() || trimmed.starts_with('#') {
                // Detect "disabled segment" pattern: `# segment foo ...`.
                if let Some(stripped) = trimmed.strip_prefix('#') {
                    let s = stripped.trim_start();
                    if let Some(rest) = s.strip_prefix("segment ") {
                        let mut seg = parse_segment_body(rest);
                        seg.enabled = false;
                        seg.comments_above = std::mem::take(&mut pending);
                        if !have_first_segment {
                            self.leading_comments.append(&mut seg.comments_above);
                            have_first_segment = true;
                        }
                        self.segments.push(seg);
                        continue;
                    }
                }
                pending.push(raw.to_string());
                continue;
            }

            // segment NAME ...
            if let Some(rest) = trimmed.strip_prefix("segment ") {
                let mut seg = parse_segment_body(rest);
                seg.enabled = true;
                seg.comments_above = std::mem::take(&mut pending);
                if !have_first_segment {
                    self.leading_comments.append(&mut seg.comments_above);
                    have_first_segment = true;
                }
                self.segments.push(seg);
                continue;
            }

            // key = value
            if let Some((k, v)) = trimmed.split_once('=') {
                let key = k.trim();
                let val = v.trim();
                if self.apply_global(key, val) {
                    // Recognised global; the hardcoded globals block
                    // in save_config() will re-emit it from struct
                    // state. Don't push to pending or it would be
                    // duplicated in the output.
                    continue;
                }
                // Unknown key: preserve the original line verbatim so
                // a future strip option (or a hand-edited key) survives
                // a glassconf-style round-trip clobber.
                pending.push(raw.to_string());
                continue;
            }

            // Unknown line: forward to pending so it survives a save.
            pending.push(raw.to_string());
        }
        // Any leftover comments at EOF: stash them under the final
        // segment's "comments_above" reservoir slot (a synthetic empty
        // segment-after marker would be fancier; we just append to
        // leading_comments so save flushes them at the bottom-end via
        // a synthesised tail block).
        if !pending.is_empty() {
            // Mark with sentinel: prepend to leading_comments only if
            // there's no segment at all — otherwise dangle as a final
            // trailer by appending to last segment's tail comments.
            if let Some(last) = self.segments.last_mut() {
                last.comments_above.extend(std::mem::take(&mut pending).into_iter().map(|s| {
                    // Tag with marker so save can route to file bottom.
                    format!("\u{0001}{}", s)
                }));
            } else {
                self.leading_comments.append(&mut pending);
            }
        }
    }

    // Apply a global `key = value` line. Returns true if `key` is a
    // known global (so the caller can drop the source line — save
    // re-emits it from struct state); false if unknown (the caller
    // preserves the verbatim line).
    fn apply_global(&mut self, key: &str, val: &str) -> bool {
        match key {
            "height"     => { if let Ok(n) = val.parse() { self.globals.height = n; } true }
            "top_offset" => { if let Ok(n) = val.parse() { self.globals.top_offset = n; } true }
            "bg"         => { if normalize_hex(val).is_some() { self.globals.bg = normalize_hex(val).unwrap(); } true }
            "fg"         => { if normalize_hex(val).is_some() { self.globals.fg = normalize_hex(val).unwrap(); } true }
            "gap"        => { if let Ok(n) = val.parse() { self.globals.gap = n; } true }
            "font"       => { self.globals.font = val.to_string(); true }
            "char_width" => { if let Ok(n) = val.parse() { self.globals.char_width = n; } true }
            "baseline"   => { if let Ok(n) = val.parse() { self.globals.baseline = n; } true }
            _ => false,
        }
    }

    fn save_config(&self) {
        let mut out = String::new();
        for c in &self.leading_comments { out.push_str(c); out.push('\n'); }
        if !self.leading_comments.is_empty() { out.push('\n'); }

        // Globals block.
        out.push_str(&format!("height     = {}\n", self.globals.height));
        out.push_str(&format!("top_offset = {}\n", self.globals.top_offset));
        out.push_str(&format!("bg         = {}\n", self.globals.bg));
        out.push_str(&format!("fg         = {}\n", self.globals.fg));
        out.push_str(&format!("gap        = {}\n", self.globals.gap));
        if !self.globals.font.is_empty() {
            out.push_str(&format!("font       = {}\n", self.globals.font));
        }
        if self.globals.char_width != 0 {
            out.push_str(&format!("char_width = {}\n", self.globals.char_width));
        }
        if self.globals.baseline != 0 {
            out.push_str(&format!("baseline   = {}\n", self.globals.baseline));
        }
        out.push('\n');

        // Segments in order; preserve attached comment blocks.
        let mut tail_comments: Vec<String> = Vec::new();
        for seg in &self.segments {
            for c in &seg.comments_above {
                if let Some(rest) = c.strip_prefix('\u{0001}') {
                    tail_comments.push(rest.to_string());
                } else {
                    out.push_str(c);
                    out.push('\n');
                }
            }
            out.push_str(&serialize_segment(seg));
            out.push('\n');
        }
        for c in tail_comments { out.push_str(&c); out.push('\n'); }

        atomic_write(&self.config_path, out.as_bytes());
    }

    // --- reload-strip -----------------------------------------------

    // strip has no SIGUSR1 path: a config-affecting change requires a
    // full restart. -x = exact name match (so we don't kill stripconf
    // itself). setsid + null fds detaches the new strip from our
    // controlling tty so it survives if stripconf exits.
    fn restart_strip() -> bool {
        let killed = Command::new("pkill")
            .args(["-x", "strip"])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .status().map(|s| s.success()).unwrap_or(false);
        // Brief pause so the kernel reaps before re-spawn.
        std::thread::sleep(std::time::Duration::from_millis(120));
        let _ = Command::new("setsid")
            .arg("strip")
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .spawn();
        killed
    }

    fn prompt_reload(status: &mut Pane) {
        status.say(&style::fg(" Saved. Restart strip? (y/N)", 220));
        let Some(k) = Input::getchr(None) else { return; };
        match k.as_str() {
            "y" | "Y" => {
                let was_running = Self::restart_strip();
                let msg = if was_running { " Saved + restarted strip" } else { " Saved + spawned strip" };
                status.say(&style::fg(msg, 82));
            }
            _ => { status.say(&style::fg(" Saved (no restart)", 82)); }
        }
        std::thread::sleep(std::time::Duration::from_millis(700));
    }

    // --- helpers ----------------------------------------------------

    fn fg24(text: &str, hex: &str) -> String {
        if let Some((r, g, b)) = parse_hex(hex) {
            format!("\x1b[38;2;{};{};{}m{}\x1b[0m", r, g, b, text)
        } else { text.to_string() }
    }
    fn bg24(text: &str, hex: &str) -> String {
        if let Some((r, g, b)) = parse_hex(hex) {
            format!("\x1b[48;2;{};{};{}m{}\x1b[0m", r, g, b, text)
        } else { text.to_string() }
    }

    fn category_count(&self) -> usize { 2 }                 // Bar globals + Segments
    fn category_name(&self, i: usize) -> &'static str {
        match i { 0 => "Bar globals", _ => "Segments" }
    }

    fn current_items_len(&self) -> usize {
        match self.cat_index {
            0 => GLOBAL_KEYS.len(),
            _ => self.segments.len(),
        }
    }

    fn clamp_item_index(&mut self) {
        let len = self.current_items_len();
        if self.item_index >= len.max(1) { self.item_index = len.saturating_sub(1); }
    }

    // --- render -----------------------------------------------------

    fn render(&mut self) {
        let dirty_mark = if self.dirty { " [modified]" } else { "" };
        let preview = format!(" {} ",
            Self::bg24(&Self::fg24(" cpu mem disk ", &self.globals.fg), &self.globals.bg));
        self.top.say(&format!(" stripconf{}    bar:{}", dirty_mark, preview));

        // Left: category names.
        let mut lines = Vec::new();
        for i in 0..self.category_count() {
            let name = self.category_name(i);
            if i == self.cat_index {
                lines.push(style::reverse(&format!(" {} ", name)));
            } else {
                lines.push(format!(" {} ", name));
            }
        }
        self.left.set_text(&lines.join("\n"));
        self.left.ix = 0;
        self.left.full_refresh();

        match self.cat_index {
            0 => self.render_globals(),
            _ => self.render_segments(),
        }

        let len = self.current_items_len();
        let hint = match self.cat_index {
            0 => "j/k:item h/l:adjust Enter:edit W/s:save q:quit",
            _ => "j/k:seg Shift-J/K:reorder t:toggle a:add d:del Enter:edit W/s:save q:quit",
        };
        self.status.say(&format!(" {}/{}  {}",
            self.item_index + 1, len.max(1), hint));
    }

    fn render_globals(&mut self) {
        let mut lines = Vec::new();
        lines.push(style::fg(&style::bold("Bar globals"), 81));
        lines.push(style::fg(&"\u{2500}".repeat(40), 245));
        lines.push(String::new());

        for (i, (key, _help)) in GLOBAL_KEYS.iter().enumerate() {
            let selected = i == self.item_index;
            let label = format!("{:<12}", key);
            let label = if selected { style::underline(&label) } else { label };
            let al = if selected { "\u{25C0} " } else { "  " };
            let ar = if selected { " \u{25B6}" } else { "  " };

            let val_str = match *key {
                "height"     => format!("{}", self.globals.height),
                "top_offset" => format!("{}", self.globals.top_offset),
                "bg"         => format!("{} {}", Self::bg24("    ", &self.globals.bg), self.globals.bg),
                "fg"         => format!("{} {}", Self::bg24("    ", &self.globals.fg), self.globals.fg),
                "gap"        => format!("{}", self.globals.gap),
                "font"       => if self.globals.font.is_empty() {
                                    style::fg("(default)", 245)
                                } else { self.globals.font.clone() },
                "char_width" => if self.globals.char_width == 0 { style::fg("(default)", 245) }
                                else { format!("{}", self.globals.char_width) },
                "baseline"   => if self.globals.baseline == 0 { style::fg("(default)", 245) }
                                else { format!("{}", self.globals.baseline) },
                _ => String::new(),
            };
            lines.push(format!("  {}{}{}{}", label, al, val_str, ar));
        }
        // Help line for selected key.
        if let Some((_, help)) = GLOBAL_KEYS.get(self.item_index) {
            lines.push(String::new());
            lines.push(style::fg(help, 245));
        }
        self.right.set_text(&lines.join("\n"));
        self.right.ix = 0;
        self.right.full_refresh();
    }

    fn render_segments(&mut self) {
        let mut lines = Vec::new();
        lines.push(style::fg(&style::bold("Segments (top → left of bar)"), 81));
        lines.push(style::fg(&"\u{2500}".repeat(60), 245));
        lines.push(String::new());

        if self.segments.is_empty() {
            lines.push(style::fg("  (no segments — press `a` to add one)", 245));
        } else {
            for (i, seg) in self.segments.iter().enumerate() {
                let selected = i == self.item_index;
                let mark = if seg.enabled { style::fg("\u{25CF}", 82) }
                           else { style::fg("\u{25CB}", 240) };
                let gap_str = if seg.gap_override > 0 {
                    style::fg(&format!("+{:<3}", seg.gap_override), 245)
                } else { "    ".into() };
                let col_str = if seg.color.is_empty() {
                    "      ".into()
                } else {
                    Self::bg24("  ", &seg.color)
                };
                let interval_str = if seg.interval == 0 {
                    style::fg(" static", 245)
                } else { format!("{:>3}s   ", seg.interval) };

                let name = format!("{:<10}", truncate(&seg.name, 10));
                let cmd = truncate(&seg.command, 50);
                let row = format!("  {} {} {} {} {} {}",
                    mark, name, gap_str, col_str, interval_str,
                    style::fg(&cmd, if seg.enabled { 252 } else { 240 }));
                if selected {
                    lines.push(style::reverse(&row));
                } else {
                    lines.push(row);
                }
            }
        }
        lines.push(String::new());
        lines.push(style::fg("Edit fields: Name → +Gap → #Color → Command → Interval", 245));
        lines.push(style::fg("Empty input keeps current value. Color: blank to clear.", 245));

        self.right.set_text(&lines.join("\n"));
        self.right.ix = 0;
        self.right.full_refresh();
    }

    // --- navigation -------------------------------------------------

    fn move_down(&mut self) {
        let len = self.current_items_len();
        if self.item_index + 1 < len { self.item_index += 1; }
    }
    fn move_up(&mut self) {
        if self.item_index > 0 { self.item_index -= 1; }
    }
    fn next_category(&mut self) {
        if self.cat_index + 1 < self.category_count() {
            self.cat_index += 1; self.item_index = 0;
        }
    }
    fn prev_category(&mut self) {
        if self.cat_index > 0 { self.cat_index -= 1; self.item_index = 0; }
    }

    // h/l adjust: globals only — increments numbers, cycles ignored
    // for strings.
    fn adjust(&mut self, delta: i32) {
        if self.cat_index != 0 { return; }
        let key = match GLOBAL_KEYS.get(self.item_index) { Some((k, _)) => *k, None => return };
        let bump = |v: u32, d: i32, max: u32| -> u32 {
            let n = (v as i64 + d as i64).clamp(0, max as i64);
            n as u32
        };
        match key {
            "height"     => { self.globals.height     = bump(self.globals.height, delta, 200); self.dirty = true; }
            "top_offset" => { self.globals.top_offset = bump(self.globals.top_offset, delta, 200); self.dirty = true; }
            "gap"        => { self.globals.gap        = bump(self.globals.gap, delta, 200); self.dirty = true; }
            "char_width" => { self.globals.char_width = bump(self.globals.char_width, delta, 64); self.dirty = true; }
            "baseline"   => { self.globals.baseline   = bump(self.globals.baseline, delta, 64); self.dirty = true; }
            _ => {}
        }
    }

    // --- editing ----------------------------------------------------

    fn edit_value(&mut self) {
        match self.cat_index {
            0 => self.edit_global(),
            _ => self.edit_segment(),
        }
    }

    fn edit_global(&mut self) {
        let (key, _) = match GLOBAL_KEYS.get(self.item_index) { Some(x) => *x, None => return };
        let initial = match key {
            "height"     => self.globals.height.to_string(),
            "top_offset" => self.globals.top_offset.to_string(),
            "bg"         => self.globals.bg.clone(),
            "fg"         => self.globals.fg.clone(),
            "gap"        => self.globals.gap.to_string(),
            "font"       => self.globals.font.clone(),
            "char_width" => self.globals.char_width.to_string(),
            "baseline"   => self.globals.baseline.to_string(),
            _ => return,
        };
        let new_val = self.ask(&format!("{}: ", key), &initial);
        if new_val.is_empty() { return; }
        match key {
            "height" | "top_offset" | "gap" | "char_width" | "baseline" => {
                if let Ok(n) = new_val.parse::<u32>() {
                    self.apply_global(key, &n.to_string());
                    self.dirty = true;
                }
            }
            "bg" | "fg" => {
                if let Some(n) = normalize_hex(&new_val) {
                    self.apply_global(key, &n);
                    self.dirty = true;
                }
            }
            "font" => { self.globals.font = new_val; self.dirty = true; }
            _ => {}
        }
    }

    fn edit_segment(&mut self) {
        let i = self.item_index;
        if i >= self.segments.len() { return; }

        // Sequential field prompts. Empty input = keep.
        let (cur_name, cur_gap, cur_col, cur_cmd, cur_int) = {
            let s = &self.segments[i];
            (s.name.clone(), s.gap_override.to_string(), s.color.clone(),
             s.command.clone(), s.interval.to_string())
        };

        let name = self.ask("Name: ", &cur_name);
        if !name.is_empty() { self.segments[i].name = name; self.dirty = true; }

        let gap = self.ask("Extra gap (px, 0 for none): ", &cur_gap);
        if !gap.is_empty() {
            if let Ok(n) = gap.parse::<u32>() { self.segments[i].gap_override = n; self.dirty = true; }
        }

        let color = self.ask("Color (#RRGGBB, blank to use default fg): ", &cur_col);
        // For colour, an empty string explicitly means "clear" — so only
        // treat it as keep-current if the user pressed ESC (returns empty
        // *and* matches the original with the clear-sentinel below).
        // crust::ask returns empty for both ESC and explicit clear; we
        // can't distinguish, so we apply: empty = clear.
        if color != cur_col {
            if color.is_empty() {
                self.segments[i].color.clear();
                self.dirty = true;
            } else if let Some(n) = normalize_hex(&color) {
                self.segments[i].color = n;
                self.dirty = true;
            }
        }

        let cmd = self.ask("Command (full, with args): ", &cur_cmd);
        if !cmd.is_empty() { self.segments[i].command = cmd; self.dirty = true; }

        let interval = self.ask("Interval seconds (0 = static): ", &cur_int);
        if !interval.is_empty() {
            if let Ok(n) = interval.parse::<u32>() { self.segments[i].interval = n; self.dirty = true; }
        }
    }

    fn ask(&mut self, prompt: &str, initial: &str) -> String {
        let orig_bg = self.status.bg;
        self.status.bg = 18;
        let v = self.status.ask(prompt, initial);
        self.status.bg = orig_bg;
        v.trim().to_string()
    }

    // Toggle enabled flag of selected segment.
    fn toggle_segment(&mut self) {
        if self.cat_index != 1 || self.item_index >= self.segments.len() { return; }
        self.segments[self.item_index].enabled = !self.segments[self.item_index].enabled;
        self.dirty = true;
    }

    fn delete_segment(&mut self) {
        if self.cat_index != 1 || self.item_index >= self.segments.len() { return; }
        self.status.say(&style::fg(&format!(
            " Delete segment '{}'? (y/N)",
            self.segments[self.item_index].name), 220));
        let Some(k) = Input::getchr(None) else { return };
        if k != "y" && k != "Y" { return; }
        self.segments.remove(self.item_index);
        self.clamp_item_index();
        self.dirty = true;
    }

    fn add_segment(&mut self) {
        if self.cat_index != 1 { return; }
        let name = self.ask("New segment name: ", "");
        if name.is_empty() { return; }
        let cmd = self.ask("Command: ", "");
        if cmd.is_empty() { return; }
        let interval = self.ask("Interval (0 = static): ", "0");
        let interval = interval.parse::<u32>().unwrap_or(0);
        self.segments.push(Segment {
            enabled: true,
            name,
            gap_override: 0,
            color: String::new(),
            command: cmd,
            interval,
            comments_above: Vec::new(),
        });
        self.item_index = self.segments.len() - 1;
        self.dirty = true;
    }

    fn move_segment_down(&mut self) {
        if self.cat_index != 1 || self.item_index + 1 >= self.segments.len() { return; }
        self.segments.swap(self.item_index, self.item_index + 1);
        self.item_index += 1;
        self.dirty = true;
    }
    fn move_segment_up(&mut self) {
        if self.cat_index != 1 || self.item_index == 0 || self.item_index >= self.segments.len() { return; }
        self.segments.swap(self.item_index, self.item_index - 1);
        self.item_index -= 1;
        self.dirty = true;
    }
}

// --- per-segment parser ----------------------------------------------

// Parse the body following `segment ` (or `# segment `): NAME [+GAP]
// [#RRGGBB] CMD... [INTERVAL]. Mirrors register_segment in strip.asm.
fn parse_segment_body(body: &str) -> Segment {
    let mut seg = Segment {
        enabled: true,
        name: String::new(),
        gap_override: 0,
        color: String::new(),
        command: String::new(),
        interval: 0,
        comments_above: Vec::new(),
    };
    let mut s = body.trim_start();

    // NAME (up to whitespace).
    let name_end = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
    seg.name = s[..name_end].to_string();
    s = s[name_end..].trim_start();

    // Optional +GAP (digits then whitespace/EOL).
    if let Some(rest) = s.strip_prefix('+') {
        let dig_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let after = &rest[dig_end..];
        if dig_end > 0 && (after.is_empty() || after.starts_with(|c: char| c.is_whitespace())) {
            if let Ok(n) = rest[..dig_end].parse::<u32>() {
                seg.gap_override = n;
                s = after.trim_start();
            }
        }
    }

    // Optional #RRGGBB followed by whitespace/EOL.
    if let Some(rest) = s.strip_prefix('#') {
        if rest.len() >= 6 && rest[..6].chars().all(|c| c.is_ascii_hexdigit())
           && (rest.len() == 6 || rest[6..].starts_with(|c: char| c.is_whitespace()))
        {
            seg.color = format!("#{}", rest[..6].to_ascii_lowercase());
            s = rest[6..].trim_start();
        }
    }

    // Trailing INTERVAL: scan from end for digits preceded by whitespace.
    let cmd_str = s.trim_end();
    let bytes = cmd_str.as_bytes();
    let mut end = bytes.len();
    let mut digits_start = end;
    while digits_start > 0 {
        let c = bytes[digits_start - 1];
        if c.is_ascii_digit() { digits_start -= 1; } else { break; }
    }
    if digits_start < end && digits_start > 0 {
        let prev = bytes[digits_start - 1];
        if prev == b' ' || prev == b'\t' {
            if let Ok(n) = cmd_str[digits_start..end].parse::<u32>() {
                seg.interval = n;
                end = digits_start;
            }
        }
    }
    seg.command = cmd_str[..end].trim_end().to_string();
    seg
}

fn serialize_segment(seg: &Segment) -> String {
    let prefix = if seg.enabled { "" } else { "# " };
    let mut out = format!("{}segment {}", prefix, seg.name);
    if seg.gap_override > 0 { out.push_str(&format!(" +{}", seg.gap_override)); }
    if !seg.color.is_empty() { out.push(' '); out.push_str(&seg.color); }
    if !seg.command.is_empty() { out.push(' '); out.push_str(&seg.command); }
    if seg.interval > 0 { out.push_str(&format!(" {}", seg.interval)); }
    out
}

fn truncate(s: &str, max: usize) -> String {
    let mut count = 0;
    for (i, _) in s.char_indices() {
        if count == max { return format!("{}\u{2026}", &s[..i]); }
        count += 1;
    }
    s.to_string()
}

fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
    let h = hex.trim().trim_start_matches('#');
    if h.len() != 6 { return None; }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some((r, g, b))
}
fn normalize_hex(hex: &str) -> Option<String> {
    parse_hex(hex).map(|(r, g, b)| format!("#{:02x}{:02x}{:02x}", r, g, b))
}

// Atomic file replace: write PATH.tmp, rename PATH→PATH.bak, rename
// PATH.tmp→PATH. Guarantees the target file is never empty/truncated
// even if killed mid-save; PATH.bak holds the previous good copy.
fn atomic_write(path: &std::path::Path, data: &[u8]) {
    use std::ffi::OsString;
    let mut tmp: OsString = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let mut bak: OsString = path.as_os_str().to_owned();
    bak.push(".bak");
    if std::fs::write(&tmp, data).is_err() { return; }
    let _ = std::fs::rename(path, &bak);
    let _ = std::fs::rename(&tmp, path);
}

fn main() {
    Crust::init();
    let mut app = App::new();
    app.left.border_refresh();
    app.right.border_refresh();
    app.render();

    loop {
        let Some(key) = Input::getchr(None) else { continue };
        match key.as_str() {
            "q" | "ESC" => {
                if app.dirty {
                    app.status.say(&style::fg(" Save changes? (y/n)", 220));
                    if let Some(k) = Input::getchr(None) {
                        if k == "y" || k == "Y" {
                            app.save_config();
                            App::prompt_reload(&mut app.status);
                        }
                    }
                }
                break;
            }
            "j" | "DOWN"   => { app.move_down();     app.render(); }
            "k" | "UP"     => { app.move_up();       app.render(); }
            "J"            => { app.move_segment_down(); app.render(); }
            "K"            => { app.move_segment_up();   app.render(); }
            "PgDOWN"       => { app.next_category(); app.render(); }
            "PgUP"         => { app.prev_category(); app.render(); }
            "TAB"          => { app.next_category(); app.render(); }
            "S-TAB"        => { app.prev_category(); app.render(); }
            "l" | "RIGHT"  => { app.adjust( 1); app.render(); }
            "h" | "LEFT"   => { app.adjust(-1); app.render(); }
            "ENTER"        => { app.edit_value(); app.render(); }
            "t" | "T"      => { app.toggle_segment(); app.render(); }
            "a" | "A"      => { app.add_segment(); app.render(); }
            "d" | "D"      => { app.delete_segment(); app.render(); }
            "W" | "s"      => {
                app.save_config();
                app.dirty = false;
                App::prompt_reload(&mut app.status);
                app.render();
            }
            "RESIZE" => {
                let (cols, rows) = Crust::terminal_size();
                let split = 22u16;
                let lw = split - 1;
                let rx = split + 3;
                let rw = cols.saturating_sub(rx).saturating_sub(1);
                app.top    = Pane::new(1, 1, cols, 1, 0, 236);
                app.left   = Pane::new(2, 3, lw, rows.saturating_sub(4), 255, 0);
                app.right  = Pane::new(rx, 3, rw, rows.saturating_sub(4), 252, 0);
                app.status = Pane::new(1, rows, cols, 1, 252, 236);
                app.left.border = true;
                app.right.border = true;
                Crust::clear_screen();
                app.left.border_refresh();
                app.right.border_refresh();
                app.render();
            }
            _ => {}
        }
    }

    Crust::cleanup();
}
