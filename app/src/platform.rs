//! What differs by operating system, behind one face: a notification, and
//! starting at login.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Show a notification. Best effort: a platform that refuses is not a
/// reason to stop watching.
#[cfg_attr(test, allow(dead_code))]
pub fn notify(title: &str, body: &str) {
    let _ = notify_rust::Notification::new().summary(title).body(body).show();
}

/// Start this very program at login, or stop doing so. What each platform
/// keeps is a file naming the program, in the place its session manager
/// reads at login; taking it away is the whole of turning it off.
#[cfg_attr(test, allow(dead_code))]
pub fn launch_at_login(on: bool) -> Result<bool> {
    let exe = std::env::current_exe().context("finding this program")?;
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let entry = login_entry(&home, &exe);
    match on {
        true => {
            if let Some(dir) = entry.path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            std::fs::write(&entry.path, entry.body)
                .with_context(|| format!("writing {}", entry.path.display()))?;
        }
        false => match std::fs::remove_file(&entry.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", entry.path.display())),
        },
    }
    Ok(on)
}

/// A login entry: where it goes and what it says.
pub struct LoginEntry {
    pub path: PathBuf,
    pub body: String,
}

/// The entry for this platform. macOS reads LaunchAgents; a desktop on
/// Linux reads XDG autostart. The bundle id names both.
pub fn login_entry(home: &Path, exe: &Path) -> LoginEntry {
    if cfg!(target_os = "macos") {
        LoginEntry {
            path: home.join("Library/LaunchAgents/dev.orthory.ccs.plist"),
            body: agent_plist(exe),
        }
    } else {
        LoginEntry { path: home.join(".config/autostart/ccs.desktop"), body: desktop_entry(exe) }
    }
}

/// A launchd agent that runs the program at login and does not restart it
/// when it quits: quitting is the user's to do.
pub fn agent_plist(exe: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>dev.orthory.ccs</string>
	<key>ProgramArguments</key>
	<array>
		<string>{}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<false/>
</dict>
</plist>
"#,
        exe.display()
    )
}

/// An XDG autostart entry, which is the desktop entry the app ships with
/// pointed at this very program.
pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=ccs\nComment=Switch Claude and Codex accounts\nExec={}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        exe.display()
    )
}

/// Bring the dashboard into the user's current Space before activating the app.
#[cfg(target_os = "macos")]
#[cfg_attr(test, allow(dead_code))]
pub fn follow_active_space() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSWindowCollectionBehavior};

    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    for window in app.windows() {
        if window.title().to_string() == "ccs" {
            let mut behavior = window.collectionBehavior();
            behavior.remove(NSWindowCollectionBehavior::CanJoinAllSpaces);
            behavior.insert(NSWindowCollectionBehavior::MoveToActiveSpace);
            window.setCollectionBehavior(behavior);
            window.makeKeyAndOrderFront(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_entry_names_the_program_and_lives_where_the_session_reads() {
        let entry = login_entry(
            Path::new("/Users/you"),
            Path::new("/Applications/ccs.app/Contents/MacOS/ccs-app"),
        );
        assert!(
            entry.body.contains("/Applications/ccs.app/Contents/MacOS/ccs-app"),
            "{}",
            entry.body
        );
        let expected = match cfg!(target_os = "macos") {
            true => "/Users/you/Library/LaunchAgents/dev.orthory.ccs.plist",
            false => "/Users/you/.config/autostart/ccs.desktop",
        };
        assert_eq!(entry.path, Path::new(expected));
    }

    #[test]
    fn the_agent_runs_at_load_and_is_not_kept_alive() {
        let plist = agent_plist(Path::new("/x/ccs-app"));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(plist.contains("<key>KeepAlive</key>\n\t<false/>"));
        assert!(plist.contains("<string>dev.orthory.ccs</string>"));
    }

    #[test]
    fn the_desktop_entry_is_an_application_that_starts_without_a_terminal() {
        let entry = desktop_entry(Path::new("/x/ccs-app"));
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Exec=/x/ccs-app\n"));
        assert!(entry.contains("Terminal=false\n"));
    }
}
