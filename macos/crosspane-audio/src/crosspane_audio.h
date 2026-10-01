/* Crosspane audio driver (WP-3.4): constants frozen by WP-3.0b / WP-3.4.
 *
 * Own original code (no sample or third-party driver code is copied). This header has no
 * CoreAudio dependency so the owner probe can share the exact UID literals with the driver.
 */
#ifndef CROSSPANE_AUDIO_H
#define CROSSPANE_AUDIO_H

/* ---- Frozen topology and driver constants (WP-3.4 "Frozen topology and driver behavior") --- */
#define CROSSPANE_AUDIO_BUNDLE_ID "io.frostdev.crosspane.audio.driver"
#define CROSSPANE_SPEAKERS_APP_UID "io.frostdev.crosspane.audio.v0.speakers.app"
#define CROSSPANE_SPEAKERS_LOOPBACK_UID "io.frostdev.crosspane.audio.v0.speakers.loopback"
#define CROSSPANE_MIC_APP_UID "io.frostdev.crosspane.audio.v0.microphone.app"
#define CROSSPANE_MIC_LOOPBACK_UID "io.frostdev.crosspane.audio.v0.microphone.loopback"
#define CROSSPANE_AUDIO_RATE 48000
#define CROSSPANE_AUDIO_HISTORY_FRAMES 16384
#define CROSSPANE_AUDIO_TRANSFER_DELAY_FRAMES 1024
#define CROSSPANE_AUDIO_TIMESTAMP_PERIOD 16384
#define CROSSPANE_AUDIO_CLOCK_DOMAIN 0x43504130
#define CROSSPANE_AUDIO_MAX_CALLBACK_FRAMES 4096

/* ---- Names, plug-in identity ------------------------------------------------------------- */
#define CROSSPANE_AUDIO_MANUFACTURER "Frostdev"
#define CROSSPANE_AUDIO_MODEL_UID "io.frostdev.crosspane.audio.v0.model"
#define CROSSPANE_SPEAKERS_APP_NAME "Crosspane speakers"
#define CROSSPANE_SPEAKERS_LOOPBACK_NAME "Crosspane speakers loopback"
#define CROSSPANE_MIC_APP_NAME "Crosspane microphone"
#define CROSSPANE_MIC_LOOPBACK_NAME "Crosspane microphone loopback"
/* Stable CFPlugIn factory UUID; the build script copies it into Info.plist. */
#define CROSSPANE_AUDIO_FACTORY_UUID "35FEF394-1825-43D6-90C3-CDD937F58D6A"

/* ---- Limits chosen by this implementation (documented in the driver) --------------------- */
#define CROSSPANE_AUDIO_MAX_CLIENTS 64 /* registered clients per device */

/* ---- Static object IDs: one plug-in, four devices, four streams, all distinct ------------ */
enum {
    CROSSPANE_OBJ_PLUGIN = 1,
    CROSSPANE_DEV_SPEAKERS_APP = 2,      /* visible, output, stereo   */
    CROSSPANE_DEV_SPEAKERS_LOOPBACK = 3, /* hidden,  input,  stereo   */
    CROSSPANE_DEV_MIC_APP = 4,           /* visible, input,  mono     */
    CROSSPANE_DEV_MIC_LOOPBACK = 5,      /* hidden,  output, mono     */
    CROSSPANE_STREAM_SPEAKERS_APP = 6,
    CROSSPANE_STREAM_SPEAKERS_LOOPBACK = 7,
    CROSSPANE_STREAM_MIC_APP = 8,
    CROSSPANE_STREAM_MIC_LOOPBACK = 9
};

#endif
