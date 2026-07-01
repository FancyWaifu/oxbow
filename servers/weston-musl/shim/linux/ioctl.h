/* oxbow shim: musl-oxbow's linux-headers ships asm/ioctl.h but not this wrapper, which
 * weston's linux-sync-file-uapi.h includes. The canonical linux/ioctl.h is exactly this. */
#include <asm/ioctl.h>
