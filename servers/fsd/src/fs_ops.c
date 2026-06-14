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
	if (r == EOK)
		r = ext4_fread(&f, buf, len, rd);
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
	int r = ext4_fremove(path);
	if (r == EOK)
		return EOK;
	return ext4_dir_rm(path);
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
