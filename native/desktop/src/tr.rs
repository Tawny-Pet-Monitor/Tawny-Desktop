//! The language the app speaks: Spanish when the system's is, else English.
//! The screens' words come translated from `lang/` (Slint); the few lines
//! the Rust side says (toasts, statuses) go through [`t`].

use std::sync::OnceLock;

/// Whether the system's language is Spanish (`LC_ALL`, `LC_MESSAGES`, `LANG`
/// on Linux; the user's locale on Windows).
#[must_use]
pub fn spanish() -> bool {
    static ES: OnceLock<bool> = OnceLock::new();
    *ES.get_or_init(|| {
        let env = ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"]
            .iter()
            .filter_map(std::env::var_os)
            .map(|v| v.to_string_lossy().to_lowercase())
            .find(|v| !v.is_empty() && v != "c" && v != "posix");
        env.is_some_and(|v| v.starts_with("es"))
    })
}

/// A line in the user's language.
#[must_use]
pub fn t(en: &'static str) -> &'static str {
    if spanish() { es(en).unwrap_or(en) } else { en }
}

/// The Spanish for the lines the Rust side says (as the web app's i18n.js).
fn es(en: &str) -> Option<&'static str> {
    Some(match en {
        "Connecting" => "Conectando",
        "Live" => "En vivo",
        "On air" => "Al aire",
        "Waiting" => "Esperando",
        "Reconnecting" => "Reconectando",
        "Monitor paused" => "Monitor en pausa",
        "The monitor is paused" => "El monitor está en pausa",
        "Its screen turned off or the app moved to the background. The picture comes back on its own when the monitor phone is woken." => {
            "Se apagó su pantalla o la app pasó a segundo plano. La imagen vuelve sola cuando se despierta el teléfono monitor."
        }
        "Waiting for the Viewer" => "Esperando al visor",
        "Link copied" => "Enlace copiado",
        "Couldn’t copy the link." => "No se pudo copiar el enlace.",
        "Copy the pairing link first, then paste it here." => {
            "Copia primero el enlace de vinculación y pégalo aquí."
        }
        "That isn’t a Tawny pairing link." => "Ese no es un enlace de vinculación de Tawny.",
        "That pairing code has expired. Ask the Monitor for a fresh one." => {
            "Ese código de vinculación caducó. Pide uno nuevo al monitor."
        }
        "The Monitor already has as many viewers as it takes." => {
            "El monitor ya tiene todos los visores que admite."
        }
        "The Monitor didn’t let this device in. Pair it again." => {
            "El monitor no dejó entrar a este dispositivo. Vuelve a vincularlo."
        }
        "Couldn’t start the Monitor. Check the network." => {
            "No se pudo iniciar el monitor. Revisa la red."
        }
        "This channel isn’t paired yet. Scan its Monitor’s code." => {
            "Este canal aún no está vinculado. Escanea el código de su monitor."
        }
        "Couldn’t open the camera. Is another app using it?" => {
            "No se pudo abrir la cámara. ¿La está usando otra app?"
        }
        "No camera to scan with. Paste the pairing link instead." => {
            "No hay cámara para escanear. Pega el enlace de vinculación."
        }
        "No microphone found: the call goes on without one." => {
            "No hay micrófono: la llamada sigue sin él."
        }
        "This computer has one camera." => "Este ordenador tiene una sola cámara.",
        "The Monitor’s camera has no light on this side." => {
            "La cámara del monitor no tiene luz de este lado."
        }
        "Nothing to capture yet." => "Todavía no hay nada que capturar.",
        "Snapshot saved to Pictures" => "Foto guardada en Imágenes",
        "Couldn’t save the snapshot." => "No se pudo guardar la foto.",
        "Recording — up to 20s" => "Grabando: hasta 20 s",
        "Video saved to Videos" => "Video guardado en Videos",
        "Couldn’t save the video." => "No se pudo guardar el video.",
        "Couldn’t record a video." => "No se pudo grabar un video.",
        "Give this channel a name." => "Ponle un nombre a este canal.",
        "Tawny keeps up to 12 channels. Delete one to add another." => {
            "Tawny guarda hasta 12 canales. Elimina uno para añadir otro."
        }
        "Couldn’t save that. Is the disk full?" => "No se pudo guardar. ¿Está lleno el disco?",
        "List up to 5 relays." => "Indica hasta 5 relés.",
        "Check this code matches the one on the other phone:" => {
            "Revisa que este código coincida con el del otro teléfono:"
        }
        "This connection couldn’t be checked. Disconnect unless you just paired this phone." => {
            "No se pudo comprobar esta conexión. Desconecta salvo que acabes de vincular este teléfono."
        }
        "Pet camera" => "Cámara de mascota",
        "Theme: follow system" => "Tema: según el sistema",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::es;

    #[test]
    fn every_line_has_its_spanish() {
        assert_eq!(es("On air"), Some("Al aire"));
        assert_eq!(es("not a line"), None);
        // Every t("…") in the sources is in the table.
        for src in [include_str!("app.rs"), include_str!("session.rs")] {
            for part in src.split("crate::tr::t(").skip(1) {
                let rest = part.trim_start();
                let Some(body) = rest.strip_prefix('"') else {
                    continue;
                };
                let Some(end) = body.find('"') else { continue };
                let line = &body[..end];
                assert!(es(line).is_some(), "no Spanish for {line:?}");
            }
        }
    }
}
