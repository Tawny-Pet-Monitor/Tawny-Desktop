//! A running Viewer for the app: [`WatchHandle`] and [`WatchListener`].

use std::sync::Arc;

use libwebrtc::peer_connection_factory::IceServer;
use tawny_core::call::media::MediaConfig;
use tawny_core::engine::viewer::{Watch, WatchConfig, WatchEvent};
use tawny_core::relay::health::HealthBook;
use tokio::runtime::Runtime;

use crate::{IceServerSpec, TawnyError, parse_json, runtime, secret_bytes, sound, video};

/// How to watch a Monitor: the stored pairing plus this phone's own keys.
#[derive(Debug, Clone, uniffi::Record)]
pub struct WatchSettings {
    /// The channel key.
    pub channel_key: String,
    /// This phone's bond id with that Monitor ([`crate::new_bond_id`]).
    pub pid: String,
    /// The pairing code, the first time only.
    pub code: Option<String>,
    /// A short name for this phone, shown on the Monitor.
    pub label: String,
    /// The person's name, if they gave one.
    pub name: Option<String>,
    /// Talk-back microphone available.
    pub mic: bool,
    /// The Monitor's LAN relay (`ws://ip:port`), from the pairing.
    pub lan: Option<String>,
    /// The rendezvous (`wss://host`).
    pub rendezvous: Option<String>,
    /// The build's own rendezvous, used if `rendezvous` (the user's) won't
    /// let us in; `None` without a custom one or under tighter privacy.
    pub rendezvous_fallback: Option<String>,
    /// The rendezvous admission ticket.
    pub ticket: Option<String>,
    /// The Monitor's Nostr public key, from the pairing.
    pub nostr_station: Option<String>,
    /// This phone's Nostr secret key, 64 hex characters.
    pub nostr_secret: Option<String>,
    /// Nostr relays.
    pub nostr_relays: Vec<String>,
    /// The relay health book, as JSON (empty for none).
    pub nostr_health: String,
    /// STUN and TURN servers.
    pub ice_servers: Vec<IceServerSpec>,
}

/// What a running Viewer tells the app. Calls arrive on one background
/// thread, in order (video sizes on the decoder's); return quickly.
#[uniffi::export(with_foreign)]
pub trait WatchListener: Send + Sync {
    /// The call is being placed over `lan`, `nostr` or `rendezvous`.
    fn on_way(&self, name: String);
    /// The call connected.
    fn on_connected(&self);
    /// The security code (only off the Wi-Fi); `None`: show a warning.
    fn on_security_code(&self, code: Option<String>);
    /// The Monitor turned this phone away (`expired`, `full`, …).
    fn on_refused(&self, reason: String);
    /// The call ended; another way may pick it up.
    fn on_left(&self);
    /// The picture's upright size changed.
    fn on_video_size(&self, width: u32, height: u32);
    /// A control frame from the Monitor, as JSON.
    fn on_control(&self, json: String);
    /// A way came up or went down.
    fn on_transport(&self, name: String, up: bool);
    /// Store this as the relay health book.
    fn on_relay_health(&self, json: String);
    /// A diagnostics line.
    fn on_log(&self, line: String);
}

fn deliver(rt: &Runtime, l: &Arc<dyn WatchListener>, e: WatchEvent) {
    match e {
        WatchEvent::Video(t) => {
            let (sizes, lines) = (l.clone(), l.clone());
            video::play(
                rt,
                t,
                Arc::new(move |w, h| sizes.on_video_size(w, h)),
                Arc::new(move |line| lines.on_log(line)),
            );
        }
        WatchEvent::Audio(t) => sound::listen(rt, "monitor".into(), t),
        WatchEvent::Way(w) => l.on_way(w.to_owned()),
        WatchEvent::Connected => l.on_connected(),
        WatchEvent::Sas(c) => l.on_security_code(c),
        WatchEvent::Refused(r) => l.on_refused(r),
        WatchEvent::Left => {
            sound::SPEAKER.forget("monitor");
            l.on_left();
        }
        WatchEvent::Control(v) => l.on_control(v.to_string()),
        WatchEvent::Transport { name, up } => l.on_transport(name.to_owned(), up),
        WatchEvent::Health(h) => {
            if let Ok(j) = serde_json::to_string(&h) {
                l.on_relay_health(j);
            }
        }
        WatchEvent::Log(s) => l.on_log(s),
    }
}

/// A running Viewer. Dropping it hangs up.
#[derive(uniffi::Object)]
pub struct WatchHandle {
    watch: Watch,
}

#[uniffi::export]
impl WatchHandle {
    /// Starts dialling the Monitor every way the pairing allows. Call off
    /// the main thread.
    ///
    /// # Errors
    /// [`TawnyError::BadSetting`] or [`TawnyError::Start`] (the pairing names
    /// no usable way in).
    #[uniffi::constructor]
    pub fn start(
        settings: WatchSettings,
        listener: Arc<dyn WatchListener>,
    ) -> Result<Arc<Self>, TawnyError> {
        let health: HealthBook = parse_json("relay health", &settings.nostr_health)?;
        let nostr_secret = settings
            .nostr_secret
            .as_deref()
            .map(secret_bytes)
            .transpose()?;
        if !tawny_core::call::policy::is_bond_id(&settings.pid) {
            return Err(TawnyError::BadSetting(
                "the bond id is not 32 hex characters".into(),
            ));
        }
        let cfg = WatchConfig {
            channel_key: settings.channel_key,
            pid: settings.pid,
            code: settings.code,
            label: settings.label,
            name: settings.name,
            mic: settings.mic,
            lan: settings.lan,
            rendezvous: settings.rendezvous,
            rendezvous_fallback: settings.rendezvous_fallback,
            ticket: settings.ticket,
            nostr_station: settings.nostr_station,
            nostr_secret,
            nostr_relays: settings.nostr_relays,
            nostr_health: health,
            media: MediaConfig {
                ice_servers: settings
                    .ice_servers
                    .into_iter()
                    .map(|i| IceServer {
                        urls: i.urls,
                        username: i.username,
                        password: i.password,
                    })
                    .collect(),
            },
        };
        let rt = runtime()?;
        let (watch, mut rx) = {
            let _g = rt.enter();
            Watch::start(cfg).map_err(|e| TawnyError::Start(e.to_string()))?
        };
        if let Ok(mut m) = sound::MIC.lock() {
            *m = watch.audio_source();
        }
        let pump = std::thread::Builder::new()
            .name("tawny-watch-events".into())
            .spawn(move || {
                crate::on_thread_start();
                while let Some(e) = rx.blocking_recv() {
                    deliver(rt, &listener, e);
                }
            });
        if let Err(e) = pump {
            watch.stop();
            return Err(TawnyError::Start(format!("no event thread: {e}")));
        }
        Ok(Arc::new(Self { watch }))
    }

    /// Talk-back on (the user is holding Talk) or off.
    pub fn set_talking(&self, on: bool) {
        self.watch.set_talking(on);
    }

    /// "My camera": this phone's camera to the Monitor (`true`), or stop.
    /// While on, `NativeBridge.pushFrame` feeds it, as it feeds a Monitor.
    pub fn set_camera(&self, on: bool) {
        if let Ok(mut s) = crate::frames::SOURCE.lock() {
            *s = on.then(|| self.watch.video_source());
        }
        self.watch.set_camera(on);
    }

    /// Sends a control frame (`torch`, `camera-control`, `chime`, …) as JSON.
    ///
    /// # Errors
    /// [`TawnyError::BadSetting`] when it isn't a JSON object with a `type`.
    pub fn send_control(&self, json: &str) -> Result<(), TawnyError> {
        let v: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| TawnyError::BadSetting(format!("control: {e}")))?;
        if !v["type"].is_string() {
            return Err(TawnyError::BadSetting("control: no type".into()));
        }
        self.watch.send_control(v);
        Ok(())
    }

    /// Hangs up and stops every way.
    pub fn stop(&self) {
        if let Ok(mut m) = sound::MIC.lock() {
            *m = None;
        }
        if let Ok(mut s) = crate::frames::SOURCE.lock() {
            *s = None;
        }
        sound::SPEAKER.clear();
        self.watch.stop();
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop();
    }
}
