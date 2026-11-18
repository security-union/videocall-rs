// Minimal stand-in for rtc_base/checks.h so the golden generator does not
// need rtc_base/checks.cc. Checks abort; messages are discarded.
#ifndef RTC_BASE_CHECKS_H_
#define RTC_BASE_CHECKS_H_
#include <stdlib.h>
#define RTC_DCHECK_IS_ON 0
#define RTC_NORETURN __attribute__((__noreturn__))
#ifdef __cplusplus
namespace stubchecks {
struct Fatal {
  template <typename T> Fatal& operator<<(const T&) { return *this; }
  ~Fatal() { abort(); }
};
struct Sink {
  template <typename T> Sink& operator<<(const T&) { return *this; }
};
}  // namespace stubchecks
#define RTC_CHECK(c) if (c) {} else ::stubchecks::Fatal()
#define RTC_DCHECK(c) if (true) {} else ::stubchecks::Sink()
#else
#define RTC_CHECK(c) do { if (!(c)) abort(); } while (0)
#define RTC_DCHECK(c) do { } while (0)
#endif
#define RTC_CHECK_OP(a, op, b) RTC_CHECK((a) op (b))
#define RTC_CHECK_EQ(a, b) RTC_CHECK_OP(a, ==, b)
#define RTC_CHECK_NE(a, b) RTC_CHECK_OP(a, !=, b)
#define RTC_CHECK_LE(a, b) RTC_CHECK_OP(a, <=, b)
#define RTC_CHECK_LT(a, b) RTC_CHECK_OP(a, <, b)
#define RTC_CHECK_GE(a, b) RTC_CHECK_OP(a, >=, b)
#define RTC_CHECK_GT(a, b) RTC_CHECK_OP(a, >, b)
#define RTC_DCHECK_EQ(a, b) RTC_DCHECK((a) == (b))
#define RTC_DCHECK_NE(a, b) RTC_DCHECK((a) != (b))
#define RTC_DCHECK_LE(a, b) RTC_DCHECK((a) <= (b))
#define RTC_DCHECK_LT(a, b) RTC_DCHECK((a) < (b))
#define RTC_DCHECK_GE(a, b) RTC_DCHECK((a) >= (b))
#define RTC_DCHECK_GT(a, b) RTC_DCHECK((a) > (b))
#define RTC_DCHECK_NOTREACHED() do { } while (0)
#define RTC_CHECK_NOTREACHED() abort()
#endif
