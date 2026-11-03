#include <stdint.h>
#ifdef _WIN32
#define API __declspec(dllexport)
#else
#define API
#endif
API uint32_t native_negotiate(uint32_t requested) { return requested == 1 ? 1 : 0; }
API int64_t native_add(int64_t a, int64_t b) { return (int64_t)((uint64_t)a + (uint64_t)b); }
