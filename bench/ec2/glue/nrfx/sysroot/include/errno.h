/* A stand-in for the C library's errno.h, for compiling only. */
#ifndef STUB_ERRNO_H
#define STUB_ERRNO_H
extern int errno;
#define EPERM 1
#define ENOENT 2
#define EIO 5
#define ENXIO 6
#define E2BIG 7
#define EAGAIN 11
#define ENOMEM 12
#define EACCES 13
#define EFAULT 14
#define EBUSY 16
#define EEXIST 17
#define ENODEV 19
#define EINVAL 22
#define ENOSPC 28
#define ERANGE 34
#define ENOSYS 38
#define ENODATA 61
#define ETIME 62
#define EBADMSG 74
#define EOVERFLOW 75
#define ENOTSUP 95
#define ECONNRESET 104
#define ENOBUFS 105
#define ETIMEDOUT 110
#define EALREADY 114
#define EINPROGRESS 115
#define ECANCELED 125
#endif
