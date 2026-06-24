/* fs_ops — thin C wrappers over lwext4 that hide its stack structs, so the Rust
 * FS server can serve the oxbow FS IPC protocol (one path string per call; the
 * server keeps the id->path table). All paths are absolute lwext4 paths
 * ("/mp/..."). Return 0 (EOK) on success or the lwext4 error code. */
#include <stdint.h>
#include <stddef.h>
#include <string.h>
#include "ext4.h"
#include "ext4_errno.h"
#include "ext4_oflags.h"
#include "ext4_types.h"
#include "ext4_inode.h" /* ext4_inode_get_mode/size/time accessors (single-walk statx2) */

/* Stat: is_dir = 1 for a directory, 0 for a regular file; size in bytes. */
int oxfs_stat(const char *path, int *is_dir, uint64_t *size)
{
	ext4_dir d;
	if (ext4_dir_open(&d, path) == EOK) {
		*is_dir = 1;
		*size = 0;
		ext4_dir_close(&d);
		return EOK;
	}
	ext4_file f;
	if (ext4_fopen(&f, path, "rb") == EOK) {
		*is_dir = 0;
		*size = ext4_fsize(&f);
		ext4_fclose(&f);
		return EOK;
	}
	return ENOENT;
}

/* Read len bytes at offset off into buf; *rd gets the count read. */
int oxfs_pread(const char *path, uint64_t off, void *buf, size_t len, size_t *rd)
{
	ext4_file f;
	*rd = 0;
	int r = ext4_fopen(&f, path, "rb");
	if (r != EOK)
		return r;
	r = ext4_fseek(&f, (int64_t)off, SEEK_SET);
	/* Loop: lwext4's ext4_fread can return a SHORT count for a multi-block (or even a
	 * single-block, across an indirect boundary) request without being at EOF. The
	 * block read cache stores the returned length per 4 KiB block and returns 0 for
	 * offsets past it, so a short read here truncated reads of files like DOOM's WAD
	 * mid-lump. Keep reading until `len` is satisfied or a genuine EOF (chunk == 0). */
	while (r == EOK && *rd < len) {
		size_t chunk = 0;
		r = ext4_fread(&f, (char *)buf + *rd, len - *rd, &chunk);
		if (r != EOK || chunk == 0)
			break;
		*rd += chunk;
	}
	ext4_fclose(&f);
	return r;
}

/* Write len bytes at offset off (no truncate); *wr gets the count written. */
int oxfs_pwrite(const char *path, uint64_t off, const void *buf, size_t len, size_t *wr)
{
	ext4_file f;
	*wr = 0;
	int r = ext4_fopen(&f, path, "r+b"); /* existing file, keep contents */
	if (r != EOK)
		return r;
	r = ext4_fseek(&f, (int64_t)off, SEEK_SET);
	if (r == EOK)
		r = ext4_fwrite(&f, buf, len, wr);
	ext4_fclose(&f);
	return r;
}

/* Create or truncate a regular file. */
int oxfs_create(const char *path)
{
	ext4_file f;
	int r = ext4_fopen(&f, path, "wb");
	if (r == EOK)
		ext4_fclose(&f);
	return r;
}

int oxfs_mkdir(const char *path)
{
	return ext4_dir_mk(path);
}

/* Remove a regular file, or an empty directory. */
int oxfs_remove(const char *path)
{
	/* A directory must go through ext4_dir_rm; ext4_fremove returns EOK on a dir
	 * WITHOUT removing it, so std::fs::remove_dir silently no-op'd. Dispatch by
	 * type: if the path opens as a directory, rm the directory; else fremove. */
	ext4_dir d;
	if (ext4_dir_open(&d, path) == EOK) {
		ext4_dir_close(&d);
		return ext4_dir_rm(path);
	}
	return ext4_fremove(path);
}

int oxfs_rename(const char *path, const char *new_path)
{
	return ext4_frename(path, new_path);
}

/* Flush the lwext4 block cache to disk (so a write survives a reboot). */
int oxfs_flush(void)
{
	return ext4_cache_flush("/mp/");
}

/* Toggle write-back caching: on=fast batch (seeding), off=write-through+flush. */
int oxfs_writeback(int on)
{
	return ext4_cache_write_back("/mp/", on ? true : false);
}

/* Directory entry at index `cursor` (skipping "." and ".."): name (NUL-term) into
 * name_out (<= cap bytes), is_dir set. Returns 0 if present, -1 past the end. */
int oxfs_readdir(const char *path, uint32_t cursor, char *name_out, uint32_t cap, int *is_dir)
{
	ext4_dir d;
	if (ext4_dir_open(&d, path) != EOK)
		return -1;
	const ext4_direntry *de;
	uint32_t i = 0;
	int found = -1;
	while ((de = ext4_dir_entry_next(&d)) != NULL) {
		/* lwext4 includes "." and ".." — skip them to match the old fs. */
		if (de->name_length == 1 && de->name[0] == '.')
			continue;
		if (de->name_length == 2 && de->name[0] == '.' && de->name[1] == '.')
			continue;
		if (i == cursor) {
			uint32_t n = de->name_length;
			if (n >= cap)
				n = cap - 1;
			memcpy(name_out, de->name, n);
			name_out[n] = 0;
			*is_dir = (de->inode_type == EXT4_DE_DIR);
			found = 0;
			break;
		}
		i++;
	}
	ext4_dir_close(&d);
	return found;
}

/* Read ext2 second-resolution mtime/atime (Unix epoch). Best-effort: a missing
 * time leaves the out-param at 0. */
int oxfs_times(const char *path, uint32_t *mtime, uint32_t *atime)
{
	uint32_t m = 0, a = 0;
	ext4_mtime_get(path, &m);
	ext4_atime_get(path, &a);
	*mtime = m;
	*atime = a;
	return 0;
}

/* set_len: truncate (or extend) the file to `size` bytes. */
int oxfs_truncate(const char *path, uint64_t size)
{
	ext4_file f;
	int r = ext4_fopen(&f, path, "r+b");
	if (r != EOK)
		return r;
	uint64_t cur = ext4_fsize(&f);
	if (size <= cur) {
		/* lwext4's ext4_ftruncate only shrinks. */
		r = ext4_ftruncate(&f, size);
	} else {
		/* Grow: ext4_fseek won't go past EOF, so append zero bytes from the
		 * current end up to `size` (POSIX set_len zero-extends). */
		static const uint8_t zeros[512] = {0};
		r = ext4_fseek(&f, (int64_t)cur, SEEK_SET);
		uint64_t remaining = size - cur;
		while (r == EOK && remaining > 0) {
			size_t chunk = remaining > sizeof(zeros) ? sizeof(zeros) : (size_t)remaining;
			size_t wr = 0;
			r = ext4_fwrite(&f, zeros, chunk, &wr);
			if (wr == 0)
				break;
			remaining -= wr;
		}
	}
	ext4_fclose(&f);
	return r;
}

/* Set mtime and/or atime (Unix epoch seconds), gated by set_m/set_a. */
int oxfs_set_times(const char *path, uint32_t mtime, uint32_t atime, int set_m, int set_a)
{
	int r = EOK;
	if (set_m) {
		int rr = ext4_mtime_set(path, mtime);
		if (rr != EOK)
			r = rr;
	}
	if (set_a) {
		int rr = ext4_atime_set(path, atime);
		if (rr != EOK)
			r = rr;
	}
	return r;
}

/* Type-aware stat: kind (1=dir, 2=file, 3=symlink), size, mtime/atime. Uses
 * ext4_mode_get for existence+type so a symlink is detected without following it. */
int oxfs_statx2(const char *path, int *kind, uint64_t *size, uint32_t *mtime, uint32_t *atime)
{
	/* ONE path walk. ext4_raw_inode_fill resolves path -> inode and reads it; the
	 * kind, size, and times then come straight from the in-memory inode struct (no
	 * further path resolution). This replaces FIVE separate path-based lwext4 calls
	 * the old version made for a regular file (ext4_dir_open + ext4_mode_get +
	 * ext4_fopen-for-size + ext4_mtime_get + ext4_atime_get), each of which re-walked
	 * the ext2 directory tree — that redundant walking was the dominant fs-open cost
	 * (TAG_FS_OPEN calls this on every open). The mount's superblock (needed by the
	 * mode/size accessors) is constant, so it's fetched once. */
	uint32_t ino;
	struct ext4_inode inode;
	if (ext4_raw_inode_fill(path, &ino, &inode) != EOK)
		return -1; /* not found */
	static struct ext4_sblock *sb;
	if (!sb)
		ext4_get_sblock("/mp/", &sb);
	uint32_t mode = sb ? ext4_inode_get_mode(sb, &inode) : 0;
	switch (mode & 0xF000) {
	case 0x4000: /* S_IFDIR */
		*kind = 1;
		*size = 0; /* dir size is irrelevant to the protocol (readdir uses a cursor) */
		break;
	case 0xA000: /* S_IFLNK — inode size is the link-target length, like the old readlink */
		*kind = 3;
		*size = sb ? ext4_inode_get_size(sb, &inode) : 0;
		break;
	default: /* regular file */
		*kind = 2;
		*size = sb ? ext4_inode_get_size(sb, &inode) : 0;
		break;
	}
	*mtime = ext4_inode_get_modif_time(&inode);
	*atime = ext4_inode_get_access_time(&inode);
	return 0;
}

/* Create a symlink at `linkpath` whose contents are the literal `target`. */
int oxfs_symlink(const char *target, const char *linkpath)
{
	return ext4_fsymlink(target, linkpath);
}

/* Read a symlink's target into `buf`; returns the byte count via `*rcnt` (0 on error). */
int oxfs_readlink(const char *path, char *buf, size_t bufsize, size_t *rcnt)
{
	*rcnt = 0;
	return ext4_readlink(path, buf, bufsize, rcnt);
}

/* Create a hard link `dst` referring to the same inode as `src`. */
int oxfs_link(const char *src, const char *dst)
{
	return ext4_flink(src, dst);
}
