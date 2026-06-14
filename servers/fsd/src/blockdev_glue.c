/* blockdev_glue — the lwext4 block device (physical I/O implemented in Rust as
 * ox_bread/ox_bwrite, talking to the virtio-blk service over IPC) plus small C
 * wrappers that hide lwext4's stack structs from the Rust side. */
#include <stdint.h>
#include <stddef.h>
#include <string.h>
#include "ext4.h"
#include "ext4_mkfs.h"
#include "ext4_fs.h"
#include "ext4_blockdev.h"
#include "ext4_errno.h"

/* The virtio-blk disk: 512-byte sectors. oxbow-disk.img is 16 MiB = 32768. */
#define OX_BSIZE 512u
#define OX_BCNT 32768u

int ox_open(struct ext4_blockdev *bdev);
int ox_bread(struct ext4_blockdev *bdev, void *buf, uint64_t blk_id, uint32_t blk_cnt);
int ox_bwrite(struct ext4_blockdev *bdev, const void *buf, uint64_t blk_id, uint32_t blk_cnt);
int ox_close(struct ext4_blockdev *bdev);

EXT4_BLOCKDEV_STATIC_INSTANCE(oxblk, OX_BSIZE, OX_BCNT, ox_open, ox_bread, ox_bwrite,
			      ox_close, 0, 0);

struct ext4_blockdev *oxblk_get(void)
{
	return &oxblk;
}

/* Format the device as ext2 (1 KiB blocks, no journal). */
int oxfs_mkfs_ext2(struct ext4_blockdev *bd)
{
	struct ext4_fs fs;
	struct ext4_mkfs_info info;
	memset(&info, 0, sizeof info);
	info.block_size = 1024;
	info.journal = false;
	return ext4_mkfs(&fs, bd, &info, F_SET_EXT2);
}

/* Read / write a single u32 to a file path (the boot-counter self-test). */
int oxfs_read_u32(const char *path, uint32_t *out)
{
	ext4_file f;
	size_t rc = 0;
	int r = ext4_fopen(&f, path, "rb");
	if (r != EOK)
		return r;
	r = ext4_fread(&f, out, sizeof(*out), &rc);
	ext4_fclose(&f);
	if (rc != sizeof(*out))
		return EIO;
	return r;
}

int oxfs_write_u32(const char *path, uint32_t val)
{
	ext4_file f;
	size_t wc = 0;
	int r = ext4_fopen(&f, path, "wb");
	if (r != EOK)
		return r;
	r = ext4_fwrite(&f, &val, sizeof val, &wc);
	ext4_fclose(&f);
	return r;
}
