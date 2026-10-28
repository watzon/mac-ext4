/*
 * ext4fs.h - C interface to the Rust ext2/3/4 engine.
 *
 * Conventions:
 *  - Every function that can fail returns 0 on success or a positive POSIX
 *    errno (Darwin numbering) on failure.
 *  - Names are raw bytes (not NUL-terminated) with an explicit length.
 *  - Inode numbers are passed as uint64_t. The root directory is
 *    EXT4FS_ROOT_INO.
 *  - All functions on one ext4fs_volume are thread-safe.
 *  - Functions that return a message write a NUL-terminated UTF-8 string of
 *    at most msg_cap bytes into msg (msg may be NULL when msg_cap is 0).
 */
#ifndef EXT4FS_H
#define EXT4FS_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define EXT4FS_ROOT_INO 2

/* ---- Block device supplied by the caller ------------------------------ */

/* Callbacks return 0 or errno. Reads and writes must transfer exactly len
 * bytes. Offsets and lengths passed to the callbacks are always multiples of
 * `alignment`. */
typedef struct ext4fs_device {
    void *ctx;
    int32_t (*read)(void *ctx, uint64_t offset, void *buf, size_t len);
    int32_t (*write)(void *ctx, uint64_t offset, const void *buf, size_t len);
    int32_t (*flush)(void *ctx);
    uint64_t size;      /* bytes */
    uint32_t alignment; /* physical sector size in bytes (1 = none) */
    bool read_only;
} ext4fs_device;

/* ---- Probe ------------------------------------------------------------- */

typedef struct ext4fs_probe_info {
    char label[68];      /* NUL-terminated UTF-8 (lossy) */
    uint8_t uuid[16];
    char flavor[8];      /* "ext2", "ext3", or "ext4" */
    uint32_t block_size;
    uint64_t blocks;
    bool readable;       /* the engine can mount it */
    bool writable;       /* the engine can mount it read-write */
} ext4fs_probe_info;

/* Returns 0 if the device holds an ext2/3/4 file system. EINVAL if not. */
int32_t ext4fs_probe(const ext4fs_device *dev, ext4fs_probe_info *out, char *msg, size_t msg_cap);

/* ---- Mount ------------------------------------------------------------- */

typedef struct ext4fs_volume ext4fs_volume;

#define EXT4FS_MOUNT_READ_ONLY        0x1u
#define EXT4FS_MOUNT_IGNORE_CHECKSUMS 0x2u

/* The device struct is copied; dev->ctx must stay valid until unmount.
 * Replays the journal if needed. On success *out is a new volume. If the
 * volume cannot be written, it mounts read-only and *read_only_out is true
 * (msg then explains why). */
int32_t ext4fs_mount(const ext4fs_device *dev, uint32_t flags, ext4fs_volume **out,
                     bool *read_only_out, char *msg, size_t msg_cap);

/* Commits all changes, marks the file system clean, and frees the volume.
 * The volume pointer is invalid afterwards, even on error. */
int32_t ext4fs_unmount(ext4fs_volume *vol);

/* Commits the running transaction and flushes the device. */
int32_t ext4fs_sync(ext4fs_volume *vol);

/* ---- Attributes -------------------------------------------------------- */

typedef struct ext4fs_timespec {
    int64_t sec;
    uint32_t nsec;
} ext4fs_timespec;

/* BSD file flags reported in ext4fs_attr.flags (subset of <sys/stat.h>). */
#define EXT4FS_UF_NODUMP     0x00000001u
#define EXT4FS_UF_IMMUTABLE  0x00000002u
#define EXT4FS_UF_APPEND     0x00000004u

typedef struct ext4fs_attr {
    uint64_t ino;
    uint64_t parent_ino;   /* 0 if unknown (non-directories) */
    uint32_t mode;         /* type and permission bits, as in st_mode */
    uint32_t uid;
    uint32_t gid;
    uint32_t nlink;
    uint64_t size;
    uint64_t alloc_size;   /* bytes allocated on disk */
    uint32_t flags;        /* EXT4FS_UF_* */
    uint32_t rdev;         /* makedev(major, minor) for device nodes */
    uint32_t generation;
    bool has_btime;
    ext4fs_timespec atime;
    ext4fs_timespec mtime;
    ext4fs_timespec ctime;
    ext4fs_timespec btime;
} ext4fs_attr;

typedef struct ext4fs_statfs {
    uint32_t block_size;
    uint64_t total_blocks;
    uint64_t free_blocks;
    uint64_t avail_blocks;   /* free minus root-reserved */
    uint64_t total_inodes;
    uint64_t free_inodes;
    uint32_t name_max;
    uint8_t uuid[16];
    char label[68];
    char flavor[8];
    bool read_only;
} ext4fs_statfs;

int32_t ext4fs_statfs_get(ext4fs_volume *vol, ext4fs_statfs *out);

/* Set the volume label (at most 16 bytes). */
int32_t ext4fs_set_label(ext4fs_volume *vol, const uint8_t *label, size_t len);

int32_t ext4fs_getattr(ext4fs_volume *vol, uint64_t ino, ext4fs_attr *out);

/* Look up name in dir. ENOENT if absent. */
int32_t ext4fs_lookup(ext4fs_volume *vol, uint64_t dir, const uint8_t *name, size_t name_len,
                      ext4fs_attr *out);

/* Which fields of ext4fs_setattr to apply. */
#define EXT4FS_SET_MODE   0x01u  /* permission bits only; type bits ignored */
#define EXT4FS_SET_UID    0x02u
#define EXT4FS_SET_GID    0x04u
#define EXT4FS_SET_SIZE   0x08u
#define EXT4FS_SET_ATIME  0x10u
#define EXT4FS_SET_MTIME  0x20u
#define EXT4FS_SET_CTIME  0x40u
#define EXT4FS_SET_BTIME  0x80u
#define EXT4FS_SET_FLAGS  0x100u

typedef struct ext4fs_setattr {
    uint32_t valid;   /* EXT4FS_SET_* */
    uint32_t mode;
    uint32_t uid;
    uint32_t gid;
    uint64_t size;
    uint32_t flags;   /* EXT4FS_UF_* */
    ext4fs_timespec atime;
    ext4fs_timespec mtime;
    ext4fs_timespec ctime;
    ext4fs_timespec btime;
} ext4fs_setattr;

int32_t ext4fs_setattr_apply(ext4fs_volume *vol, uint64_t ino, const ext4fs_setattr *attr,
                             ext4fs_attr *out);

/* ---- Directories ------------------------------------------------------- */

/* Directory entry file types (match ext4 dirent file_type). */
#define EXT4FS_DT_UNKNOWN 0
#define EXT4FS_DT_REG     1
#define EXT4FS_DT_DIR     2
#define EXT4FS_DT_CHR     3
#define EXT4FS_DT_BLK     4
#define EXT4FS_DT_FIFO    5
#define EXT4FS_DT_SOCK    6
#define EXT4FS_DT_LNK     7

/* Called once per entry, in order, including "." and "..".
 * next_cookie resumes enumeration after this entry. attr is NULL for "."
 * and "..", and for every entry when the caller did not ask for attributes;
 * it is valid for this call only.
 * Return false to stop (for example when the caller's buffer is full).
 * The callback must not call back into this volume: the volume stays
 * locked for the whole enumeration. */
typedef bool (*ext4fs_dirent_fn)(void *ctx, const uint8_t *name, size_t name_len, uint64_t ino,
                                 uint8_t file_type, uint64_t next_cookie, const ext4fs_attr *attr);

/* Enumerate dir starting at cookie (0 = start). With with_attrs, each entry
 * other than "." and ".." comes with its attributes (as ext4fs_getattr).
 * *verifier_out receives a value that changes whenever the directory is
 * modified. */
int32_t ext4fs_readdir(ext4fs_volume *vol, uint64_t dir, uint64_t cookie, bool with_attrs,
                       ext4fs_dirent_fn fn, void *ctx, uint64_t *verifier_out);

/* ---- Files ------------------------------------------------------------- */

int32_t ext4fs_read(ext4fs_volume *vol, uint64_t ino, uint64_t offset, void *buf, size_t len,
                    size_t *read_out);

int32_t ext4fs_write(ext4fs_volume *vol, uint64_t ino, uint64_t offset, const void *buf, size_t len,
                     size_t *written_out);

/* Writes the symlink target (not NUL-terminated) into buf. ERANGE if cap is
 * too small; *len_out always receives the full length. */
int32_t ext4fs_readlink(ext4fs_volume *vol, uint64_t ino, uint8_t *buf, size_t cap, size_t *len_out);

/* ---- Namespace changes ------------------------------------------------- */

/* Create a regular file, directory, FIFO, socket, or device node.
 * mode includes the S_IFMT type bits. rdev is used for device nodes. */
int32_t ext4fs_create(ext4fs_volume *vol, uint64_t dir, const uint8_t *name, size_t name_len,
                      uint32_t mode, uint32_t uid, uint32_t gid, uint32_t rdev, ext4fs_attr *out);

int32_t ext4fs_symlink(ext4fs_volume *vol, uint64_t dir, const uint8_t *name, size_t name_len,
                       const uint8_t *target, size_t target_len, uint32_t uid, uint32_t gid,
                       ext4fs_attr *out);

/* Add a hard link to ino named name in dir. */
int32_t ext4fs_link(ext4fs_volume *vol, uint64_t ino, uint64_t dir, const uint8_t *name,
                    size_t name_len, ext4fs_attr *out);

/* Remove name from dir (unlink for files, rmdir for empty directories).
 * An inode whose link count reaches zero stays allocated until
 * ext4fs_reclaim is called for it. */
int32_t ext4fs_remove(ext4fs_volume *vol, uint64_t dir, const uint8_t *name, size_t name_len);

#define EXT4FS_RENAME_NOREPLACE 0x1u
#define EXT4FS_RENAME_EXCHANGE  0x2u

/* Rename; replaces an existing destination of a compatible type unless
 * EXT4FS_RENAME_NOREPLACE. */
int32_t ext4fs_rename(ext4fs_volume *vol, uint64_t src_dir, const uint8_t *src_name, size_t src_len,
                      uint64_t dst_dir, const uint8_t *dst_name, size_t dst_len, uint32_t flags);

/* The host no longer references ino. Frees it if it has no links left. */
int32_t ext4fs_reclaim(ext4fs_volume *vol, uint64_t ino);

/* ---- Extended attributes ----------------------------------------------- */
/* Names use the macOS view: ext4 "user.foo" appears as "foo". Other ext4
 * namespaces appear with a "linux." prefix (for example
 * "linux.security.selinux") and are read-only. */

/* Writes NUL-separated names into buf. ERANGE if cap is too small;
 * *len_out always receives the full length. cap 0 queries the size. */
int32_t ext4fs_listxattr(ext4fs_volume *vol, uint64_t ino, uint8_t *buf, size_t cap, size_t *len_out);

/* ENOATTR if absent. ERANGE if cap is too small; *len_out receives the
 * value size. cap 0 queries the size. */
int32_t ext4fs_getxattr(ext4fs_volume *vol, uint64_t ino, const uint8_t *name, size_t name_len,
                        uint8_t *buf, size_t cap, size_t *len_out);

#define EXT4FS_XATTR_CREATE  0x1u /* fail with EEXIST if present */
#define EXT4FS_XATTR_REPLACE 0x2u /* fail with ENOATTR if absent */

int32_t ext4fs_setxattr(ext4fs_volume *vol, uint64_t ino, const uint8_t *name, size_t name_len,
                        const uint8_t *value, size_t value_len, uint32_t flags);

int32_t ext4fs_removexattr(ext4fs_volume *vol, uint64_t ino, const uint8_t *name, size_t name_len);

/* ---- Format and check -------------------------------------------------- */

typedef struct ext4fs_mkfs_options {
    const char *label;       /* NULL or NUL-terminated, at most 16 bytes */
    uint32_t revision;       /* 2, 3, or 4 */
    uint32_t block_size;     /* 0 = automatic, else 1024/2048/4096/65536 */
    uint32_t inode_size;     /* 0 = default (256) */
    uint32_t inode_ratio;    /* bytes per inode, 0 = default (16384) */
    uint32_t reserved_percent; /* root-reserved blocks percentage; 0xFFFFFFFF = default (5) */
} ext4fs_mkfs_options;

/* Progress callback: fraction in [0, 1]. May be NULL. */
typedef void (*ext4fs_progress_fn)(void *ctx, double fraction);

int32_t ext4fs_mkfs(const ext4fs_device *dev, const ext4fs_mkfs_options *opts,
                    ext4fs_progress_fn progress, void *progress_ctx, char *msg, size_t msg_cap);

#define EXT4FS_CHECK_REPAIR 0x1u /* replay journal and fix what can be fixed */
#define EXT4FS_CHECK_QUICK  0x2u /* only check the clean flag and journal state */
#define EXT4FS_CHECK_FORCE  0x4u /* with REPAIR: full scan even if replay leaves it clean */

/* Called for each problem found (NUL-terminated UTF-8). May be NULL. */
typedef void (*ext4fs_message_fn)(void *ctx, const char *message);

/* Checks consistency. Returns 0 if clean, EIO if problems remain, or another
 * errno if the check could not run. */
int32_t ext4fs_check(const ext4fs_device *dev, uint32_t flags, ext4fs_message_fn message,
                     void *message_ctx, ext4fs_progress_fn progress, void *progress_ctx);

#ifdef __cplusplus
}
#endif

#endif /* EXT4FS_H */
