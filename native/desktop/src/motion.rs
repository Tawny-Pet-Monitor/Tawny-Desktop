//! Whether the person has asked their system for less motion, as the old
//! Desktop's page did through `prefers-reduced-motion`. With it on, the scene
//! holds a still pose and transitions don't animate (`Theme.still`).
//!
//! - Linux: the desktop portal's settings, which also answers inside the
//!   Flatpak: its own `reduced-motion` key where the portal has it, and the
//!   GNOME/GTK `enable-animations` switch (what GTK, and so Chromium, follow).
//! - Windows: the "Show animations in Windows" setting.
//!
//! It's read when the app starts and every half minute after, so turning the
//! setting on or off takes effect without a restart.

use std::time::Duration;

use slint::ComponentHandle;

use crate::ui::{AppWindow, Theme};

const RECHECK: Duration = Duration::from_secs(30);

/// Keeps `Theme.still` in step with the system's setting, from a thread of its
/// own (a desktop bus that doesn't answer must never hold up the window).
pub fn follow(ui: &AppWindow) {
    let weak = ui.as_weak();
    let spawned = std::thread::Builder::new()
        .name("motion".into())
        .spawn(move || {
            let source = imp::Source::new();
            let mut last = None;
            loop {
                let read = source.as_ref().and_then(imp::Source::reduced);
                let still = read.unwrap_or(false);
                if last != Some(read) {
                    eprintln!(
                        "tawny: motion: {}",
                        match read {
                            Some(true) => "the system asks for less, animations off",
                            Some(false) => "the system allows it, animations on",
                            None => "no system setting found, animations on",
                        }
                    );
                    last = Some(read);
                }
                let alive = weak.upgrade_in_event_loop(move |ui| {
                    let theme = ui.global::<Theme>();
                    if theme.get_still() != still {
                        theme.set_still(still);
                    }
                });
                if alive.is_err() {
                    return;
                }
                std::thread::sleep(RECHECK);
            }
        });
    if let Err(e) = spawned {
        eprintln!("tawny: can't follow the system's motion setting: {e}");
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::time::Duration;

    use zbus::blocking::{Connection, Proxy, connection};
    use zbus::zvariant::{OwnedValue, Value};

    pub struct Source {
        // Kept so the proxy's connection lives as long as it does.
        _conn: Connection,
        settings: Proxy<'static>,
    }

    impl Source {
        pub fn new() -> Option<Self> {
            let conn = connection::Builder::session()
                .ok()?
                .method_timeout(Duration::from_secs(2))
                .build()
                .ok()?;
            let settings = Proxy::new(
                &conn,
                "org.freedesktop.portal.Desktop",
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Settings",
            )
            .ok()?;
            Some(Self {
                _conn: conn,
                settings,
            })
        }

        /// `None` when the portal has neither setting.
        pub fn reduced(&self) -> Option<bool> {
            let portal = self
                .read("org.freedesktop.appearance", "reduced-motion")
                .and_then(|v| u32::try_from(&v).ok())
                .map(|n| n == 1);
            let gnome = self
                .read("org.gnome.desktop.interface", "enable-animations")
                .and_then(|v| bool::try_from(&v).ok())
                .map(|on| !on);
            match (portal, gnome) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(false) || b.unwrap_or(false)),
            }
        }

        /// One setting: `ReadOne` (newer portals), else `Read`, whose answer
        /// comes wrapped in one more variant.
        fn read(&self, namespace: &str, key: &str) -> Option<OwnedValue> {
            if let Ok(v) = self
                .settings
                .call::<_, _, OwnedValue>("ReadOne", &(namespace, key))
            {
                return Some(v);
            }
            let v: OwnedValue = self.settings.call("Read", &(namespace, key)).ok()?;
            match &*v {
                Value::Value(inner) => OwnedValue::try_from(inner.as_ref().try_clone().ok()?).ok(),
                _ => Some(v),
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    #![expect(
        unsafe_code,
        reason = "one SystemParametersInfo read into a local BOOL"
    )]

    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW,
    };

    pub struct Source;

    impl Source {
        #[expect(clippy::unnecessary_wraps, reason = "the same shape on every system")]
        pub fn new() -> Option<Self> {
            Some(Self)
        }

        pub fn reduced(&self) -> Option<bool> {
            let mut on: i32 = 1;
            // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL to the
            // pointer, which is a live local of that size.
            let ok = unsafe {
                SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut on).cast(), 0)
            };
            (ok != 0).then_some(on == 0)
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    pub struct Source;

    impl Source {
        #[expect(clippy::unnecessary_wraps, reason = "the same shape on every system")]
        pub fn new() -> Option<Self> {
            None
        }

        #[expect(clippy::unused_self, reason = "the same shape on every system")]
        pub fn reduced(&self) -> Option<bool> {
            None
        }
    }
}
