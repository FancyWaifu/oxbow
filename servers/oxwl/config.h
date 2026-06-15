/* Hand-written config.h for the libwayland port on oxbow. Normally meson
 * generates this; we declare the minimal set and leave optional Linux features
 * (accept4, memfd, mremap, ucred, broken-cmsg) UNDEFINED so wayland-os.c picks
 * its portable fallbacks. */
#ifndef OXBOW_WL_CONFIG_H
#define OXBOW_WL_CONFIG_H

#define PACKAGE_VERSION "1.22.0"
#define WAYLAND_VERSION "1.22.0"

/* Deliberately NOT defined (use fallbacks):
 *   HAVE_ACCEPT4, HAVE_SYS_UCRED_H, HAVE_XUCRED, HAVE_BROKEN_MSG_CMSG_CLOEXEC,
 *   HAVE_MEMFD_CREATE, HAVE_MREMAP, HAVE_POSIX_FALLOCATE, HAVE_MKOSTEMP,
 *   HAVE_SYS_PRCTL_H, HAVE_SYS_RANDOM_H
 */

#endif
