/* fstest — Stage 1 of the lwext4/ext2 port. Prove the vendored, BSD-clean
 * lwext4 builds and runs on oxbow: mkfs an ext2 filesystem on a RAM-backed
 * block device, mount it, and do real file + directory I/O. The block-service
 * (virtio-blk) backing and the fs-server rewrite come in later stages. */
#include <stdint.h>
#include <stddef.h>
#include <string.h>
#include <stdio.h>
#include "ext4.h"
#include "ext4_mkfs.h"
#include "ext4_fs.h"
#include "ext4_blockdev.h"
#include "ext4_errno.h"
#include "ext4_super.h"

/* A 4 MiB RAM disk: 512-byte physical blocks, like a real sector device. */
#define BSIZE 512u
#define BCNT  8192u
static uint8_t ramdisk[BSIZE * BCNT];

static int rd_open(struct ext4_blockdev *b) { (void)b; return EOK; }
static int rd_close(struct ext4_blockdev *b) { (void)b; return EOK; }
static int rd_bread(struct ext4_blockdev *b, void *buf, uint64_t blk, uint32_t cnt)
{
	(void)b;
	memcpy(buf, ramdisk + blk * BSIZE, (size_t)cnt * BSIZE);
	return EOK;
}
static int rd_bwrite(struct ext4_blockdev *b, const void *buf, uint64_t blk, uint32_t cnt)
{
	(void)b;
	memcpy(ramdisk + blk * BSIZE, buf, (size_t)cnt * BSIZE);
	return EOK;
}

EXT4_BLOCKDEV_STATIC_INSTANCE(ramdev, BSIZE, BCNT, rd_open, rd_bread, rd_bwrite,
			      rd_close, 0, 0);

int main(void)
{
	printf("[fstest] lwext4 ext2 on a 4 MiB RAM disk\n");

	static struct ext4_fs fs;
	static struct ext4_mkfs_info info;
	memset(&info, 0, sizeof info);
	info.block_size = 1024;
	info.journal = false;

	int r = ext4_mkfs(&fs, &ramdev, &info, F_SET_EXT2);
	printf("[fstest] mkfs ext2: r=%d\n", r);
	if (r != EOK)
		return 1;

	r = ext4_device_register(&ramdev, "extdev");
	printf("[fstest] device_register: r=%d\n", r);
	r = ext4_mount("extdev", "/mp/", false);
	printf("[fstest] mount /mp/: r=%d\n", r);
	if (r != EOK)
		return 1;

	r = ext4_dir_mk("/mp/data");
	printf("[fstest] mkdir /mp/data: r=%d\n", r);

	ext4_file f;
	size_t wc = 0, rc = 0;
	const char *msg = "Hello from a real ext2 filesystem on oxbow!";
	r = ext4_fopen(&f, "/mp/data/hello.txt", "wb");
	if (r == EOK) {
		r = ext4_fwrite(&f, msg, strlen(msg), &wc);
		ext4_fclose(&f);
	}
	printf("[fstest] wrote %u bytes (r=%d)\n", (unsigned)wc, r);

	char buf[128];
	rc = 0;
	r = ext4_fopen(&f, "/mp/data/hello.txt", "rb");
	if (r == EOK) {
		r = ext4_fread(&f, buf, sizeof buf - 1, &rc);
		ext4_fclose(&f);
	}
	buf[rc] = 0;
	printf("[fstest] read %u bytes: \"%s\"\n", (unsigned)rc, buf);

	printf("[fstest] ls /mp/data:\n");
	ext4_dir d;
	if (ext4_dir_open(&d, "/mp/data") == EOK) {
		const ext4_direntry *de;
		while ((de = ext4_dir_entry_next(&d)) != NULL)
			printf("    %s\n", de->name);
		ext4_dir_close(&d);
	}

	ext4_umount("/mp/");
	printf("[fstest] DONE - ext2 read/write works on oxbow\n");
	return 0;
}
