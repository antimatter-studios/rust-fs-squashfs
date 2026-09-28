/*
 * fs_squashfs.h — C ABI for the pure-Rust read-only SquashFS driver.
 *
 * Link against libfs_squashfs.a and #include this header. NULL / -1
 * failure sentinels with thread-local error detail via
 * fs_squashfs_last_error() / fs_squashfs_last_errno().
 *
 * PATHS ARE BYTES, NOT TEXT. Every `const char *path` is read as the
 * bytes up to the NUL and compared byte for byte against the names in
 * the image. It is never decoded, so no encoding is assumed and none
 * is required: UTF-8 works because UTF-8 is a byte string, and so does
 * anything else. SquashFS directory entry names are raw bytes and the
 * format has no field that could say what encoding they are in — an
 * image built on a box with a non-UTF-8 locale, or copied off a legacy
 * volume, holds names that are not valid UTF-8, and they are ordinary
 * rather than hostile.
 *
 * So a name fs_squashfs_dir_next() hands you can always be handed
 * straight back to fs_squashfs_stat(), fs_squashfs_read_file() and the
 * rest. It could not before, and those entries were visible, listed and
 * unopenable.
 *
 * A consumer whose own namespace requires valid UTF-8 — macOS, where
 * APFS and FSKit do — should escape such a name REVERSIBLY (percent-
 * encoding, or surrogate escapes) so it can be turned back into these
 * bytes. A lossy conversion maps distinct names onto one and makes two
 * files indistinguishable.
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
    /* Block/char devices: the device number as stored, Linux
     * new_encode_dev packing -- major = (rdev & 0xfff00) >> 8,
     * minor = (rdev & 0xff) | ((rdev >> 12) & 0xfff00). 0 for every
     * other type. Appended last, so earlier fields keep their offsets, but
     * the struct grew: a compiled consumer must rebuild against this. */
    uint32_t rdev;
} fs_squashfs_attr_t;

/* Directory entry (returned during iteration). */
typedef struct {
    uint32_t inode;
    uint8_t  file_type;   /* fs_squashfs_file_type_t */
    uint16_t name_len;    /* bytes before the NUL; up to 256 */
    char     name[257];   /* null-terminated; SquashFS names are <= 256 bytes */
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

/* Stat by inode number, through the image's export table.
 *
 * The number is the one fs_squashfs_stat and fs_squashfs_dir_next already
 * hand back in their `inode` field. Without this a caller that keeps those
 * numbers has no way to turn one back into a file except by walking the
 * tree again.
 *
 * Returns 0 on success, or -1 with:
 *   ENOTSUP  the image was built with `mksquashfs -no-exports` and carries
 *            no such map -- this will never succeed for this image;
 *   ENOENT   the image has no inode with that number.
 */
int fs_squashfs_stat_ino(fs_squashfs_fs_t *fs, uint32_t inode_number,
                         fs_squashfs_attr_t *attr);

/* Whether this image can answer fs_squashfs_stat_ino at all: 1 yes, 0 no,
 * -1 on a NULL handle. Worth asking once at mount. */
int fs_squashfs_is_exportable(fs_squashfs_fs_t *fs);

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

/*
 * Read a symlink target into buf.
 *
 * On success returns the target's length in bytes, NOT counting the NUL --
 * the same number Linux readlink(2) returns -- and writes the target
 * followed by a NUL terminator into buf. Test for success with `>= 0`,
 * not `== 0`: a link's length is its result.
 *
 * If bufsize < length + 1 (including bufsize == 0), returns -1 with
 * fs_squashfs_last_errno() == ERANGE, fs_squashfs_last_error() naming the
 * size needed, and NOTHING written into buf. This deliberately differs
 * from readlink(2), which truncates silently: a truncated target is a
 * wrong answer that looks like a right one.
 *
 * NULL fs, path or buf returns -1 with errno EINVAL. Any other
 * failure returns -1 with errno set: ENOENT for a missing path, EINVAL
 * for a path that is not a symlink, EIO for a corrupt image.
 */
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
