/* Hand-written config.h for the oxbow libweston port (replaces meson's generated one).
 * Software path only: no EGL/GL, no libinput/udev/dbus/systemd. musl 1.2.5 provides the
 * probed optional libc funcs. See docs/weston-port.md. */
#ifndef OXBOW_WESTON_CONFIG_H
#define OXBOW_WESTON_CONFIG_H

#define _GNU_SOURCE 1
#define _ALL_SOURCE 1

#define PACKAGE_STRING "weston 9.0.0"
#define PACKAGE_VERSION "9.0.0"
#define VERSION "9.0.0"
#define PACKAGE_URL "https://wayland.freedesktop.org"
#define PACKAGE_BUGREPORT "https://gitlab.freedesktop.org/wayland/weston/issues/"

/* install dirs — unused at runtime on oxbow (no filesystem module loading), but the
 * sources reference them. */
#define BINDIR "/bin"
#define DATADIR "/share"
#define LIBEXECDIR "/libexec"
#define MODULEDIR "/lib/weston"
#define LIBWESTON_MODULEDIR "/lib/libweston"
#define WESTON_NATIVE_BACKEND "fbdev-backend.so"

/* musl 1.2.5 has all of these. */
#define HAVE_MKOSTEMP 1
#define HAVE_STRCHRNUL 1
#define HAVE_POSIX_FALLOCATE 1
#define HAVE_MEMFD_CREATE 1

/* xkbcommon compose is available in oxxkb. */
#define HAVE_XKBCOMMON_COMPOSE 1

#endif
