//! UI settings: everything about the look of the admin that can change without code.

use std::path::PathBuf;

use serde::Serialize;

use crate::error::AdminError;

/// Colour scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// Follow the browser/OS preference.
    #[default]
    Auto,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

/// A link added to the navigation bar.
#[derive(Debug, Clone, Serialize)]
pub struct NavLink {
    /// Link text.
    pub label: String,
    /// Target URL.
    pub url: String,
}

/// Look-and-feel settings. Build in code, or load with [`UiSettings::from_env`].
#[derive(Debug, Clone, Serialize)]
pub struct UiSettings {
    pub(crate) site_title: String,
    pub(crate) accent_color: String,
    pub(crate) logo_url: Option<String>,
    pub(crate) theme: Theme,
    pub(crate) custom_css_url: Option<String>,
    pub(crate) footer_text: Option<String>,
    pub(crate) page_size: u64,
    pub(crate) nav_links: Vec<NavLink>,
    /// Whether pages load their typeface from Google Fonts; otherwise the system's is used.
    pub(crate) web_fonts: bool,
    #[serde(skip)]
    pub(crate) template_dir: Option<PathBuf>,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            site_title: "Administration".to_owned(),
            accent_color: "#4f46e5".to_owned(),
            logo_url: None,
            theme: Theme::Auto,
            custom_css_url: None,
            footer_text: None,
            page_size: 25,
            nav_links: Vec::new(),
            web_fonts: true,
            template_dir: None,
        }
    }
}

impl UiSettings {
    /// Default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads settings from the environment; unset variables keep their defaults.
    ///
    /// | Variable | Meaning |
    /// |---|---|
    /// | `ADMIN_SITE_TITLE` | Title in the header and browser tab |
    /// | `ADMIN_ACCENT_COLOR` | Hex colour, e.g. `#0a7f5a` |
    /// | `ADMIN_LOGO_URL` | Image shown next to the title |
    /// | `ADMIN_THEME` | `auto`, `light` or `dark` |
    /// | `ADMIN_CUSTOM_CSS_URL` | Extra stylesheet loaded after the built-in one |
    /// | `ADMIN_FOOTER_TEXT` | Text in the page footer |
    /// | `ADMIN_PAGE_SIZE` | Rows per list page |
    /// | `ADMIN_NAV_LINKS` | Extra links: `Label=/url,Other=https://…` |
    /// | `ADMIN_TEMPLATE_DIR` | Directory whose files override built-in templates |
    /// | `ADMIN_WEB_FONTS` | `false` uses the system typeface instead of loading Fira Sans from Google Fonts |
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if a variable is set to an invalid value.
    pub fn from_env() -> Result<Self, AdminError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Same as [`UiSettings::from_env`] but reads from `lookup`, so tests need no real environment.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if a value is invalid.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, AdminError> {
        let mut settings = Self::default();
        if let Some(v) = lookup("ADMIN_SITE_TITLE") {
            settings.site_title = v;
        }
        if let Some(v) = lookup("ADMIN_ACCENT_COLOR") {
            settings = settings.accent_color(&v)?;
        }
        settings.logo_url = lookup("ADMIN_LOGO_URL");
        if let Some(v) = lookup("ADMIN_THEME") {
            settings.theme = match v.to_ascii_lowercase().as_str() {
                "auto" => Theme::Auto,
                "light" => Theme::Light,
                "dark" => Theme::Dark,
                other => {
                    return Err(AdminError::Config(format!(
                        "ADMIN_THEME must be auto, light or dark, got `{other}`"
                    )));
                }
            };
        }
        settings.custom_css_url = lookup("ADMIN_CUSTOM_CSS_URL");
        settings.footer_text = lookup("ADMIN_FOOTER_TEXT");
        if let Some(v) = lookup("ADMIN_PAGE_SIZE") {
            settings.page_size = v.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| {
                AdminError::Config("ADMIN_PAGE_SIZE must be a positive integer".into())
            })?;
        }
        if let Some(v) = lookup("ADMIN_NAV_LINKS") {
            for pair in v.split(',').filter(|p| !p.trim().is_empty()) {
                let (label, url) = pair.split_once('=').ok_or_else(|| {
                    AdminError::Config("ADMIN_NAV_LINKS entries must look like Label=/url".into())
                })?;
                settings = settings.nav_link(label.trim(), url.trim());
            }
        }
        if let Some(v) = lookup("ADMIN_WEB_FONTS") {
            settings.web_fonts = match v.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => true,
                "false" | "0" | "no" => false,
                _ => {
                    return Err(AdminError::Config(
                        "ADMIN_WEB_FONTS must be true or false".into(),
                    ));
                }
            };
        }
        settings.template_dir = lookup("ADMIN_TEMPLATE_DIR").map(PathBuf::from);
        Ok(settings)
    }

    /// Sets the title shown in the header and browser tab.
    #[must_use]
    pub fn site_title(mut self, title: impl Into<String>) -> Self {
        self.site_title = title.into();
        self
    }

    /// Sets the accent colour.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] unless `color` is `#rgb` or `#rrggbb`; the value is written
    /// into a stylesheet, so anything else is refused rather than escaped.
    pub fn accent_color(mut self, color: &str) -> Result<Self, AdminError> {
        let hex = color.strip_prefix('#').unwrap_or("");
        if !(matches!(hex.len(), 3 | 6) && hex.chars().all(|c| c.is_ascii_hexdigit())) {
            return Err(AdminError::Config(format!(
                "accent colour must be #rgb or #rrggbb, got `{color}`"
            )));
        }
        self.accent_color = color.to_owned();
        Ok(self)
    }

    /// Loads Fira Sans from Google Fonts (the default), or uses the system typeface, which
    /// makes no request to another site.
    #[must_use]
    pub fn web_fonts(mut self, enabled: bool) -> Self {
        self.web_fonts = enabled;
        self
    }

    /// Sets the logo image URL.
    #[must_use]
    pub fn logo_url(mut self, url: impl Into<String>) -> Self {
        self.logo_url = Some(url.into());
        self
    }

    /// Sets the colour scheme.
    #[must_use]
    pub fn theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Loads an extra stylesheet after the built-in one.
    #[must_use]
    pub fn custom_css_url(mut self, url: impl Into<String>) -> Self {
        self.custom_css_url = Some(url.into());
        self
    }

    /// Sets the footer text.
    #[must_use]
    pub fn footer_text(mut self, text: impl Into<String>) -> Self {
        self.footer_text = Some(text.into());
        self
    }

    /// Sets the default rows per list page.
    #[must_use]
    pub fn page_size(mut self, rows: u64) -> Self {
        self.page_size = rows.max(1);
        self
    }

    /// Adds a link to the navigation bar.
    #[must_use]
    pub fn nav_link(mut self, label: impl Into<String>, url: impl Into<String>) -> Self {
        self.nav_links.push(NavLink {
            label: label.into(),
            url: url.into(),
        });
        self
    }

    /// Overrides built-in templates with same-named files from `dir`
    /// (`base.html`, `login.html`, `index.html`, `list.html`, `form.html`).
    #[must_use]
    pub fn template_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.template_dir = Some(dir.into());
        self
    }
}
