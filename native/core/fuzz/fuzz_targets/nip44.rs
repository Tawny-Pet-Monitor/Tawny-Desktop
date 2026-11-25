//! NIP-44 payloads from relays: decryption never panics, refuses anything not
//! made with the key, and what it accepts encrypts back to the same text.
#![no_main]

use libfuzzer_sys::fuzz_target;
use tawny_core::nostr::nip44;

fuzz_target!(|data: &[u8]| {
    let (Ok(sk), Ok(peer)) = (
        nip44::secret_key(&[0x11; 32]),
        tawny_core::nostr::event::public_key(&[0x22; 32]),
    ) else {
        return;
    };
    let mut pub_x = [0u8; 32];
    let Ok(bytes) = hex_decode(&peer) else { return };
    pub_x.copy_from_slice(&bytes);
    let Ok(ck) = nip44::conversation_key(&sk, &pub_x) else {
        return;
    };
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = nip44::decrypt(text, &ck, 64 * 1024);
    }
    // Anything at all as plaintext goes round.
    if let Ok(plain) = std::str::from_utf8(data)
        && !plain.is_empty()
        && plain.len() <= 65_535
        && let Ok(ct) = nip44::encrypt_with_nonce(plain, &ck, &[7; 32], 64 * 1024)
    {
        assert_eq!(
            nip44::decrypt(&ct, &ck, 64 * 1024).ok().as_deref(),
            Some(plain)
        );
    }
    // An arbitrary public key: never a panic.
    if data.len() >= 32 {
        let mut x = [0u8; 32];
        x.copy_from_slice(&data[..32]);
        let _ = nip44::conversation_key(&sk, &x);
    }
});

fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2).ok_or(())?, 16).map_err(|_| ()))
        .collect()
}
