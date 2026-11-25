# tawny-core

The shared engine inside the Tawny apps. **This is not an app.** To get Tawny:

- Android: [Tawny-APK](https://github.com/Tawny-Pet-Monitor/Tawny-APK)
- Linux and Windows: [Tawny-Desktop](https://github.com/Tawny-Pet-Monitor/Tawny-Desktop)
- Self-hosting: [Tawny-Docker](https://github.com/Tawny-Pet-Monitor/Tawny-Docker)

It holds everything that has to be exactly the same on every platform: reading
and writing pairing links, the room id, the security code, and the
signalling and call logic. The apps use it through their own native code.

## Building

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

## Licence

MIT, see [LICENSE](LICENSE).
