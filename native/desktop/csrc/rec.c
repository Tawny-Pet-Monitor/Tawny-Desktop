/*
 * The clip recorder's encoders: VP8 (libvpx) and Opus (libopus), the copies
 * already built into libwebrtc, reached through this thin layer so the Rust
 * side never mirrors libvpx's structs. Only plain numbers and buffers cross;
 * every size is checked here before libvpx or libopus sees it.
 *
 * Headers: csrc/vendor, from the same libwebrtc release (see its README).
 */

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "vpx/vp8cx.h"
#include "vpx/vpx_encoder.h"
#include "opus.h"

/* ---- VP8 ------------------------------------------------------------------- */

typedef struct tawny_vp8 {
  vpx_codec_ctx_t ctx;
  vpx_codec_iter_t iter;
  unsigned int w, h;
} tawny_vp8;

/* An encoder for w x h frames (even, 16..4096), timestamps in milliseconds. */
tawny_vp8 *tawny_vp8_new(unsigned int w, unsigned int h, unsigned int kbps) {
  if (w < 16 || h < 16 || w > 4096 || h > 4096 || (w & 1) || (h & 1)) return NULL;
  if (kbps < 100 || kbps > 20000) return NULL;
  vpx_codec_enc_cfg_t cfg;
  if (vpx_codec_enc_config_default(vpx_codec_vp8_cx(), &cfg, 0) != VPX_CODEC_OK) return NULL;
  cfg.g_w = w;
  cfg.g_h = h;
  cfg.g_timebase.num = 1;
  cfg.g_timebase.den = 1000;
  cfg.rc_target_bitrate = kbps;
  cfg.rc_end_usage = VPX_VBR;
  cfg.g_threads = 2;
  cfg.g_lag_in_frames = 0;
  cfg.kf_mode = VPX_KF_AUTO;
  cfg.kf_max_dist = 120;
  tawny_vp8 *e = calloc(1, sizeof *e);
  if (!e) return NULL;
  if (vpx_codec_enc_init(&e->ctx, vpx_codec_vp8_cx(), &cfg, 0) != VPX_CODEC_OK) {
    free(e);
    return NULL;
  }
  vpx_codec_control(&e->ctx, VP8E_SET_CPUUSED, 8);
  e->w = w;
  e->h = h;
  return e;
}

/* Encodes one I420 frame, its planes back to back (len must be exactly
 * w*h*3/2), or flushes when i420 is NULL. Packets are then read with
 * tawny_vp8_next. Returns 0 when it went in. */
int tawny_vp8_encode(tawny_vp8 *e, const uint8_t *i420, size_t len, int64_t pts_ms,
                     uint32_t dur_ms, int force_key) {
  if (!e) return -1;
  e->iter = NULL;
  if (!i420) return vpx_codec_encode(&e->ctx, NULL, pts_ms, 1, 0, VPX_DL_REALTIME) == VPX_CODEC_OK ? 0 : -1;
  if (len != (size_t)e->w * e->h * 3 / 2) return -1;
  vpx_image_t img;
  /* Wrapped, not copied; libvpx only reads it during this call. */
  if (!vpx_img_wrap(&img, VPX_IMG_FMT_I420, e->w, e->h, 1, (unsigned char *)i420)) return -1;
  vpx_codec_err_t r = vpx_codec_encode(&e->ctx, &img, pts_ms, dur_ms ? dur_ms : 1,
                                       force_key ? VPX_EFLAG_FORCE_KF : 0, VPX_DL_REALTIME);
  return r == VPX_CODEC_OK ? 0 : -1;
}

/* The next encoded frame since the last encode: copied into out (cap bytes),
 * its size, time and whether it is a keyframe. 1: a frame, 0: no more,
 * -1: it did not fit. */
int tawny_vp8_next(tawny_vp8 *e, uint8_t *out, size_t cap, size_t *len, int64_t *pts_ms, int *key) {
  if (!e || !out || !len || !pts_ms || !key) return -1;
  const vpx_codec_cx_pkt_t *pkt;
  while ((pkt = vpx_codec_get_cx_data(&e->ctx, &e->iter)) != NULL) {
    if (pkt->kind != VPX_CODEC_CX_FRAME_PKT) continue;
    if (pkt->data.frame.sz > cap) return -1;
    memcpy(out, pkt->data.frame.buf, pkt->data.frame.sz);
    *len = pkt->data.frame.sz;
    *pts_ms = pkt->data.frame.pts;
    *key = (pkt->data.frame.flags & VPX_FRAME_IS_KEY) ? 1 : 0;
    return 1;
  }
  return 0;
}

void tawny_vp8_free(tawny_vp8 *e) {
  if (!e) return;
  vpx_codec_destroy(&e->ctx);
  free(e);
}

/* ---- Opus ------------------------------------------------------------------ */

/* A 48 kHz mono encoder; its lookahead (the stream's pre-skip) in samples. */
OpusEncoder *tawny_opus_new(int32_t *lookahead) {
  if (!lookahead) return NULL;
  int err = 0;
  OpusEncoder *o = opus_encoder_create(48000, 1, OPUS_APPLICATION_AUDIO, &err);
  if (err != OPUS_OK || !o) return NULL;
  opus_encoder_ctl(o, OPUS_SET_BITRATE(48000));
  opus_int32 la = 0;
  if (opus_encoder_ctl(o, OPUS_GET_LOOKAHEAD(&la)) != OPUS_OK) la = 312;
  *lookahead = la;
  return o;
}

/* Encodes exactly 960 samples (20 ms). Returns the packet's size, or < 0. */
int32_t tawny_opus_encode(OpusEncoder *o, const int16_t *pcm, size_t samples, uint8_t *out, int32_t cap) {
  if (!o || !pcm || !out || samples != 960 || cap < 1) return -1;
  return opus_encode(o, pcm, 960, out, cap);
}

void tawny_opus_free(OpusEncoder *o) {
  if (o) opus_encoder_destroy(o);
}
