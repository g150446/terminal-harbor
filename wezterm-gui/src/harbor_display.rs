//! Moving the window between the Mac and an iPad used as a Sidecar display.

/// macOS names a Sidecar screen after the iPad (its `localizedName`), and
/// "Sidecar" appears in it on some releases, so either word identifies it.
fn is_sidecar_screen(name: &str) -> bool {
    let name = name.to_lowercase();
    name.contains("sidecar") || name.contains("ipad")
}

/// Pick where "Move Window to iPad / Back to Mac" goes: from a Sidecar screen
/// back to the screen with the menu bar, otherwise to the first Sidecar
/// screen. `screens` lists the menu-bar screen first, as `NSScreen.screens`
/// does. `None` means there is no iPad to go to, or nowhere to return to.
pub fn sidecar_toggle_target(screens: &[String], current: &str) -> Option<String> {
    if is_sidecar_screen(current) {
        return screens
            .first()
            .filter(|main| main.as_str() != current)
            .cloned();
    }
    screens.iter().find(|name| is_sidecar_screen(name)).cloned()
}

/// Move `window` to the iPad, or back to the Mac when it is already there.
/// The move runs on the window's thread; a failure is reported as a
/// notification, since a menu item has nowhere else to show it. Screen names
/// go to the log only.
pub fn toggle_sidecar(window: &::window::Window) {
    use ::window::WindowOps;
    use wezterm_toast_notification::persistent_toast_notification;

    let future = window.move_to_screen(sidecar_toggle_target);
    promise::spawn::spawn(async move {
        match future.await {
            Ok(Some(screen)) => log::info!("moved the window to screen {screen:?}"),
            Ok(None) => persistent_toast_notification(
                "No iPad display found",
                "Connect the iPad as a Sidecar display, then try again.",
            ),
            Err(err) => persistent_toast_notification(
                "Cannot move the window",
                &format!("{err:#}. Exit full screen first."),
            ),
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::sidecar_toggle_target;

    fn screens(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn moves_from_the_mac_to_the_ipad() {
        let all = screens(&["Built-in Retina Display", "Studio iPad"]);
        assert_eq!(
            sidecar_toggle_target(&all, "Built-in Retina Display").as_deref(),
            Some("Studio iPad")
        );
    }

    #[test]
    fn moves_from_an_external_monitor_to_the_ipad() {
        let all = screens(&["Built-in Retina Display", "DELL U2720Q", "Sidecar Display"]);
        assert_eq!(
            sidecar_toggle_target(&all, "DELL U2720Q").as_deref(),
            Some("Sidecar Display")
        );
    }

    #[test]
    fn returns_from_the_ipad_to_the_menu_bar_screen() {
        let all = screens(&["DELL U2720Q", "Built-in Retina Display", "Studio iPad"]);
        assert_eq!(
            sidecar_toggle_target(&all, "Studio iPad").as_deref(),
            Some("DELL U2720Q")
        );
    }

    #[test]
    fn matches_the_ipad_without_regard_to_case() {
        let all = screens(&["Built-in Retina Display", "IPAD PRO"]);
        assert_eq!(
            sidecar_toggle_target(&all, "Built-in Retina Display").as_deref(),
            Some("IPAD PRO")
        );
    }

    #[test]
    fn stays_put_without_an_ipad() {
        let all = screens(&["Built-in Retina Display", "DELL U2720Q"]);
        assert_eq!(sidecar_toggle_target(&all, "Built-in Retina Display"), None);
    }

    #[test]
    fn stays_put_when_the_ipad_is_the_only_screen() {
        let all = screens(&["Studio iPad"]);
        assert_eq!(sidecar_toggle_target(&all, "Studio iPad"), None);
    }
}
