/* Kiseki native client — C FFI header (Phase 10.5).
 *
 * Link with: -lkiseki_client
 * Build: cargo build -p kiseki-client --features ffi
 */

#ifndef KISEKI_CLIENT_H
#define KISEKI_CLIENT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle to a Kiseki client session. */
typedef struct KisekiHandle KisekiHandle;

/* Status codes. */
typedef enum {
    KISEKI_OK              = 0,
    KISEKI_NOT_FOUND       = 1,
    KISEKI_PERMISSION      = 2,
    KISEKI_IO_ERROR        = 3,
    KISEKI_INVALID_ARG     = 4,
    KISEKI_NOT_CONNECTED   = 5,
    KISEKI_TIMED_OUT       = 6,
} KisekiStatus;

/* Cache statistics. */
typedef struct {
    uint64_t l1_hits;
    uint64_t l2_hits;
    uint64_t misses;
    uint64_t bypasses;
    uint64_t errors;
    uint64_t l1_bytes;
    uint64_t l2_bytes;
    uint64_t meta_hits;
    uint64_t meta_misses;
    uint64_t wipes;
} KisekiCacheStats;

/* Open a connection to a Kiseki cluster.
 * seed_addr: "host:port" of a seed node (null-terminated).
 * handle_out: receives the allocated handle (free with kiseki_close). */
KisekiStatus kiseki_open(const char *seed_addr, KisekiHandle **handle_out);

/* Close a client session and free resources. */
KisekiStatus kiseki_close(KisekiHandle *handle);

/* Read data from an object.
 * path: object path (null-terminated).
 * offset: byte offset to start reading.
 * buf / buf_len: output buffer.
 * bytes_read: actual bytes read (output). */
KisekiStatus kiseki_read(KisekiHandle *handle, const char *path,
                         uint64_t offset, uint8_t *buf, uint64_t buf_len,
                         uint64_t *bytes_read);

/* Write data to an object (creates or overwrites).
 * path: object path (null-terminated).
 * data / data_len: input buffer.
 * bytes_written: actual bytes written (output). */
KisekiStatus kiseki_write(KisekiHandle *handle, const char *path,
                          const uint8_t *data, uint64_t data_len,
                          uint64_t *bytes_written);

/* Get object attributes (size).
 * path: object path (null-terminated).
 * size_out: object size in bytes (output). */
KisekiStatus kiseki_stat(KisekiHandle *handle, const char *path,
                         uint64_t *size_out);

/* Stage a dataset into the local cache.
 * path: namespace path (null-terminated).
 * timeout_secs: maximum seconds to wait for staging to complete. */
KisekiStatus kiseki_stage(KisekiHandle *handle, const char *path,
                          uint32_t timeout_secs);

/* Release a previously staged dataset.
 * path: namespace path (null-terminated). */
KisekiStatus kiseki_release(KisekiHandle *handle, const char *path);

/* Get current cache statistics.
 * stats_out: pointer to a KisekiCacheStats struct (output). */
KisekiStatus kiseki_cache_stats(KisekiHandle *handle,
                                KisekiCacheStats *stats_out);

#ifdef __cplusplus
} /* extern "C" */

/* C++ wrapper (Phase 10.5). */
#include <string>
#include <stdexcept>
#include <vector>

namespace kiseki {

class ClientError : public std::runtime_error {
public:
    KisekiStatus status;
    ClientError(KisekiStatus s, const char *msg)
        : std::runtime_error(msg), status(s) {}
};

class Client {
    KisekiHandle *handle_ = nullptr;
public:
    explicit Client(const std::string &seed_addr) {
        KisekiStatus s = kiseki_open(seed_addr.c_str(), &handle_);
        if (s != KISEKI_OK)
            throw ClientError(s, "kiseki_open failed");
    }
    ~Client() { if (handle_) kiseki_close(handle_); }

    Client(const Client &) = delete;
    Client &operator=(const Client &) = delete;
    Client(Client &&o) noexcept : handle_(o.handle_) { o.handle_ = nullptr; }
    Client &operator=(Client &&o) noexcept {
        if (handle_) kiseki_close(handle_);
        handle_ = o.handle_; o.handle_ = nullptr;
        return *this;
    }

    std::vector<uint8_t> read(const std::string &path, uint64_t offset = 0,
                               uint64_t len = 1024*1024) {
        std::vector<uint8_t> buf(len);
        uint64_t got = 0;
        KisekiStatus s = kiseki_read(handle_, path.c_str(), offset,
                                     buf.data(), len, &got);
        if (s != KISEKI_OK) throw ClientError(s, "kiseki_read failed");
        buf.resize(got);
        return buf;
    }

    uint64_t write(const std::string &path, const uint8_t *data, uint64_t len) {
        uint64_t written = 0;
        KisekiStatus s = kiseki_write(handle_, path.c_str(), data, len, &written);
        if (s != KISEKI_OK) throw ClientError(s, "kiseki_write failed");
        return written;
    }

    uint64_t stat(const std::string &path) {
        uint64_t size = 0;
        KisekiStatus s = kiseki_stat(handle_, path.c_str(), &size);
        if (s != KISEKI_OK) throw ClientError(s, "kiseki_stat failed");
        return size;
    }

    void stage(const std::string &path, uint32_t timeout_secs = 300) {
        KisekiStatus s = kiseki_stage(handle_, path.c_str(), timeout_secs);
        if (s != KISEKI_OK) throw ClientError(s, "kiseki_stage failed");
    }

    void release(const std::string &path) {
        KisekiStatus s = kiseki_release(handle_, path.c_str());
        if (s != KISEKI_OK) throw ClientError(s, "kiseki_release failed");
    }

    KisekiCacheStats cache_stats() {
        KisekiCacheStats stats = {};
        KisekiStatus s = kiseki_cache_stats(handle_, &stats);
        if (s != KISEKI_OK) throw ClientError(s, "kiseki_cache_stats failed");
        return stats;
    }
};

} /* namespace kiseki */
#endif /* __cplusplus */

#endif /* KISEKI_CLIENT_H */
