#ifndef LKV_H
#define LKV_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define LKV_OK 0
#define LKV_NOT_FOUND 1
#define LKV_BUFFER_TOO_SMALL 2
#define LKV_INVALID_ARGUMENT 3
#define LKV_IO_ERROR 4
#define LKV_CORRUPTED 5
#define LKV_UNSUPPORTED 6
#define LKV_BUSY 7
#define LKV_DATABASE_FULL 8
#define LKV_MAINTENANCE_REQUIRED 9
#define LKV_POISONED 10
#define LKV_PANIC 255

#define LKV_VERIFICATION_ON_READ 0
#define LKV_VERIFICATION_FULL 1

typedef int32_t lkv_status;

typedef struct lkv_database lkv_database;
typedef struct lkv_snapshot lkv_snapshot;
typedef struct lkv_write_batch lkv_write_batch;

typedef struct lkv_options {
  uint32_t verification;
  size_t overlay_memory_limit;
  uint64_t max_database_bytes;
} lkv_options;

typedef struct lkv_database_stats {
  uint64_t storage_bytes;
  uint64_t base_bytes;
  size_t base_entries;
  size_t overlay_entries;
  uint64_t overlay_log_bytes;
  size_t overlay_memory_bytes;
  uint64_t stale_bytes;
  uint64_t generation;
} lkv_database_stats;

typedef int32_t (*lkv_visit_fn)(void *context,
                                const uint8_t *key,
                                size_t key_len,
                                const uint8_t *value,
                                size_t value_len);

uint32_t lkv_version(void);

const char *lkv_last_error_message(void);

lkv_status lkv_options_init(lkv_options *output);

lkv_status lkv_database_open(const char *path,
                             const lkv_options *options,
                             lkv_database **output);

lkv_status lkv_database_create(const char *path,
                               const lkv_options *options,
                               lkv_database **output);
lkv_status lkv_database_open_memory(const lkv_options *options,
                                    lkv_database **output);
lkv_status lkv_database_close(lkv_database *database);

lkv_status lkv_database_get(const lkv_database *database,
                            const uint8_t *key,
                            size_t key_len,
                            uint8_t *value,
                            size_t value_capacity,
                            size_t *value_len);

lkv_status lkv_database_get_ref(const lkv_database *database,
                                const uint8_t *key,
                                size_t key_len,
                                const uint8_t **value,
                                size_t *value_len);

lkv_status lkv_database_put(const lkv_database *database,
                            const uint8_t *key,
                            size_t key_len,
                            const uint8_t *value,
                            size_t value_len);

lkv_status lkv_database_delete(const lkv_database *database,
                               const uint8_t *key,
                               size_t key_len);

lkv_status lkv_database_len(const lkv_database *database, size_t *output);
lkv_status lkv_database_overlay_memory_usage(const lkv_database *database,
                                             size_t *output);
lkv_status lkv_database_get_stats(const lkv_database *database,
                                  lkv_database_stats *output);
lkv_status lkv_database_sync(const lkv_database *database);
lkv_status lkv_database_verify(const lkv_database *database);
lkv_status lkv_database_compact(const lkv_database *database);
lkv_status lkv_database_vacuum(const lkv_database *database);

lkv_status lkv_database_visit(const lkv_database *database,
                              lkv_visit_fn visitor,
                              void *context);

lkv_status lkv_snapshot_create(const lkv_database *database,
                               lkv_snapshot **output);
lkv_status lkv_snapshot_close(lkv_snapshot *snapshot);

lkv_status lkv_snapshot_get(const lkv_snapshot *snapshot,
                            const uint8_t *key,
                            size_t key_len,
                            uint8_t *value,
                            size_t value_capacity,
                            size_t *value_len);

lkv_status lkv_snapshot_get_ref(const lkv_snapshot *snapshot,
                                const uint8_t *key,
                                size_t key_len,
                                const uint8_t **value,
                                size_t *value_len);
lkv_status lkv_snapshot_visit(const lkv_snapshot *snapshot,
                              lkv_visit_fn visitor,
                              void *context);

lkv_status lkv_write_batch_create(lkv_write_batch **output);
lkv_status lkv_write_batch_close(lkv_write_batch *batch);
lkv_status lkv_write_batch_clear(lkv_write_batch *batch);
lkv_status lkv_write_batch_len(const lkv_write_batch *batch, size_t *output);
lkv_status lkv_write_batch_put(lkv_write_batch *batch,
                               const uint8_t *key,
                               size_t key_len,
                               const uint8_t *value,
                               size_t value_len);
lkv_status lkv_write_batch_delete(lkv_write_batch *batch,
                                  const uint8_t *key,
                                  size_t key_len);
lkv_status lkv_database_commit_write_batch(const lkv_database *database,
                                           lkv_write_batch *batch);

#ifdef __cplusplus
}
#endif

#endif /* LKV_H */
