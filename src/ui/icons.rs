//! The app's own icons, compiled by `build.rs` from `data/icons.gresource.xml`
//! and embedded in the binary, so they resolve wherever Tributary is installed
//! rather than only when `data/icons` sits next to the executable.

use std::sync::Once;

use gtk::gio;

/// Where the embedded icons live: a hicolor-layout tree
/// (`scalable/actions/…`), as `gtk::IconTheme::add_resource_path` expects.
const RESOURCE_PATH: &str = "/io/github/jm2/tributary/hicolor";

/// Make the embedded icons available to `theme`. The resource is registered
/// with GIO once per process; adding the path to further themes is cheap.
pub fn add_bundled_icons(theme: &gtk::IconTheme) {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        gio::resources_register_include!("icons.gresource")
            .expect("the embedded icon resource is valid");
    });
    theme.add_resource_path(RESOURCE_PATH);
}

/// GTK-touching contracts folded into the crate's single consolidated
/// GTK-initializing test (browser.rs `gtk_widget_contracts_hold_on_one_session`);
/// see `ui::widget_test_session`. Mirrors the caller's macOS gate so these
/// helpers are never dead code there.
#[cfg(all(test, not(target_os = "macos")))]
pub mod widget_tests {
    use super::*;
    use crate::ui::header_bar::EQUALIZER_ICON;

    /// An installed binary has no `data/icons` beside it, so the EQ icon must
    /// come from the embedded resource alone.
    pub fn bundled_icons_resolve_without_a_search_path() {
        let theme = gtk::IconTheme::new();
        theme.set_search_path(&[] as &[&std::path::Path]);
        theme.set_theme_name(Some("hicolor"));
        assert!(
            !theme.has_icon(EQUALIZER_ICON),
            "no icon before the resource is added"
        );

        add_bundled_icons(&theme);
        assert!(theme.has_icon(EQUALIZER_ICON));
    }
}
