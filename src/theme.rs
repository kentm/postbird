use std::{collections::HashMap, fs, path::PathBuf};

use directories::BaseDirs;
use gtk::{gdk, glib};

#[derive(Debug, PartialEq)]
struct Palette {
    background: String,
    dark_background: String,
    darker_background: String,
    lighter_background: String,
    foreground: String,
    muted: String,
    accent: String,
    selection: String,
    red: String,
    green: String,
    yellow: String,
}

pub fn install_omarchy_integration() {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let layout = gtk::CssProvider::new();
    layout.load_from_string(
        "expander-widget.message-section > box > title { padding: 6px 8px; }
         expander-widget.message-section > box > title > expander { margin-right: 6px; }",
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &layout,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let Some(base_dirs) = BaseDirs::new() else {
        return;
    };
    let colors_path = base_dirs
        .home_dir()
        .join(".local/state/omarchy/current/theme/colors.toml");
    if !colors_path.exists() {
        return;
    }

    let provider = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let mut previous = String::new();
    apply_current_palette(&provider, &colors_path, &mut previous);

    glib::timeout_add_seconds_local(1, move || {
        apply_current_palette(&provider, &colors_path, &mut previous);
        glib::ControlFlow::Continue
    });
}

fn apply_current_palette(provider: &gtk::CssProvider, path: &PathBuf, previous: &mut String) {
    let Ok(contents) = fs::read_to_string(path) else {
        return;
    };
    if contents == *previous {
        return;
    }
    let Some(palette) = parse_palette(&contents) else {
        return;
    };
    provider.load_from_string(&palette_css(&palette));
    *previous = contents;
}

fn parse_palette(contents: &str) -> Option<Palette> {
    let values = contents
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let value = value.trim().trim_matches('"');
            valid_color(value).then(|| (key.trim(), value.to_owned()))
        })
        .collect::<HashMap<_, _>>();
    let get = |key: &str| values.get(key).cloned();
    Some(Palette {
        background: get("background")?,
        dark_background: get("dark_background")?,
        darker_background: get("darker_background")?,
        lighter_background: get("lighter_background")?,
        foreground: get("foreground")?,
        muted: get("muted")?,
        accent: get("accent")?,
        selection: get("selection")?,
        red: get("red")?,
        green: get("green")?,
        yellow: get("yellow")?,
    })
}

fn valid_color(value: &str) -> bool {
    matches!(value.len(), 4 | 7 | 9)
        && value.starts_with('#')
        && value[1..]
            .chars()
            .all(|character| character.is_ascii_hexdigit())
}

fn contrasting_text_color(background: &str) -> &'static str {
    let channels = if background.len() == 4 {
        [1, 2, 3]
            .map(|index| u8::from_str_radix(&background[index..=index].repeat(2), 16).unwrap_or(0))
    } else {
        [1, 3, 5].map(|index| u8::from_str_radix(&background[index..index + 2], 16).unwrap_or(0))
    };
    let luminance = channels
        .map(|channel| {
            let value = f64::from(channel) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        })
        .into_iter()
        .zip([0.2126, 0.7152, 0.0722])
        .map(|(channel, weight)| channel * weight)
        .sum::<f64>();
    if luminance > 0.179 {
        "#000000"
    } else {
        "#ffffff"
    }
}

fn palette_css(palette: &Palette) -> String {
    format!(
        r#"
:root {{
  --window-bg-color: {background};
  --window-fg-color: {foreground};
  --view-bg-color: {background};
  --view-fg-color: {foreground};
  --headerbar-bg-color: {darker_background};
  --headerbar-fg-color: {foreground};
  --sidebar-bg-color: {dark_background};
  --sidebar-fg-color: {foreground};
  --card-bg-color: {lighter_background};
  --card-fg-color: {foreground};
  --popover-bg-color: {lighter_background};
  --popover-fg-color: {foreground};
  --dialog-bg-color: {background};
  --dialog-fg-color: {foreground};
  --accent-color: {accent};
  --accent-bg-color: {accent};
  --accent-fg-color: {accent_foreground};
  --destructive-color: {red};
  --destructive-bg-color: {red};
  --success-color: {green};
  --success-bg-color: {green};
  --warning-color: {yellow};
  --warning-bg-color: {yellow};
  --border-color: {muted};
}}

window.background, .background {{
  background-color: var(--window-bg-color);
  color: var(--window-fg-color);
}}
.navigation-sidebar {{ background-color: var(--sidebar-bg-color); }}
.navigation-sidebar row:selected {{ background-color: {selection}; }}
"#,
        background = palette.background,
        dark_background = palette.dark_background,
        darker_background = palette.darker_background,
        lighter_background = palette.lighter_background,
        foreground = palette.foreground,
        muted = palette.muted,
        accent = palette.accent,
        accent_foreground = contrasting_text_color(&palette.accent),
        selection = palette.selection,
        red = palette.red,
        green = palette.green,
        yellow = palette.yellow,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_sanitizes_omarchy_palette() {
        let palette = parse_palette(
            r##"
background = "#111c18"
dark_background = "#0c1512"
darker_background = "#090f0d"
lighter_background = "#23372B"
foreground = "#C1C497"
muted = "#53685B"
accent = "#509475"
selection = "#32473B"
red = "#FF5345"
green = "#549e6a"
yellow = "#459451"
"##,
        )
        .unwrap();
        assert_eq!(palette.background, "#111c18");
        assert!(palette_css(&palette).contains("--accent-color: #509475"));
        assert_eq!(contrasting_text_color("#56949f"), "#000000");
        assert_eq!(contrasting_text_color("#315c9e"), "#ffffff");
        assert!(parse_palette("background = \"red; window { color: red; }\"").is_none());
    }
}
