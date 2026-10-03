#include <limits.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

#define likely(x) __builtin_expect(!!(x), 1)
#define unlikely(x) __builtin_expect(!!(x), 0)

void __rosemary_stack_chk_failure(const char *msg) {
  fprintf(stderr, "rosemary: stack check failed: %s\n", msg);
  abort();
}

int rosemary_sprintf_chk(char *s, int flags __attribute__((unused)),
                         size_t slen, const char *format, ...) {
  va_list arg;
  int done;

  va_start(arg, format);
  if (unlikely(slen > (size_t)INT_MAX))
    done = vsprintf(s, format, arg);
  else {
    done = vsnprintf(s, slen, format, arg);
    if (unlikely(done >= 0 && (size_t)done >= slen))
      __rosemary_stack_chk_failure("done >= 0 && (size_t)done >= slen");
  }
  va_end(arg);
  return done;
}
