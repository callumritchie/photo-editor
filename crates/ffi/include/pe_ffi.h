/* C interface to the photo-editor core (crates/ffi). See crates/ffi/src/lib.rs for details.
 * Strings: NUL-terminated UTF-8; free returned strings with pe_string_free.
 * Buffers: free with pe_bytes_free(ptr, len). JSON errors look like {"error":"..."}. */
#ifndef PE_FFI_H
#define PE_FFI_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct PeCatalog PeCatalog;
typedef void (*PeProgressFn)(void *ctx, uint64_t done, uint64_t total);

PeCatalog *pe_catalog_open(const char *path, bool create, char **err_out);
void pe_catalog_close(PeCatalog *h);

char *pe_catalog_folders_json(PeCatalog *h);
char *pe_catalog_images_json(PeCatalog *h, const char *query_json);
char *pe_catalog_import_json(PeCatalog *h, const char *sources_json, const char *options_json,
                             PeProgressFn progress, void *ctx);

bool pe_catalog_set_rating(PeCatalog *h, const int64_t *ids, size_t count, uint8_t rating);
bool pe_catalog_set_flag(PeCatalog *h, const int64_t *ids, size_t count, int8_t flag);
bool pe_catalog_set_label(PeCatalog *h, const int64_t *ids, size_t count, const char *label);

/* Grid thumbnail JPEG; never blocks on a render (stale/placeholder returned, render queued). */
uint8_t *pe_catalog_thumbnail(PeCatalog *h, int64_t id, size_t *out_len);

/* Background renders: cached 2048 px loupe previews and edited thumbnails. */
typedef void (*PeRenderedFn)(void *ctx, int64_t image_id, bool thumbnail_changed);
uint8_t *pe_catalog_preview(PeCatalog *h, int64_t id, bool queue_if_stale, size_t *out_len, bool *out_fresh);
void pe_catalog_request_renders(PeCatalog *h, const int64_t *ids, size_t count, bool urgent);
size_t pe_catalog_pending_renders(PeCatalog *h);
void pe_catalog_set_render_callback(PeCatalog *h, PeRenderedFn callback, void *ctx);
uint8_t *pe_quick_image(const char *path, uint8_t orientation, uint32_t long_edge, uint32_t *w, uint32_t *h);
uint8_t *pe_render(const char *path, const char *settings_json, uint32_t long_edge, uint32_t *w, uint32_t *h);

/* Develop */
char *pe_develop_schema_json(void);
char *pe_catalog_develop_state_json(PeCatalog *h, int64_t id);
char *pe_catalog_develop_json(PeCatalog *h, const int64_t *ids, size_t count, const char *command_json);
char *pe_catalog_presets_json(PeCatalog *h);
char *pe_catalog_save_preset_json(PeCatalog *h, const char *name, const char *groups_json, const char *settings_json);
bool pe_catalog_delete_preset(PeCatalog *h, int64_t id);

/* Library: {"command": "collections" | "createCollection" | "addKeywords" | "stack" | ...} (see LibraryCommand) */
char *pe_catalog_library_json(PeCatalog *h, const char *command_json);

/* Export: settings/presets are ExportSettings JSON; returns {"written":[...],"skipped":[...],"failed":[[path,err],...]} */
char *pe_catalog_export_json(PeCatalog *h, const int64_t *ids, size_t count, const char *settings_json,
                             PeProgressFn progress, void *ctx);
char *pe_export_defaults_json(void);
char *pe_catalog_export_presets_json(PeCatalog *h);
char *pe_catalog_save_export_preset_json(PeCatalog *h, const char *name, const char *settings_json);
bool pe_catalog_delete_export_preset(PeCatalog *h, int64_t id);

/* Interactive preview: decode once, re-render quickly with new settings */
typedef struct PeImage PeImage;
PeImage *pe_image_open(const char *path, uint32_t long_edge);
void pe_image_close(PeImage *h);
char *pe_image_info_json(PeImage *h);
uint8_t *pe_image_render(PeImage *h, const char *settings_json, uint32_t *w, uint32_t *hh);
/* Mask rasters (subject / sky selections): 8-bit grey, fractions of the oriented, uncropped frame. */
char *pe_catalog_save_raster(PeCatalog *h, int64_t image_id, uint32_t w, uint32_t hh, const uint8_t *data, size_t len);
bool pe_image_attach_rasters(PeImage *img, PeCatalog *h, int64_t image_id);
bool pe_image_add_raster(PeImage *img, const char *id, uint32_t w, uint32_t hh, const uint8_t *data, size_t len);
uint8_t *pe_image_select_sky(PeImage *img, uint32_t *w, uint32_t *hh);
/* Mask overlay (8-bit grey over the oriented, uncropped preview frame) and colour picking (OKLab). */
uint8_t *pe_image_mask_overlay(PeImage *img, const char *settings_json, uint32_t index, uint32_t *w, uint32_t *hh);
bool pe_image_pick_color(PeImage *img, const char *settings_json, float fx, float fy, float *out_lab);
/* Convert n (x, y) points in place between output fractions and mask (frame) fractions. */
/* Lens database folder (call once at start-up). */
void pe_set_lens_database(const char *dir);
/* {"lens": recorded lens or null, "profile": matched database lens or null}; free with pe_string_free. */
char *pe_image_lens_profile(PeImage *img);
bool pe_image_map_points(PeImage *img, const char *settings_json, bool to_frame, float *pts, size_t n);

/* 1:1 viewing: full-resolution output size, and a rectangle of it rendered at 1:1 (RGBA8). */
bool pe_image_output_size(PeImage *img, const char *settings_json, uint32_t *w, uint32_t *hh);
uint8_t *pe_image_render_region(PeImage *img, const char *settings_json, uint32_t x, uint32_t y, uint32_t rw,
                                uint32_t rh, uint32_t *w, uint32_t *hh);

/* "Apple M1 (Metal)" or "CPU": the renderer used by pe_image_render. */
char *pe_renderer_name(void);

void pe_string_free(char *s);
void pe_bytes_free(uint8_t *p, size_t len);

#endif
