/*
 * fs_squashfs.h — C ABI for the pure-Rust read-only SquashFS driver.
 *
 * Link against libfs_squashfs.a and #include this header. UTF-8 paths,
 * NULL / -1 failure sentinels with thread-local error detail via
 * fs_squashfs_last_error() / fs_squashfs_last_errno().
 *
 * SquashFS is read-only: there is no mkfs / create / write surface.
 *
 * MIT License — see LICENSE
 */

#ifndef FS_SQUASHFS_H
#define FS_SQUASHFS_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle to a mounted SquashFS filesystem. */
typedef struct fs_squashfs_fs fs_squashfs_fs_t;

/* File type enumeration (matches the directory-entry type byte). */
typedef enum {
    FS_SQUASHFS_FT_UNKNOWN  = 0,
    FS_SQUASHFS_FT_REG_FILE = 1,
    FS_SQUASHFS_FT_DIR      = 2,
    FS_SQUASHFS_FT_CHRDEV   = 3,
    FS_SQUASHFS_FT_BLKDEV   = 4,
    FS_SQUASHFS_FT_FIFO     = 5,
    FS_SQUASHFS_FT_SOCK     = 6,
    FS_SQUASHFS_FT_SYMLINK  = 7,
} fs_squashfs_file_type_t;

/* File/directory attributes. `mode` carries permission bits only; combine
 * with the type bits implied by `file_type` to form a full st_mode. */
typedef struct {
    uint32_t inode;
    uint16_t mode;
    uint32_t uid;
    uint32_t gid;
    uint64_t size;
    uint32_t mtime;
    uint32_t link_count;
    uint32_t file_type;   /* fs_squashfs_file_type_t */
} fs_squashfs_attr_t;

/* Directory entry (returned during iteration). */
typedef struct {
    uint32_t inode;
    uint8_t  file_type;   /* fs_squashfs_file_type_t */
    uint8_t  name_len;
    char     name[256];   /* null-terminated */
} fs_squashfs_dirent_t;

/* Volume information snapshotted from the superblock. */
typedef struct {
    uint32_t block_size;
    uint16_t compression_id;     /* 1=gzip 2=lzma 3=lzo 4=xz 5=lz4 6=zstd */
    char     compression_name[16];
    uint32_t inode_count;
    uint32_t fragment_count;
    uint16_t id_count;
    uint64_t bytes_used;
    uint32_t mkfs_time;          /* unix epoch seconds */
    uint16_t version_major;
    uint16_t version_minor;
    uint16_t flags;
} fs_squashfs_volume_info_t;

/* ---- Block device callback interface (read-only) ---- */

/*
 * Read callback. Must read exactly `length` bytes at `offset` into `buf`.
 * Returns 0 on success, non-zero on error. `context` is passed back
 * verbatim from fs_squashfs_blockdev_cfg_t.
 */
typedef int (*fs_squashfs_read_fn)(void *context, void *buf,
                                   uint64_t offset, uint64_t length);

typedef struct {
    fs_squashfs_read_fn read;
    void   *context;      /* opaque; e.g. an FSBlockDeviceResource pointer */
    uint64_t size_bytes;  /* total device / partition size */
    uint32_t block_size;  /* physical block size (e.g. 512); informational */
} fs_squashfs_blockdev_cfg_t;

/* ---- Lifecycle ---- */

/* Mount from a device/image path (direct POSIX I/O). NULL on failure. */
fs_squashfs_fs_t *fs_squashfs_mount(const char *device_path);

/* Mount via a caller-supplied read callback (sandboxed FSKit path). */
fs_squashfs_fs_t *fs_squashfs_mount_with_callbacks(
    const fs_squashfs_blockdev_cfg_t *cfg);

/*
 * Mount via an FsCoreDevice handle from a sister crate (e.g.
 * fs_core_device_from_callbacks / fs_core_device_slice_ro from am-fs-core,
 * qcow2_open from am-img-qcow2). The handle's refcount is incremented
 * internally; the caller still owns its *FsCoreDevice and frees it via
 * fs_core_device_close. Forward declared — full definition in fs_core.h.
 * NULL on failure.
 */
struct FsCoreDevice;
fs_squashfs_fs_t *fs_squashfs_mount_with_fs_core_device(struct FsCoreDevice *handle);

/* Unmount and free all resources. Safe to call with NULL. */
void fs_squashfs_umount(fs_squashfs_fs_t *fs);

/* ---- Queries ---- */

/* Fill `info` from the superblock. Returns 0 on success, -1 on failure. */
int fs_squashfs_get_volume_info(fs_squashfs_fs_t *fs,
                                fs_squashfs_volume_info_t *info);

/* Stat a path (relative to mount root, e.g. "/etc/passwd"). Symlinks are
 * NOT followed. Returns 0 on success, -1 on failure. */
int fs_squashfs_stat(fs_squashfs_fs_t *fs, const char *path,
                     fs_squashfs_attr_t *attr);

/* ---- Directory listing ---- */

typedef struct fs_squashfs_dir_iter fs_squashfs_dir_iter_t;

/* Open a directory for iteration. NULL on failure. */
fs_squashfs_dir_iter_t *fs_squashfs_dir_open(fs_squashfs_fs_t *fs,
                                             const char *path);

/* Next entry. Returns a pointer into the iterator's buffer (valid until
 * the next call / close), or NULL at end. */
const fs_squashfs_dirent_t *fs_squashfs_dir_next(fs_squashfs_dir_iter_t *iter);

/* Close + free a directory iterator. */
void fs_squashfs_dir_close(fs_squashfs_dir_iter_t *iter);

/* ---- File / symlink reading ---- */

/* Read file contents. Returns bytes read, or -1 on error. */
int64_t fs_squashfs_read_file(fs_squashfs_fs_t *fs, const char *path,
                              void *buf, uint64_t offset, uint64_t length);

/* Read a symlink target (NUL-terminated) into buf. Returns 0 on success,
 * -1 on failure (e.g. buffer too small -> errno ERANGE). */
int fs_squashfs_readlink(fs_squashfs_fs_t *fs, const char *path,
                         char *buf, size_t bufsize);

/* ---- Extended attributes (read only) ---- */

/*
 * List extended attribute names for a path.
 *
 * Writes NUL-separated fully-qualified names ("user.colour\0user.tag\0")
 * into buf. If buf is NULL or bufsize is 0, no bytes are written but the
 * return value still reports the required total size -- use this to probe.
 *
 * Names come back assembled. SquashFS stores the namespace prefix as a
 * small integer and the rest of the name after it; a caller does not have
 * to know that.
 *
 * A path with no attributes returns 0, and so does every path in an image
 * built with -no-xattrs. Neither is an error.
 *
 * Returns: total bytes of output (names + NUL terminators) on success,
 *          -1 on error. If bufsize is less than the required size, writes
 *          as many WHOLE names as fit and still returns the required size.
 *
 * Signature and semantics match fs_ext4_listxattr, so a layer above can
 * treat the drivers alike.
 */
int64_t fs_squashfs_listxattr(fs_squashfs_fs_t *fs, const char *path,
                              char *buf, size_t bufsize);

/*
 * Get one extended attribute value by fully-qualified name
 * (e.g. "user.colour").
 *
 * Writes raw value bytes (no NUL terminator) into buf. If buf is NULL or
 * bufsize is 0, returns the value size without writing -- use this to probe.
 *
 * A zero-length value is a real value and returns 0, which is NOT an
 * error; an absent attribute returns -1 with ENOENT. Check the return
 * against -1, not against 0.
 *
 * Returns: value size in bytes on success,
 *          -1 if the name is not present or on error. If bufsize is less
 *          than the value size, writes as much as fits and still returns
 *          the value size.
 */
int64_t fs_squashfs_getxattr(fs_squashfs_fs_t *fs, const char *path,
                             const char *name, void *buf, size_t bufsize);

/* ---- Error reporting ---- */

/* Last error message for the current thread (valid until next FFI call). */
const char *fs_squashfs_last_error(void);

/* POSIX errno for the last failed FFI call on this thread (0 if none). */
int fs_squashfs_last_errno(void);

#ifdef __cplusplus
}
#endif

#endif /* FS_SQUASHFS_H */
