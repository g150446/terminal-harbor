//! Pairing actions behind the Pairing menu. They used to be sidebar rows; the
//! menu has no room for status text, so outcomes are reported as toasts.

use config::keyassignment::KeyAssignment;
use wezterm_toast_notification::persistent_toast_notification;
use window::WindowOps;

impl crate::TermWindow {
    /// Run a pairing action. Returns `false` for actions this does not own.
    pub fn perform_harbor_pairing(&mut self, action: &KeyAssignment) -> bool {
        match action {
            KeyAssignment::HarborPairMobile => self.harbor_offer_mobile_pairing(false),
            KeyAssignment::HarborNewMobilePairQr => self.harbor_offer_mobile_pairing(true),
            KeyAssignment::HarborCopyMobilePairUri => {
                if let Some(view) = crate::harbor_mobile::begin_mobile_pairing(false) {
                    self.harbor_set_clipboard(view.uri);
                    persistent_toast_notification(
                        "Mobile pair URI copied",
                        &format!("Valid for {}.", expiry_text(view.expires_in_sec)),
                    );
                }
            }
            KeyAssignment::HarborPairPeerFromClipboard => {
                self.harbor_pair_with_clipboard(crate::harbor_peer::pair_from_uri, "pair URI")
            }
            KeyAssignment::HarborPairPeerTailscaleFromClipboard => {
                // A pair URI is still accepted here, so either menu item works
                // with either clipboard content.
                self.harbor_pair_with_clipboard(
                    crate::harbor_peer::pair_from_text,
                    "Tailscale device id",
                )
            }
            KeyAssignment::HarborToggleTailnetPair => {
                crate::harbor_mobile::toggle_tailnet_pairing();
                crate::commands::CommandDef::recreate_menubar(&config::configuration());
            }
            KeyAssignment::HarborUnpairPeer(server_id) => {
                crate::harbor_peer::unpair(server_id);
            }
            KeyAssignment::HarborConfirmLanFallback(server_id) => {
                crate::harbor_peer::confirm_lan_fallback(server_id);
            }
            _ => return false,
        }
        true
    }

    fn harbor_offer_mobile_pairing(&mut self, fresh: bool) {
        let Some(view) = crate::harbor_mobile::begin_mobile_pairing(fresh) else {
            log::error!("mobile pairing offer is unavailable");
            return;
        };
        // Surface the scannable QR immediately and keep the URI on the
        // clipboard as the manual-entry fallback. The URI carries the pairing
        // token, so it goes to the clipboard only, never into the toast.
        crate::harbor_mobile::open_pair_qr();
        self.harbor_set_clipboard(view.uri);
        persistent_toast_notification(
            "Scan the QR code with Terminal Harbor mobile",
            &format!(
                "The pair URI is on the clipboard. Valid for {}.",
                expiry_text(view.expires_in_sec)
            ),
        );
    }

    fn harbor_set_clipboard(&self, text: String) {
        if let Some(window) = self.window.as_ref() {
            window.set_clipboard(window::Clipboard::Clipboard, text);
        }
    }

    /// Pair using the clipboard content off the main thread, then report the
    /// outcome. Only the generic status from `harbor_peer` is shown, so no
    /// address or identifier from the clipboard reaches the notification.
    fn harbor_pair_with_clipboard(&self, pair: fn(&str) -> anyhow::Result<()>, what: &'static str) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let future = window.get_clipboard(window::Clipboard::Clipboard);
        promise::spawn::spawn(async move {
            match future.await {
                Ok(text) => {
                    promise::spawn::spawn_into_new_thread(move || {
                        match pair(&text) {
                            Ok(()) => persistent_toast_notification(
                                "Paired another Harbor",
                                "Its workspaces appear in the sidebar.",
                            ),
                            Err(err) => {
                                log::error!("pairing another Harbor: {err:#}");
                                persistent_toast_notification(
                                    "Pairing failed",
                                    &crate::harbor_peer::pair_status().unwrap_or_else(|| {
                                        format!("Check the {what} on the clipboard.")
                                    }),
                                );
                            }
                        }
                        Ok::<(), anyhow::Error>(())
                    })
                    .detach();
                }
                Err(err) => log::error!("reading {what} from clipboard: {err:#}"),
            }
        })
        .detach();
    }
}

fn expiry_text(seconds: u64) -> String {
    if seconds >= 120 {
        format!("{} minutes", seconds / 60)
    } else {
        format!("{seconds} seconds")
    }
}

#[cfg(test)]
mod tests {
    use super::expiry_text;

    #[test]
    fn expiry_reads_in_minutes_once_it_is_long() {
        assert_eq!(expiry_text(600), "10 minutes");
        assert_eq!(expiry_text(119), "119 seconds");
        assert_eq!(expiry_text(0), "0 seconds");
    }
}
