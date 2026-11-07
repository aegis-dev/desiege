/*
 * desiege_mod.h - code mod API for Desiege.
 *
 * A code mod is a 32-bit Windows DLL (Dungeon Siege is a 32-bit game) that exports
 *
 *     int desiege_mod_init(const DesiegeApi *api);
 *
 * desiege.dll loads every enabled code mod when the game starts and calls desiege_mod_init
 * once, on the game's render thread, before anything is drawn. Return 0 on success; any other
 * value makes Desiege unload the mod again (it logs the value).
 *
 * Rules:
 * - Everything here uses the C calling convention (__cdecl), including your callbacks.
 * - The `api` pointer and its functions stay valid for the lifetime of the process.
 * - All callbacks run on the game's render thread. Keep them fast: they run every frame.
 * - Check `api->version` and `api->size` before using functions added in later versions.
 *   Desiege only ever appends to this struct, so older code mods keep working.
 *
 * Desiege is licensed under the GPL-3.0; see LICENSE in the Desiege repository.
 */

#ifndef DESIEGE_MOD_H
#define DESIEGE_MOD_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define DESIEGE_API_VERSION 1

/* Text alignment for draw_text: x is the left edge, the centre or the right edge. */
#define DESIEGE_ALIGN_LEFT   0
#define DESIEGE_ALIGN_CENTER 1
#define DESIEGE_ALIGN_RIGHT  2

typedef void (__cdecl *DesiegeFrameFn)(void *user);

typedef struct DesiegeApi {
    /* DESIEGE_API_VERSION of the running Desiege, and sizeof(DesiegeApi) as it knows it. */
    uint32_t version;
    uint32_t size;

    /* Append a line to %LOCALAPPDATA%\Desiege\desiege.log (UTF-8, no trailing newline needed). */
    void (__cdecl *log)(const char *utf8);

    /* Call `fn(user)` once per frame, right before the frame is shown. This is the place to
     * call draw_text. May be called from desiege_mod_init only. */
    void (__cdecl *on_frame)(DesiegeFrameFn fn, void *user);

    /* Draw text on top of this frame (call it from an on_frame callback). Coordinates are
     * screen pixels of the real display, origin top-left; see screen_size. `argb` is the
     * colour (alpha = 0xFF for opaque); `size_px` is the font height in pixels (6..200).
     * Text gets a soft shadow for legibility. */
    void (__cdecl *draw_text)(int32_t x, int32_t y, const char *utf8, uint32_t argb, int32_t size_px, uint32_t align);

    /* Size of the real display in pixels (not the game's virtual resolution under UI scale). */
    void (__cdecl *screen_size)(int32_t *width, int32_t *height);

    /* Seconds since an arbitrary start point, high resolution. */
    double (__cdecl *time_seconds)(void);

    /* How much larger than its original pixel size the game's UI is drawn (1.0 without UI
     * scale, 2.4 for the 4:3 menus on a 1440p screen, ...). Multiply your sizes and offsets by
     * it to line up with the game's interface. */
    float (__cdecl *ui_scale)(void);
} DesiegeApi;

/* Implement this in your code mod. */
__declspec(dllexport) int __cdecl desiege_mod_init(const DesiegeApi *api);

#ifdef __cplusplus
}
#endif

#endif /* DESIEGE_MOD_H */
