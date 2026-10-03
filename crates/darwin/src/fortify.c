#include <limits.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

int rosemary_sprintf_chk(char *s, int flags __attribute__((unused)),
                                size_t slen, const char *format, ...) {
  va_list arg;
  int done;

  va_start(arg, format);
  if (slen > (size_t)INT_MAX)
    done = vsprintf(s, format, arg);
  else {
    done = vsnprintf(s, slen, format, arg);
    if (done >= 0 && (size_t)done >= slen)
      abort();
  }
  va_end(arg);
  return done;
}
