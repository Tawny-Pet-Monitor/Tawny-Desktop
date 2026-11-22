# Vendored headers

The public headers of libvpx (`vpx/`) and libopus (`opus/`) exactly as
shipped inside the libwebrtc build this app links (LiveKit's
`webrtc-a545de4`, `include/third_party/...`). The encoders themselves are not
here: they are the copies already inside libwebrtc. `csrc/rec.c` is compiled
against these so it matches that build.

Update them together with the libwebrtc version (`webrtc-sys`).

Licences: libvpx is BSD-3-Clause with a patent grant (`vpx/LICENSE`,
`vpx/PATENTS`); Opus is BSD-3-Clause (`opus/COPYING`).
